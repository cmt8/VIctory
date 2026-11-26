//! `xmem.dll`: console memory, served from host memory. The GPU side works
//! from CPU addresses (unified memory), so memory types and bandwidth
//! classes change nothing here; what constrains addresses (alignment,
//! 2 MiB pages, address-bit limits) is honoured.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::{Layout, alloc, dealloc};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_COMMITMENT_LIMIT, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER,
    MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3,
    MemExtendedParameterAddressRequirements, PAGE_NOACCESS, PAGE_READWRITE, SEC_COMMIT,
    SEC_RESERVE, UnmapViewOfFile, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualProtect,
    VirtualQueryEx,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION,
    PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::core::BOOL;

use crate::{HleModule, hle_export};

const MIB: usize = 1 << 20;

/// Whether we run under Wine (its ntdll exports `wine_get_version`).
pub(crate) fn on_wine() -> bool {
    static WINE: LazyLock<bool> = LazyLock::new(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        // SAFETY: plain lookups in ntdll, always loaded.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
        }
    });
    *WINE
}

/// Standard memory the title has handed to the system.
static TRANSFERRED: AtomicUsize = AtomicUsize::new(0);

// Console additions to the host's flags.
/// 2 MiB pages; the host reads this bit as large pages, which need a
/// privilege.
const PAGES_2M: u32 = 0x2000_0000;
/// With `PAGES_2M`: 64 KiB pages; the host reads it as AWE memory.
const PAGES_64K_BIT: u32 = 0x40_0000;
const PAGES_64K: u32 = PAGES_2M | PAGES_64K_BIT;
/// The host's protection constants and `PAGE_GUARD`. Above them the console
/// adds GPU access bits (0x800-0x40000). Write-combining and uncached are
/// dropped too: our GPU side reads this memory through the CPU.
const HOST_PROTECTION: u32 = 0x1ff;
/// XMem flags of memory both processors reach: all of ours.
const GRAPHICS: u64 = 3;
/// XMem flag bit of 32-bit graphics memory.
const ADDRESS_32BIT: u64 = 4;
/// The host's write watching (not in windows-sys' memory module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// The addresses XMem `flags` allow, first and last: the low 4 GiB for
/// 32-bit memory; else from 4 GiB (kept for 32-bit requests, which the
/// title's big pools would otherwise crowd out) up to 46 down to 40 bits
/// from the field at bit 12, or 40 bits without one: the console keeps
/// title memory there, and its ray-tracing structures must be (
/// the title's BVH builder refuses one above; the title asks for its GPU heap
/// with no field). Decision 0001.
fn address_range(flags: u64) -> (usize, usize) {
    let bits = match (flags & ADDRESS_32BIT, flags >> 12 & 7) {
        (0, 0) => 40,
        (0, field) => 47 - field,
        _ => return (0, u32::MAX as usize),
    };
    (1 << 32, (1 << bits) - 1)
}

pub extern "system" fn XMemVirtualAlloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
) -> *mut c_void {
    let (lowest, highest) = address_range(flags);
    let mut requirements = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: lowest as *mut c_void,
        HighestEndingAddress: highest as *mut c_void,
        Alignment: if kind & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M {
            2 * MIB
        } else {
            0
        },
    };
    let mut protect = match protect & HOST_PROTECTION {
        0 => PAGE_NOACCESS,
        p => p,
    };
    // A reservation of 64 KiB pages takes physical pages later
    // (`XMemMapPhysicalPages`); on the host only a placeholder can.
    // shortcut: committing part of such a reservation later would fail
    // (the title's 23 calls never do); replace that part of the
    // placeholder then.
    let placeholder = kind & PAGES_64K == PAGES_64K && kind & MEM_COMMIT == 0;
    if placeholder {
        if kind & MEM_REPLACE_PLACEHOLDER != 0 {
            // Part of a placeholder the title split: it stays one.
            return base;
        }
        protect = PAGE_NOACCESS;
    }
    let mut kind = kind & !(PAGES_2M | PAGES_64K_BIT);
    if placeholder {
        kind |= MEM_RESERVE | MEM_RESERVE_PLACEHOLDER;
    }
    let mut parameter = MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // Address requirements apply to reservations at an address of our choice.
    let constrained = base.is_null();
    if constrained {
        kind |= MEM_RESERVE;
        // The GPU side copies only the pages the title wrote since its last
        // copy (`victory_gpu`, decision 0007); placeholders cannot be
        // watched (their physical-page views are copied whole).
        if kind & MEM_RESERVE_PLACEHOLDER == 0 {
            kind |= MEM_WRITE_WATCH;
        }
    }
    // SAFETY: the title's own arguments, cleaned of console bits; the
    // parameter points at `requirements`, which outlives the call.
    let at = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            base,
            size,
            kind,
            protect,
            if constrained {
                &mut parameter
            } else {
                std::ptr::null_mut()
            },
            constrained.into(),
        )
    };
    if base.is_null() {
        tracing::debug!(
            size,
            kind = format_args!("{kind:#x}"),
            flags = format_args!("{flags:#x}"),
            at = format_args!("{at:p}"),
            "XMemVirtualAlloc"
        );
    }
    // Physical pages are mapped there only by `map` (which tells the GPU
    // side of every view), so the GPU side can say what is mapped there
    // without asking the host (`VICTORY_VIEW_BOOKS`).
    if placeholder && !at.is_null() {
        victory_gpu::title_memory_placeholder(at as usize, size);
        // A new reservation: no cut of ours inside it (`isolate`).
        if coalesce() {
            let (from, to) = (at as usize, (at as usize).saturating_add(size));
            let mut physical = physical();
            let stale: Vec<usize> = physical.cuts.range(from..to).copied().collect();
            for cut in stale {
                physical.cuts.remove(&cut);
            }
        }
    }
    at
}

/// `MEMORY_BASIC_INFORMATION` (the console's first 0x30 bytes are the
/// same), then the XMem flags and tag.
#[repr(C)]
pub struct BasicInformation {
    host: MEMORY_BASIC_INFORMATION,
    flags: u64,
    tag_code: u32,
    tag: [u8; 4],
}

// shortcut: every committed page reports graphics memory and no tag, since
// all of it is GPU-reachable here (the title's BVH builder, the
// only caller, wants graphics memory); remember the flags of
// `XMemVirtualAlloc` regions if a title tells CPU-only memory apart.
pub extern "system" fn XMemVirtualQuery(
    process: HANDLE,
    address: *const c_void,
    out: *mut BasicInformation,
) -> BOOL {
    let size = size_of::<MEMORY_BASIC_INFORMATION>();
    let timed = victory_gpu::vmstat::time(victory_gpu::vmstat::Site::TitleQuery);
    // SAFETY: the title passes room for the whole structure; the host
    // fails on a null one.
    unsafe {
        let found = VirtualQueryEx(process, address, out.cast(), size);
        drop(timed);
        if found == 0 {
            return 0;
        }
        let info = &mut *out;
        info.flags = if info.host.State == MEM_COMMIT {
            GRAPHICS
        } else {
            0
        };
        info.tag_code = 0;
        info.tag = [0; 4];
    }
    1
}

// `XMemAlloc` attributes: alignment as log2 (5 bits at 24, 4 bytes and up
// from 2), then the memory type, then bit 33: a 32-bit address.
const ALIGNMENT_SHIFT: u64 = 24;
const ALLOC_32BIT: u64 = 1 << 33;
/// Our block header: the block's size and alignment, just below the
/// pointer handed out.
const HEADER: usize = 2 * size_of::<usize>();

fn layout(size: usize, attributes: u64) -> Option<Layout> {
    // An alignment of at least 16 leaves room for the header.
    let align = 1usize << (attributes >> ALIGNMENT_SHIFT & 0x1f).max(4);
    Layout::from_size_align(size.checked_add(align)?, align).ok()
}

type AllocRoutine = extern "system" fn(usize, u64) -> *mut c_void;
type FreeRoutine = extern "system" fn(*mut c_void, u64);

/// The allocation and free routines the title's exe exports as
/// `__XMemAllocationHooks__` (the title does): `XMemAlloc` and
/// `XMemFree` call them, for the title's own calls and its middleware's;
/// the `…Default` exports do not.
fn hooks() -> [usize; 2] {
    static AT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let at = match AT.get() {
        Some(&at) => at,
        // Looked up once the title is mapped, which is before it runs.
        None => match crate::runtime::main_image() {
            Some(base) => *AT.get_or_init(|| {
                use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
                // SAFETY: a mapped image and a NUL-terminated name.
                let at = unsafe {
                    GetProcAddress(base as _, c"__XMemAllocationHooks__".as_ptr().cast())
                }
                .map_or(0, |f| f as usize);
                tracing::info!(found = at != 0, "xmem allocation hooks");
                at
            }),
            None => 0,
        },
    };
    if at == 0 {
        return [0; 2];
    }
    // SAFETY: the export is two routine pointers.
    unsafe { (at as *const [usize; 2]).read() }
}

/// The title's general allocator, for middleware too: its hook if it has
/// one, else [`XMemAllocDefault`].
pub extern "system" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
    match hooks()[0] {
        0 => XMemAllocDefault(size, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let alloc: AllocRoutine = unsafe { std::mem::transmute(hook) };
            alloc(size, attributes)
        }
    }
}

/// See [`XMemAlloc`].
pub extern "system" fn XMemFree(address: *mut c_void, attributes: u64) {
    match hooks()[1] {
        0 => XMemFreeDefault(address, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let free: FreeRoutine = unsafe { std::mem::transmute(hook) };
            free(address, attributes)
        }
    }
}

/// The system allocator behind [`XMemAlloc`].
pub extern "system" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
    if attributes & ALLOC_32BIT != 0 {
        return XMemVirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | ADDRESS_32BIT,
            PAGE_READWRITE,
        );
    }
    let Some(layout) = layout(size, attributes) else {
        return std::ptr::null_mut();
    };
    // shortcut: the host heap's addresses lie above 2^40, unlike the
    // console's; nothing seen builds ray-tracing structures in this memory
    // (the title uses its GPU heap). Sub-allocate from a
    // reservation below 2^40 if one does.
    // SAFETY: the layout has a non-zero size.
    let block = unsafe { alloc(layout) };
    if block.is_null() {
        return block.cast();
    }
    // SAFETY: the block holds the alignment (at least the header) and then
    // `size` bytes.
    unsafe {
        let user = block.add(layout.align());
        user.sub(HEADER)
            .cast::<[usize; 2]>()
            .write([layout.size(), layout.align()]);
        user.cast()
    }
}

/// Frees what [`XMemAllocDefault`] returned, given the same attributes.
pub extern "system" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
    if address.is_null() {
        return;
    }
    // SAFETY: the address came from `XMemAlloc` with these attributes: a
    // whole reservation, or a header-prefixed block.
    unsafe {
        if attributes & ALLOC_32BIT != 0 {
            VirtualFree(address, 0, MEM_RELEASE);
            return;
        }
        let user = address.cast::<u8>();
        let [size, align] = user.sub(HEADER).cast::<[usize; 2]>().read();
        dealloc(
            user.sub(align),
            Layout::from_size_align_unchecked(size, align),
        );
    }
}

/// A retail console has no tools memory.
pub extern "system" fn XMemIsToolsMemAvailable() -> BOOL {
    0
}

// Physical pages: on the console, 64 KiB frames a title maps into its own
// reservations, at more than one address if it likes. Here each allocation
// is a pagefile-backed section, its pages are 64 KiB slices of it, and a
// mapping is a view of consecutive slices in place of a host placeholder.

const PAGE: usize = 64 << 10;

struct Section {
    handle: HANDLE,
    size: usize,
    /// Under Wine: a view of the whole section of our own, mapped when a
    /// page of it is first written from outside its title views
    /// ([`View::map`]) and kept until the section goes; `None` in it if the
    /// host refused it. See [`whole_views`].
    whole: std::sync::OnceLock<Option<usize>>,
}

// SAFETY: a section handle and its whole view may be used from any thread.
unsafe impl Send for Section {}
// SAFETY: as above.
unsafe impl Sync for Section {}

impl Section {
    fn new(handle: HANDLE, size: usize) -> Section {
        Section {
            handle,
            size,
            whole: std::sync::OnceLock::new(),
        }
    }

    /// The address of the section's whole view, mapped now if it is not
    /// yet; `None` if there is none to be had ([`whole_views`] off, or the
    /// host refused).
    fn whole(&self) -> Option<usize> {
        if !whole_views() {
            return None;
        }
        *self.whole.get_or_init(|| {
            // Above the low 1 TiB, where the title's own memory goes
            // (`address_range`), when the host can.
            let mut requirements = MEM_ADDRESS_REQUIREMENTS {
                LowestStartingAddress: (1usize << 40) as *mut c_void,
                ..Default::default()
            };
            let mut parameter = MEM_EXTENDED_PARAMETER::default();
            parameter.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
            parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
            // SAFETY: a view of our own section, anywhere the requirements
            // allow; the parameter points at `requirements`, which outlives
            // the call.
            let view = unsafe {
                MapViewOfFile3(
                    self.handle,
                    GetCurrentProcess(),
                    std::ptr::null(),
                    0,
                    self.size,
                    0,
                    PAGE_READWRITE,
                    &mut parameter,
                    1,
                )
            };
            if view.Value.is_null() {
                tracing::warn!(
                    size = self.size,
                    error = %std::io::Error::last_os_error(),
                    "no whole view of a physical-page section; its pages are written through views of their own"
                );
                return None;
            }
            Some(view.Value as usize)
        })
    }
}

