//! The water trace (`VICTORY_WATER_TRACE=1`, off by default) and
//! `VICTORY_HEAP_WRITES` (off by default).
//!
//! In the title's first mission after the prologue the boat sinks to the
//! sea bed and will not move, though the water is drawn, and on foot the
//! water does not exist for the player (reported on every host and fork:
//! fxceless/victory-fork#279, #372, #618, and here on Proton). The title's
//! CPU decides what floats; what it is given of the GPU's work reaches it
//! only through the paths below, and the trace says, for one run, what
//! each of them did:
//!
//! - stores through buffer views a shader picks from the view heap by an
//!   index it computes (`crate::draw`, `heap_buffers`): those stay in the
//!   host copies and never reach title memory (upstream VIctory's own
//!   `bindless.rs` names the same gap, "so the CPU does not see it", and
//!   closed it with shader write marks, cd94d35388), unless
//!   [`heap_writes`] asks for them. Each such shader is named by hash;
//! - texture to buffer copies (read backs) as they land in title memory,
//!   with whether every byte was zero;
//! - query resolves, whose results are made up, not measured
//!   (`crate::list`, `resolve_queries`);
//! - in the runtime (`victory-hle`), file and DirectStorage reads that fail
//!   or come back short;
//! - with `VICTORY_WATER_TRACE=2`, the title's CPU touching the memory of a
//!   texture the host renders to or writes ([`touch_frame`]): title memory
//!   there never holds the GPU's texels (no path copies a texture back),
//!   so a CPU that reads it (legal on the console) reads stale bytes.
//!
//! The lines are rate-limited per key: the first eight, then every power
//! of two, then whenever something they report changes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

/// Whether the water trace is on (`VICTORY_WATER_TRACE=1`, or `2` with
/// the texture touch tracer as well).
pub fn water_trace() -> bool {
    level() >= 1
}

/// Whether the texture touch tracer is on (`VICTORY_WATER_TRACE=2`,
/// [`touch_frame`]).
pub fn touch_trace() -> bool {
    level() >= 2
}

fn level() -> u8 {
    static LEVEL: OnceLock<u8> = OnceLock::new();
    *LEVEL.get_or_init(|| match std::env::var("VICTORY_WATER_TRACE").ok().as_deref().map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    })
}

/// The `n`th event of a kind is logged: the first eight, then every
/// power of two.
pub fn worth(n: u64) -> bool {
    n <= 8 || n.is_power_of_two()
}

/// What `VICTORY_HEAP_WRITES` asks for: whether a list that runs a shader
/// picking buffer views from the view heap by a computed index, and able
/// to store, gives the heap's buffer views to the copy-back as written, so
/// that what the shader stored there reaches title memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeapWrites {
    /// Unset or `0`: nothing is declared (as before).
    Off,
    /// `1`: the heap's unordered access buffer views, for every such shader.
    Unordered,
    /// `all`: every buffer view of the heap (the console's buffer
    /// descriptor is the same for reading and writing, so a shader may
    /// write through a view the title made as a shader resource view).
    All,
    /// A comma-separated list of shader hashes (the 16 hex digits the log
    /// names shaders by): the unordered access views, for those shaders
    /// only.
    Only(Vec<String>),
}

impl HeapWrites {
    /// The switch's value as given.
    pub fn parse(value: Option<&str>) -> HeapWrites {
        let Some(v) = value.map(str::trim) else {
            return HeapWrites::Off;
        };
        match v {
            "" | "0" => HeapWrites::Off,
            "1" => HeapWrites::Unordered,
            "all" => HeapWrites::All,
            list => {
                let hashes: Vec<String> = list
                    .split(',')
                    .map(|h| h.trim().to_ascii_lowercase())
                    .filter(|h| !h.is_empty())
                    .collect();
                if hashes.is_empty() { HeapWrites::Off } else { HeapWrites::Only(hashes) }
            }
        }
    }

    /// Whether a list running shader `hash` declares the heap's views.
    pub fn wants(&self, hash: &str) -> bool {
        match self {
            HeapWrites::Off => false,
            HeapWrites::Unordered | HeapWrites::All => true,
            HeapWrites::Only(hashes) => hashes.iter().any(|h| h.eq_ignore_ascii_case(hash)),
        }
    }

    /// Whether views the title made as shader resource views count too.
    pub fn all_views(&self) -> bool {
        matches!(self, HeapWrites::All)
    }
}

