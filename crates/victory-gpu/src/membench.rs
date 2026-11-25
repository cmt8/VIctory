//! What the memory calls behind title-memory tracking cost on the host at
//! hand, in the patterns the runtime and the GPU side make them: the
//! runtime's page mappings (`victory-hle` xmem: a placeholder split per
//! 64 KiB page, a section view in its place, unmapped back to a
//! placeholder), the questions the copies and textures ask between them
//! (`VirtualQuery`, `GetWriteWatch`, `VirtualProtect`,
//! `QueryWorkingSetEx`) and the kernel copies in and out of title memory.
//!
//! Only with `VICTORY_MEMBENCH=1`; the test returns at once otherwise. Run
//! under Wine with
//! `VICTORY_MEMBENCH=1 cargo xtask wine-test -- -p victory-gpu --release --lib membench -- --nocapture`.
//! Under Wine a view is a wineserver request on map and on unmap, and the
//! server keeps a process's views in a list it walks; these numbers say what
//! that costs as views pile up, and what the other calls wait meanwhile.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, GetWriteWatch, MEM_COMMIT, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE,
    MEM_REPLACE_PLACEHOLDER, MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION,
    MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
    SEC_COMMIT, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualProtect,
    VirtualQuery,
};
use windows_sys::Win32::System::ProcessStatus::{K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const PAGE: usize = 64 << 10;
const MEM_WRITE_WATCH: u32 = 0x20_0000;

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    fn NtWriteVirtualMemory(
        process: *mut c_void,
        base: *mut c_void,
        buffer: *const c_void,
        size: usize,
        written: *mut usize,
    ) -> i32;
}

fn on_wine() -> bool {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    // SAFETY: plain lookups in ntdll, always loaded.
    unsafe {
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
    }
}

fn on() -> bool {
    std::env::var_os("VICTORY_MEMBENCH").is_some_and(|v| v == "1")
}

/// Nanoseconds per call of `f`, over `n` calls.
fn ns_per(n: usize, mut f: impl FnMut(usize)) -> f64 {
    let started = Instant::now();
    for i in 0..n {
        f(i);
    }
    started.elapsed().as_nanos() as f64 / n as f64
}

fn report(what: &str, ns: f64) {
    println!("membench: {what}: {ns:.0} ns/op");
}

/// A placeholder of `pages` 64 KiB pages and a committed pagefile section
/// as big, mapped page by page as the runtime maps physical pages under
/// Wine (`SEC_COMMIT` there).
struct Pool {
    base: usize,
    pages: usize,
    section: HANDLE,
    mapped: Vec<bool>,
}

// SAFETY: the handle and the addresses may be used from any thread; the
// bench keeps each page to one thread.
unsafe impl Send for Pool {}
// SAFETY: as above.
unsafe impl Sync for Pool {}

impl Pool {
    fn new(pages: usize) -> Pool {
        let size = pages * PAGE;
        // SAFETY: a fresh placeholder and section, released on drop.
        unsafe {
            let base = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                size,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(base, 0, "placeholder of {pages} pages");
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_COMMIT,
                (size as u64 >> 32) as u32,
                size as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null(), "section of {pages} pages");
            Pool {
                base,
                pages,
                section,
                mapped: vec![false; pages],
            }
        }
    }

    fn at(&self, page: usize) -> usize {
        self.base + page * PAGE
    }

    /// As xmem's `map`: splits the placeholder at the page (harmlessly
    /// failing once it is split) and maps the section's page there.
    fn map(&self, page: usize) -> bool {
        let at = self.at(page);
        // SAFETY: placeholder pages of ours.
        let view = unsafe {
            VirtualFree(at as *mut c_void, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
            MapViewOfFile3(
                self.section,
                GetCurrentProcess(),
                at as *const c_void,
                (page * PAGE) as u64,
                PAGE,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        };
        view.Value as usize == at
    }

    fn split(&self, page: usize) {
        // SAFETY: placeholder pages of ours.
        unsafe { VirtualFree(self.at(page) as *mut c_void, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) };
    }

    fn remap(&self, page: usize) -> bool {
        let at = self.at(page);
        // SAFETY: as in `map`, the page already split.
        let view = unsafe {
            MapViewOfFile3(
                self.section,
                GetCurrentProcess(),
                at as *const c_void,
                (page * PAGE) as u64,
                PAGE,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        };
        view.Value as usize == at
    }

    fn unmap(&self, page: usize) -> bool {
        // SAFETY: a view of ours, back to a placeholder.
        unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.at(page) as *mut c_void,
                },
                MEM_PRESERVE_PLACEHOLDER,
            ) != 0
        }
    }

    /// Maps pages `[from, to)`; nanoseconds per page.
    fn grow(&mut self, from: usize, to: usize) -> f64 {
        let started = Instant::now();
        for page in from..to {
            assert!(self.map(page), "mapping page {page}");
            self.mapped[page] = true;
        }
        started.elapsed().as_nanos() as f64 / (to - from).max(1) as f64
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        for page in 0..self.pages {
            if self.mapped[page] {
                self.unmap(page);
            }
        }
        for page in 0..self.pages {
            // SAFETY: our placeholder pieces; the unsplit tail goes at once.
            unsafe { VirtualFree(self.at(page) as *mut c_void, 0, MEM_RELEASE) };
        }
        // SAFETY: our handle.
        unsafe { CloseHandle(self.section) };
    }
}

