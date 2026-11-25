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
//!   or come back short.
//!
//! The lines are rate-limited per key: the first eight, then every power
//! of two, then whenever something they report changes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

/// Whether the water trace is on (`VICTORY_WATER_TRACE=1`).
pub fn water_trace() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WATER_TRACE").is_some_and(|v| v == "1"))
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

    #[test]
    fn rate_limit_logs_the_first_eight_then_powers_of_two() {
        let logged: Vec<u64> = (1..=70).filter(|&n| worth(n)).collect();
        assert_eq!(logged, vec![1, 2, 3, 4, 5, 6, 7, 8, 16, 32, 64]);
    }
}