/// `VICTORY_HEAP_WRITES`, read once.
pub fn heap_writes() -> &'static HeapWrites {
    static MODE: OnceLock<HeapWrites> = OnceLock::new();
    MODE.get_or_init(|| {
        let mode = HeapWrites::parse(std::env::var("VICTORY_HEAP_WRITES").ok().as_deref());
        if mode != HeapWrites::Off {
            tracing::info!(?mode, "VICTORY_HEAP_WRITES: what shaders store through buffer views picked from the heap goes back to title memory");
        }
        mode
    })
}

/// Ranges `[from, to)` (any order, overlapping or not) as the fewest
/// disjoint ranges covering the same bytes, in order. Ranges that touch
/// are joined.
pub fn coalesce(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.retain(|r| r.0 < r.1);
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (from, to) in ranges {
        match out.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    out
}

/// Counts of an event by key, for [`worth`].
struct Counts(Mutex<Option<HashMap<String, u64>>>);

impl Counts {
    const fn new() -> Self {
        Counts(Mutex::new(None))
    }

    /// The new count of `key`.
    fn bump(&self, key: &str) -> u64 {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let n = map.get_or_insert_with(HashMap::new).entry(key.to_owned()).or_insert(0);
        *n += 1;
        *n
    }
}

/// With the trace: a draw or dispatch runs shader `hash`, which picks
/// buffer views from the view heap by an index it computes and can store
/// to buffers. `declared`: [`heap_writes`] gave the heap's views to the
/// copy-back for its list.
pub fn picking_store(hash: &str, compute: bool, declared: bool) {
    static SEEN: Counts = Counts::new();
    let n = SEEN.bump(hash);
    if worth(n) {
        tracing::info!(
            frame = crate::frame::current(),
            hash,
            stage = if compute { "compute" } else { "graphics" },
            runs = n,
            declared,
            "water trace: a shader that can store picks buffer views from the heap by a computed index: what it stores there stays on the card unless declared (VICTORY_HEAP_WRITES)"
        );
    }
}

/// With the trace: a list declared `copies` host copies' ranges, `bytes`
/// of them, written for [`heap_writes`].
pub fn heap_declared(copies: usize, bytes: u64) {
    static LISTS: AtomicU64 = AtomicU64::new(0);
    let n = LISTS.fetch_add(1, Relaxed) + 1;
    if worth(n) {
        tracing::info!(
            frame = crate::frame::current(),
            lists = n,
            copies,
            mb = format_args!("{:.1}", bytes as f64 / 1048576.0),
            "water trace: a list gives the heap's buffer views to the copy-back as written (VICTORY_HEAP_WRITES)"
        );
    }
}

/// With the trace: a read back of `rows` rows of `row` bytes (title pitch
/// `pitch`) landed in title memory at `at`; `first` is its first row as
/// copied.
pub fn read_back(at: usize, row: usize, rows: usize, pitch: usize, first: &[u8], all_zero: bool) {
    static SEEN: Mutex<Option<HashMap<usize, (u64, bool)>>> = Mutex::new(None);
    let (n, changed) = {
        let mut map = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.get_or_insert_with(HashMap::new).entry(at).or_insert((0, !all_zero));
        entry.0 += 1;
        let changed = entry.1 != all_zero;
        entry.1 = all_zero;
        (entry.0, changed)
    };
    if !worth(n) && !changed {
        return;
    }
    let words: Vec<f32> = first
        .chunks_exact(4)
        .take(8)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    tracing::info!(
        frame = crate::frame::current(),
        to = format_args!("{at:#x}"),
        row,
        rows,
        pitch,
        landed = n,
        all_zero,
        first_values = ?words,
        "water trace: a read back lands in title memory"
    );
}

/// With the trace: `ResolveQueryData` of `count` queries of type `kind`
/// into title memory at `at`. Nothing is measured: occlusion says a
/// million samples passed, binary occlusion visible, pipeline statistics
/// zero, timestamps the GPU clock at submission.
pub fn query_resolve(kind: u32, count: u32, at: u64, written: bool) {
    static SEEN: Counts = Counts::new();
    let n = SEEN.bump(&kind.to_string());
    if worth(n) {
        let what = match kind {
            0 => "occlusion: 1048576 samples, made up",
            1 => "binary occlusion: visible, made up",
            2 => "timestamp: the GPU clock at submission",
            3 => "pipeline statistics: zeros, made up",
            _ => "zeros, made up",
        };
        tracing::info!(
            frame = crate::frame::current(),
            kind,
            count,
            to = format_args!("{at:#x}"),
            resolves = n,
            written,
            what,
            "water trace: a query resolve"
        );
    }
}

/// With the trace: a root unordered access view (a shader's writes through
/// it) is bound to a null view, `why`: what the shader stores there is lost.
pub fn root_uav_lost(address: Option<u64>, why: &'static str) {
    static SEEN: Counts = Counts::new();
    let n = SEEN.bump(why);
    if worth(n) {
        tracing::info!(
            frame = crate::frame::current(),
            address = ?address.map(|a| format!("{a:#x}")),
            times = n,
            why,
            "water trace: a shader's writes through a root unordered view go nowhere"
        );
    }
}

/// With the trace, from the runtime: a read of a file or DirectStorage
/// request into title memory failed or came back short. `what` names the
/// path; `key` groups the rate limit (a file, an error).
pub fn read_trouble(what: &'static str, key: &str, detail: std::fmt::Arguments<'_>) {
    static SEEN: Counts = Counts::new();
    let n = SEEN.bump(&format!("{what} {key}"));
    if worth(n) {
        tracing::info!(what, key, times = n, "water trace: a read into title memory: {detail}");
    }
}

/// The texture touch tracer (`VICTORY_WATER_TRACE=2`), at each present:
/// logs the touches recorded since the last frame, and every
/// [`TOUCH_EVERY`] frames arms the memory of every title texture the host
/// renders to or writes (`crate::resource::targets`, up to
/// `VICTORY_WATER_TEXTURE_MB` each, 64 by default) with `PAGE_GUARD`.
///
/// The first access to an armed page raises `STATUS_GUARD_PAGE_VIOLATION`
/// once and clears the guard (Wine: `virtual_handle_fault`,
/// dlls/ntdll/unix/virtual.c), and [`on_guard`] notes where and from
/// which instruction, and lets the access go on. Only pages that are
/// plainly read-write are armed, under the guards' lock
/// (`crate::watch::with_guards_held`): a page our write guard holds
/// read-only is left alone, and no guard of ours changes a protection
/// while the arming reads and sets them. A kernel read of an armed page
/// (ours through `ReadProcessMemory`) fails once instead of faulting.
#[cfg(windows)]
pub(crate) fn touch_frame() {
    if !touch_trace() {
        return;
    }
    let frame = crate::frame::current();
    drain(frame);
    static ARMED_AT: AtomicU64 = AtomicU64::new(u64::MAX);
    let round = frame / TOUCH_EVERY;
    if ARMED_AT.swap(round, Relaxed) != round {
        arm(frame);
    }
}

/// Frames between armings of the texture touch tracer.
#[cfg(windows)]
const TOUCH_EVERY: u64 = 64;

/// Touches the handler noted and the drain has not logged: per slot, the
/// sequence number it holds plus one (0: none yet), the address, the
/// instruction and the access kind (0 read, 1 write, 8 execute).
#[cfg(windows)]
const RING: usize = 256;
#[cfg(windows)]
static HITS: [[AtomicU64; 4]; RING] = [const { [const { AtomicU64::new(0) }; 4] }; RING];
#[cfg(windows)]
static NEXT: AtomicU64 = AtomicU64::new(0);
/// The lowest and the end of the highest address armed: a guard page
/// violation outside is not ours.
#[cfg(windows)]
static SPAN: [AtomicU64; 2] = [AtomicU64::new(u64::MAX), AtomicU64::new(0)];
/// The textures armed by address: bytes and description.
#[cfg(windows)]
static ARMED: Mutex<std::collections::BTreeMap<u64, (u64, String)>> = Mutex::new(std::collections::BTreeMap::new());

/// Notes a guard page violation inside the armed span and lets the access
/// go on (the system cleared the guard). Takes no lock and allocates
/// nothing: it may interrupt any thread.
#[cfg(windows)]
unsafe extern "system" fn on_guard(info: *mut windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS) -> i32 {
    use windows_sys::Win32::Foundation::STATUS_GUARD_PAGE_VIOLATION;
    use windows_sys::Win32::System::Diagnostics::Debug::{EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH};
    // SAFETY: the system passes the exception being dispatched.
    let (r, rip) = unsafe { (&*(*info).ExceptionRecord, (*(*info).ContextRecord).Rip) };
    if r.ExceptionCode != STATUS_GUARD_PAGE_VIOLATION || r.NumberParameters < 2 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let at = r.ExceptionInformation[1] as u64;
    if at < SPAN[0].load(Relaxed) || at >= SPAN[1].load(Relaxed) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let seq = NEXT.fetch_add(1, Relaxed);
    let slot = &HITS[(seq % RING as u64) as usize];
    slot[1].store(at, Relaxed);
    slot[2].store(rip, Relaxed);
    slot[3].store(r.ExceptionInformation[0] as u64, Relaxed);
    slot[0].store(seq + 1, std::sync::atomic::Ordering::Release);
    EXCEPTION_CONTINUE_EXECUTION
}

/// Registers [`on_guard`], once.
#[cfg(windows)]
fn install() {
    use windows_sys::Win32::System::Diagnostics::Debug::AddVectoredExceptionHandler;
    static INSTALL: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler only reads the exception and stores atomics.
    INSTALL.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_guard));
    });
}