/// Committed private memory, write-watched or not.
struct Block(usize, usize);

impl Block {
    fn new(size: usize, watched: bool) -> Block {
        let flags = MEM_RESERVE | MEM_COMMIT | if watched { MEM_WRITE_WATCH } else { 0 };
        // SAFETY: a fresh allocation, freed on drop.
        let at = unsafe { VirtualAlloc(std::ptr::null(), size, flags, PAGE_READWRITE) } as usize;
        assert_ne!(at, 0);
        Block(at, size)
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        // SAFETY: our allocation.
        unsafe { VirtualFree(self.0 as *mut c_void, 0, MEM_RELEASE) };
    }
}

fn query(at: usize) -> MEMORY_BASIC_INFORMATION {
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    unsafe { VirtualQuery(at as *const c_void, &mut info, size_of_val(&info)) };
    info
}

fn ask_write_watch(at: usize, len: usize, found: &mut Vec<usize>) -> usize {
    let mut count = len / 4096;
    found.resize(count, 0);
    let mut granularity = 0;
    // SAFETY: room for `count` answers.
    let failed = unsafe {
        GetWriteWatch(1, at as *const c_void, len, found.as_mut_ptr().cast(), &mut count, &mut granularity)
    } != 0;
    assert!(!failed, "write watch of {len} bytes");
    count
}

