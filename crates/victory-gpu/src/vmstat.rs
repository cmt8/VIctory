//! Our calls that take the system's lock on the memory map, timed by
//! caller and by thread (`VICTORY_VM_STATS=1`; off by default, in every
//! build).
//!
//! Under Wine every `VirtualProtect`, `VirtualQuery`, `GetWriteWatch`,
//! `QueryWorkingSetEx`, view mapping and page fault takes one process-wide
//! lock in ntdll (`virtual_mutex`), held across wineserver requests: a
//! call costs its own work plus the wait for whoever holds the lock. The
//! status lines time some of our callers (the guards' protection changes,
//! the write-watch askings), the frame log the runtime's remapping; this
//! module times every caller of ours the same way, on the thread that
//! made the call, with the threads' own processor time beside it, so a
//! frame can be split into running, waiting in those calls, and the rest.
//!
//! A call is timed with two reads of the performance counter
//! ([`time`]) and added to the calling thread's own row ([`Row`]): no lock
//! and no shared cache line between threads. Calls inside a timed call
//! (the fault handler's protection change) count for their own caller and
//! not again in the thread's total. A call of [`SLOW_US`] or more is
//! counted apart as well: an idle call is a few microseconds under Wine
//! (`membench`), so the time in those is mostly time spent waiting.
//!
//! Every `VICTORY_VM_STATS_EVERY` seconds (5), at a present, one line per
//! caller, one per thread and one in all, per presented frame, start with
//! `vm stats:`. Off, [`time`] is one load of a flag and a branch.

use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};

use windows_sys::Win32::Foundation::{FILETIME, HANDLE};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Threading::{
    GetCurrentThreadId, GetThreadDescription, GetThreadTimes, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
};

/// Our callers of the calls that take the lock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Site {
    /// `VirtualProtect` of the guards (`crate::watch`), by what it is for,
    /// in the order of `watch::PROTECTS`' rows.
    ProtectGuard,
    ProtectGuardOther,
    ProtectOpenFault,
    ProtectOpenWrite,
    ProtectOpenPages,
    ProtectOpenOther,
    ProtectOpenMapped,
    /// `GetWriteWatch` of a submission's ranges asked ahead, and of one
    /// range (`crate::watch`).
    WatchAhead,
    WatchRange,
    /// `VirtualQuery` of the guards: whether memory is a view, where a
    /// commitment ends, whether a faulting page is writable.
    QueryView,
    QueryUncommitted,
    QueryWritable,
    /// The handler of write faults on guarded memory, all of it (its
    /// protection change and query count again under their own sites).
    Fault,
    /// `VirtualQuery` of a copy read around holes (`host::read_sparse`).
    QueryCopy,
    /// The kernel's copy (`ReadProcessMemory`) out of title memory, and
    /// the copy-back's write into it (`host::write_mapped`).
    KernelRead,
    KernelWrite,
    /// `VirtualQuery` and `QueryWorkingSetEx` of the texture refreshes.
    QueryTexture,
    WorkingSet,
    /// `VirtualQuery` of the in-place copies (`crate::inplace`).
    QueryInplace,
    /// The runtime changing what title memory shows (`victory-hle` xmem):
    /// from asking for the remapping lock until letting it go, its view
    /// calls included.
    Remap,
    /// DirectStorage's windows on physical pages (`victory-hle` xmem),
    /// mapped and unmapped.
    DiskWindow,
    /// `XMemVirtualQuery` for the title (`victory-hle` xmem).
    TitleQuery,
    Count,
}

const SITES: usize = Site::Count as usize;

impl Site {
    const ALL: [Site; SITES] = [
        Site::ProtectGuard,
        Site::ProtectGuardOther,
        Site::ProtectOpenFault,
        Site::ProtectOpenWrite,
        Site::ProtectOpenPages,
        Site::ProtectOpenOther,
        Site::ProtectOpenMapped,
        Site::WatchAhead,
        Site::WatchRange,
        Site::QueryView,
        Site::QueryUncommitted,
        Site::QueryWritable,
        Site::Fault,
        Site::QueryCopy,
        Site::KernelRead,
        Site::KernelWrite,
        Site::QueryTexture,
        Site::WorkingSet,
        Site::QueryInplace,
        Site::Remap,
        Site::DiskWindow,
        Site::TitleQuery,
    ];

