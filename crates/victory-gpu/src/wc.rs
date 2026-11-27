//! Diagnostics (`VICTORY_WC_STATS=1`): how much of what is copied up to the
//! card and back into title memory lies in memory the title asked for as
//! write-combined or uncached, which the console's CPU writes but does not
//! read back (as PC titles keep their per-frame rings in memory they only
//! write). XMem reports every protection the title asks for
//! ([`title_memory_protection`]); copies are counted by the title address
//! they start at, against the newest protection asked for there.
//!
//! Off, every call returns at once. On, a copy costs a read lock and a
//! map lookup.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

/// Kinds of title memory, by what the title asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Kind {
    /// No protection was ever noted there (host memory, or memory the
    /// runtime gave the title another way).
    Unknown = 0,
    /// Cached for the CPU (no write-combining or uncached bit).
    Cached = 1,
    /// The console's write-combined bit (0x400).
    WriteCombined = 2,
    /// The console's uncached bit (0x200).
    Uncached = 3,
}

const KINDS: usize = 4;
const NAMES: [&str; KINDS] = ["unknown", "cached", "write-combined", "uncached"];

/// The console's protection bits for how the CPU reaches memory.
const UNCACHED: u32 = 0x200;
const WRITE_COMBINED: u32 = 0x400;

/// Whether title memory is sorted by kind: for the stats, or for
/// [`no_back`], which needs to know it.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WC_STATS").is_some_and(|v| v == "1") || no_back())
}

/// `VICTORY_WC_NO_BACK=1` (off by default): what shaders write into title
/// memory the title asked for as write-combined is not copied back into it.
/// The card's side keeps it, as a PC title keeps what its GPU writes: the
/// console's CPU does not read such memory back (belorx_8: all 46 MB a frame
/// of the copy-back went there). Such a range is not declared as written at
/// all (no snapshot, no compare, no copy back); it is noted as the card's
/// ([`crate::shadow::Shadow::card_owns`]) so that a refresh of a copy whose
/// writes are not watched does not put the title's old bytes over it.
/// Where the copy is watched, pages the CPU writes are still sent up: its
/// newer bytes win, as they do today.
///
/// Ranges under `VICTORY_WC_NO_BACK_MIN_KB` (64 by default) are copied back
/// as before: the few results the title's CPU does read (the walls'
/// culling, small read-outs) are small.
pub(crate) fn no_back() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WC_NO_BACK").is_some_and(|v| v == "1"))
}

fn no_back_min() -> usize {
    static MIN: OnceLock<usize> = OnceLock::new();
    *MIN.get_or_init(|| {
        std::env::var("VICTORY_WC_NO_BACK_MIN_KB").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(64) << 10
    })
}

/// Declared writes left to the card ([`skip_back`]) and small ones kept,
/// in declared bytes (not bytes shaders changed); then refreshes that left
/// such ranges out because the memory is not watched (what the CPU wrote
/// there does not go up: [`unwatched`]), and their bytes; and bytes left to
/// a copy that is not in the card's memory ([`uncarded`]). Taken by
/// [`take`].
/// Then bytes kept as before because the copy is not known to be
/// write-watched, or is not in the card's memory ([`skip_back`]).
static SKIPPED: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

/// Whether copies the card keeps ranges of send up only the dwords the
/// title changed (`VICTORY_WC_KEEP_STORES`; `0`: the parts written
/// since, whole, as for any copy). Whole, a part the CPU wrote in part goes
/// up with title memory's older bytes beside the CPU's, over what shaders
/// wrote there; dword by dword, a value the title writes over the same
/// value is not seen (a counter the CPU sets back to what title memory
/// still holds stays as the shaders left it).
///
/// On with [`no_back`] unless `VICTORY_WC_KEEP_STORES=0` asks for whole
/// parts: whole, 2.5 MB a frame of title memory's older bytes went up over
/// ranges left on the card (run pb4, the opening: 31 parts a frame, 2.7 MB,
/// of which the title had changed 0.15 MB, run pb5), which is what leaving
/// them on the card must not do.
pub(crate) fn keep_stores() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| keep_stores_asked(no_back(), std::env::var("VICTORY_WC_KEEP_STORES").ok().as_deref()))
}

/// [`keep_stores`], of what was asked: with [`no_back`] unless `0`, and
/// never without it.
fn keep_stores_asked(no_back: bool, asked: Option<&str>) -> bool {
    no_back && asked.map(str::trim) != Some("0")
}