impl Drop for Section {
    fn drop(&mut self) {
        // Before the handle is closed: until then no other section has it.
        SECTIONS.lock().unwrap_or_else(|e| e.into_inner()).remove(&(self.handle as usize));
        if let Some(Some(at)) = self.whole.get() {
            // SAFETY: our whole view; nothing refers to it any more (a
            // `View` on it holds the section).
            unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: *at as *mut c_void }) };
        }
        // SAFETY: our handle; views keep the section alive on their own.
        unsafe { CloseHandle(self.handle) };
    }
}

/// The sections committed whole (under Wine), by handle, for
/// [`write_section`]: a section goes from here when it goes. Taken alone
/// (no other lock is taken while it is held).
static SECTIONS: LazyLock<Mutex<HashMap<usize, std::sync::Weak<Section>>>> = LazyLock::new(Default::default);

/// For the GPU side (`victory_gpu::title_memory_section_writer`): writes
/// `bytes` at byte `offset` of the section `section` (a handle) through the
/// section's kept whole view ([`whole_views`]), mapped now if it is not
/// yet. False, writing nothing, if the section is gone or not committed
/// whole, the bytes do not fit in it, or there is no whole view to be had.
///
/// The GPU side's copy-back writes what shaders wrote into title memory;
/// a write at the title's address of guarded view memory first made the
/// block writable (a `VirtualProtect` under ntdll's lock on the memory map
/// under Wine, per block and per other view of its pages) and the next
/// check made it read-only again. Through this view no guard lies in the
/// way; the GPU side notes the blocks at every title address of the pages
/// written itself, as for DirectStorage's writes ([`pages_written`]).
/// The section is held while the bytes are copied, so its view stays
/// mapped.
pub(crate) fn write_section(section: usize, offset: u64, bytes: &[u8]) -> bool {
    let Some(section) = SECTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&section)
        .and_then(std::sync::Weak::upgrade)
    else {
        return false;
    };
    let Ok(offset) = usize::try_from(offset) else {
        return false;
    };
    if offset.checked_add(bytes.len()).is_none_or(|end| end > section.size) {
        return false;
    }
    let Some(whole) = section.whole() else {
        return false;
    };
    // SAFETY: the whole view maps all `size` bytes of the section, committed
    // (`SEC_COMMIT`, the only kind in `SECTIONS`) and read-write, and stays
    // mapped while `section` is held; the range lies inside it (checked
    // above). Other views of the pages only alias these bytes, as the
    // console's do; `bytes` is the caller's own memory.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), (whole + offset) as *mut u8, bytes.len()) };
    true
}

/// Whether physical pages written from outside the title's views
/// (DirectStorage's page destinations: [`page_window`], [`write_physical`])
/// are written through one view of their whole section, kept, instead of a
/// view mapped and unmapped for each write. On by default under Wine, off
/// elsewhere; `VICTORY_SECTION_VIEWS=1` or `=0` says.
///
/// Under Wine a view's map and unmap are wineserver requests made with
/// ntdll's lock on the memory map held (dlls/ntdll/unix/virtual.c
/// `virtual_map_section`, `unmap_view_of_section`), and the server walks
/// its list of the process's views for each (server/mapping.c `map_view`:
/// `is_valid_view_addr`; `unmap_view`: `find_mapped_view`). Measured with
/// victory-gpu's membench test under Wine 11.22: `MapViewOfFile` and
/// `UnmapViewOfFile` of 64 KiB 29 us with 16 views, 54 us with 10,000,
/// 151 us with 40,000, 289-291 us while another thread remaps; every other
/// memory call of the process waits for that lock meanwhile. With the
/// whole view a write is a copy into it. There sections are committed
/// whole (`XMemAllocatePhysicalPages`), so the view needs no commit and
/// costs address space only; on Windows a view per write stays, which
/// commits the pages it writes.
fn whole_views() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| match std::env::var_os("VICTORY_SECTION_VIEWS") {
        Some(v) => v == "1",
        None => on_wine(),
    });
    *ON
}

/// Where a page lives, or what a view starts with: a section and a page
/// index in it.
#[derive(Clone)]
struct Slice {
    section: Arc<Section>,
    index: usize,
}

impl Slice {
    /// The key in `Physical::shown` of the page `k` pages after this one.
    fn key(&self, k: usize) -> (usize, usize) {
        (Arc::as_ptr(&self.section) as usize, self.index + k)
    }
}

#[derive(Default)]
struct Physical {
    next_page: usize,
    pages: HashMap<usize, Slice>,
    /// Mapped views by address: their length and what they show.
    views: BTreeMap<usize, (usize, Slice)>,
    /// Where each page shows: by its section and its index there, the
    /// address of that page in every view that shows it now ([`shown_at`];
    /// as upstream's c75fdba keeps it). The title maps pages where the
    /// card reads them and has them filled afterwards by DirectStorage,
    /// which writes through views of our own.
    shown: HashMap<(usize, usize), Vec<usize>>,
    /// With [`coalesce`]: where we cut a placeholder in two, the only
    /// boundaries [`isolate`] joins across.
    cuts: BTreeSet<usize>,
}

static PHYSICAL: LazyLock<Mutex<Physical>> = LazyLock::new(|| {
    Mutex::new(Physical {
        // Page numbers are opaque to titles; 0 stays unused.
        next_page: 0x10_0000,
        ..Default::default()
    })
});

fn physical() -> MutexGuard<'static, Physical> {
    PHYSICAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// `count` is in and out (pages wanted, pages given); all or nothing here.
pub extern "system" fn XMemAllocatePhysicalPages(
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
) -> BOOL {
    if count.is_null() || pages.is_null() {
        return 0;
    }
    // SAFETY: checked non-null.
    let n = unsafe { *count };
    let Some(size) = n.checked_mul(PAGE).filter(|&s| s > 0) else {
        return 0;
    };
    // A pagefile-backed section of `size` bytes, its pages committed when
    // first mapped or written (`commit`): the title asks for 6.1
    // GiB at once, which a busy host often cannot commit (error 1450).
    // SAFETY: plain section creation.
    let handle = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            // Under Wine a committed section is a sparse file, free until
            // touched, and committing a reserved one page by page costs a
            // wineserver request each that scans every range committed so
            // far: commit it whole there.
            PAGE_READWRITE | if on_wine() { SEC_COMMIT } else { SEC_RESERVE },
            (size as u64 >> 32) as u32,
            size as u32,
            std::ptr::null(),
        )
    };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        tracing::warn!(n, %error, "XMemAllocatePhysicalPages failed");
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
        return 0;
    }
    tracing::debug!(
        n,
        flags = format_args!("{flags:#x}"),
        "XMemAllocatePhysicalPages"
    );
    // For the GPU side's in-place census: only a committed section can be
    // opened as a host heap.
    victory_gpu::title_memory_section(handle as usize, size as u64, on_wine());
    let section = Arc::new(Section::new(handle, size));
    if on_wine() {
        // Committed whole (above): its kept view can be written as it is.
        SECTIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(handle as usize, Arc::downgrade(&section));
        static TOLD: std::sync::Once = std::sync::Once::new();
        TOLD.call_once(|| victory_gpu::title_memory_section_writer(write_section));
    }
    let mut physical = physical();
    let first = physical.next_page;
    physical.next_page += n * FRAMES;
    for index in 0..n {
        let slice = Slice {
            section: section.clone(),
            index,
        };
        let number = first + index * FRAMES;
        physical.pages.insert(number, slice);
        // SAFETY: the title passes room for `n` page numbers.
        unsafe { pages.add(index).write(number) };
    }
    1
}

/// Page numbers count 4 KiB frames, so a 64 KiB page's is a multiple of
/// 16: the title keeps them as 21-bit `number >> 4` and rebuilds
/// them with `<< 4` (128 GiB of pages at most).
const FRAMES: usize = PAGE >> 12;

/// Pages still mapped stay readable through their views until unmapped.
pub extern "system" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> BOOL {
    if pages.is_null() {
        return 0;
    }
    let mut physical = physical();
    for i in 0..n {
        // SAFETY: the title passes `n` page numbers.
        physical.pages.remove(&unsafe { *pages.add(i) });
    }
    1
}

/// A view of our own on consecutive slices of one section, committed and
/// writable, unmapped when dropped; or a window on the section's whole
/// view ([`whole_views`]), which stays, and the section with it.
struct View {
    at: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
    /// The section whose whole view `at` lies in.
    kept: Option<Arc<Section>>,
}

impl View {
    /// Maps `n` slices from `slice` on.
    fn map(slice: &Slice, n: usize) -> Option<View> {
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        let _timed = victory_gpu::vmstat::time(victory_gpu::vmstat::Site::DiskWindow);
        let (offset, len) = ((slice.index * PAGE) as u64, n * PAGE);
        if let Some(whole) = slice.section.whole()
            && offset as usize + len <= slice.section.size
        {
            let at = whole + offset as usize;
            return commit(at, len).then(|| View {
                at: MEMORY_MAPPED_VIEW_ADDRESS { Value: at as *mut c_void },
                len,
                kept: Some(slice.section.clone()),
            });
        }
        // SAFETY: slices of a section we own; the view is ours until dropped.
        let at = unsafe {
            MapViewOfFile(
                slice.section.handle,
                FILE_MAP_WRITE,
                (offset >> 32) as u32,
                offset as u32,
                len,
            )
        };
        if at.Value.is_null() {
            return None;
        }
        let view = View { at, len, kept: None };
        commit(at.Value as usize, len).then_some(view)
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the view holds `len` committed read-write bytes, ours
        // alone while `self` lives (other views of the slices only alias
        // them, as the console's do).
        unsafe { std::slice::from_raw_parts_mut(self.at.Value.cast(), self.len) }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        if self.kept.is_none() {
            let _timed = victory_gpu::vmstat::time(victory_gpu::vmstat::Site::DiskWindow);
            // SAFETY: our view.
            unsafe { UnmapViewOfFile(self.at) };
        }
    }
}

/// The pages behind bytes `[offset, offset + len)` of `pages` (64 KiB
/// each), as runs of consecutive slices of one section: each run's first
/// slice and its length in pages. One look at the page table, which the
/// title's own mapping calls share with every DirectStorage worker. `None`
/// if a page is unknown or `pages` ends early.
fn runs(pages: &[usize], offset: usize, len: usize) -> Option<Vec<(Slice, usize)>> {
    let end = offset.checked_add(len)?;
    let pages = pages.get(offset / PAGE..end.div_ceil(PAGE))?;
    let physical = physical();
    let mut runs: Vec<(Slice, usize)> = Vec::new();
    for page in pages {
        let slice = physical.pages.get(page)?;
        match runs.last_mut() {
            Some((first, n))
                if Arc::ptr_eq(&first.section, &slice.section)
                    && slice.index == first.index + *n =>
            {
                *n += 1;
            }
            _ => runs.push((slice.clone(), 1)),
        }
    }
    Some(runs)
}

/// Whether every page under `[offset, offset + len)` is known (a write that
/// fails with them known is a host failure).
pub(crate) fn pages_known(pages: &[usize], offset: usize, len: usize) -> bool {
    runs(pages, offset, len).is_some()
}

/// Copies `data` into physical pages, from `offset` bytes into the first,
/// whether or not they are mapped anywhere (DirectStorage destinations).
/// One view per run of consecutive pages, not one per page: each view
/// costs a map, a commit and an unmap (which flushes every core's TLB).
/// False if a page is unknown or the host fails.
pub(crate) fn write_physical(pages: &[usize], offset: usize, data: &[u8]) -> bool {
    let Some(runs) = runs(pages, offset, data.len()) else {
        return false;
    };
    let (mut within, mut done) = (offset % PAGE, 0);
    for (slice, n) in &runs {
        let Some(mut view) = View::map(slice, *n) else {
            return false;
        };
        let k = (n * PAGE - within).min(data.len() - done);
        view.bytes()[within..within + k].copy_from_slice(&data[done..done + k]);
        (within, done) = (0, done + k);
    }
    true
}

/// Bytes `[offset, offset + len)` of physical pages, writable in place
/// until dropped: DirectStorage decodes into it instead of a buffer it
/// would then copy.
pub(crate) struct PageWindow {
    view: View,
    within: usize,
    len: usize,
}

impl PageWindow {
    pub(crate) fn bytes(&mut self) -> &mut [u8] {
        let (within, len) = (self.within, self.len);
        &mut self.view.bytes()[within..within + len]
    }
}

/// A window on bytes `[offset, offset + len)` of `pages` when they are
/// consecutive pages of one allocation (how titles usually hand them out);
/// `None` otherwise (pages of several allocations, out of order, or
/// unknown, or a host failure), which `write_physical` then handles or
/// reports.
pub(crate) fn page_window(pages: &[usize], offset: usize, len: usize) -> Option<PageWindow> {
    let runs = runs(pages, offset, len)?;
    let [(slice, n)] = runs.as_slice() else {
        return None;
    };
    Some(PageWindow {
        view: View::map(slice, *n)?,
        within: offset % PAGE,
        len,
    })
}