fn working_set(at: usize, pages: usize) {
    // SAFETY: plain data.
    let mut info: [PSAPI_WORKING_SET_EX_INFORMATION; 256] = unsafe { std::mem::zeroed() };
    for (i, entry) in info[..pages].iter_mut().enumerate() {
        entry.VirtualAddress = (at + i * 4096) as *mut c_void;
    }
    // SAFETY: `pages` entries; our own process.
    unsafe {
        K32QueryWorkingSetEx(
            GetCurrentProcess(),
            info.as_mut_ptr().cast(),
            (pages * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
        )
    };
}

fn rpm(src: usize, dst: usize, len: usize) -> bool {
    let mut done = 0;
    // SAFETY: our own process; the kernel checks both ranges.
    unsafe { ReadProcessMemory(GetCurrentProcess(), src as *const c_void, dst as *mut c_void, len, &mut done) != 0 }
}

fn wpm(dst: usize, src: usize, len: usize) -> bool {
    let mut done = 0;
    // SAFETY: as above.
    unsafe { WriteProcessMemory(GetCurrentProcess(), dst as *const c_void, src as *const c_void, len, &mut done) != 0 }
}

fn ntwrite(dst: usize, src: usize, len: usize) -> bool {
    let mut done = 0;
    // SAFETY: as above.
    unsafe { NtWriteVirtualMemory(GetCurrentProcess(), dst as *mut c_void, src as *const c_void, len, &mut done) >= 0 }
}

/// The calls the GPU side makes between remaps, one round each, timed.
fn questions(label: &str, pool: &Pool, watched: &Block, plain: &Block, n: usize) {
    let mut found = Vec::new();
    let mapped = pool.at(1);
    report(&format!("{label} VirtualQuery of a mapped 64 KiB view"), ns_per(n, |_| {
        query(mapped);
    }));
    report(&format!("{label} VirtualQuery of a 1 GiB committed write-watched block"), ns_per(n, |_| {
        query(watched.0);
    }));
    report(&format!("{label} GetWriteWatch(reset) of 1 MiB"), ns_per(n, |_| {
        ask_write_watch(watched.0, 1 << 20, &mut found);
    }));
    report(&format!("{label} GetWriteWatch(reset) of 64 MiB"), ns_per(n.min(200), |_| {
        ask_write_watch(watched.0, 64 << 20, &mut found);
    }));
    report(&format!("{label} VirtualProtect of a 64 KiB view (read-only, then read-write: 2 calls)"), ns_per(n, |_| {
        let mut old = 0;
        // SAFETY: our view; only its protection changes.
        unsafe {
            VirtualProtect(mapped as *const c_void, PAGE, PAGE_READONLY, &mut old);
            VirtualProtect(mapped as *const c_void, PAGE, PAGE_READWRITE, &mut old);
        }
    }));
    report(&format!("{label} QueryWorkingSetEx of 16 pages"), ns_per(n, |_| working_set(plain.0, 16)));
    let byte = [0u8; 1];
    report(&format!("{label} ReadProcessMemory of one byte in each of 16 pages"), ns_per(n, |_| {
        for page in 0..16 {
            assert!(rpm(plain.0 + page * 4096, byte.as_ptr() as usize, 1));
        }
    }));
    report(&format!("{label} QueryWorkingSetEx of 256 pages"), ns_per(n, |_| working_set(plain.0, 256)));
    report(&format!("{label} MapViewOfFile anywhere + UnmapViewOfFile of 64 KiB (a DirectStorage page window)"), ns_per(n.min(500), |_| {
        // SAFETY: a view of our section, unmapped at once.
        unsafe {
            let view = windows_sys::Win32::System::Memory::MapViewOfFile(
                pool.section,
                windows_sys::Win32::System::Memory::FILE_MAP_WRITE,
                0,
                0,
                PAGE,
            );
            assert!(!view.Value.is_null());
            windows_sys::Win32::System::Memory::UnmapViewOfFile(view);
        }
    }));
    let buffer = vec![0u8; PAGE];
    report(&format!("{label} ReadProcessMemory 64 KiB from a view"), ns_per(n, |_| {
        assert!(rpm(mapped, buffer.as_ptr() as usize, PAGE));
    }));
    // The last page of the pool is never mapped.
    let hole = pool.at(pool.pages - 1);
    report(&format!("{label} ReadProcessMemory 4 KiB from an unmapped placeholder (fails)"), ns_per(n, |_| {
        assert!(!rpm(hole, buffer.as_ptr() as usize, 4096));
    }));
    // A refresh of a copy of view memory (`watch::writes`): the kernel's
    // write watch, which fails for a view, then the guards; once the guards
    // know the view, the guards alone.
    report(&format!("{label} GetWriteWatch of a mapped 64 KiB view (fails)"), ns_per(n, |_| {
        let mut count = PAGE / 4096;
        found.resize(count, 0);
        let mut granularity = 0;
        // SAFETY: room for `count` answers.
        let failed = unsafe {
            GetWriteWatch(1, mapped as *const c_void, PAGE, found.as_mut_ptr().cast(), &mut count, &mut granularity)
        } != 0;
        assert!(failed, "a view is not write-watched");
    }));
    assert!(crate::watch::writes(mapped, mapped + PAGE, true).is_some(), "a view is guarded");
    report(&format!("{label} watch::writes of a guarded 64 KiB view the guards know"), ns_per(n, |_| {
        assert!(crate::watch::writes(mapped, mapped + PAGE, false).is_none());
        assert!(crate::watch::writes(mapped, mapped + PAGE, true).is_some());
    }));
    // Read-write and unguarded again, for what follows.
    crate::watch::title_memory_remapped(mapped, PAGE);
    let mut old = 0;
    // SAFETY: our view; only its protection changes.
    unsafe { VirtualProtect(mapped as *const c_void, PAGE, PAGE_READWRITE, &mut old) };
}

#[test]
fn membench() {
    if !on() {
        return;
    }
    println!(
        "membench: wine {}",
        if on_wine() { "yes" } else { "no" }
    );
    let n = 2000;

    // Copies in and out of title memory, by size.
    let title = Block::new(64 << 20, true);
    let host = vec![7u8; 64 << 20];
    for len in [4096usize, PAGE, 1 << 20, 16 << 20] {
        let reps = (n * 4096 / len).clamp(20, n);
        let kb = len >> 10;
        report(&format!("ReadProcessMemory {kb} KiB title -> host"), ns_per(reps, |_| {
            assert!(rpm(title.0, host.as_ptr() as usize, len));
        }));
        report(&format!("ReadProcessMemory {kb} KiB host -> title (swapped)"), ns_per(reps, |_| {
            assert!(rpm(host.as_ptr() as usize, title.0, len));
        }));
        report(&format!("NtWriteVirtualMemory {kb} KiB host -> title"), ns_per(reps, |_| {
            assert!(ntwrite(title.0, host.as_ptr() as usize, len));
        }));
        report(&format!("WriteProcessMemory {kb} KiB host -> title"), ns_per(reps, |_| {
            assert!(wpm(title.0, host.as_ptr() as usize, len));
        }));
        report(&format!("memcpy {kb} KiB (for scale)"), ns_per(reps, |_| {
            // SAFETY: both ours, `len` bytes each.
            unsafe { std::ptr::copy_nonoverlapping(host.as_ptr(), title.0 as *mut u8, len) };
        }));
    }
    drop(title);

    // Map and unmap as views pile up, and the other questions meanwhile.
    let watched = Block::new(1 << 30, true);
    let plain = Block::new(1 << 20, false);
    // SAFETY: committed memory of ours; touched so the pages are there.
    unsafe { (plain.0 as *mut u8).write_bytes(1, plain.1) };
    let most: usize = std::env::var("VICTORY_MEMBENCH_VIEWS").ok().and_then(|v| v.parse().ok()).unwrap_or(40_000);
    let mut pool = Pool::new(most + 64);
    let mut views = 0;
    for target in [16usize, 1_000, 5_000, 10_000, 20_000, 40_000, 80_000] {
        if target > most {
            break;
        }
        let grown = pool.grow(views, target);
        views = target;
        report(&format!("{views} views: placeholder split + MapViewOfFile3 while growing"), grown);
        // Unmap and map again the last pages mapped (the newest views).
        let k = 200;
        let mut unmap_ns = 0u128;
        let mut map_ns = 0u128;
        for i in 0..k {
            let page = views - 1 - i % 50.min(views);
            let t = Instant::now();
            assert!(pool.unmap(page));
            unmap_ns += t.elapsed().as_nanos();
            let t = Instant::now();
            assert!(pool.remap(page));
            map_ns += t.elapsed().as_nanos();
        }
        report(&format!("{views} views: UnmapViewOfFile2(preserve placeholder) of a newest view"), unmap_ns as f64 / k as f64);
        report(&format!("{views} views: MapViewOfFile3 into its placeholder"), map_ns as f64 / k as f64);
        // The oldest views.
        let mut both = 0u128;
        for i in 0..k {
            let page = i % 50.min(views);
            let t = Instant::now();
            assert!(pool.unmap(page));
            assert!(pool.remap(page));
            both += t.elapsed().as_nanos();
        }
        report(&format!("{views} views: unmap + map of an oldest view"), both as f64 / k as f64);
        report(&format!("{views} views: placeholder split alone (already split, fails)"), ns_per(n, |i| pool.split(i % views)));
        questions(&format!("{views} views, idle:"), &pool, &watched, &plain, n);
    }

    // The same questions while another thread remaps (the runtime streaming),
    // and while it holds nothing but asks the same questions (contention of
    // the questions alone).
    let pool = Arc::new(pool);
    for churners in [1usize, 2] {
        let stop = Arc::new(AtomicBool::new(false));
        let remaps = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let threads: Vec<_> = (0..churners)
            .map(|t| {
                let (pool, stop, remaps) = (pool.clone(), stop.clone(), remaps.clone());
                std::thread::spawn(move || {
                    let mut i = 0;
                    while !stop.load(Ordering::Relaxed) {
                        let page = views - 1 - (t * 25 + i % 25);
                        assert!(pool.unmap(page));
                        assert!(pool.remap(page));
                        remaps.fetch_add(1, Ordering::Relaxed);
                        i += 1;
                    }
                })
            })
            .collect();
        let started = Instant::now();
        questions(&format!("{views} views, {churners} thread(s) remapping:"), &pool, &watched, &plain, 500);
        let elapsed = started.elapsed();
        stop.store(true, Ordering::Relaxed);
        for t in threads {
            t.join().unwrap();
        }
        println!(
            "membench: {views} views, {churners} thread(s) remapping: {:.0} remaps/s meanwhile",
            remaps.load(Ordering::Relaxed) as f64 / elapsed.as_secs_f64()
        );
    }
    // Questions from four threads at once, no remapping.
    let watched = Arc::new(watched);
    let started = Instant::now();
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let watched = watched.clone();
            std::thread::spawn(move || {
                let mut found = Vec::new();
                for _ in 0..n {
                    ask_write_watch(watched.0, 1 << 20, &mut found);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    report(
        "4 threads at once: GetWriteWatch(reset) of 1 MiB (wall time per call of all)",
        started.elapsed().as_nanos() as f64 / (4 * n) as f64,
    );
    drop(Arc::try_unwrap(pool).ok());
}

/// What a texture's staging memory costs on the host at hand
/// (`VICTORY_MEMBENCH=1`, on the first hardware adapter): a buffer made,
/// mapped, written and let go per upload, as `crate::texture::staging`
/// does, in the upload heap and in the PC's cached memory, against one
/// buffer written again. Written sequentially and in 64-byte runs a row
/// pitch apart (a detile's pattern).
#[test]
fn staging_bench() {
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1, IDXGIFactory4};
    if !on() {
        return;
    }
    // SAFETY: plain D3D12 setup.
    let device: ID3D12Device = unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let adapter: IDXGIAdapter1 = factory.EnumAdapters1(0).unwrap();
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).unwrap();
        device.unwrap()
    };
    let make = |size: usize, upload: bool| -> (ID3D12Resource, *mut u8) {
        let heap = if upload {
            D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() }
        } else {
            D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                ..Default::default()
            }
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size as u64,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let state = if upload { D3D12_RESOURCE_STATE_GENERIC_READ } else { D3D12_RESOURCE_STATE_COMMON };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping.
        unsafe {
            device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut buffer).unwrap();
            buffer.as_ref().unwrap().Map(0, None, Some(&mut data)).unwrap();
        }
        (buffer.unwrap(), data.cast())
    };
    let source = vec![7u8; 16 << 20];
    // Sequential, or 64-byte runs: a 256-byte-pitch tile's rows, tile after tile.
    let write = |to: *mut u8, size: usize, runs: bool| {
        if !runs {
            // SAFETY: `size` bytes of the mapping.
            unsafe { std::ptr::copy_nonoverlapping(source.as_ptr(), to, size) };
            return;
        }
        let pitch = 4096;
        let rows = size / pitch;
        let mut from = 0;
        for tile_row in (0..rows).step_by(8) {
            for tile_col in (0..pitch).step_by(64) {
                for row in tile_row..(tile_row + 8).min(rows) {
                    // SAFETY: inside the mapping and the source.
                    unsafe { std::ptr::copy_nonoverlapping(source.as_ptr().add(from), to.add(row * pitch + tile_col), 64) };
                    from = (from + 64) % (8 << 20);
                }
            }
        }
    };
    for size in [64usize << 10, 256 << 10, 1 << 20, 4 << 20] {
        let n = ((64usize << 20) / size).clamp(8, 512);
        let mb = |ns: f64| size as f64 / ns * 1e3;
        for upload in [true, false] {
            let heap = if upload { "upload heap" } else { "cached PC memory" };
            for runs in [false, true] {
                let how = if runs { "64-byte runs" } else { "sequential" };
                let ns = ns_per(n, |_| {
                    let (buffer, data) = make(size, upload);
                    write(data, size, runs);
                    drop(buffer);
                });
                println!("membench: staging {} KiB, made per upload, {heap}, {how}: {ns:.0} ns ({:.0} MB/s)", size >> 10, mb(ns));
                let (_buffer, data) = make(size, upload);
                write(data, size, runs);
                let ns = ns_per(n, |_| write(data, size, runs));
                println!("membench: staging {} KiB, one buffer written again, {heap}, {how}: {ns:.0} ns ({:.0} MB/s)", size >> 10, mb(ns));
            }
        }
    }
}