/// A refresh left out `bytes` the card keeps, the memory not being watched.
pub(crate) fn unwatched(bytes: u64) {
    SKIPPED[4].fetch_add(1, Ordering::Relaxed);
    SKIPPED[5].fetch_add(bytes, Ordering::Relaxed);
}

/// `bytes` were left to a copy in host memory, where a refresh of a part
/// the CPU wrote goes over what shaders wrote beside it.
pub(crate) fn uncarded(bytes: u64) {
    SKIPPED[6].fetch_add(bytes, Ordering::Relaxed);
}

/// Whether shaders' writes to `[address, address + len)` stay on the card
/// ([`no_back`]): the whole range lies in write-combined title memory and
/// is not small, and its copy is in the card's memory (`carded`) and known
/// to be write-watched (`watched`). Where the memory is not watched a
/// refresh cannot tell what the CPU wrote, and would either put title
/// memory's older bytes over the card's or never send the CPU's up; a copy
/// in host memory holds what shaders wrote itself, and a refresh of a part
/// the CPU wrote goes over it. Those are copied back as before.
pub(crate) fn skip_back(address: usize, len: usize, carded: bool, watched: bool) -> bool {
    if !no_back() || len == 0 {
        return false;
    }
    // (All of it, not only its ends: memory of another kind between two
    // write-combined ranges is memory the title's CPU may read back.)
    let wc = all_of(
        &ranges().read().unwrap_or_else(|e| e.into_inner()),
        address,
        address.saturating_add(len),
        Kind::WriteCombined,
    );
    if !wc {
        return false;
    }
    if len < no_back_min() {
        SKIPPED[2].fetch_add(1, Ordering::Relaxed);
        SKIPPED[3].fetch_add(len as u64, Ordering::Relaxed);
        return false;
    }
    if !carded || !watched {
        SKIPPED[if carded { 7 } else { 8 }].fetch_add(len as u64, Ordering::Relaxed);
        return false;
    }
    SKIPPED[0].fetch_add(1, Ordering::Relaxed);
    SKIPPED[1].fetch_add(len as u64, Ordering::Relaxed);
    true
}

/// Title memory whose shaders' writes were left on the card, of all copies
/// ([`crate::shadow::Shadow::card_owns`]): it may hold older bytes than the
/// card does.
fn owned() -> &'static RwLock<Ranges> {
    static OWNED: OnceLock<RwLock<Ranges>> = OnceLock::new();
    OWNED.get_or_init(Default::default)
}

/// Title memory of [`owned`] that the runtime itself took on the CPU
/// ([`cpu_reads`], [`orphaned`]): what shaders write there is copied back as
/// before from then on ([`read_on_cpu`]).
fn read() -> &'static RwLock<Ranges> {
    static READ: OnceLock<RwLock<Ranges>> = OnceLock::new();
    READ.get_or_init(Default::default)
}

/// What became of memory left on the card, since the counters were last
/// taken: declared bytes copied back as before because the runtime read
/// there ([`read_on_cpu`]); refreshes that took parts of it the title
/// wrote, the bytes of those parts that are the card's, and the bytes that
/// went up ([`taken_over`]).
static GUARD: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Since the start (the line shows one sample in five, and these are rare):
/// places of memory left on the card that steps of ours read on the CPU,
/// and their bytes ([`cpu_reads`]); copies let go that held some, and its
/// bytes ([`orphaned`]).
static EVER: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The first places are named in the log, each with the step that read it.
const NAMED: u64 = 16;

/// Shaders' writes to `[from, to)` stay on the card from now on.
pub(crate) fn owns(from: usize, to: usize) {
    if from >= to || owned().read().unwrap_or_else(|e| e.into_inner()).covers(from, to) {
        return;
    }
    owned().write().unwrap_or_else(|e| e.into_inner()).add(from, to);
}

/// `[from, to)` is copied back again, or its copy is gone.
/// shortcut: where two copies hold the same title memory and both left
/// writes on the card, the other's is taken out with it.
pub(crate) fn disowns(from: usize, to: usize) {
    if from >= to || owned().read().unwrap_or_else(|e| e.into_inner()).is_empty() {
        return;
    }
    owned().write().unwrap_or_else(|e| e.into_inner()).remove(from, to);
}