/// The title addresses that show bytes `[offset, offset + len)` of `pages`
/// now, byte for byte, as runs (address, bytes) in address order: a write
/// to the pages through a view of our own ([`page_window`],
/// [`write_physical`]) lands there too, and nothing that watches those
/// addresses sees it (the GPU side's guards fault only on a write at the
/// guarded address itself). Empty if no view shows them. Byte for byte,
/// not by whole pages: the title packs several small textures into one
/// page and fills each with a request of its own.
pub(crate) fn shown_at(pages: &[usize], offset: usize, len: usize) -> Vec<(usize, usize)> {
    let Some(end) = offset.checked_add(len) else {
        return Vec::new();
    };
    let first = offset / PAGE;
    let Some(touched) = pages.get(first..end.div_ceil(PAGE)) else {
        return Vec::new();
    };
    let mut pieces: Vec<(usize, usize)> = Vec::new();
    {
        let physical = physical();
        for (i, page) in touched.iter().enumerate() {
            let Some(addresses) = physical.pages.get(page).and_then(|slice| physical.shown.get(&slice.key(0))) else {
                continue;
            };
            // The bytes of this page that the write covers.
            let lo = if i == 0 { offset % PAGE } else { 0 };
            let hi = (end - (first + i) * PAGE).min(PAGE);
            pieces.extend(addresses.iter().map(|&at| (at + lo, hi - lo)));
        }
    }
    pieces.sort_unstable();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (at, bytes) in pieces {
        match runs.last_mut() {
            Some((start, n)) if *start + *n == at => *n += bytes,
            _ => runs.push((at, bytes)),
        }
    }
    runs
}

/// Bytes `[offset, offset + len)` of `pages` have been written through a
/// view of our own: the GPU side is told at every title address that
/// shows them (`victory_gpu::title_memory_written`; what it does with it
/// is `VICTORY_PAGE_WRITES`' to say, and it counts either way). Call when
/// the bytes are there, and without the page table's lock ([`shown_at`]
/// takes and leaves it).
pub(crate) fn pages_written(pages: &[usize], offset: usize, len: usize) {
    for (at, bytes) in shown_at(pages, offset, len) {
        victory_gpu::title_memory_written(at, bytes, true);
    }
}

/// Maps `n` pages at `address`, in place of what is mapped there; null
/// `pages` unmaps. Returns `address`, or null if a page or the host fails.
pub extern "system" fn XMemMapPhysicalPages(
    address: *mut c_void,
    n: usize,
    pages: *const usize,
) -> *mut c_void {
    let base = address as usize;
    let mut physical = physical();
    if coalesce() && base.is_multiple_of(PAGE) {
        // SAFETY: the title passes `n` page numbers unless null.
        let pages = (!pages.is_null()).then(|| unsafe { std::slice::from_raw_parts(pages, n) });
        let mut held = None;
        let mapped = map_pages(&mut physical, &mut held, base, n, pages, false);
        drop(held);
        return if mapped { address } else { std::ptr::null_mut() };
    }
    // Until the pages are mapped again: `unmap` takes whole views away and
    // maps what stays of them again, and a mapped view is a reservation
    // until it is committed. A read of title memory in place must not fall
    // into that gap (`victory_gpu::title_memory_remapping`).
    let _changing = victory_gpu::title_memory_remapping();
    unmap(&mut physical, base, n * PAGE);
    if pages.is_null() {
        return address;
    }
    // SAFETY: the title passes `n` page numbers.
    let pages = unsafe { std::slice::from_raw_parts(pages, n) };
    let mut i = 0;
    while i < n {
        let Some(first) = physical.pages.get(&pages[i]).cloned() else {
            tracing::warn!(page = pages[i], "XMemMapPhysicalPages: unknown page");
            return std::ptr::null_mut();
        };
        // One view per run of consecutive slices of one section.
        let run = 1 + pages[i + 1..]
            .iter()
            .zip(1..)
            .take_while(|&(page, k)| {
                physical.pages.get(page).is_some_and(|s| {
                    Arc::ptr_eq(&s.section, &first.section) && s.index == first.index + k
                })
            })
            .count();
        if !map(&mut physical, base + i * PAGE, run * PAGE, first) {
            return std::ptr::null_mut();
        }
        i += run;
    }
    address
}

/// Count consecutive zero-page entries, without looking ahead across input
/// arrays that an earlier unmap could invalidate or alter.
fn scatter_unmap_run(
    start: usize,
    remaining: usize,
    input_ranges: &[(usize, usize)],
    mut entry: impl FnMut(usize) -> (usize, bool),
) -> usize {
    let safe = |at: usize| {
        at != 0
            && at.is_multiple_of(PAGE)
            && at.checked_add(PAGE).is_some_and(|end| {
                input_ranges.iter().all(|&(lo, hi)| end <= lo || at >= hi)
            })
    };
    if !safe(start) {
        return 1;
    }
    let mut count = 1usize;
    while count < remaining {
        let Some(expected) = count.checked_mul(PAGE).and_then(|n| start.checked_add(n)) else {
            break;
        };
        let (at, unmap) = entry(count);
        if at != expected || !unmap || !safe(at) {
            break;
        }
        count += 1;
    }
    count
}

/// One page at each address; null `pages`, or a page number 0, unmaps
/// (the title passes 0 for addresses to clear).
pub extern "system" fn XMemMapPhysicalPagesScatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *const usize,
) -> BOOL {
    if addresses.is_null() {
        return 0;
    }
    let inputs = n.checked_mul(size_of::<usize>()).and_then(|bytes| {
        let first = addresses as usize;
        let addresses = (first, first.checked_add(bytes)?);
        let pages = if pages.is_null() {
            (0, 0)
        } else {
            let second = pages as usize;
            (second, second.checked_add(bytes)?)
        };
        Some([addresses, pages])
    });
    if coalesce() {
        return scatter(addresses, n, pages, inputs.as_ref(), victory_gpu::resource::remap_batch());
    }
    let mut i = 0;
    while i < n {
        // SAFETY: the title passes `n` addresses and, unless null, `n` pages.
        let (address, page) = unsafe {
            (
                *addresses.add(i),
                if pages.is_null() || *pages.add(i) == 0 {
                    std::ptr::null()
                } else {
                    pages.add(i)
                },
            )
        };
        let count = if page.is_null() {
            inputs.as_ref().map_or(1, |ranges| {
                scatter_unmap_run(address as usize, n - i, ranges, |offset| {
                    // SAFETY: the title passes n entries in both arrays.
                    unsafe {
                        let next = *addresses.add(i + offset) as usize;
                        let unmap = pages.is_null() || *pages.add(i + offset) == 0;
                        (next, unmap)
                    }
                })
            })
        } else {
            1
        };
        if XMemMapPhysicalPages(address, count, page).is_null() {
            return 0;
        }
        i += count;
    }
    1
}

/// Maps the physical pages shown at `[from, from + len)` at `to` as well,
/// in place of what `to` shows; `from` 0 unmaps. The GPU's page-mapping
/// copies (`victory_gpu`, queue `CopyPageMappingsBatchX`):
/// the title maps pages into a pool view, then has the GPU map them into its tiled
/// heap. Runs of consecutive slices of one section as one view each with
/// [`coalesce`], else page by page; false if a source page is not mapped
/// physical memory.
pub fn map_alias(to: u64, from: u64, len: u64) -> bool {
    let mut physical = physical();
    if coalesce() {
        let mut held = None;
        let ok = alias(&mut physical, &mut held, to, from, len);
        drop(held);
        return ok;
    }
    // As in `XMemMapPhysicalPages`: no read in place meets the gap.
    let _changing = victory_gpu::title_memory_remapping();
    alias_by_page(&mut physical, to as usize, from as usize, len as usize)
}

/// The page-by-page [`map_alias`], as before [`coalesce`]: with `physical`
/// and the remapping lock held.
fn alias_by_page(physical: &mut Physical, to: usize, from: usize, len: usize) -> bool {
    unmap(physical, to, len);
    for k in (0..len).step_by(PAGE) {
        if from == 0 {
            break;
        }
        let Some(slice) = slice_at(physical, from + k) else {
            return false;
        };
        if !map(physical, to + k, PAGE, slice) {
            return false;
        }
    }
    true
}

/// The slice that the page at `at` shows, if a view of ours shows one.
fn slice_at(physical: &Physical, at: usize) -> Option<Slice> {
    physical
        .views
        .range(..=at)
        .next_back()
        .filter(|(base, (size, _))| at < *base + size)
        .map(|(base, (_, s))| Slice {
            index: s.index + (at - base) / PAGE,
            section: s.section.clone(),
        })
}

/// Several [`map_alias`] in order, under one hold of `physical` and of the
/// remapping lock (`victory_gpu::resource::remap_batch`, a page-mapping
/// copy's ranges); false if any failed, the others are still made. After
/// vxlayer d5106b6e4.
pub fn map_aliases(ops: &[(u64, u64, u64)]) -> bool {
    let mut physical = physical();
    let mut held = None;
    if coalesce() {
        return ops
            .iter()
            .fold(true, |ok, &(to, from, len)| alias(&mut physical, &mut held, to, from, len) & ok);
    }
    held = Some(victory_gpu::title_memory_remapping());
    let ok = ops.iter().fold(true, |ok, &(to, from, len)| {
        alias_by_page(&mut physical, to as usize, from as usize, len as usize) & ok
    });
    drop(held);
    ok
}

/// Whether runs of pages are mapped and unmapped as one view each, and
/// pages that show what a call asks for already are left as they are
/// (`VICTORY_MAP_COALESCE`; on, `=0` as before).
///
/// Under Wine every map and unmap of a view is a wineserver round trip
/// made with ntdll's lock on the process's memory map held
/// (dlls/ntdll/unix/virtual.c `virtual_map_section`: a `get_mapping_info`
/// request, then a `map_view` one inside `virtual_mutex`;
/// `unmap_view_of_section`: an `unmap_view` request inside it), and every
/// protection change, query and write fault of every thread waits for that
/// lock meanwhile. The page-mapping copies ([`map_alias`]) made a view per
/// page, and unmapping a range took a call per view: in a cutscene the
/// title's two mapping threads spent 13 of 15 seconds in some 17,800 maps
/// and 23,000 unmaps. With the switch:
///
/// - the pages a call maps go in as runs of consecutive slices of one
///   section, one view each: a page-mapping copy's run of pool pages, a
///   scattered call's pages at consecutive addresses, and as before the
///   runs of `XMemMapPhysicalPages` ([`place`]);
/// - a view that shows what a call asks for already, with none of the
///   title's own protections in that part, stays: no call, and no remapping
///   lock if nothing else changes;
/// - what stays of a view a call changes in part is mapped again (with the
///   call's new pages where they follow on in the same section), the
///   title's own protections there are set again, and a thread that touches
///   it meanwhile waits in the fault handler for the change to end
///   (`victory_gpu::title_memory_gaps_waited`);
/// - a run is mapped over exactly one placeholder, split off at its ends
///   and joined from those under it ([`isolate`]): a view replaces one
///   whole placeholder (virtual.c `map_view`: `MEM_REPLACE_PLACEHOLDER`
///   wants one of that base and size) and pages unmapped one by one leave
///   one each. Splits, joins and queries are ntdll's alone, no request.
///
/// `=0`: a view per page for page-mapping copies and scattered pages,
/// views that show the same pages unmapped and mapped again, and a fault
/// in a change's gap not waited for: as before.
fn coalesce() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| {
        let on = std::env::var_os("VICTORY_MAP_COALESCE").is_none_or(|v| v != "0");
        victory_gpu::title_memory_gaps_waited(on);
        tracing::info!(
            on,
            "physical pages: runs mapped as one view each, pages that show what is asked left as they are (VICTORY_MAP_COALESCE)"
        );
        on
    });
    let on = *ON;
    #[cfg(test)]
    if let Some(on) = tests::COALESCE.with(std::cell::Cell::get) {
        return on;
    }
    on
}

/// What [`coalesce`]'s mapping counts since the start, by index: views
/// mapped and the pages they show, views unmapped and theirs, pages left
/// as they were, placeholder joins, the title's protections set again.
static VIEW_STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
const MAPS: usize = 0;
const MAP_PAGES: usize = 1;
const UNMAPS: usize = 2;
const UNMAP_PAGES: usize = 3;
const KEPT: usize = 4;
const JOINS: usize = 5;
const RESTORED: usize = 6;

fn count(what: usize, n: usize) {
    if n == 0 {
        return;
    }
    VIEW_STATS[what].fetch_add(n as u64, Ordering::Relaxed);
    use victory_gpu::framelog::{T, add};
    let row = [T::ViewMaps, T::ViewMapPages, T::ViewUnmaps, T::ViewUnmapPages, T::ViewPagesKept];
    if let Some(&t) = row.get(what) {
        add(t, n as u64);
    }
    #[cfg(test)]
    tests::COUNTED.with(|c| {
        let mut v = c.get();
        v[what] += n as u64;
        c.set(v);
    });
}

/// Every 5 s, if the title mapped anything: views mapped and unmapped per
/// frame and pages per view, pages left as they were, placeholder joins,
/// protections set again, faults that waited out a change.
fn view_line() {
    static LAST: Mutex<Option<(std::time::Instant, u64, [u64; 7], u64)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    let read = || -> ([u64; 7], u64, u64) {
        (
            std::array::from_fn(|i| VIEW_STATS[i].load(Ordering::Relaxed)),
            victory_gpu::frame::current(),
            victory_gpu::title_memory_gaps_met(),
        )
    };
    let Some((then, frames_then, stats_then, gaps_then)) = *last else {
        let (stats, frames, gaps) = read();
        *last = Some((now, frames, stats, gaps));
        return;
    };
    if now.duration_since(then).as_secs() < 5 {
        return;
    }
    let (stats, frames, gaps) = read();
    *last = Some((now, frames, stats, gaps));
    drop(last);
    let d: [u64; 7] = std::array::from_fn(|i| stats[i] - stats_then[i]);
    let frames = (frames - frames_then).max(1) as f64;
    let per = |n: u64, of: u64| n as f64 / of.max(1) as f64;
    tracing::info!(
        seconds = now.duration_since(then).as_secs(),
        frames,
        maps_per_frame = format_args!("{:.1}", d[MAPS] as f64 / frames),
        pages_per_map = format_args!("{:.2}", per(d[MAP_PAGES], d[MAPS])),
        unmaps_per_frame = format_args!("{:.1}", d[UNMAPS] as f64 / frames),
        pages_per_unmap = format_args!("{:.2}", per(d[UNMAP_PAGES], d[UNMAPS])),
        kept_pages_per_frame = format_args!("{:.1}", d[KEPT] as f64 / frames),
        joins = d[JOINS],
        protections_restored = d[RESTORED],
        faults_waited = gaps - gaps_then,
        "physical-page views (VICTORY_MAP_COALESCE)"
    );
}