/// What a submission's texture checks cost on one thread and several
/// (`VICTORY_MEMBENCH=1`; `crate::host` `wide_textures`): each check as
/// `crate::texture::Staging` makes it under Wine (the copy-first read
/// through the kernel into a read buffer of its thread's, the hash of all
/// of it, and, for a texture that changed, the detiling of every level
/// into its staging memory), for a mix of texture sizes the world streams,
/// one after another and on the threads kept for the snapshots
/// (`crate::shadow::wide_run`).
#[test]
fn texture_checks_bench() {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, VirtualAlloc, VirtualFree};
    if !on() {
        return;
    }
    thread_local! {
        static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    struct Texture {
        layout: victory_formats::texture::Layout,
        memory: usize,
        staged: std::sync::Mutex<(Vec<u8>, Option<blake3::Hash>)>,
    }
    // A plain 2D texture of 8-byte texels (format 10), `levels` levels.
    let make = |side: u32, levels: u32, seed: usize| -> Texture {
        let words: [u32; 14] = [3, 0, 0, 0, side, 0, side, 1 | levels << 16, 10, 1, 0, 256, 3_145_728, 0];
        // SAFETY: fourteen words are a resource description.
        let desc: crate::resource::ResourceDesc = unsafe { std::mem::transmute(words) };
        let layout = crate::resource::texture_layout(&desc).expect("a console layout").planes[0].layout.clone();
        let size = layout.size as usize;
        // SAFETY: a fresh allocation of ours, freed at the end.
        let memory = unsafe { VirtualAlloc(std::ptr::null(), size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) } as usize;
        assert_ne!(memory, 0);
        // SAFETY: the allocation's `size` bytes.
        let bytes = unsafe { std::slice::from_raw_parts_mut(memory as *mut u8, size) };
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i * 31 + i / 4093 + seed) as u8;
        }
        let staged: usize = layout.mips.iter().map(|m| (m.width as usize * 8).next_multiple_of(256) * m.height as usize).sum();
        Texture { layout, memory, staged: std::sync::Mutex::new((vec![0; staged], None)) }
    };
    let mut textures = Vec::new();
    for (side, levels, count) in [(256u32, 9u32, 64usize), (512, 10, 24), (1024, 11, 8)] {
        for i in 0..count {
            textures.push(Arc::new(make(side, levels, i)));
        }
    }
    let bytes: usize = textures.iter().map(|t| t.layout.size as usize).sum();
    // One check: read, hash, and detile if the hash changed (`changed`
    // forgets the hash first, as a texture that streamed in does).
    let check = |t: &Texture, changed: bool| {
        let size = t.layout.size as usize;
        READ.with(|read| {
            let mut read = read.borrow_mut();
            if read.len() < size {
                read.resize(size, 0);
            }
            let read = &mut read[..size];
            assert!(crate::host::read_into(t.memory as *const u8, read));
            let hash = blake3::hash(read);
            let mut staged = t.staged.lock().unwrap();
            if changed {
                staged.1 = None;
            }
            if staged.1 == Some(hash) {
                return;
            }
            staged.1 = Some(hash);
            let mut at = 0;
            for (mip, m) in t.layout.mips.iter().enumerate() {
                let pitch = (m.width as usize * 8).next_multiple_of(256);
                let len = pitch * m.height as usize;
                t.layout.detile(read, mip as u32, 0, &mut staged.0[at..at + len], pitch, len).expect("detiled");
                at += len;
            }
        });
    };
    let mb_s = |ms: f64| bytes as f64 / (1 << 20) as f64 / (ms / 1e3);
    for changed in [true, false] {
        let what = if changed { "changed (read, hash, detile)" } else { "unchanged (read, hash)" };
        for round in 0..3 {
            let started = Instant::now();
            for t in &textures {
                check(t, changed);
            }
            let one = started.elapsed().as_secs_f64() * 1e3;
            let started = Instant::now();
            let jobs: Vec<Box<dyn FnOnce() + Send>> = textures
                .iter()
                .map(|t| {
                    let t = t.clone();
                    Box::new(move || check(&t, changed)) as Box<dyn FnOnce() + Send>
                })
                .collect();
            crate::shadow::wide_run(jobs);
            let wide = started.elapsed().as_secs_f64() * 1e3;
            println!(
                "membench: texture checks, {} textures {:.0} MB, {what}, round {round}: one thread {one:.1} ms ({:.0} MB/s), wide {wide:.1} ms ({:.0} MB/s)",
                textures.len(),
                bytes as f64 / (1 << 20) as f64,
                mb_s(one),
                mb_s(wide)
            );
        }
    }
    for t in &textures {
        // SAFETY: our allocation.
        unsafe { VirtualFree(t.memory as *mut c_void, 0, MEM_RELEASE) };
    }
}