/// What of `[at, end)` lies in `owned`, put into `read`. Returns its bytes,
/// or 0 if all of it was noted before.
fn note_read(owned: &Ranges, read: &mut Ranges, at: usize, end: usize) -> usize {
    let found: Vec<(usize, usize)> =
        owned.overlapping(at, end).map(|(a, e)| (a.max(at), e.min(end))).filter(|&(a, e)| !read.covers(a, e)).collect();
    for &(a, e) in &found {
        read.add(a, e);
    }
    found.iter().map(|&(a, e)| e - a).sum()
}

/// A step of the runtime (`what`) takes `[at, at + len)` of title memory on
/// the CPU: a fill of upload memory, a title buffer copy's source, a
/// texture's bytes, the refresh of another host copy of the same memory
/// (what the title has the card do, we do on the CPU: no read of the
/// title's own, so the kind of the memory says nothing of it). Where shaders'
/// writes were left on the card ([`no_back`]) those bytes are older than the
/// card's: the range is noted, and what shaders write there from the next
/// declaration on is copied back as before ([`read_on_cpu`]).
/// Counted since the start and the first ones named in the log, so that a
/// session shows whether it ever happened (the opening: never in runs pb4
/// and pb5, one place of 4 MB in run pb6).
///
/// The declared ranges are wider than what shaders write (a view with no
/// size reaches to its copy's end), so a place noted here need not hold a
/// byte of theirs: then the read was good, and the range goes back for
/// nothing.
///
/// shortcut: this tells and stops it going on; it does not make the read
/// good. What shaders wrote before and do not write again is not fetched
/// (the compare on the card finds only what a flight changes), so title
/// memory keeps the older bytes there.
/// shortcut: reads away from a submission's steps (the pass over textures
/// picked by index, descriptor records) are not seen here.
pub(crate) fn cpu_reads(what: &'static str, at: usize, len: usize) {
    if !no_back() || len == 0 {
        return;
    }
    let end = at.saturating_add(len);
    let owned = owned().read().unwrap_or_else(|e| e.into_inner());
    if owned.is_empty() || owned.overlapping(at, end).next().is_none() {
        return;
    }
    let bytes = note_read(&owned, &mut read().write().unwrap_or_else(|e| e.into_inner()), at, end);
    if bytes == 0 {
        return;
    }
    EVER[1].fetch_add(bytes as u64, Ordering::Relaxed);
    if EVER[0].fetch_add(1, Ordering::Relaxed) < NAMED {
        tracing::warn!(
            "VICTORY_WC_NO_BACK: {what} read title memory {at:#x}+{len:#x} on the CPU, {bytes} bytes of it where shaders' \
             writes were left on the card (older bytes, if shaders wrote there); what they write there is copied back as \
             before from now on"
        );
    }
}

/// What of `[from, to)` is left on the card by any copy, cut to it.
pub(crate) fn owned_in(from: usize, to: usize) -> Vec<(usize, usize)> {
    if !no_back() {
        return Vec::new();
    }
    let owned = owned().read().unwrap_or_else(|e| e.into_inner());
    if owned.is_empty() {
        return Vec::new();
    }
    owned.overlapping(from, to).map(|(a, e)| (a.max(from), e.min(to))).collect()
}

/// Whether the runtime took some of `[address, address + len)` on the CPU
/// while shaders' writes there were left on the card ([`cpu_reads`]): such a
/// range is declared, compared and copied back as before.
pub(crate) fn read_on_cpu(address: usize, len: usize) -> bool {
    if !no_back() || len == 0 {
        return false;
    }
    let read = read().read().unwrap_or_else(|e| e.into_inner());
    if read.is_empty() || !read.overlaps(address, address.saturating_add(len)) {
        return false;
    }
    GUARD[0].fetch_add(len as u64, Ordering::Relaxed);
    true
}

/// A refresh took a part of title memory the title wrote, `over` bytes of
/// which lie where shaders' writes are left on the card, and `sent` bytes of
/// the part go up: all of it, title memory's older bytes beside the title's
/// new ones, or with [`keep_stores`] only the dwords that changed.
pub(crate) fn taken_over(over: usize, sent: usize) {
    GUARD[1].fetch_add(1, Ordering::Relaxed);
    GUARD[2].fetch_add(over as u64, Ordering::Relaxed);
    GUARD[3].fetch_add(sent as u64, Ordering::Relaxed);
}