/// [`XMemMapPhysicalPages`] with [`coalesce`], `physical` held: `pages`
/// (`None` unmaps) from `base` on. An unknown page fails the call with the
/// pages before it mapped; after it, the rest of the range is unmapped, or
/// with `rest_untouched` left as it is (a scattered call's run, whose
/// entries after the failing one are not looked at).
fn map_pages(
    physical: &mut Physical,
    held: &mut Option<victory_gpu::Remapping>,
    base: usize,
    n: usize,
    pages: Option<&[usize]>,
    rest_untouched: bool,
) -> bool {
    let mut want: Vec<Option<Slice>> = vec![None; n];
    let mut ok = true;
    for (i, page) in pages.unwrap_or(&[]).iter().enumerate() {
        match physical.pages.get(page) {
            Some(slice) => want[i] = Some(slice.clone()),
            None => {
                tracing::warn!(page, "XMemMapPhysicalPages: unknown page");
                ok = false;
                if rest_untouched {
                    want.truncate(i + 1);
                }
                break;
            }
        }
    }
    place(physical, held, base, &want) && ok
}

/// [`map_alias`] with [`coalesce`], `physical` held: the source pages are
/// looked up first, then placed as runs ([`place`]). Addresses off a page,
/// and a source that overlaps the destination (each source page there
/// looked up after the pages before it changed), go page by page as before.
fn alias(physical: &mut Physical, held: &mut Option<victory_gpu::Remapping>, to: u64, from: u64, len: u64) -> bool {
    let (to, from, len) = (to as usize, from as usize, len as usize);
    let fits = to.checked_add(len).is_some() && from.checked_add(len).is_some();
    let aligned = to.is_multiple_of(PAGE) && from.is_multiple_of(PAGE) && len.is_multiple_of(PAGE);
    let overlaps = from != 0 && fits && from < to + len && to < from + len;
    if !fits || !aligned || overlaps {
        held.get_or_insert_with(victory_gpu::title_memory_remapping);
        return alias_by_page(physical, to, from, len);
    }
    let n = len / PAGE;
    let mut want: Vec<Option<Slice>> = vec![None; n];
    let mut ok = true;
    if from != 0 {
        for (k, w) in want.iter_mut().enumerate() {
            match slice_at(physical, from + k * PAGE) {
                Some(slice) => *w = Some(slice),
                // As before: the pages before it mapped, the rest not.
                None => {
                    ok = false;
                    break;
                }
            }
        }
    }
    place(physical, held, to, &want) && ok
}

/// [`XMemMapPhysicalPagesScatter`] with [`coalesce`]: entries that map (or
/// unmap) consecutive addresses go as one run, under one hold of the locks
/// with `batch` (`VICTORY_REMAP_BATCH=1`, after vxlayer d5106b6e4) or one
/// per run. A run never reaches into the input arrays
/// ([`scatter_unmap_run`]), so its page numbers are read before it changes
/// anything, as they were one by one.
fn scatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *const usize,
    inputs: Option<&[(usize, usize); 2]>,
    batch: bool,
) -> BOOL {
    // SAFETY (the closure's): the title passes `n` addresses and, unless
    // null, `n` pages; an entry is read once its turn comes, as before.
    let entry = |i: usize| unsafe {
        let page = if pages.is_null() { 0 } else { *pages.add(i) };
        (*addresses.add(i) as usize, page)
    };
    let mut batched = batch.then(|| (physical(), None));
    let mut i = 0;
    while i < n {
        let (address, page) = entry(i);
        if address == 0 {
            // As before: `XMemMapPhysicalPages` returns the address it was
            // given, and a null one reads as a failure; nothing after it.
            return 0;
        }
        if !address.is_multiple_of(PAGE) {
            // Off a page: as before (the host refuses it), alone.
            let page = (page != 0).then_some(page);
            let ok = match &mut batched {
                Some((physical, held)) => {
                    held.get_or_insert_with(victory_gpu::title_memory_remapping);
                    unmap(physical, address, PAGE);
                    page.is_none_or(|page| {
                        physical.pages.get(&page).cloned().is_some_and(|slice| map(physical, address, PAGE, slice))
                    })
                }
                // SAFETY: entry `i` of the title's pages, when it maps one.
                None => !XMemMapPhysicalPages(address as *mut c_void, 1, if page.is_some() { unsafe { pages.add(i) } } else { std::ptr::null() }).is_null(),
            };
            if !ok {
                return 0;
            }
            i += 1;
            continue;
        }
        let maps = page != 0;
        // Entries that do as this one does (map, or unmap) at the addresses
        // that follow; a run of one where the arrays are not known.
        let count = inputs.map_or(1, |ranges| {
            scatter_unmap_run(address, n - i, ranges, |k| {
                let (at, page) = entry(i + k);
                (at, (page != 0) == maps)
            })
        });
        let numbers: Option<Vec<usize>> = maps.then(|| (0..count).map(|k| entry(i + k).1).collect());
        let ok = match &mut batched {
            Some((physical, held)) => map_pages(physical, held, address, count, numbers.as_deref(), true),
            None => map_pages(&mut physical(), &mut None, address, count, numbers.as_deref(), true),
        };
        if !ok {
            return 0;
        }
        i += count;
    }
    1
}

/// What `[base, base + want.len() pages)` is to show, page by page (`None`:
/// nothing), with [`coalesce`]. The views there that show it already, with
/// none of the title's own protections in that part, stay as they are.
/// Every other view that meets the range goes, and what stays of it outside
/// the range is mapped again with the title's protections there; the pages
/// to show go in as runs of consecutive slices of one section, one view
/// each, joined with what stays of a view beside them where it follows on.
/// The remapping lock is taken at the first change (`held`, which keeps it
/// to the end of a batch). False if a page asked for could not be mapped
/// (what could, is).
fn place(
    physical: &mut Physical,
    held: &mut Option<victory_gpu::Remapping>,
    base: usize,
    want: &[Option<Slice>],
) -> bool {
    let n = want.len();
    let Some(end) = n.checked_mul(PAGE).and_then(|len| base.checked_add(len)) else {
        return false;
    };
    if n == 0 {
        return true;
    }
    let shows = |at: usize, slice: &Slice, p: usize| {
        want[(p - base) / PAGE]
            .as_ref()
            .is_some_and(|w| Arc::ptr_eq(&w.section, &slice.section) && w.index == slice.index + (p - at) / PAGE)
    };
    // Views do not overlap: the one holding `base`, then those starting
    // inside.
    let first = physical
        .views
        .range(..=base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(at, _)| *at);
    let met: Vec<usize> = first
        .into_iter()
        .chain(physical.views.range(base + 1..end).map(|(at, _)| *at))
        .collect();
    let mut shown = vec![false; n];
    let mut going: Vec<(usize, usize, Slice)> = Vec::new();
    for at in met {
        let (size, slice) = physical.views[&at].clone();
        let (lo, hi) = (at.max(base), (at + size).min(end));
        if (lo..hi).step_by(PAGE).all(|p| shows(at, &slice, p))
            && victory_gpu::title_memory_own_protections(lo, hi - lo).is_empty()
        {
            for p in (lo..hi).step_by(PAGE) {
                shown[(p - base) / PAGE] = true;
            }
            continue;
        }
        going.push((at, size, slice));
    }
    count(KEPT, shown.iter().filter(|&&s| s).count());
    // What goes in: what stays of the views that go, and the pages asked
    // for that are not shown already; the title's own protections of what
    // stays (looked at before the guards forget them).
    let mut pieces: Vec<(usize, Slice)> = Vec::new();
    let mut protections: Vec<(usize, usize, u32)> = Vec::new();
    for (at, size, slice) in &going {
        for p in (*at..at + size).step_by(PAGE) {
            if p < base || p >= end {
                pieces.push((
                    p,
                    Slice {
                        section: slice.section.clone(),
                        index: slice.index + (p - at) / PAGE,
                    },
                ));
            }
        }
        if *at < base {
            protections.extend(victory_gpu::title_memory_own_protections(*at, base - at));
        }
        if at + size > end {
            protections.extend(victory_gpu::title_memory_own_protections(end, at + size - end));
        }
    }
    for (i, w) in want.iter().enumerate() {
        if let (false, Some(w)) = (shown[i], w) {
            pieces.push((base + i * PAGE, w.clone()));
        }
    }
    if going.is_empty() && pieces.is_empty() {
        return true;
    }
    // Until the pages are mapped again: no read in place meets the gap
    // (`victory_gpu::title_memory_remapping`).
    held.get_or_insert_with(victory_gpu::title_memory_remapping);
    // Forgotten, and the GPU side told, before they go (as `unmap`).
    for (at, size, slice) in &going {
        physical.views.remove(at);
        forget_shown(physical, *at, *size, slice);
        victory_gpu::title_memory_remapped(*at, *size);
    }
    // From here until what stays is mapped again, system calls only: a
    // thread that faults on a page of it waits for the change in the fault
    // handler, and must find no lock of ours that the change still needs.
    for (at, size, _) in &going {
        // SAFETY: a view we mapped; it becomes a placeholder again.
        unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: *at as *mut c_void,
                },
                MEM_PRESERVE_PLACEHOLDER,
            )
        };
        count(UNMAPS, 1);
        count(UNMAP_PAGES, size / PAGE);
    }
    pieces.sort_unstable_by_key(|p| p.0);
    let mut runs: Vec<(usize, usize, Slice)> = Vec::new();
    for (p, slice) in pieces {
        match runs.last_mut() {
            Some((at, len, first))
                if *at + *len == p
                    && Arc::ptr_eq(&first.section, &slice.section)
                    && slice.index == first.index + *len / PAGE =>
            {
                *len += PAGE;
            }
            _ => runs.push((p, PAGE, slice)),
        }
    }
    let mut ok = true;
    let mut mapped = Vec::with_capacity(runs.len());
    for (at, len, slice) in runs {
        if map_view(&mut physical.cuts, at, len, &slice) {
            mapped.push((at, len, slice));
        } else if at < end && at + len > base {
            // (A failure outside the range is what stays of a view: as
            // before, the call does not fail for it.)
            ok = false;
        }
    }
    for (at, len, protect) in protections {
        let mut old = 0;
        // SAFETY: pages of a view just mapped again; only the protection
        // the title had given them.
        if unsafe { VirtualProtect(at as *const c_void, len, protect, &mut old) } != 0 {
            count(RESTORED, 1);
        } else {
            tracing::warn!(
                address = format_args!("{at:#x}"),
                len,
                protect = format_args!("{protect:#x}"),
                error = %std::io::Error::last_os_error(),
                "a protection of the title's could not be set again on pages mapped again"
            );
        }
    }
    // Noted, and the GPU side told, now that they show it.
    for (at, len, slice) in mapped {
        note_mapped(physical, at, len, slice);
    }
    view_line();
    ok
}

/// `shown`'s entries for the view `[at, at + size)` of `slice` go.
fn forget_shown(physical: &mut Physical, at: usize, size: usize, slice: &Slice) {
    for k in 0..size.div_ceil(PAGE) {
        let key = slice.key(k);
        if let Some(addresses) = physical.shown.get_mut(&key) {
            addresses.retain(|&a| a != at + k * PAGE);
            if addresses.is_empty() {
                physical.shown.remove(&key);
            }
        }
    }
}

/// The system calls of a mapping with [`coalesce`]: `[base, base + len)`
/// made one placeholder of its own ([`isolate`]), `slice` and the slices
/// after it mapped over it, and committed. Whether the host did as asked.
fn map_view(cuts: &mut BTreeSet<usize>, base: usize, len: usize, slice: &Slice) -> bool {
    let view = if isolate(cuts, base, len) {
        count(MAPS, 1);
        count(MAP_PAGES, len / PAGE);
        // SAFETY: maps the section over the placeholder just made exactly
        // this range; acts only on a placeholder.
        unsafe {
            MapViewOfFile3(
                slice.section.handle,
                GetCurrentProcess(),
                base as *const c_void,
                (slice.index * PAGE) as u64,
                len,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        }
    } else {
        MEMORY_MAPPED_VIEW_ADDRESS { Value: std::ptr::null_mut() }
    };
    if view.Value as usize != base {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            len,
            error = %std::io::Error::last_os_error(),
            "mapping physical pages failed (not a placeholder?)"
        );
        return false;
    }
    if !commit(base, len) {
        // SAFETY: the view just mapped, back to a placeholder.
        unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
        return false;
    }
    true
}