/// Arms the textures' pages (see [`touch_frame`]).
#[cfg(windows)]
fn arm(frame: u64) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_READWRITE, VirtualProtect, VirtualQuery,
    };
    install();
    static LIMIT: OnceLock<u64> = OnceLock::new();
    let limit = *LIMIT.get_or_init(|| {
        std::env::var("VICTORY_WATER_TEXTURE_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(64) << 20
    });
    let targets = crate::resource::targets();
    let (mut textures, mut pages, mut skipped) = (0u64, 0u64, 0u64);
    let (mut low, mut high) = (u64::MAX, 0u64);
    let mut armed = std::collections::BTreeMap::new();
    crate::watch::with_guards_held(|| {
        for (start, bytes, what) in &targets {
            if *bytes == 0 || *bytes > limit {
                skipped += 1;
                continue;
            }
            let end = start + bytes;
            let mut at = *start;
            let mut any = false;
            while at < end {
                // SAFETY: plain data filled in by the query.
                let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
                // SAFETY: a valid out pointer of the right size.
                let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
                if n == 0 || info.RegionSize == 0 {
                    break;
                }
                let region_end = (info.BaseAddress as u64 + info.RegionSize as u64).min(end);
                if info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE {
                    let mut old = 0;
                    // SAFETY: committed read-write title memory; the guard
                    // only makes the next access raise once.
                    let ok = unsafe {
                        VirtualProtect(at as *const _, (region_end - at) as usize, PAGE_READWRITE | PAGE_GUARD, &mut old)
                    } != 0;
                    if ok {
                        any = true;
                        pages += (region_end - at).div_ceil(4096);
                    }
                }
                at = region_end.max(at + 1);
            }
            if any {
                textures += 1;
                low = low.min(*start);
                high = high.max(end);
                armed.insert(*start, (*bytes, what.clone()));
            }
        }
    });
    *ARMED.lock().unwrap_or_else(|e| e.into_inner()) = armed;
    SPAN[0].store(low.min(SPAN[0].load(Relaxed)), Relaxed);
    SPAN[1].store(high.max(SPAN[1].load(Relaxed)), Relaxed);
    static ROUNDS: AtomicU64 = AtomicU64::new(0);
    let n = ROUNDS.fetch_add(1, Relaxed) + 1;
    if worth(n) {
        tracing::info!(
            frame,
            rounds = n,
            targets = targets.len(),
            armed = textures,
            pages,
            over_the_limit = skipped,
            "water trace: the memory of textures the GPU writes is watched for the CPU's touches (VICTORY_WATER_TRACE=2)"
        );
    }
}