/// A copy that held `[from, to)` left on the card is let go: what shaders
/// wrote there is gone with it, and title memory holds older bytes. A copy
/// made of that memory later starts from those; its shaders' writes are
/// copied back as before ([`read_on_cpu`]). Counted since the start.
pub(crate) fn orphaned(from: usize, to: usize) {
    if !no_back() || from >= to {
        return;
    }
    disowns(from, to);
    read().write().unwrap_or_else(|e| e.into_inner()).add(from, to);
    EVER[2].fetch_add(1, Ordering::Relaxed);
    EVER[3].fetch_add((to - from) as u64, Ordering::Relaxed);
}

/// A set of disjoint `[start, end)` ranges, joined where they touch.
#[derive(Default)]
pub(crate) struct Ranges(BTreeMap<usize, usize>);

impl Ranges {
    pub(crate) fn add(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let (mut from, mut to) = (from, to);
        // One that starts before and reaches `from` is joined.
        if let Some((&s, &e)) = self.0.range(..=from).next_back()
            && e >= from
        {
            if e >= to {
                return;
            }
            from = s;
        }
        let inside: Vec<usize> = self.0.range(from..=to).map(|(&s, _)| s).collect();
        for s in inside {
            if let Some(e) = self.0.remove(&s) {
                to = to.max(e);
            }
        }
        self.0.insert(from, to);
    }

    /// The ranges that overlap `[from, to)`.
    pub(crate) fn overlapping(&self, from: usize, to: usize) -> impl Iterator<Item = (usize, usize)> + '_ {
        let first = self.0.range(..from).next_back().filter(|(_, e)| **e > from).map(|(&s, &e)| (s, e));
        first.into_iter().chain(self.0.range(from..to).map(|(&s, &e)| (s, e)))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether any of `[from, to)` is in the set.
    pub(crate) fn overlaps(&self, from: usize, to: usize) -> bool {
        from < to && self.overlapping(from, to).next().is_some()
    }

    /// Takes `[from, to)` out, cutting the ranges it overlaps.
    pub(crate) fn remove(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let cut: Vec<(usize, usize)> = self.overlapping(from, to).collect();
        for (s, e) in cut {
            self.0.remove(&s);
            if s < from {
                self.0.insert(s, from);
            }
            if e > to {
                self.0.insert(to, e);
            }
        }
    }

    /// Whether all of `[from, to)` is in the set.
    pub(crate) fn covers(&self, from: usize, to: usize) -> bool {
        from >= to || self.0.range(..=from).next_back().is_some_and(|(_, &e)| e >= to)
    }

    pub(crate) fn contains(&self, at: usize) -> bool {
        self.0.range(..=at).next_back().is_some_and(|(_, &e)| e > at)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.0.iter().map(|(&s, &e)| (s, e))
    }
}

/// Start of a range -> (end, kind); ranges never overlap.
fn ranges() -> &'static RwLock<BTreeMap<usize, (usize, Kind)>> {
    static RANGES: OnceLock<RwLock<BTreeMap<usize, (usize, Kind)>>> = OnceLock::new();
    RANGES.get_or_init(|| RwLock::new(BTreeMap::new()))
}

fn kind_of_protection(protect: u32) -> Kind {
    if protect & WRITE_COMBINED != 0 {
        Kind::WriteCombined
    } else if protect & UNCACHED != 0 {
        Kind::Uncached
    } else {
        Kind::Cached
    }
}

/// The title asked for `[at, at + len)` with the console's `protect`
/// (reserved, committed or allocated): the newest word on that range.
pub fn title_memory_protection(at: usize, len: usize, protect: u32) {
    if !on() || at == 0 || len == 0 {
        return;
    }
    insert(&mut ranges().write().unwrap_or_else(|e| e.into_inner()), at, at.saturating_add(len), kind_of_protection(protect));
}

/// Puts `[from, to)` as `kind`, cutting what it overlaps.
fn insert(map: &mut BTreeMap<usize, (usize, Kind)>, from: usize, to: usize, kind: Kind) {
    // A range that starts before `from` and reaches into it keeps its head
    // (and its tail past `to`, if it reaches past).
    if let Some((&start, &(end, k))) = map.range(..from).next_back()
        && end > from
    {
        map.insert(start, (from, k));
        if end > to {
            map.insert(to, (end, k));
        }
    }
    // Ranges that start inside go; the last may keep its tail.
    let inside: Vec<usize> = map.range(from..to).map(|(&s, _)| s).collect();
    for start in inside {
        if let Some((end, k)) = map.remove(&start)
            && end > to
        {
            map.insert(to, (end, k));
        }
    }
    map.insert(from, (to, kind));
}