    /// The guards' protection change of `watch::PROTECTS`' row `kind`.
    pub(crate) fn protect(kind: usize) -> Site {
        Site::ALL[(Site::ProtectGuard as usize + kind).min(Site::ProtectOpenMapped as usize)]
    }

    /// One word for the lines (and `perf-summary`), then what it is.
    fn name(self) -> (&'static str, &'static str) {
        match self {
            Site::ProtectGuard => ("protect-guard", "VirtualProtect, guarding (watch::protect_here)"),
            Site::ProtectGuardOther => ("protect-guard-other", "VirtualProtect, guarding other views of the pages"),
            Site::ProtectOpenFault => ("protect-open-fault", "VirtualProtect, opening for a write fault"),
            Site::ProtectOpenWrite => ("protect-open-write", "VirtualProtect, opening for a write through the kernel"),
            Site::ProtectOpenPages => ("protect-open-pages", "VirtualProtect, opening for a DirectStorage write"),
            Site::ProtectOpenOther => ("protect-open-other", "VirtualProtect, opening other views of the pages"),
            Site::ProtectOpenMapped => ("protect-open-mapped", "VirtualProtect, opening where a view was mapped"),
            Site::WatchAhead => ("watch-ahead", "GetWriteWatch, a submission's ranges asked ahead"),
            Site::WatchRange => ("watch-range", "GetWriteWatch, one range (watch::writes)"),
            Site::QueryView => ("query-view", "VirtualQuery, whether memory is a view (watch::is_view)"),
            Site::QueryUncommitted => ("query-uncommitted", "VirtualQuery, where a commitment ends (watch)"),
            Site::QueryWritable => ("query-writable", "VirtualQuery in the fault handler (watch::writable)"),
            Site::Fault => ("fault-handler", "the write-fault handler, after Wine's own fault path"),
            Site::QueryCopy => ("query-copy", "VirtualQuery, a copy read around holes (host::read_sparse)"),
            Site::KernelRead => ("kernel-read", "ReadProcessMemory out of title memory (host::read)"),
            Site::KernelWrite => ("kernel-write", "the copy-back's kernel write into title memory (host::write_mapped)"),
            Site::QueryTexture => ("query-texture", "VirtualQuery of the texture refreshes"),
            Site::WorkingSet => ("working-set", "QueryWorkingSetEx of the texture refreshes"),
            Site::QueryInplace => ("query-inplace", "VirtualQuery of the in-place copies"),
            Site::Remap => ("remap", "the runtime remapping title memory, lock asked for to let go (xmem)"),
            Site::DiskWindow => ("disk-window", "DirectStorage page windows mapped and unmapped (xmem)"),
            Site::TitleQuery => ("title-query", "XMemVirtualQuery for the title (xmem)"),
            Site::Count => ("count", ""),
        }
    }
}

/// What a thread was seen doing, by bit.
pub const SUBMIT: u8 = 1;
pub const RECORD: u8 = 2;
pub const PRESENT: u8 = 4;

/// A call of this many microseconds or more is counted apart
/// (`VICTORY_VM_STATS_SLOW_US`).
const SLOW_US: u64 = 50;

/// 0: not asked yet, 1: off, 2: on.
static STATE: AtomicU8 = AtomicU8::new(0);
/// Counter ticks a second, and of [`SLOW_US`].
static FREQUENCY: AtomicU64 = AtomicU64::new(1);
static SLOW_TICKS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Whether calls are timed; the first call reads `VICTORY_VM_STATS`.
#[inline]
pub fn on() -> bool {
    match STATE.load(Relaxed) {
        2 => true,
        1 => false,
        _ => decide(),
    }
}