/// What [`map`] notes and tells once a view of `slice` and the slices after
/// it shows at `[base, base + len)`.
fn note_mapped(physical: &mut Physical, base: usize, len: usize, slice: Slice) {
    // New contents, read-write: the GPU side's copies no longer guard it.
    victory_gpu::title_memory_remapped(base, len);
    // What the range shows now, for the GPU side's in-place census.
    victory_gpu::title_memory_view(base, len, slice.section.handle as usize, (slice.index * PAGE) as u64);
    for k in 0..len.div_ceil(PAGE) {
        physical.shown.entry(slice.key(k)).or_default().push(base + k * PAGE);
    }
    physical.views.insert(base, (len, slice));
}

/// Makes `[base, base + len)` exactly one placeholder: cut out of the one
/// it lies in, or, over several (pages unmapped one by one leave one
/// each), their ends cut off at its edges and the rest joined. Every page
/// of it must be a placeholder, and every boundary between placeholders
/// inside it one that we cut (`cuts`): one the title cut, by splitting its
/// own reservation, stays (a part the title lets go later must not take
/// its neighbours with it). False if not, or if the host refuses.
///
/// Under Wine (dlls/ntdll/unix/virtual.c, the same in Proton
/// Experimental's ValveSoftware/wine 5b557605 and Wine 11.22):
/// `VirtualQuery` of a placeholder gives its whole extent from the address
/// on and its start as the allocation base (a placeholder is a view of its
/// own); `MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER` cuts a range out of the
/// one placeholder holding it, and refuses a range that is that whole
/// placeholder or reaches past it (`NtFreeVirtualMemory`,
/// `free_pages_preserve_placeholder`); `MEM_RELEASE |
/// MEM_COALESCE_PLACEHOLDERS` joins the adjacent placeholders that begin at
/// the address and add up to exactly the size (`coalesce_placeholders`).
/// None of these is a wineserver request. Windows documents the same rules.
fn isolate(cuts: &mut BTreeSet<usize>, base: usize, len: usize) -> bool {
    // `MEM_COALESCE_PLACEHOLDERS` (winnt.h; windows-sys keeps it in a
    // module this crate does not enable).
    const MEM_COALESCE_PLACEHOLDERS: u32 = 0x1;
    let end = base + len;
    let Some((start, stop)) = placeholder_at(base) else {
        return false;
    };
    if start == base && stop == end {
        return true;
    }
    // SAFETY (the block): splits and joins of placeholders inside the
    // range alone; the host refuses, harmlessly, what is not one.
    unsafe {
        if stop >= end {
            if VirtualFree(base as *mut c_void, len, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) == 0 {
                return false;
            }
            cuts.extend([(start < base).then_some(base), (stop > end).then_some(end)].into_iter().flatten());
            return true;
        }
        let mut last = (start, stop);
        while last.1 < end {
            // A boundary inside the run that is not ours: no join.
            if !cuts.contains(&last.1) {
                return false;
            }
            let Some(next) = placeholder_at(last.1) else {
                return false;
            };
            last = next;
        }
        if start < base {
            if VirtualFree(base as *mut c_void, stop - base, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) == 0 {
                return false;
            }
            cuts.insert(base);
        }
        if last.1 > end {
            if VirtualFree(last.0 as *mut c_void, end - last.0, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) == 0 {
                return false;
            }
            cuts.insert(end);
        }
        if VirtualFree(base as *mut c_void, len, MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS) == 0 {
            return false;
        }
        count(JOINS, 1);
        let inside: Vec<usize> = cuts.range(base + 1..end).copied().collect();
        for at in inside {
            cuts.remove(&at);
        }
        true
    }
}

/// The placeholder at `at`: its start and its end (see [`isolate`]).
fn placeholder_at(at: usize) -> Option<(usize, usize)> {
    use windows_sys::Win32::System::Memory::VirtualQuery;
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: a query of our own address space into a local.
    let n = unsafe { VirtualQuery(at as *const c_void, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
    (n != 0 && info.State == MEM_RESERVE).then(|| (info.AllocationBase as usize, info.BaseAddress as usize + info.RegionSize))
}

/// Maps `slice` and the slices after it over `[base, base + len)`, which
/// must lie in one placeholder.
fn map(physical: &mut Physical, base: usize, len: usize, slice: Slice) -> bool {
    // With `coalesce`, where the cut below falls is noted (`isolate` joins
    // across our own cuts only).
    let around = coalesce().then(|| placeholder_at(base)).flatten();
    // SAFETY: splits the placeholder around the range (failing harmlessly
    // when the range is the whole placeholder), then maps the section
    // there; both act only on placeholders.
    let view = unsafe {
        let cut = VirtualFree(
            base as *mut c_void,
            len,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        );
        if let (true, Some((start, stop))) = (cut != 0, around) {
            physical.cuts.extend([(start < base).then_some(base), (stop > base + len).then_some(base + len)].into_iter().flatten());
        }
        MapViewOfFile3(
            slice.section.handle,
            GetCurrentProcess(),
            base as *const c_void,
            (slice.index * PAGE) as u64,
            len,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    if view.Value as usize != base {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            len,
            error = %std::io::Error::last_os_error(),
            "mapping physical pages failed (not a placeholder?)"
        );
        return false;
    }
    if !commit(base, len) {
        // SAFETY: the view just mapped, back to a placeholder.
        unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
        return false;
    }
    // New contents, read-write: the GPU side's copies no longer guard it.
    victory_gpu::title_memory_remapped(base, len);
    // What the range shows now, for the GPU side's in-place census.
    victory_gpu::title_memory_view(base, len, slice.section.handle as usize, (slice.index * PAGE) as u64);
    for k in 0..len.div_ceil(PAGE) {
        physical.shown.entry(slice.key(k)).or_default().push(base + k * PAGE);
    }
    physical.views.insert(base, (len, slice));
    true
}

/// Commits the section pages a view shows (once per page, whichever view).
fn commit(base: usize, len: usize) -> bool {
    if on_wine() {
        return true; // sections are committed whole there
    }
    // SAFETY: `[base, base + len)` is a view of a reserved section.
    if !unsafe { VirtualAlloc(base as *const c_void, len, MEM_COMMIT, PAGE_READWRITE) }.is_null() {
        return true;
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_COMMITMENT_LIMIT as i32) {
        let mut info = PERFORMANCE_INFORMATION {
            cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
            ..Default::default()
        };
        // SAFETY: the buffer and size describe a writable PERFORMANCE_INFORMATION.
        let queried = unsafe { K32GetPerformanceInfo(&mut info, info.cb) } != 0;
        victory_loader::log_crash(&commit_limit_message(len, &error, queried.then_some(&info)));
    } else {
        tracing::warn!(len, %error, "committing physical pages failed");
    }
    false
}

fn commit_limit_message(
    len: usize,
    error: &std::io::Error,
    info: Option<&PERFORMANCE_INFORMATION>,
) -> String {
    let mut message = format!(
        "VIctory: Windows commit limit reached while committing {len} bytes of physical pages: {error}.\n"
    );
    if let Some(info) = info {
        use std::fmt::Write;

        let committed = info.CommitTotal * info.PageSize / MIB;
        let limit = info.CommitLimit * info.PageSize / MIB;
        let _ = writeln!(
            message,
            "System commit: {committed} MiB of {limit} MiB in use."
        );
    }
    message.push_str(
        "Close other applications or increase the Windows paging file size, then retry.\n",
    );
    message
}

/// Returns `[base, base + len)` to placeholders; views reaching past it
/// are mapped again outside it.
fn unmap(physical: &mut Physical, base: usize, len: usize) {
    // No bytes: no view to return. The search below would still find the
    // view holding `base` and cut it in two there, and its range of views
    // starting inside would run backwards (which `BTreeMap` panics on).
    if len == 0 {
        return;
    }
    let end = base + len;
    // Views do not overlap: the one holding `base`, then those starting
    // inside (the GPU's page mappings make one view per page).
    let first = physical
        .views
        .range(..=base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(at, _)| *at);
    let hit: Vec<usize> = first
        .into_iter()
        .chain(physical.views.range(base + 1..end).map(|(at, _)| *at))
        .collect();
    for at in hit {
        let Some((size, slice)) = physical.views.remove(&at) else {
            continue;
        };
        for k in 0..size.div_ceil(PAGE) {
            let key = slice.key(k);
            if let Some(addresses) = physical.shown.get_mut(&key) {
                addresses.retain(|&a| a != at + k * PAGE);
                if addresses.is_empty() {
                    physical.shown.remove(&key);
                }
            }
        }
        victory_gpu::title_memory_remapped(at, size);
        // SAFETY: a view we mapped; it becomes a placeholder again.
        unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: at as *mut c_void,
                },
                MEM_PRESERVE_PLACEHOLDER,
            )
        };
        if at < base {
            map(physical, at, base - at, slice.clone());
        }
        if at + size > end {
            let index = slice.index + (end - at) / PAGE;
            map(physical, end, at + size - end, Slice { index, ..slice });
        }
    }
}

/// The oldest layout we know; newer titles pass other flag values (Minecraft
/// 0x20000001) and read the same leading fields.
#[repr(C)]
#[derive(Debug, Default)]
pub struct WorkingSet {
    pub title_limit: usize,
    pub title_used: usize,
    pub tools_limit: usize,
    pub tools_used: usize,
    pub stack_used: u32,
    pub physical_pages: u32,
    pub gpu_optimal_limit: usize,
    pub gpu_optimal_used: usize,
    pub tools_gpu_optimal_limit: usize,
    pub tools_gpu_optimal_used: usize,
    pub kernel_pool_used: u32,
    pub kernel_pool_limit: u32,
}

/// The title's budget and our process's commit as what it uses. No tools
/// memory: a retail console.
// shortcut: all fields are filled whatever the flags ask for, and use is
// not split by bandwidth class (GPU-optimal use reads 0); track
// allocations per class if a title budgets from that field.
pub extern "system" fn XMemGetWorkingSetStatistics(_which: u32, out: *mut WorkingSet) -> BOOL {
    if out.is_null() {
        return 0;
    }
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    let (gpu_optimal, standard) = victory_gpu::console::current().title_memory();
    let stats = WorkingSet {
        title_limit: gpu_optimal + standard - TRANSFERRED.load(Ordering::Relaxed),
        title_used: counters.PagefileUsage,
        gpu_optimal_limit: gpu_optimal,
        ..Default::default()
    };
    // What the title is told, logged at the first asking and at every
    // 1,024th: "used" is all the process has committed, the layer's own
    // memory and the card's included (27 GB in run 60's gameplay against a
    // limit of 13.5 GB). A title that budgets its streaming from these two
    // figures believes it is far over its limit.
    // shortcut: the figure is not yet the title's own memory alone (its
    // heaps and the pages it mapped); count those if this line shows the
    // title asking.
    static ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let asked = ASKED.fetch_add(1, Ordering::Relaxed);
    if asked.is_multiple_of(1024) {
        tracing::info!(
            asked = asked + 1,
            which = _which,
            limit_mb = stats.title_limit >> 20,
            used_mb = stats.title_used >> 20,
            "the title asks how much memory it uses"
        );
    }
    // SAFETY: checked non-null; the title passes room for the structure.
    unsafe { out.write(stats) };
    1
}