/// Whether all of `[from, to)` is noted as `kind`: no byte of another
/// kind, and none that was never noted.
fn all_of(map: &BTreeMap<usize, (usize, Kind)>, from: usize, to: usize, kind: Kind) -> bool {
    if from >= to {
        return false;
    }
    let first = map.range(..=from).next_back().filter(|&(_, &(end, _))| end > from);
    let mut at = from;
    for (&start, &(end, k)) in first.into_iter().chain(map.range(from + 1..to)) {
        if start > at || k != kind {
            return false;
        }
        at = end;
        if at >= to {
            return true;
        }
    }
    false
}

pub(crate) fn kind(address: usize) -> Kind {
    let map = ranges().read().unwrap_or_else(|e| e.into_inner());
    match map.range(..=address).next_back() {
        Some((_, &(end, k))) if address < end => k,
        _ => Kind::Unknown,
    }
}

/// Bytes copied up to the card (refreshes, copies, fills), by kind, and
/// what of them were counted.
static UP: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];
/// Bytes put back into title memory from what shaders wrote, by kind.
static BACK: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];

/// `bytes` of title memory at `address` were copied up.
pub(crate) fn up(address: usize, bytes: u64) {
    if on() && bytes != 0 {
        UP[kind(address) as usize].fetch_add(bytes, Ordering::Relaxed);
    }
}

/// `bytes` were put back into title memory at `address`.
pub(crate) fn back(address: usize, bytes: u64) {
    if on() && bytes != 0 {
        BACK[kind(address) as usize].fetch_add(bytes, Ordering::Relaxed);
    }
}

/// MB per frame, by kind, copied up and copied back.
pub(crate) type Sample = Option<([f64; KINDS], [f64; KINDS], [f64; 9], [f64; 4])>;

/// Takes the counters as the frame-rate line takes its own (every second,
/// over the same frames), so that the line printed beside it matches it.
pub(crate) fn take(frames: f64) -> Sample {
    if !on() {
        return None;
    }
    let take = |a: &[AtomicU64; KINDS]| -> [f64; KINDS] {
        std::array::from_fn(|i| a[i].swap(0, Ordering::Relaxed) as f64 / 1e6 / frames.max(1.0))
    };
    let per_frame = |v: u64| v as f64 / frames.max(1.0);
    let skipped = std::array::from_fn(|i| per_frame(SKIPPED[i].swap(0, Ordering::Relaxed)));
    let guard = std::array::from_fn(|i| per_frame(GUARD[i].swap(0, Ordering::Relaxed)));
    Some((take(&UP), take(&BACK), skipped, guard))
}