#[cold]
fn decide() -> bool {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let on = std::env::var_os("VICTORY_VM_STATS").is_some_and(|v| v == "1");
        if on {
            let mut frequency = 0;
            // SAFETY: a valid out pointer.
            unsafe { QueryPerformanceFrequency(&mut frequency) };
            let frequency = (frequency as u64).max(1);
            let slow = std::env::var("VICTORY_VM_STATS_SLOW_US")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(SLOW_US);
            FREQUENCY.store(frequency, Relaxed);
            SLOW_TICKS.store(slow.saturating_mul(frequency) / 1_000_000, Relaxed);
            tracing::info!(
                slow_us = slow,
                "VICTORY_VM_STATS=1: our calls that take the lock on the memory map are timed by caller and thread (\"vm stats:\" lines)"
            );
        }
        STATE.store(if on { 2 } else { 1 }, Relaxed);
    });
    STATE.load(Relaxed) == 2
}

#[inline]
fn ticks() -> u64 {
    let mut now = 0;
    // SAFETY: a valid out pointer.
    unsafe { QueryPerformanceCounter(&mut now) };
    now as u64
}

/// One thread's counts, never reset: calls, ticks, calls of [`SLOW_US`] or
/// more and their ticks, and the longest call since the last report, per
/// site; the same for the thread's outermost calls; what it was seen doing.
#[repr(align(64))]
struct Row {
    thread: AtomicU32,
    handle: AtomicUsize,
    roles: AtomicU8,
    sites: [[AtomicU64; 5]; SITES],
    all: [AtomicU64; 4],
}

/// Rows for this many threads; later threads share the last.
const ROWS: usize = 192;
static ROW: [Row; ROWS] = [const {
    Row {
        thread: AtomicU32::new(0),
        handle: AtomicUsize::new(0),
        roles: AtomicU8::new(0),
        sites: [const { [const { AtomicU64::new(0) }; 5] }; SITES],
        all: [const { AtomicU64::new(0) }; 4],
    }
}; ROWS];
static TAKEN: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// This thread's row plus one (0: none yet), the roles it was marked
    /// with, and how many timed calls it is inside.
    static MINE: Cell<(u16, u8, u8)> = const { Cell::new((0, 0, 0)) };
}

fn row() -> &'static Row {
    let (at, roles, depth) = MINE.with(Cell::get);
    if at != 0 {
        return &ROW[usize::from(at) - 1];
    }
    let at = TAKEN.fetch_add(1, Relaxed).min(ROWS - 1);
    let row = &ROW[at];
    if at < ROWS - 1 {
        // SAFETY: plain queries; the handle is kept for the reports.
        let id = unsafe { GetCurrentThreadId() };
        let handle = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, id) };
        row.handle.store(handle as usize, Relaxed);
        row.thread.store(id, Relaxed);
    }
    MINE.with(|m| m.set((at as u16 + 1, roles, depth)));
    row
}

/// The calling thread was seen doing `role` ([`SUBMIT`], ...).
#[inline]
pub fn mark(role: u8) {
    if !on() {
        return;
    }
    let (at, roles, _) = MINE.with(Cell::get);
    if at != 0 && roles & role != 0 {
        return;
    }
    let row = row();
    row.roles.fetch_or(role, Relaxed);
    MINE.with(|m| {
        let (at, roles, depth) = m.get();
        m.set((at, roles | role, depth));
    });
}

/// A call being timed: started by [`time`], counted when dropped.
#[must_use]
pub struct Timed {
    started: u64,
    site: Site,
}

/// Times what follows, until the result is dropped, as a call of `site`.
#[inline]
pub fn time(site: Site) -> Timed {
    if !on() {
        return Timed { started: 0, site };
    }
    MINE.with(|m| {
        let (at, roles, depth) = m.get();
        m.set((at, roles, depth.saturating_add(1)));
    });
    Timed { started: ticks(), site }
}

impl Drop for Timed {
    #[inline]
    fn drop(&mut self) {
        if self.started != 0 {
            done(self.site, ticks().saturating_sub(self.started));
        }
    }
}