/// What the questions about a texture streaming in cost (`VICTORY_MEMBENCH=1`):
/// a 4 MiB texture in a placeholder with every other 64 KiB block mapped,
/// as the texture pass asks how much of it is there
/// (`crate::texture::mapped_bytes`) and a refresh reads it around its
/// holes, from the runtime's word (`crate::inplace::view_books`, a pool
/// whose placeholder and views were told of) and from the kernel's (a pool
/// that was not); idle, and while another thread remaps pages of a third
/// pool (the runtime streaming).
#[test]
fn view_books_bench() {
    if !on() {
        return;
    }
    const BLOCKS: usize = 64;
    let mut told = Pool::new(BLOCKS + 1);
    let mut untold = Pool::new(BLOCKS + 1);
    crate::inplace::title_memory_placeholder(told.base, (BLOCKS + 1) * PAGE);
    for block in (0..BLOCKS).step_by(2) {
        assert!(told.map(block));
        assert!(untold.map(block));
        (told.mapped[block], untold.mapped[block]) = (true, true);
        crate::inplace::title_memory_view(told.at(block), PAGE, told.section as usize, (block * PAGE) as u64);
    }
    let len = BLOCKS * PAGE;
    assert_eq!(crate::inplace::mapped_bytes(told.base, len), Some((len / 2) as u64), "the books know the told pool");
    assert_eq!(crate::inplace::mapped_bytes(untold.base, len), None, "and not the other");
    assert_eq!(crate::texture::mapped_bytes(told.base as *const u8, len), (len / 2) as u64);
    assert_eq!(crate::texture::mapped_bytes(untold.base as *const u8, len), (len / 2) as u64);
    let mut read = vec![0u8; len];
    let round = |label: &str, n: usize, read: &mut Vec<u8>| {
        report(&format!("{label}: mapped_bytes of a half-mapped 4 MiB texture, from the books"), ns_per(n, |_| {
            std::hint::black_box(crate::texture::mapped_bytes(told.base as *const u8, len));
        }));
        report(&format!("{label}: mapped_bytes of a half-mapped 4 MiB texture, asking the kernel"), ns_per(n, |_| {
            std::hint::black_box(crate::texture::mapped_bytes(untold.base as *const u8, len));
        }));
        report(&format!("{label}: read around the holes, from the books"), ns_per(n, |_| {
            let parts = crate::inplace::mapped_parts(told.base, len).expect("told");
            read.fill(0);
            for (at, n) in parts {
                assert!(crate::host::read_into(at as *const u8, &mut read[at - told.base..at - told.base + n]));
            }
        }));
        report(&format!("{label}: read around the holes, the kernel copy (fails, then asks)"), ns_per(n, |_| {
            assert!(!crate::host::read_into(untold.base as *const u8, read));
            crate::host::copy_mapped(read.as_mut_ptr(), untold.base as *const u8, len, false);
        }));
    };
    round("idle", 200, &mut read);
    let mut churn = Pool::new(64);
    for page in 0..50 {
        assert!(churn.map(page));
        churn.mapped[page] = true;
    }
    let churn = Arc::new(churn);
    for remappers in [1usize, 2] {
        let stop = Arc::new(AtomicBool::new(false));
        let threads: Vec<_> = (0..remappers)
            .map(|t| {
                let (churn, stop) = (churn.clone(), stop.clone());
                std::thread::spawn(move || {
                    let mut i = 0;
                    while !stop.load(Ordering::Relaxed) {
                        let page = t * 25 + i % 25;
                        assert!(churn.unmap(page));
                        assert!(churn.remap(page));
                        i += 1;
                    }
                })
            })
            .collect();
        round(&format!("{remappers} thread(s) remapping"), 40, &mut read);
        stop.store(true, Ordering::Relaxed);
        for t in threads {
            t.join().unwrap();
        }
    }
    for pool in [&told, &untold] {
        crate::watch::title_memory_remapped(pool.base, (BLOCKS + 1) * PAGE);
    }
    crate::inplace::forget_placeholder(told.base);
    // The pools' views are unmapped by their drops.
    drop(churn);
}