/// With the frame-rate line: what [`take`] took, and the title memory
/// noted of each kind.
pub(crate) fn log(sample: Sample) {
    let Some((up, back, skipped, g)) = sample else {
        return;
    };
    let mut noted = [0u64; KINDS];
    for (start, (end, k)) in ranges().read().unwrap_or_else(|e| e.into_inner()).iter() {
        noted[*k as usize] += (end - start) as u64;
    }
    let part = |v: &[f64; KINDS]| {
        (0..KINDS).map(|i| format!("{} {:.1}", NAMES[i], v[i])).collect::<Vec<_>>().join(", ")
    };
    tracing::info!(
        "title memory by what the title asked for (VICTORY_WC_STATS), MB per frame: copied up {}; copied back {}; \
         noted so far: {}",
        part(&up),
        part(&back),
        (0..KINDS).skip(1).map(|i| format!("{} {} MB", NAMES[i], noted[i] >> 20)).collect::<Vec<_>>().join(", ")
    );
    if no_back() {
        let [n, bytes, small, small_bytes, unwatched, unwatched_bytes, uncarded, kept_unwatched, kept_uncarded] = skipped;
        tracing::info!(
            "shaders' writes into write-combined title memory left on the card (VICTORY_WC_NO_BACK, under {} KiB kept), \
             per frame: {n:.0} declarations, {:.0} MB declared (a range counted each time it is declared); \
             small ones kept and copied back as before: {small:.0}, {:.1} MB declared; \
             refreshes that left them out, the memory not watched: {unwatched:.1}, {:.2} MB; \
             left to copies not in the card's memory: {:.2} MB; \
             kept as before, the copy not known to be watched: {:.1} MB, not in the card's memory: {:.1} MB declared",
            no_back_min() >> 10,
            bytes / 1e6,
            small_bytes / 1e6,
            unwatched_bytes / 1e6,
            uncarded / 1e6,
            kept_unwatched / 1e6,
            kept_uncarded / 1e6
        );
        let left = |set: &RwLock<Ranges>| {
            set.read().unwrap_or_else(|e| e.into_inner()).iter().map(|(s, e)| e - s).sum::<usize>() >> 20
        };
        let ever: [u64; 4] = std::array::from_fn(|i| EVER[i].load(Ordering::Relaxed));
        tracing::info!(
            "the runtime's own use of memory left on the card (VICTORY_WC_NO_BACK): since the start {} places of it read on the \
             CPU by steps of ours, {:.1} KB (older bytes than the card's where shaders wrote; shaders' writes there copied back \
             as before from then on, {:.1} MB declared a frame kept for it), {} copies let go that held some, {:.1} KB; per \
             frame, refreshes that took parts of it the title wrote: {:.1}, {:.1} KB of them left on the card, {:.1} KB sent up \
             (VICTORY_WC_KEEP_STORES={}: {}); left on the card now {} MB, copied back again {} MB",
            ever[0],
            ever[1] as f64 / 1e3,
            g[0] / 1e6,
            ever[2],
            ever[3] as f64 / 1e3,
            g[1],
            g[2] / 1e3,
            g[3] / 1e3,
            u8::from(keep_stores()),
            if keep_stores() { "only the dwords the title changed" } else { "whole, title memory's older bytes with them" },
            left(owned()),
            left(read()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_protection_wins_and_cuts_older_ranges() {
        let mut map = BTreeMap::new();
        insert(&mut map, 0x1000, 0x9000, Kind::Cached);
        insert(&mut map, 0x3000, 0x5000, Kind::WriteCombined);
        let got: Vec<_> = map.iter().map(|(&s, &(e, k))| (s, e, k)).collect();
        assert_eq!(
            got,
            [(0x1000, 0x3000, Kind::Cached), (0x3000, 0x5000, Kind::WriteCombined), (0x5000, 0x9000, Kind::Cached)]
        );
        insert(&mut map, 0x2000, 0x6000, Kind::Uncached);
        let got: Vec<_> = map.iter().map(|(&s, &(e, k))| (s, e, k)).collect();
        assert_eq!(got, [(0x1000, 0x2000, Kind::Cached), (0x2000, 0x6000, Kind::Uncached), (0x6000, 0x9000, Kind::Cached)]);
    }

    /// A step that reads memory left on the card on the CPU is noted where
    /// it read such memory and nowhere else, and from then on a declared
    /// write that holds any of it goes back as before; others do not.
    #[test]
    fn a_read_on_the_cpu_of_memory_left_on_the_card_brings_its_range_back() {
        let mut owned = Ranges::default();
        owned.add(0x10000, 0x50000);
        owned.add(0x80000, 0x90000);
        let mut read = Ranges::default();
        // Beside what is left on the card: nothing noted.
        assert_eq!(note_read(&owned, &mut read, 0x50000, 0x80000), 0);
        assert!(read.is_empty());
        // A fill of 480 bytes inside, and a copy that reaches over the end.
        assert_eq!(note_read(&owned, &mut read, 0x20100, 0x202e0), 480);
        assert_eq!(note_read(&owned, &mut read, 0x8f000, 0xa0000), 0x1000);
        assert_eq!(read.iter().collect::<Vec<_>>(), vec![(0x20100, 0x202e0), (0x8f000, 0x90000)]);
        // The same place again is no new place; one that reaches past it is.
        assert_eq!(note_read(&owned, &mut read, 0x20100, 0x202e0), 0);
        assert_eq!(note_read(&owned, &mut read, 0x8e000, 0x90000), 0x2000);
        assert_eq!(read.iter().collect::<Vec<_>>(), vec![(0x20100, 0x202e0), (0x8e000, 0x90000)]);
        // The declared write of the whole buffer holds the fill: kept. One
        // of another part of it does not, nor one that ends where it begins.
        assert!(read.overlaps(0x10000, 0x50000));
        assert!(!read.overlaps(0x30000, 0x50000));
        assert!(!read.overlaps(0x10000, 0x20100));
        assert!(read.overlaps(0x202df, 0x30000));
        assert!(!read.overlaps(0x20100, 0x20100));
    }

    /// What is left on the card must be write-combined from end to end:
    /// ranges that touch count as one, a range of another kind or a gap
    /// between them does not pass, though both ends are write-combined.
    #[test]
    fn only_memory_that_is_write_combined_throughout_passes() {
        let mut map = BTreeMap::new();
        insert(&mut map, 0x10000, 0x30000, Kind::WriteCombined);
        insert(&mut map, 0x30000, 0x50000, Kind::WriteCombined);
        insert(&mut map, 0x50000, 0x60000, Kind::Cached);
        insert(&mut map, 0x60000, 0x80000, Kind::WriteCombined);
        insert(&mut map, 0x90000, 0xa0000, Kind::WriteCombined);
        let wc = |from, to| all_of(&map, from, to, Kind::WriteCombined);
        assert!(wc(0x10000, 0x30000));
        assert!(wc(0x18000, 0x28000));
        assert!(wc(0x10000, 0x50000));
        assert!(wc(0x2ffff, 0x30001));
        // Cached memory between: both ends are write-combined, the middle is not.
        assert!(!wc(0x40000, 0x70000));
        assert!(!wc(0x4ffff, 0x50001));
        // A gap never noted between.
        assert!(!wc(0x70000, 0x98000));
        // Before the first range, past the last, and nothing at all.
        assert!(!wc(0x8000, 0x20000));
        assert!(!wc(0x98000, 0xa0001));
        assert!(!wc(0x20000, 0x20000));
        assert!(!all_of(&BTreeMap::new(), 0x10000, 0x20000, Kind::WriteCombined));
    }

    /// Leaving shaders' writes on the card sends up only what the title
    /// changed unless whole parts are asked for; without it nothing changes.
    #[test]
    fn what_is_left_on_the_card_keeps_stores_unless_asked_not_to() {
        assert!(keep_stores_asked(true, None));
        assert!(keep_stores_asked(true, Some("1")));
        assert!(!keep_stores_asked(true, Some("0")));
        assert!(!keep_stores_asked(true, Some(" 0 ")));
        assert!(!keep_stores_asked(false, None));
        assert!(!keep_stores_asked(false, Some("1")));
    }

    #[test]
    fn ranges_remove_cuts_and_cover() {
        let mut r = Ranges::default();
        r.add(0, 100);
        r.add(200, 300);
        r.remove(50, 250);
        assert_eq!(r.iter().collect::<Vec<_>>(), vec![(0, 50), (250, 300)]);
        assert!(r.covers(10, 50) && !r.covers(10, 51) && r.covers(260, 300));
        assert!(r.contains(0) && !r.contains(50) && r.contains(299) && !r.contains(300));
        r.remove(0, 1000);
        assert!(r.is_empty());
    }

    #[test]
    fn ranges_join_and_answer_overlaps() {
        let mut r = Ranges::default();
        r.add(10, 20);
        r.add(30, 40);
        r.add(20, 25);
        r.add(12, 15);
        assert_eq!(r.0.iter().map(|(&s, &e)| (s, e)).collect::<Vec<_>>(), [(10, 25), (30, 40)]);
        assert_eq!(r.overlapping(0, 11).collect::<Vec<_>>(), [(10, 25)]);
        assert_eq!(r.overlapping(25, 30).collect::<Vec<_>>(), Vec::<(usize, usize)>::new());
        assert_eq!(r.overlapping(24, 31).collect::<Vec<_>>(), [(10, 25), (30, 40)]);
        r.add(5, 50);
        assert_eq!(r.0.iter().map(|(&s, &e)| (s, e)).collect::<Vec<_>>(), [(5, 50)]);
    }

    #[test]
    fn protection_bits_pick_the_kind() {
        assert_eq!(kind_of_protection(0x2404), Kind::WriteCombined);
        assert_eq!(kind_of_protection(0x2204), Kind::Uncached);
        assert_eq!(kind_of_protection(0x22004), Kind::Cached);
    }
}