/// Logs the touches noted since the last drain.
#[cfg(windows)]
fn drain(frame: u64) {
    static DRAINED: AtomicU64 = AtomicU64::new(0);
    static SEEN: Counts = Counts::new();
    let next = NEXT.load(std::sync::atomic::Ordering::Acquire);
    let mut from = DRAINED.swap(next, Relaxed);
    if next.saturating_sub(from) > RING as u64 {
        tracing::info!(frame, lost = next - from - RING as u64, "water trace: texture touches not kept (more than the ring holds)");
        from = next - RING as u64;
    }
    if from == next {
        return;
    }
    let armed = ARMED.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for seq in from..next {
        let slot = &HITS[(seq % RING as u64) as usize];
        if slot[0].load(std::sync::atomic::Ordering::Acquire) != seq + 1 {
            continue;
        }
        let (at, rip, kind) = (slot[1].load(Relaxed), slot[2].load(Relaxed), slot[3].load(Relaxed));
        let texture = armed.range(..=at).next_back().filter(|(start, (bytes, _))| at < *start + *bytes);
        let (start, what) = match texture {
            Some((start, (_, what))) => (*start, what.as_str()),
            None => (0, "a texture armed before and let go since"),
        };
        let code = module_of(rip);
        let access = match kind {
            0 => "read",
            1 => "write",
            8 => "execute",
            _ => "other",
        };
        let n = SEEN.bump(&format!("{start:x} {code} {access}"));
        if worth(n) {
            tracing::info!(
                frame,
                texture = format_args!("{start:#x}"),
                offset = at.saturating_sub(start),
                what,
                access,
                code,
                times = n,
                "water trace: the CPU touched the memory of a texture the GPU writes (title memory there never holds the GPU's texels)"
            );
        }
    }
}