/// Hands title memory (standard, in 2 MiB pages) to the system. The other
/// transfers involve tools memory, which a retail console has none of.
pub extern "system" fn XMemTransferMemory(kind: u32, size: usize) -> BOOL {
    const TITLE_TO_SYSTEM: u32 = 0;
    tracing::info!(kind, size, "XMemTransferMemory");
    if size == 0 {
        return 1;
    }
    let Some(size) = size.checked_next_multiple_of(2 * MIB) else {
        return 0;
    };
    let standard = victory_gpu::console::current().title_memory().1;
    let moved = kind == TITLE_TO_SYSTEM
        && TRANSFERRED
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                t.checked_add(size).filter(|&t| t <= standard)
            })
            .is_ok();
    moved.into()
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xmem",
        dlls: &["xmem.dll"],
        exports: vec![
            hle_export!(XMemAlloc),
            hle_export!(XMemFree),
            hle_export!(XMemAllocDefault),
            hle_export!(XMemFreeDefault),
            hle_export!(XMemIsToolsMemAvailable),
            hle_export!(XMemAllocatePhysicalPages),
            hle_export!(XMemFreePhysicalPages),
            hle_export!(XMemMapPhysicalPages),
            hle_export!(XMemMapPhysicalPagesScatter),
            hle_export!(XMemVirtualAlloc),
            hle_export!(XMemVirtualQuery),
            hle_export!(XMemGetWorkingSetStatistics),
            hle_export!(XMemTransferMemory),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// [`coalesce`] for this test thread, if said.
        pub(super) static COALESCE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
        /// [`VIEW_STATS`] of this test thread's own calls.
        pub(super) static COUNTED: std::cell::Cell<[u64; 7]> = const { std::cell::Cell::new([0; 7]) };
    }

    fn query(address: *const c_void) -> BasicInformation {
        // SAFETY: plain data; zero is a valid value.
        let mut info: BasicInformation = unsafe { std::mem::zeroed() };
        // SAFETY: a pseudo handle needs no closing.
        let process = unsafe { GetCurrentProcess() };
        assert_eq!(XMemVirtualQuery(process, address, &mut info), 1);
        info
    }

    #[test]
    fn alloc_honours_alignment_and_frees() {
        for (field, align) in [(0, 16), (4, 16), (0xc, 4096), (0x10, 64 * 1024)] {
            let attributes = field << ALIGNMENT_SHIFT | 1 << 29; // a graphics type
            let p = XMemAlloc(100, attributes);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "field {field:#x}");
            // SAFETY: 100 bytes were allocated.
            unsafe { p.cast::<u8>().write_bytes(0xab, 100) };
            XMemFree(p, attributes);
        }
        let p = XMemAlloc(4096, ALLOC_32BIT);
        assert!(!p.is_null() && (p as usize) < 1 << 32);
        XMemFree(p, ALLOC_32BIT);
    }

    #[test]
    fn virtual_alloc_meets_console_constraints() {
        const GPU_READ_WRITE: u32 = 0x2000;
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * MIB,
            PAGES_2M | MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | 7 << 12, // 40-bit addresses
            PAGE_READWRITE | GPU_READ_WRITE,
        );
        assert!(!p.is_null());
        assert_eq!(p as usize % (2 * MIB), 0);
        assert!((p as usize) < 1 << 40);
        let info = query(p);
        assert_eq!(info.host.State, MEM_COMMIT);
        assert_eq!(info.host.Protect, PAGE_READWRITE);
        assert_eq!(info.flags & GRAPHICS, GRAPHICS);
        // Write-watched, for the GPU side's copies.
        let (mut found, mut count, mut granularity) = ([0usize; 4], 4, 0);
        // SAFETY: room for `count` addresses.
        let watched = unsafe {
            windows_sys::Win32::System::Memory::GetWriteWatch(
                0,
                p,
                4 * MIB,
                found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        };
        assert_eq!(watched, 0);
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
        assert_eq!(query(p).flags, 0);
        // No address field: above the 32-bit range and below 2^40, where
        // the console keeps it.
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            16 << 30,
            MEM_RESERVE,
            GRAPHICS,
            PAGE_NOACCESS,
        );
        assert!(!p.is_null() && (p as usize) >= 1 << 32, "{p:p}");
        assert!((p as usize) + (16 << 30) <= 1 << 40, "{p:p}");
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn transfers_shrink_the_title_budget() {
        let mut before = WorkingSet::default();
        assert_eq!(XMemGetWorkingSetStatistics(0x2000_0001, &mut before), 1);
        assert!(before.title_used > 0);
        assert_eq!(XMemTransferMemory(0, 128 * MIB - 1), 1);
        assert_eq!(XMemTransferMemory(2, MIB), 0, "no tools memory");
        let standard = victory_gpu::console::current().title_memory().1;
        assert_eq!(XMemTransferMemory(0, standard), 0, "over budget");
        let mut after = WorkingSet::default();
        XMemGetWorkingSetStatistics(0x2000_0001, &mut after);
        assert_eq!(before.title_limit - after.title_limit, 128 * MIB);
    }

    #[test]
    fn scatter_unmap_groups_only_safe_increasing_zero_pages() {
        let base = 16 * PAGE;
        let entries = [
            (base, true),
            (base + PAGE, true),
            (base + 2 * PAGE, false),
            (base + 3 * PAGE, true),
            (base + 3 * PAGE, true),
            (0, true),
        ];
        let run = |offset: usize, ranges: &[(usize, usize)]| {
            scatter_unmap_run(entries[offset].0, entries.len() - offset, ranges, |i| {
                entries[offset + i]
            })
        };
        assert_eq!(run(0, &[]), 2);
        assert_eq!(run(3, &[]), 1);
        assert_eq!(run(5, &[]), 1);
        assert_eq!(run(0, &[(base, base + PAGE)]), 1);
        assert_eq!(run(0, &[(base + PAGE, base + 2 * PAGE)]), 1);
        assert_eq!(scatter_unmap_run(base + PAGE, 2, &[], |_| (base, true)), 1);
        assert_eq!(scatter_unmap_run(base, 2, &[], |_| (base + 2 * PAGE, true)), 1);
        assert_eq!(scatter_unmap_run(base + 1, 2, &[], |_| unreachable!()), 1);
        let end = usize::MAX - PAGE + 1;
        assert_eq!(scatter_unmap_run(end, 2, &[], |_| (0, true)), 1);
    }

    #[test]
    fn scatter_unmaps_adjacent_pages_and_stops_at_null_address() {
        let mut pages = [0usize; 5];
        let mut n = pages.len();
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let base = XMemVirtualAlloc(
            std::ptr::null_mut(),
            5 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!base.is_null());
        assert_eq!(XMemMapPhysicalPages(base, 5, pages.as_ptr()), base);
        let at = |i: usize| base.wrapping_byte_add(i * PAGE);
        // SAFETY: all five pages were mapped read-write above.
        unsafe {
            at(0).cast::<u8>().write(17);
            at(4).cast::<u8>().write(29);
        }
        let addresses = [at(1), at(2), std::ptr::null_mut(), at(3)];
        assert_eq!(XMemMapPhysicalPagesScatter(addresses.as_ptr(), 4, std::ptr::null()), 0);
        assert_eq!(query(at(1)).host.State, MEM_RESERVE);
        assert_eq!(query(at(2)).host.State, MEM_RESERVE);
        assert_eq!(query(at(3)).host.State, MEM_COMMIT);
        // SAFETY: the two outer pages are outside the unmapped range.
        assert_eq!(unsafe { at(0).cast::<u8>().read() }, 17);
        assert_eq!(unsafe { at(4).cast::<u8>().read() }, 29);

        assert_eq!(XMemMapPhysicalPages(at(1), 1, &pages[1]), at(1));
        assert_eq!(XMemMapPhysicalPages(at(2), 1, &pages[2]), at(2));
        let addresses = [at(1), at(2), at(3), std::ptr::null_mut(), at(4)];
        let scattered = [0, 0, pages[0], 0, 0];
        assert_eq!(XMemMapPhysicalPagesScatter(addresses.as_ptr(), addresses.len(), scattered.as_ptr()), 0);
        assert_eq!(query(at(1)).host.State, MEM_RESERVE);
        assert_eq!(query(at(2)).host.State, MEM_RESERVE);
        assert_eq!(query(at(3)).host.State, MEM_COMMIT);
        // SAFETY: the scatter mapped page 0 at slot 3 and left slot 4 alone.
        assert_eq!(unsafe { at(3).cast::<u8>().read() }, 17);
        assert_eq!(unsafe { at(4).cast::<u8>().read() }, 29);

        assert_eq!(XMemMapPhysicalPages(base, 5, std::ptr::null()), base);
        assert_eq!(XMemFreePhysicalPages(pages.len(), pages.as_ptr()), 1);
    }

    #[test]
    fn statistics_have_the_console_layout() {
        assert_eq!(size_of::<WorkingSet>(), 0x50);
        assert_eq!(size_of::<BasicInformation>(), 0x40);
        assert_eq!(std::mem::offset_of!(WorkingSet, gpu_optimal_limit), 0x28);
    }

    #[test]
    fn commit_limit_report_includes_request_and_system_commit() {
        let error = std::io::Error::from_raw_os_error(ERROR_COMMITMENT_LIMIT as i32);
        let info = PERFORMANCE_INFORMATION {
            CommitTotal: 512,
            CommitLimit: 768,
            PageSize: 4096,
            ..Default::default()
        };
        let report = commit_limit_message(4 * MIB, &error, Some(&info));
        assert!(report.contains("4194304 bytes"));
        assert!(report.contains("2 MiB of 3 MiB in use"));
        let fallback = commit_limit_message(4 * MIB, &error, None);
        for report in [&report, &fallback] {
            assert!(report.contains("os error 1455"));
            assert!(report.contains("Windows paging file size"));
        }
        assert!(!fallback.contains("System commit:"));
    }

    #[test]
    fn physical_pages_map_alias_and_unmap() {
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        assert_eq!(n, 3);
        // Packed into 21 bits as `number >> 4`, as the title keeps them.
        for p in pages {
            assert_eq!((p >> 4 & 0x1f_ffff) << 4, p);
        }
        // Reservations of 64 KiB pages, as titles make them for mapping.
        let reserve = || {
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                3 * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            )
        };
        let (a, b) = (reserve(), reserve());
        assert!(!a.is_null() && !b.is_null());
        let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);

        // The same page at a second address shows the same bytes.
        assert_eq!(
            XMemMapPhysicalPages(at(b, 1), 1, pages[2..].as_ptr()),
            at(b, 1)
        );
        // SAFETY: both pages are mapped read-write.
        unsafe {
            at(a, 2).cast::<u8>().write(7);
            assert_eq!(at(b, 1).cast::<u8>().read(), 7);
            a.cast::<u8>().write(1);
        }
        // Writes into pages (DirectStorage's) show through their views.
        assert!(write_physical(&pages[2..], 1, &[9]));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(b, 1).cast::<u8>().add(1).read() }, 9);

        // Unmapping the middle page keeps its neighbours.
        assert_eq!(
            XMemMapPhysicalPages(at(a, 1), 1, std::ptr::null()),
            at(a, 1)
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(at(a, 2)).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { at(a, 2).cast::<u8>().read() }, 7);

        // A scattered page lands in the hole.
        let addresses = [at(a, 1)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, pages.as_ptr()),
            1
        );
        // SAFETY: page 0 is now mapped at both `a` and `a + PAGE`.
        assert_eq!(unsafe { at(a, 1).cast::<u8>().read() }, 1);
        // Page number 0 unmaps.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, [0usize].as_ptr()),
            1
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        // The GPU's page-mapping copy: what `a + 2 pages` shows, at `b`.
        assert!(map_alias(b as u64, at(a, 2) as u64, PAGE as u64));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 7);
        assert!(map_alias(b as u64, 0, PAGE as u64));
        assert_eq!(query(b).host.State, MEM_RESERVE);
        assert!(!map_alias(b as u64, at(a, 1) as u64, PAGE as u64));
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
        assert_eq!(XMemIsToolsMemAvailable(), 0);
    }

    /// A thread that reads a mapped page in place, as the GPU side reads a
    /// texture, never meets the moment in which the runtime has taken the
    /// page's view away to map it again: mapping the same pages at the same
    /// address unmaps the view and maps a new one, and without the lock the
    /// reader faults on a page "reserved, never committed" (the crashes of
    /// runs 71 and 74).
    #[test]
    fn a_read_in_place_never_meets_the_gap_of_a_remapping() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            3 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        // SAFETY: mapped read-write.
        unsafe { a.cast::<u8>().write(1) };
        let first = a as usize;
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut reads = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let _reading = victory_gpu::title_memory_reading();
                    // SAFETY: the page is mapped whenever no change of
                    // mapping is under way, and none is while `_reading`
                    // is held.
                    assert_eq!(unsafe { (first as *const u8).read_volatile() }, 1);
                    reads += 1;
                }
                reads
            })
        };
        for _ in 0..2000 {
            // The same pages at the same address: the view goes and comes
            // back, with the same bytes.
            assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        }
        stop.store(true, Ordering::Relaxed);
        assert!(reader.join().expect("the reader never faulted") > 0);
        assert_eq!(XMemMapPhysicalPages(a, 3, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    #[test]
    fn mapping_no_pages_changes_nothing() {
        let pages = allocate(2);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        // SAFETY: both pages are mapped read-write.
        unsafe { a.cast::<u8>().write(5) };
        let second = a.wrapping_byte_add(PAGE);

        // A count of 0 at the start of a view, inside it and past it: with
        // pages, without (an unmap), and as the GPU's page-mapping copy.
        for at in [a, second, a.wrapping_byte_add(2 * PAGE)] {
            assert_eq!(XMemMapPhysicalPages(at, 0, pages.as_ptr()), at);
            assert_eq!(XMemMapPhysicalPages(at, 0, std::ptr::null()), at);
            assert!(map_alias(at as u64, a as u64, 0));
            assert!(map_alias(at as u64, 0, 0));
        }

        // The view is still the one view it was.
        let view = physical().views.get(&(a as usize)).map(|v| v.0);
        assert_eq!(view, Some(2 * PAGE));
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(second).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { a.cast::<u8>().read() }, 5);

        // A count of 1 still unmaps that page alone.
        assert_eq!(XMemMapPhysicalPages(second, 1, std::ptr::null()), second);
        assert_eq!(query(second).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    /// `n` pages of one new allocation.
    fn allocate(n: usize) -> Vec<usize> {
        let (mut pages, mut given) = (vec![0; n], n);
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
            1
        );
        assert_eq!(given, n);
        pages
    }

    /// The bytes of one page, through a view of our own.
    fn page_bytes(page: usize) -> Vec<u8> {
        let slice = physical().pages.get(&page).cloned().expect("known page");
        View::map(&slice, 1).expect("mapped").bytes().to_vec()
    }

    #[test]
    fn runs_group_consecutive_pages_of_one_allocation() {
        let (a, b) = (allocate(3), allocate(1));
        let lengths = |pages: &[usize], offset, len| {
            runs(pages, offset, len).map(|r| r.iter().map(|&(_, n)| n).collect::<Vec<_>>())
        };
        assert_eq!(lengths(&a, 0, 3 * PAGE), Some(vec![3]));
        // Only the pages the bytes reach.
        assert_eq!(lengths(&a, PAGE + 5, 10), Some(vec![1]));
        assert_eq!(lengths(&a, PAGE - 1, 2), Some(vec![2]));
        assert_eq!(lengths(&a, 0, 0), Some(vec![]));
        // Out of order, or across allocations: one run each.
        assert_eq!(
            lengths(&[a[0], a[2], a[1]], 0, 3 * PAGE),
            Some(vec![1, 1, 1])
        );
        assert_eq!(lengths(&[a[1], a[2], b[0]], 0, 3 * PAGE), Some(vec![2, 1]));
        // A slice that follows on in another section starts a run.
        assert_eq!(lengths(&[b[0], a[1]], 0, 2 * PAGE), Some(vec![1, 1]));
        // Too few pages for the bytes, or an unknown one.
        assert_eq!(lengths(&a, 0, 3 * PAGE + 1), None);
        assert_eq!(lengths(&[a[0], 0x10], 0, 2 * PAGE), None);
        assert_eq!(lengths(&a, usize::MAX, 2), None);
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn writes_cross_runs_and_allocations() {
        let (a, b) = (allocate(2), allocate(1));
        let pages = [a[0], a[1], b[0]];
        let data: Vec<u8> = (0..2 * PAGE + 64).map(|i| (i * 7 % 251) as u8).collect();
        let offset = PAGE - 32;
        // Past the third page: the second run is too short.
        assert!(!write_physical(&pages, offset, &data));
        assert!(write_physical(&pages, offset, &data[..2 * PAGE]));
        let seen: Vec<u8> = pages.iter().flat_map(|&p| page_bytes(p)).collect();
        assert!(seen[..offset].iter().all(|&b| b == 0), "before the offset");
        assert_eq!(seen[offset..offset + 2 * PAGE], data[..2 * PAGE]);
        assert!(seen[offset + 2 * PAGE..].iter().all(|&b| b == 0), "after");
        assert!(
            !write_physical(&[a[0], 0x10], 0, &data[..PAGE + 1]),
            "unknown page"
        );
        assert_eq!(XMemFreePhysicalPages(2, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    /// Pages say where the title has them mapped, byte for byte (after
    /// fork PR #571's test; its runs are whole pages).
    #[test]
    fn pages_say_where_they_are_mapped() {
        let pages = allocate(3);
        let reserve = |n: usize| {
            let at = XMemVirtualAlloc(std::ptr::null_mut(), n * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
            assert!(!at.is_null());
            at
        };
        // `b` keeps a page unmapped at either end, so that no run at `b`
        // ever touches one at `a`, wherever the host puts the two.
        let (a, b) = (reserve(3), reserve(4));
        assert!(shown_at(&pages, 0, 3 * PAGE).is_empty(), "mapped nowhere");
        // All three at `a`; the last two at `b`, from its second page on.
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        let second = b.wrapping_byte_add(PAGE);
        assert_eq!(XMemMapPhysicalPages(second, 2, pages[1..].as_ptr()), second);
        let (a, second) = (a as usize, second as usize);
        let sorted = |mut runs: Vec<(usize, usize)>| {
            runs.sort_unstable();
            runs
        };
        assert_eq!(shown_at(&pages, 0, PAGE), vec![(a, PAGE)]);
        // A request for part of a page names those bytes alone.
        assert_eq!(shown_at(&pages, 0x4000, 0x3000), vec![(a + 0x4000, 0x3000)]);
        // Across two pages, shown at two addresses each: one run at each.
        assert_eq!(
            shown_at(&pages, PAGE + 1, PAGE),
            sorted(vec![(a + PAGE + 1, PAGE), (second + 1, PAGE)])
        );
        // A page unmapped in the middle of a view: the rest still shows.
        let middle = (a + PAGE) as *mut c_void;
        assert_eq!(XMemMapPhysicalPages(middle, 1, std::ptr::null()), middle);
        assert_eq!(
            shown_at(&pages, 0, 3 * PAGE),
            sorted(vec![(a, PAGE), (a + 2 * PAGE, PAGE), (second, 2 * PAGE)])
        );
        // Too few pages for the bytes: nothing (the write fails too).
        assert!(shown_at(&pages, 0, 3 * PAGE + 1).is_empty());
        for (at, n) in [(a, 3), (second, 2)] {
            let at = at as *mut c_void;
            assert_eq!(XMemMapPhysicalPages(at, n, std::ptr::null()), at);
        }
        assert!(shown_at(&pages, 0, 3 * PAGE).is_empty());
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    #[test]
    fn page_windows_cover_one_run_in_place() {
        let (a, b) = (allocate(3), allocate(1));
        let mut window = page_window(&a, PAGE + 3, PAGE).expect("one run");
        assert_eq!(window.bytes().len(), PAGE);
        window.bytes().fill(0x5a);
        drop(window);
        let (second, third) = (page_bytes(a[1]), page_bytes(a[2]));
        assert!(second[..3].iter().all(|&b| b == 0));
        assert!(second[3..].iter().all(|&b| b == 0x5a));
        assert!(third[..3].iter().all(|&b| b == 0x5a));
        assert!(third[3..].iter().all(|&b| b == 0));
        // Several runs, or an unknown page: no window.
        assert!(page_window(&[a[0], b[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[a[1], a[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[0x10], 0, 1).is_none());
        // One page of several is still one run.
        assert!(page_window(&[a[0], b[0]], 0, PAGE).is_some());
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    /// Where the sections' whole views are kept (`whole_views`, under
    /// Wine), windows on pages of one section lie in that one view, at the
    /// pages' offsets, and what they write shows in the title's views.
    #[test]
    fn page_windows_of_one_section_share_its_whole_view() {
        let pages = allocate(3);
        let mut first = page_window(&pages, 0, 1).expect("one run");
        let mut third = page_window(&pages[2..], 0, 1).expect("one run");
        first.bytes()[0] = 0x11;
        third.bytes()[0] = 0x33;
        let (p, q) = (first.bytes().as_ptr() as usize, third.bytes().as_ptr() as usize);
        if whole_views() {
            assert_eq!(q.wrapping_sub(p), 2 * PAGE);
        }
        drop((first, third));
        let title = XMemVirtualAlloc(std::ptr::null_mut(), 3 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        assert!(!title.is_null());
        assert_eq!(XMemMapPhysicalPages(title, 3, pages.as_ptr()), title);
        // SAFETY: the three pages are mapped read-write there.
        unsafe {
            assert_eq!(title.cast::<u8>().read(), 0x11);
            assert_eq!(title.cast::<u8>().add(2 * PAGE).read(), 0x33);
        }
        assert_eq!(XMemMapPhysicalPages(title, 3, std::ptr::null()), title);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    /// The GPU side's copy-back writes a section through its kept view
    /// (`write_section`): the bytes show at every title address of the
    /// pages; a range past the section is refused, and the section goes
    /// when its pages and views do.
    #[test]
    fn a_section_written_through_its_kept_view_shows_at_the_titles_views() {
        if !on_wine() || !whole_views() {
            return;
        }
        let pages = allocate(2);
        let section = physical().pages[&pages[0]].section.handle as usize;
        let one = XMemVirtualAlloc(std::ptr::null_mut(), 2 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        let two = XMemVirtualAlloc(std::ptr::null_mut(), PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        assert!(!one.is_null() && !two.is_null());
        assert_eq!(XMemMapPhysicalPages(one, 2, pages.as_ptr()), one);
        assert_eq!(XMemMapPhysicalPages(two, 1, pages[1..].as_ptr()), two);
        let bytes = [0x6bu8; 40];
        assert!(write_section(section, (PAGE + 100) as u64, &bytes));
        // SAFETY: both reservations show the pages read-write; read only.
        unsafe {
            assert_eq!(std::slice::from_raw_parts(one.cast::<u8>().add(PAGE + 100), 40), bytes);
            assert_eq!(std::slice::from_raw_parts(two.cast::<u8>().add(100), 40), bytes);
        }
        assert!(!write_section(section, (2 * PAGE - 8) as u64, &bytes), "past the section's end");
        assert_eq!(XMemMapPhysicalPages(one, 2, std::ptr::null()), one);
        assert_eq!(XMemMapPhysicalPages(two, 1, std::ptr::null()), two);
        let kept = SECTIONS.lock().unwrap().get(&section).cloned().expect("a section committed whole is kept");
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
        // (Its handle may be another test's section's by now.)
        assert!(kept.upgrade().is_none(), "the section is gone, and with it what writes it");
    }

    /// Runs `f` with [`coalesce`] said for this thread.
    fn coalescing<T>(on: bool, f: impl FnOnce() -> T) -> T {
        let was = COALESCE.with(|c| c.replace(Some(on)));
        let out = f();
        COALESCE.with(|c| c.set(was));
        out
    }

    /// This thread's [`VIEW_STATS`] since `since` (and now, for the next).
    fn since(since: &mut [u64; 7]) -> [u64; 7] {
        let now = COUNTED.with(std::cell::Cell::get);
        let d = std::array::from_fn(|i| now[i] - since[i]);
        *since = now;
        d
    }

    /// A reservation of `n` 64 KiB pages for mapping, as titles make them.
    fn reservation(n: usize) -> *mut c_void {
        let at = XMemVirtualAlloc(std::ptr::null_mut(), n * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        assert!(!at.is_null());
        at
    }

    /// Our views inside `[base, base + n pages)`: their first page and how
    /// many they show.
    fn views_in(base: *mut c_void, n: usize) -> Vec<(usize, usize)> {
        let base = base as usize;
        physical()
            .views
            .range(base..base + n * PAGE)
            .map(|(at, (len, _))| ((at - base) / PAGE, len / PAGE))
            .collect()
    }

    fn byte(at: *mut c_void, page: usize) -> u8 {
        // SAFETY: the callers read pages they mapped.
        unsafe { at.cast::<u8>().add(page * PAGE).read_volatile() }
    }

    fn put(at: *mut c_void, page: usize, value: u8) {
        // SAFETY: the callers write pages they mapped read-write.
        unsafe { at.cast::<u8>().add(page * PAGE).write_volatile(value) }
    }

    /// A page-mapping copy of a run of pool pages is one view, and taking
    /// the run away again one unmap (`VICTORY_MAP_COALESCE`); without the
    /// switch, a view per page, as before. A run over the placeholders that
    /// pages unmapped one by one leave is mapped once they are joined.
    #[test]
    fn page_mapping_copies_map_runs_as_one_view() {
        let pages = allocate(4);
        let (pool, heap) = (reservation(4), reservation(4));
        assert_eq!(XMemMapPhysicalPages(pool, 4, pages.as_ptr()), pool);
        for k in 0..4 {
            put(pool, k, 10 + k as u8);
        }
        let mut counted = COUNTED.with(std::cell::Cell::get);
        coalescing(true, || {
            assert!(map_alias(heap as u64, pool as u64, 4 * PAGE as u64));
            assert_eq!(views_in(heap, 4), [(0, 4)]);
            let d = since(&mut counted);
            assert_eq!((d[MAPS], d[MAP_PAGES], d[UNMAPS]), (1, 4, 0));
            for k in 0..4 {
                assert_eq!(byte(heap, k), 10 + k as u8);
            }
            // The same pages: a write through one shows through the other.
            put(heap, 1, 21);
            assert_eq!(byte(pool, 1), 21);
            assert!(map_alias(heap as u64, 0, 4 * PAGE as u64));
            let d = since(&mut counted);
            assert_eq!((d[UNMAPS], d[UNMAP_PAGES], d[MAPS]), (1, 4, 0));
            for k in 0..4 {
                assert_eq!(query(heap.wrapping_byte_add(k * PAGE)).host.State, MEM_RESERVE);
            }
        });
        // As before: a view per page, which leaves a placeholder per page.
        coalescing(false, || {
            assert!(map_alias(heap as u64, pool as u64, 4 * PAGE as u64));
            assert_eq!(views_in(heap, 4), [(0, 1), (1, 1), (2, 1), (3, 1)]);
            assert!(map_alias(heap as u64, 0, 4 * PAGE as u64));
            assert!(views_in(heap, 4).is_empty());
        });
        // Page by page with the switch too (one call a page), then all
        // unmapped: four placeholders, cut by us, joined for the run.
        let heap = reservation(4);
        coalescing(true, || {
            for k in 0..4 {
                let (to, from) = (heap.wrapping_byte_add(k * PAGE), pool.wrapping_byte_add(k * PAGE));
                assert!(map_alias(to as u64, from as u64, PAGE as u64));
            }
            assert_eq!(views_in(heap, 4), [(0, 1), (1, 1), (2, 1), (3, 1)]);
            assert!(map_alias(heap as u64, 0, 4 * PAGE as u64));
            since(&mut counted);
            assert!(map_alias(heap as u64, pool as u64, 4 * PAGE as u64));
            assert_eq!(views_in(heap, 4), [(0, 4)]);
            let d = since(&mut counted);
            assert_eq!((d[MAPS], d[JOINS]), (1, 1), "one join of four placeholders, one map");
            for k in 0..4 {
                assert_eq!(byte(heap, k), if k == 1 { 21 } else { 10 + k as u8 });
            }
            assert!(map_alias(heap as u64, 0, 4 * PAGE as u64));
        });
        assert_eq!(XMemMapPhysicalPages(pool, 4, std::ptr::null()), pool);
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    /// Pages that show what a call asks for already are left as they are:
    /// no unmap and map again.
    #[test]
    fn pages_shown_already_are_left_as_they_are() {
        let pages = allocate(3);
        let (pool, heap) = (reservation(3), reservation(3));
        coalescing(true, || {
            assert_eq!(XMemMapPhysicalPages(pool, 3, pages.as_ptr()), pool);
            assert!(map_alias(heap as u64, pool as u64, 3 * PAGE as u64));
            put(pool, 0, 5);
            let mut counted = COUNTED.with(std::cell::Cell::get);
            assert!(map_alias(heap as u64, pool as u64, 3 * PAGE as u64));
            assert_eq!(XMemMapPhysicalPages(pool, 3, pages.as_ptr()), pool);
            assert!(map_alias(heap.wrapping_byte_add(PAGE) as u64, pool.wrapping_byte_add(PAGE) as u64, PAGE as u64));
            let d = since(&mut counted);
            assert_eq!((d[MAPS], d[UNMAPS], d[KEPT]), (0, 0, 7));
            assert_eq!(views_in(heap, 3), [(0, 3)]);
            assert_eq!(byte(heap, 0), 5);
            // One constant page over the run, as the title's copies map
            // pages it has nothing for: the first is shown already; then
            // the run goes, and the rest of it is mapped again.
            for k in 0..3 {
                assert!(map_alias(heap.wrapping_byte_add(k * PAGE) as u64, pool as u64, PAGE as u64));
            }
            for k in 0..3 {
                assert_eq!(byte(heap, k), 5);
            }
            put(heap, 2, 6);
            assert_eq!(byte(pool, 0), 6);
            assert!(map_alias(heap as u64, 0, 3 * PAGE as u64));
            assert_eq!(XMemMapPhysicalPages(pool, 3, std::ptr::null()), pool);
        });
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    /// A page taken out of a run, or mapped anew in it: the rest of the run
    /// keeps its bytes and the title's own protections; another view of the
    /// same pages is not touched; the pages say where they show.
    #[test]
    fn a_page_out_of_a_run_keeps_the_rest_and_its_protections() {
        use windows_sys::Win32::System::Memory::PAGE_READONLY;
        let pages = allocate(4);
        let (a, b) = (reservation(4), reservation(6));
        // `b` keeps a page unmapped at either end (see
        // `pages_say_where_they_are_mapped`).
        let b = b.wrapping_byte_add(PAGE);
        coalescing(true, || {
            assert_eq!(XMemMapPhysicalPages(a, 4, pages.as_ptr()), a);
            assert!(map_alias(b as u64, a as u64, 4 * PAGE as u64));
            assert_eq!(views_in(a, 4), [(0, 4)]);
            for k in 0..4 {
                put(a, k, 30 + k as u8);
            }
            // The title's own protection of a page of the run.
            let mut old = 0;
            // SAFETY: a page of ours, mapped.
            assert_ne!(unsafe { VirtualProtect(a.cast_const(), PAGE, PAGE_READONLY, &mut old) }, 0);
            let third = a.wrapping_byte_add(2 * PAGE);
            assert_eq!(XMemMapPhysicalPages(third, 1, std::ptr::null()), third);
            assert_eq!(views_in(a, 4), [(0, 2), (3, 1)]);
            assert_eq!(query(third).host.State, MEM_RESERVE);
            assert_eq!(query(a).host.Protect, PAGE_READONLY, "the title's protection stays");
            assert_eq!(query(a.wrapping_byte_add(PAGE)).host.Protect, PAGE_READWRITE);
            assert_eq!((byte(a, 0), byte(a, 1), byte(a, 3)), (30, 31, 33));
            put(a, 3, 43);
            // The other view of the pages is as it was.
            assert_eq!(views_in(b, 4), [(0, 4)]);
            assert_eq!((byte(b, 2), byte(b, 3)), (32, 43));
            let (a_at, b_at) = (a as usize, b as usize);
            let mut where_shown = shown_at(&pages, 0, 4 * PAGE);
            where_shown.sort_unstable();
            let mut expected = vec![(a_at, 2 * PAGE), (a_at + 3 * PAGE, PAGE), (b_at, 4 * PAGE)];
            expected.sort_unstable();
            assert_eq!(where_shown, expected);
            // Another page in its place: one map, nothing else touched.
            let mut counted = COUNTED.with(std::cell::Cell::get);
            assert_eq!(XMemMapPhysicalPages(third, 1, pages[0..].as_ptr()), third);
            let d = since(&mut counted);
            assert_eq!((d[MAPS], d[UNMAPS]), (1, 0));
            assert_eq!(byte(a, 2), 30);
            assert_eq!(query(a).host.Protect, PAGE_READONLY);
            // Asked for again, a view with the title's own protection is
            // mapped again, read-write, as every mapping was before.
            assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
            assert_eq!(query(a).host.Protect, PAGE_READWRITE);
            assert_eq!(byte(a, 0), 30);
            assert_eq!(XMemMapPhysicalPages(a, 4, std::ptr::null()), a);
            assert!(map_alias(b as u64, 0, 4 * PAGE as u64));
            assert!(shown_at(&pages, 0, 4 * PAGE).is_empty());
        });
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    /// Scattered pages at consecutive addresses are one view; one that is
    /// unknown fails the call with the entries after it left alone, as one
    /// by one.
    #[test]
    fn scattered_pages_at_consecutive_addresses_map_as_one_view() {
        let (pages, other) = (allocate(3), allocate(1));
        let a = reservation(4);
        let at = |k: usize| a.wrapping_byte_add(k * PAGE);
        coalescing(true, || {
            let mut counted = COUNTED.with(std::cell::Cell::get);
            let addresses = [at(0), at(1), at(2), at(3)];
            let scattered = [pages[0], pages[1], pages[2], other[0]];
            assert_eq!(XMemMapPhysicalPagesScatter(addresses.as_ptr(), 4, scattered.as_ptr()), 1);
            // Two sections: two runs.
            assert_eq!(views_in(a, 4), [(0, 3), (3, 1)]);
            assert_eq!(since(&mut counted)[MAPS], 2);
            put(a, 3, 77);
            assert_eq!(XMemMapPhysicalPagesScatter(addresses.as_ptr(), 3, [0usize; 3].as_ptr()), 1);
            assert_eq!(since(&mut counted)[UNMAPS], 1);
            assert_eq!(views_in(a, 4), [(3, 1)]);
            // An unknown page: the one before it mapped, the one after left
            // as it was.
            let scattered = [pages[0], 0x7ff0, pages[1], pages[2]];
            assert_eq!(XMemMapPhysicalPagesScatter(addresses.as_ptr(), 4, scattered.as_ptr()), 0);
            assert_eq!(query(at(0)).host.State, MEM_COMMIT);
            assert_eq!(query(at(1)).host.State, MEM_RESERVE);
            assert_eq!(query(at(2)).host.State, MEM_RESERVE);
            assert_eq!(byte(a, 3), 77);
            assert_eq!(XMemMapPhysicalPages(a, 4, std::ptr::null()), a);
        });
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, other.as_ptr()), 1);
    }

    /// A title thread that reads and writes a page of a view while other
    /// pages of the view are unmapped and mapped again, as the console lets
    /// it: the page was never given up, and the moment in which the rest of
    /// the view is mapped again is waited out in the fault handler. After
    /// avatarmanz 26d818c57's test of the same name.
    #[test]
    fn a_title_thread_goes_on_using_the_rest_of_a_view() {
        use std::sync::atomic::{AtomicBool, Ordering};
        victory_gpu::title_memory_gaps_waited(true);
        let pages = allocate(4);
        coalescing(true, || {
            for _ in 0..100 {
                // A new reservation each time: pieces of a view cut up stay
                // cut up until mapped again as one.
                let a = reservation(4);
                assert_eq!(XMemMapPhysicalPages(a, 4, pages.as_ptr()), a);
                let (first, stop) = (a as usize, AtomicBool::new(false));
                // SAFETY: mapped read-write.
                unsafe { (first as *mut u64).write_volatile(0) };
                std::thread::scope(|s| {
                    let title = s.spawn(|| {
                        let mut n = 0u64;
                        // SAFETY (the block): the first page is the title's
                        // all the time; a fault would end the process.
                        unsafe {
                            while !stop.load(Ordering::Relaxed) {
                                assert_eq!((first as *const u64).read_volatile(), n);
                                n += 1;
                                (first as *mut u64).write_volatile(n);
                            }
                        }
                        n
                    });
                    // SAFETY: as above.
                    while !title.is_finished() && unsafe { (first as *const u64).read_volatile() } < 100 {}
                    for page in [3, 2, 1] {
                        let at = a.wrapping_byte_add(page * PAGE);
                        assert_eq!(XMemMapPhysicalPages(at, 1, std::ptr::null()), at);
                        assert_eq!(XMemMapPhysicalPages(at, 1, pages[page..].as_ptr()), at);
                    }
                    stop.store(true, Ordering::Relaxed);
                    assert!(title.join().expect("the title's thread never faulted") > 0);
                });
                assert_eq!(XMemMapPhysicalPages(a, 4, std::ptr::null()), a);
            }
        });
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    /// A page-mapping copy's ranges in one call (`VICTORY_REMAP_BATCH`):
    /// mapped, unmapped and failed as one by one, with the switch and
    /// without (after vxlayer d5106b6e4's test).
    #[test]
    fn batched_aliases_map_as_one_by_one() {
        let pages = allocate(2);
        for on in [true, false] {
            coalescing(on, || {
                let (a, c) = (reservation(3), reservation(3));
                let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
                assert_eq!(XMemMapPhysicalPages(a, 1, pages.as_ptr()), a);
                assert_eq!(XMemMapPhysicalPages(at(a, 2), 1, pages[1..].as_ptr()), at(a, 2));
                put(a, 0, 5);
                put(a, 2, 6);
                // What `a` and `a + 2 pages` show, at `c` and `c + 1 page`;
                // a failing range (`a + 1 page` shows nothing) leaves the
                // others made.
                assert!(!map_aliases(&[
                    (c as u64, a as u64, PAGE as u64),
                    (at(c, 2) as u64, at(a, 1) as u64, PAGE as u64),
                    (at(c, 1) as u64, at(a, 2) as u64, PAGE as u64),
                ]));
                assert_eq!((byte(c, 0), byte(c, 1)), (5, 6));
                assert_eq!(query(at(c, 2)).host.State, MEM_RESERVE);
                assert!(map_aliases(&[(c as u64, 0, 2 * PAGE as u64)]));
                assert_eq!(query(c).host.State, MEM_RESERVE);
                assert_eq!(query(at(c, 1)).host.State, MEM_RESERVE);
                assert_eq!(XMemMapPhysicalPages(a, 3, std::ptr::null()), a);
            });
        }
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    /// Placeholders are made exactly a run: cut out of a larger one, joined
    /// from several, or left as they are when they are the run already.
    #[test]
    fn runs_are_mapped_over_exactly_one_placeholder() {
        let pages = allocate(5);
        let a = reservation(6);
        let at = |k: usize| a.wrapping_byte_add(k * PAGE);
        coalescing(true, || {
            let mut counted = COUNTED.with(std::cell::Cell::get);
            // Inside one: cut out of it, the pages beside it left reserved.
            assert_eq!(XMemMapPhysicalPages(at(2), 1, pages.as_ptr()), at(2));
            put(a, 2, 0x60);
            assert_eq!((query(at(1)).host.State, query(at(3)).host.State), (MEM_RESERVE, MEM_RESERVE));
            assert_eq!(since(&mut counted)[JOINS], 0);
            // Over two placeholders and a view of another page there: the
            // view goes, and its placeholder and the two around it are cut
            // at the run's ends and joined into one view of three.
            assert_eq!(XMemMapPhysicalPages(at(1), 3, pages[1..].as_ptr()), at(1));
            assert_eq!(views_in(a, 6), [(1, 3)]);
            assert_eq!(since(&mut counted)[JOINS], 1);
            assert_eq!(byte(a, 2), 0);
            put(a, 2, 0x62);
            assert_eq!((query(at(0)).host.State, query(at(4)).host.State), (MEM_RESERVE, MEM_RESERVE));
            // A run that starts at a placeholder's start and ends inside
            // another.
            assert_eq!(XMemMapPhysicalPages(at(1), 3, std::ptr::null()), at(1));
            assert_eq!(XMemMapPhysicalPages(at(0), 5, pages.as_ptr()), at(0));
            assert_eq!(views_in(a, 6), [(0, 5)]);
            assert_eq!(query(at(5)).host.State, MEM_RESERVE);
            assert_eq!((byte(a, 0), byte(a, 2)), (0x60, 0x62));
            assert_eq!(XMemMapPhysicalPages(a, 6, std::ptr::null()), a);
            // All of it one placeholder again: it can be let go whole.
            assert!(isolate(&mut physical().cuts, a as usize, 6 * PAGE));
        });
        // SAFETY: our reservation, nothing mapped in it.
        assert_ne!(unsafe { VirtualFree(a, 0, MEM_RELEASE) }, 0);
        // A placeholder the title cut itself is not joined across: a run
        // over its two parts fails, as it did before, and each part can
        // still be let go alone.
        let t = reservation(2);
        coalescing(true, || {
            // SAFETY: the title's own cut of its own placeholder.
            assert_ne!(unsafe { VirtualFree(t, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) }, 0);
            assert!(XMemMapPhysicalPages(t, 2, pages.as_ptr()).is_null());
            assert!(views_in(t, 2).is_empty());
            let second = t.wrapping_byte_add(PAGE);
            assert_eq!(XMemMapPhysicalPages(second, 1, pages.as_ptr()), second);
            assert_eq!(byte(t, 1), 0x60);
            assert_eq!(XMemMapPhysicalPages(second, 1, std::ptr::null()), second);
        });
        // SAFETY: the title's two placeholders, nothing mapped in them.
        unsafe {
            assert_ne!(VirtualFree(t, 0, MEM_RELEASE), 0);
            assert_eq!(query(t.wrapping_byte_add(PAGE)).host.State, MEM_RESERVE, "the other part is still there");
            assert_ne!(VirtualFree(t.wrapping_byte_add(PAGE), 0, MEM_RELEASE), 0);
        }
        assert_eq!(XMemFreePhysicalPages(5, pages.as_ptr()), 1);
    }
}
