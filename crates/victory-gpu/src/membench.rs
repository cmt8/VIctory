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