/// `module+rva` of a code address, or the address.
#[cfg(windows)]
fn module_of(rip: u64) -> String {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW,
        GetModuleHandleExW,
    };
    let mut module = std::ptr::null_mut();
    // SAFETY: a plain lookup by address; the reference count is unchanged.
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            rip as usize as *const u16,
            &mut module,
        )
    } != 0;
    if !found || module.is_null() {
        return format!("{rip:#x}");
    }
    let mut name = [0u16; 260];
    // SAFETY: a valid buffer of its length.
    let n = unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) } as usize;
    let path = String::from_utf16_lossy(&name[..n.min(name.len())]);
    let file = path.rsplit(['\\', '/']).next().unwrap_or("?");
    format!("{file}+{:#x}", rip - module as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_reads_as_documented() {
        assert_eq!(HeapWrites::parse(None), HeapWrites::Off);
        assert_eq!(HeapWrites::parse(Some("0")), HeapWrites::Off);
        assert_eq!(HeapWrites::parse(Some(" ")), HeapWrites::Off);
        assert_eq!(HeapWrites::parse(Some("1")), HeapWrites::Unordered);
        assert_eq!(HeapWrites::parse(Some("all")), HeapWrites::All);
        let only = HeapWrites::parse(Some("ceb5eb68cd99cb07, C7E8D2105D2A5E58,"));
        assert_eq!(only, HeapWrites::Only(vec!["ceb5eb68cd99cb07".into(), "c7e8d2105d2a5e58".into()]));
        assert!(only.wants("c7e8d2105d2a5e58"));
        assert!(!only.wants("0000000000000000"));
        assert!(!only.all_views());
        assert!(HeapWrites::All.all_views() && HeapWrites::All.wants("anything"));
        assert!(!HeapWrites::Off.wants("ceb5eb68cd99cb07"));
    }

    #[test]
    fn ranges_coalesce_into_the_fewest_covering_the_same_bytes() {
        assert_eq!(coalesce(vec![]), vec![]);
        assert_eq!(coalesce(vec![(10, 20), (0, 5), (5, 8), (15, 30), (40, 40), (50, 60)]), vec![(0, 8), (10, 30), (50, 60)]);
        assert_eq!(coalesce(vec![(0, 100), (10, 20)]), vec![(0, 100)]);
    }

    #[cfg(windows)]
    #[test]
    fn a_touch_of_a_guarded_page_is_noted_once_and_goes_on() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_GUARD, PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualProtect,
        };
        install();
        // SAFETY: a fresh allocation of ours, freed at the end; the guard
        // only makes the first access raise once.
        unsafe {
            let p = VirtualAlloc(std::ptr::null(), 2 * 4096, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) as u64;
            assert_ne!(p, 0);
            ((p + 4096 + 16) as *mut u32).write(7);
            SPAN[0].fetch_min(p, Relaxed);
            SPAN[1].fetch_max(p + 2 * 4096, Relaxed);
            let mut old = 0;
            assert_ne!(VirtualProtect((p + 4096) as *const _, 4096, PAGE_READWRITE | PAGE_GUARD, &mut old), 0);
            let before = NEXT.load(Relaxed);
            assert_eq!(std::ptr::read_volatile((p + 4096 + 16) as *const u32), 7);
            let after = NEXT.load(Relaxed);
            assert_eq!(after, before + 1, "one touch noted");
            let slot = &HITS[(before % RING as u64) as usize];
            assert_eq!(slot[0].load(Relaxed), before + 1);
            assert_eq!(slot[1].load(Relaxed), p + 4096 + 16);
            assert_eq!(slot[3].load(Relaxed), 0, "a read");
            // The guard is gone: no second touch.
            assert_eq!(std::ptr::read_volatile((p + 4096 + 20) as *const u32), 0);
            ((p + 4096 + 20) as *mut u32).write(1);
            assert_eq!(NEXT.load(Relaxed), after);
            VirtualFree(p as *mut _, 0, MEM_RELEASE);
        }
    }

    #[test]
    fn rate_limit_logs_the_first_eight_then_powers_of_two() {
        let logged: Vec<u64> = (1..=70).filter(|&n| worth(n)).collect();
        assert_eq!(logged, vec![1, 2, 3, 4, 5, 6, 7, 8, 16, 32, 64]);
    }
}