#[cold]
fn done(site: Site, took: u64) {
    let row = row();
    let slow = took >= SLOW_TICKS.load(Relaxed);
    let counts = &row.sites[site as usize];
    counts[0].fetch_add(1, Relaxed);
    counts[1].fetch_add(took, Relaxed);
    if slow {
        counts[2].fetch_add(1, Relaxed);
        counts[3].fetch_add(took, Relaxed);
    }
    if took > counts[4].load(Relaxed) {
        counts[4].fetch_max(took, Relaxed);
    }
    let outermost = MINE.with(|m| {
        let (at, roles, depth) = m.get();
        let depth = depth.saturating_sub(1);
        m.set((at, roles, depth));
        depth == 0
    });
    if outermost {
        row.all[0].fetch_add(1, Relaxed);
        row.all[1].fetch_add(took, Relaxed);
        if slow {
            row.all[2].fetch_add(1, Relaxed);
            row.all[3].fetch_add(took, Relaxed);
        }
    }
}

// ---------------------------------------------------------------------
// The report.
// ---------------------------------------------------------------------

/// A row's totals as last reported, and the thread's processor time then
/// (user, kernel, 100 ns).
#[derive(Clone, Default)]
struct Seen {
    sites: Vec<[u64; 4]>,
    all: [u64; 4],
    cpu: (u64, u64),
    name: Option<String>,
}

struct Report {
    since: std::time::Instant,
    frames: u64,
    seen: Vec<Seen>,
}

static REPORT: Mutex<Option<Report>> = Mutex::new(None);

/// A frame was presented, on the calling thread: every few seconds the
/// lines are written.
pub fn frame() {
    if !on() {
        return;
    }
    mark(PRESENT);
    let mut report = REPORT.lock().unwrap_or_else(|e| e.into_inner());
    let report = report.get_or_insert_with(|| Report { since: std::time::Instant::now(), frames: 0, seen: Vec::new() });
    report.frames += 1;
    static EVERY: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    let every = *EVERY.get_or_init(|| {
        std::env::var("VICTORY_VM_STATS_EVERY")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(5.0)
            .max(1.0)
    });
    let elapsed = report.since.elapsed();
    if elapsed.as_secs_f64() < every {
        return;
    }
    let frames = std::mem::take(&mut report.frames) as f64;
    report.since = std::time::Instant::now();
    for line in lines(report, frames, elapsed.as_secs_f64(), |_| true) {
        tracing::info!("{line}");
    }
}

fn thread_cpu(handle: HANDLE) -> (u64, u64) {
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: a thread handle of ours (or null, which fails) and four
    // valid out pointers.
    unsafe { GetThreadTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    let n = |t: FILETIME| u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime);
    (n(user), n(kernel))
}

fn thread_name(handle: HANDLE) -> String {
    let mut text = std::ptr::null_mut();
    // SAFETY: a thread handle of ours; the text is the system's, freed
    // with LocalFree.
    unsafe {
        if handle.is_null() || GetThreadDescription(handle, &mut text) < 0 || text.is_null() {
            return String::new();
        }
        let mut n = 0;
        while *text.add(n) != 0 && n < 256 {
            n += 1;
        }
        let name = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
        windows_sys::Win32::Foundation::LocalFree(text.cast());
        name
    }
}

/// What a thread does, by its marks: the submitting thread before the
/// presenting one before a recording one.
fn kind(roles: u8) -> &'static str {
    if roles & SUBMIT != 0 {
        "submit"
    } else if roles & PRESENT != 0 {
        "present"
    } else if roles & RECORD != 0 {
        "record"
    } else {
        "other"
    }
}