/// What the guards' protection changes cost by block and by view
/// (`VICTORY_MEMBENCH=1`; `crate::watch` `guard_runs`): 16 blocks of title
/// memory in four views of four consecutive physical pages each, as the
/// runtime maps a run of pages, guarded and opened block by block (with the
/// call over the whole range first, which spans views and is refused) and
/// a call per view; idle and while other threads remap.
#[test]
fn guard_runs_bench() {
    if !on() {
        return;
    }
    const VIEW: usize = 4;
    const VIEWS: usize = 4;
    const BLOCKS: usize = VIEW * VIEWS;
    let mut pool = Pool::new(BLOCKS + 1);
    for v in 0..VIEWS {
        let at = pool.at(v * VIEW);
        // SAFETY: placeholder pages of the pool's: split out as one piece of
        // four pages and replaced by a view of four consecutive pages.
        let view = unsafe {
            VirtualFree(at as *mut c_void, VIEW * PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
            MapViewOfFile3(
                pool.section,
                GetCurrentProcess(),
                at as *const c_void,
                (v * VIEW * PAGE) as u64,
                VIEW * PAGE,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(view.Value as usize, at, "view {v}");
    }
    let protect = |from: usize, blocks: usize, prot: u32| -> bool {
        let mut old = 0;
        // SAFETY: our views; only their protection changes.
        unsafe { VirtualProtect(pool.at(from) as *const c_void, blocks * PAGE, prot, &mut old) != 0 }
    };
    assert!(!protect(0, BLOCKS, PAGE_READONLY), "a range over four views is refused (Wine; Windows likewise)");
    let round = |label: &str, n: usize| {
        report(&format!("{label}: guard 16 blocks in 4 views, block by block (after the refused call over all)"), ns_per(n, |_| {
            assert!(!protect(0, BLOCKS, PAGE_READONLY));
            for b in 0..BLOCKS {
                assert!(protect(b, 1, PAGE_READONLY));
            }
        }));
        report(&format!("{label}: open 16 blocks in 4 views, block by block"), ns_per(n, |_| {
            for b in 0..BLOCKS {
                assert!(protect(b, 1, PAGE_READWRITE));
            }
        }));
        report(&format!("{label}: guard 16 blocks in 4 views, a call per view"), ns_per(n, |_| {
            for v in 0..VIEWS {
                assert!(protect(v * VIEW, VIEW, PAGE_READONLY));
            }
        }));
        report(&format!("{label}: open 16 blocks in 4 views, a call per view"), ns_per(n, |_| {
            for v in 0..VIEWS {
                assert!(protect(v * VIEW, VIEW, PAGE_READWRITE));
            }
        }));
    };
    round("idle", 500);
    let mut churn = Pool::new(64);
    for page in 0..50 {
        assert!(churn.map(page));
        churn.mapped[page] = true;
    }
    let churn = Arc::new(churn);
    for remappers in [1usize, 2] {
        let stop = Arc::new(AtomicBool::new(false));
        let threads: Vec<_> = (0..remappers)
            .map(|t| {
                let (churn, stop) = (churn.clone(), stop.clone());
                std::thread::spawn(move || {
                    let mut i = 0;
                    while !stop.load(Ordering::Relaxed) {
                        let page = t * 25 + i % 25;
                        assert!(churn.unmap(page));
                        assert!(churn.remap(page));
                        i += 1;
                    }
                })
            })
            .collect();
        round(&format!("{remappers} thread(s) remapping"), 20);
        stop.store(true, Ordering::Relaxed);
        for t in threads {
            t.join().unwrap();
        }
    }
    for v in 0..VIEWS {
        // SAFETY: the views mapped above, back to placeholders.
        unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS { Value: pool.at(v * VIEW) as *mut c_void },
                MEM_PRESERVE_PLACEHOLDER,
            );
        }
    }
    pool.mapped.fill(false);
}

/// What a clock read costs (`VICTORY_MEMBENCH=1`): the status lines time
/// the guards' calls, the kernel's write-watch askings and the refreshes'
/// copies with two of them each.
#[test]
fn clock_bench() {
    if !on() {
        return;
    }
    report("Instant::now + elapsed", ns_per(1_000_000, |_| {
        std::hint::black_box(Instant::now().elapsed());
    }));
}