fn roles_text(roles: u8) -> String {
    let named: Vec<&str> = [(SUBMIT, "submit"), (PRESENT, "present"), (RECORD, "record")]
        .iter()
        .filter(|(bit, _)| roles & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    if named.is_empty() { "-".into() } else { named.join("+") }
}

/// The report's lines over `frames` frames of `seconds`, from the rows'
/// totals less what was last reported, of the threads `keep` takes by id.
fn lines(report: &mut Report, frames: f64, seconds: f64, keep: impl Fn(u32) -> bool) -> Vec<String> {
    let frequency = FREQUENCY.load(Relaxed) as f64;
    let ms = |t: u64| t as f64 * 1e3 / frequency;
    let frames = frames.max(1.0);
    let rows = TAKEN.load(Relaxed).min(ROWS);
    report.seen.resize(rows, Seen::default());
    // Per site: calls, ticks, slow calls, slow ticks, longest; and ticks by
    // kind of thread.
    let mut sites = vec![[0u64; 5]; SITES];
    let kinds = ["submit", "present", "record", "other"];
    let mut by_kind = vec![[0u64; 4]; SITES];
    let mut all = [0u64; 4];
    let mut all_by_kind = [0u64; 4];
    let mut threads = Vec::new();
    for (at, row) in ROW[..rows].iter().enumerate() {
        if !keep(row.thread.load(Relaxed)) {
            continue;
        }
        let seen = &mut report.seen[at];
        seen.sites.resize(SITES, [0; 4]);
        let roles = row.roles.load(Relaxed);
        let k = kinds.iter().position(|&k| k == kind(roles)).unwrap_or(3);
        for site in 0..SITES {
            let c = &row.sites[site];
            let now = [0, 1, 2, 3].map(|i| c[i].load(Relaxed));
            let d: [u64; 4] = std::array::from_fn(|i| now[i].wrapping_sub(seen.sites[site][i]));
            seen.sites[site] = now;
            for i in 0..4 {
                sites[site][i] += d[i];
            }
            sites[site][4] = sites[site][4].max(c[4].swap(0, Relaxed));
            by_kind[site][k] += d[1];
        }
        let now = [0, 1, 2, 3].map(|i| row.all[i].load(Relaxed));
        let d: [u64; 4] = std::array::from_fn(|i| now[i].wrapping_sub(seen.all[i]));
        seen.all = now;
        for i in 0..4 {
            all[i] += d[i];
        }
        all_by_kind[k] += d[1];
        let handle = row.handle.load(Relaxed) as HANDLE;
        let cpu = thread_cpu(handle);
        let ran = (cpu.0.saturating_sub(seen.cpu.0), cpu.1.saturating_sub(seen.cpu.1));
        seen.cpu = cpu;
        if seen.name.is_none() {
            seen.name = Some(thread_name(handle));
        }
        if d[0] > 0 || roles != 0 {
            threads.push((d, ran, row.thread.load(Relaxed), roles, seen.name.clone().unwrap_or_default()));
        }
    }
    let mut out = vec![format!(
        "vm stats: {frames:.0} frames in {seconds:.1} s ({:.1} ms a frame); all our calls per frame: {:.1} calls {:.2} ms, \
         {:.1} of them over the slow mark taking {:.2} ms; by thread kind: submit {:.2} ms, present {:.2} ms, record {:.2} \
         ms, other {:.2} ms",
        seconds * 1e3 / frames,
        all[0] as f64 / frames,
        ms(all[1]) / frames,
        all[2] as f64 / frames,
        ms(all[3]) / frames,
        ms(all_by_kind[0]) / frames,
        ms(all_by_kind[1]) / frames,
        ms(all_by_kind[2]) / frames,
        ms(all_by_kind[3]) / frames,
    )];
    let mut order: Vec<usize> = (0..SITES).filter(|&s| sites[s][0] > 0).collect();
    order.sort_by_key(|&s| std::cmp::Reverse(sites[s][1]));
    for s in order {
        let (word, says) = Site::ALL[s].name();
        let c = sites[s];
        out.push(format!(
            "vm stats: site {word} per frame: {:.1} calls {:.2} ms ({:.1} us each), {:.1} slow taking {:.2} ms, longest {:.2} \
             ms; submit {:.2} ms, present {:.2} ms, record {:.2} ms, other {:.2} ms ({says})",
            c[0] as f64 / frames,
            ms(c[1]) / frames,
            ms(c[1]) * 1e3 / c[0] as f64,
            c[2] as f64 / frames,
            ms(c[3]) / frames,
            ms(c[4]),
            ms(by_kind[s][0]) / frames,
            ms(by_kind[s][1]) / frames,
            ms(by_kind[s][2]) / frames,
            ms(by_kind[s][3]) / frames,
        ));
    }
    threads.sort_by_key(|t| std::cmp::Reverse(t.0[1]));
    for (d, (user, kernel), id, roles, name) in threads.into_iter().take(24) {
        let name = if name.is_empty() { "unnamed".to_string() } else { name };
        out.push(format!(
            "vm stats: thread {id} {name} ({}) per frame: ran {:.1} ms (user {:.1}, kernel {:.1}); in our calls {:.2} ms in \
             {:.1} calls, {:.2} ms of it in {:.1} slow ones",
            roles_text(roles),
            (user + kernel) as f64 / 1e4 / frames,
            user as f64 / 1e4 / frames,
            kernel as f64 / 1e4 / frames,
            ms(d[1]) / frames,
            d[0] as f64 / frames,
            ms(d[3]) / frames,
            d[2] as f64 / frames,
        ));
    }
    out
}

/// Switches timing on or off for the test process, whatever
/// `VICTORY_VM_STATS` says (it is read once).
#[cfg(test)]
pub(crate) fn force(switch: bool) {
    let _ = on();
    let mut f = 0;
    // SAFETY: a valid out pointer.
    unsafe { QueryPerformanceFrequency(&mut f) };
    FREQUENCY.store((f as u64).max(1), Relaxed);
    SLOW_TICKS.store(FREQUENCY.load(Relaxed) * SLOW_US / 1_000_000, Relaxed);
    STATE.store(if switch { 2 } else { 1 }, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn switch_on() {
        force(true);
    }

    #[test]
    fn calls_count_for_their_site_and_once_for_their_thread() {
        switch_on();
        let mut report = Report { since: std::time::Instant::now(), frames: 0, seen: Vec::new() };
        let (sent, id) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // SAFETY: a plain query.
            sent.send(unsafe { GetCurrentThreadId() }).unwrap();
            mark(SUBMIT);
            mark(SUBMIT);
            for _ in 0..3 {
                let _outer = time(Site::Fault);
                let _inner = time(Site::ProtectOpenFault);
            }
            let _slow = time(Site::Remap);
            std::thread::sleep(std::time::Duration::from_millis(2));
        })
        .join()
        .unwrap();
        let id = id.recv().unwrap();
        let out = lines(&mut report, 2.0, 1.0, |t| t == id);
        // Seen with --nocapture: the lines as `perf-summary` reads them.
        for line in &out {
            println!("{line}");
        }
        let all = &out[0];
        // Six timed calls, four of them outermost, over two frames.
        assert!(all.contains("all our calls per frame: 2.0 calls"), "{out:#?}");
        assert!(all.contains("0.5 of them over the slow mark"), "{out:#?}");
        let site = |word: &str| out.iter().find(|l| l.contains(&format!("site {word} "))).cloned().unwrap_or_default();
        assert!(site("fault-handler").contains("per frame: 1.5 calls"), "{out:#?}");
        assert!(site("protect-open-fault").contains("per frame: 1.5 calls"), "{out:#?}");
        assert!(site("remap").contains("0.5 slow"), "{out:#?}");
        // The remap's 2 ms went to the submitting thread.
        assert!(!site("remap").contains("submit 0.00 ms"), "{out:#?}");
        let thread = out.iter().find(|l| l.contains("(submit)")).unwrap();
        assert!(thread.contains("in our calls") && thread.contains("2.0 calls"), "{out:#?}");
        // Nothing new: no site lines.
        let again = lines(&mut report, 1.0, 1.0, |t| t == id);
        assert!(again.iter().all(|l| !l.contains(" site ")), "{again:#?}");
    }

    #[test]
    fn the_guards_rows_map_to_their_sites() {
        assert_eq!(Site::protect(0), Site::ProtectGuard);
        assert_eq!(Site::protect(2), Site::ProtectOpenFault);
        assert_eq!(Site::protect(6), Site::ProtectOpenMapped);
        assert_eq!(Site::ALL.len(), SITES);
        for (i, s) in Site::ALL.iter().enumerate() {
            assert_eq!(*s as usize, i);
        }
    }
}
