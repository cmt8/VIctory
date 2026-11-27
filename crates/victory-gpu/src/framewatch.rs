//! Diagnostics: what went into each presented frame. `VICTORY_FRAME_WATCH=1`
//! (off unless set) logs one line per presented frame with what was counted
//! since the frame before: the draws, dispatches and indirect executions the
//! title recorded and those of them that were not run, what the card's
//! argument clamps noted, what the heap sync did with the records the title
//! copied into its view heap, and the slots that were held back, made null
//! or shown late.
//!
//! It is for objects that are there, go for a few frames and come back (the
//! far trees): set beside the pictures of `crate::popwatch` (a diagnostics
//! build, `VICTORY_POP_WATCH` with `VICTORY_POP_ALL=1`, which names the same
//! frames) the lines of the frames around a dropout say whether the title
//! recorded less (the counts fall), the card was asked for less (the mesh
//! groups fall while the counts stay), or the draws read something else (the
//! counts stay and a slot count rises). The dropouts are found in the
//! pictures by their frame numbers (a block of the picture that agrees in
//! the frames before and after and is another in between), and these lines
//! read around them.
//!
//! The card's notes (mesh groups, refused records) are taken when a list is
//! recorded again after it ran: they belong to a frame or two before the
//! line they are in.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

/// What is counted here, since the start, for the lines alone (the frame
/// rate line's own counters are taken and zeroed every few seconds).
#[derive(Clone, Copy)]
pub(crate) enum Count {
    /// Draws and dispatches recorded but not run: no host pipeline.
    Dropped,
    /// Not run: a stage of the ray-tracing structure builder, which is off.
    DroppedBuilder,
    /// Not run though there is a host pipeline: it could not be bound (a
    /// table that found no slots, a view that could not be made), or it is
    /// of another kind than the call.
    DroppedOther,
    /// Tables that found no free dynamic slots (`HostHeap::dynamic`).
    TablesRefused,
    /// Indirect executions not run.
    IndirectDropped,
    /// Groups asked by indirect mesh dispatches, as the clamps saw them.
    MeshGroups,
    /// Indirect records that ran empty for want of their list's budget.
    Refused,
    /// The heap sync: new records mirrored at once.
    AtOnce,
    /// Of those, records in a slot that showed another view.
    AtOnceOver,
    /// New records put to wait for a draw (`HostHeap::copied_wait`).
    Waits,
    /// Records mirrored after the wait: no draw read them through a table.
    Waited,
    /// Of those, records in a slot that showed another view all that time.
    WaitedOver,
    /// Slots that showed a view and were made null for a buffer view that
    /// waits for a draw or the heap's pass.
    WaitNull,
}

const COUNTS: usize = Count::WaitNull as usize + 1;
static COUNTED: [AtomicU64; COUNTS] = [const { AtomicU64::new(0) }; COUNTS];

/// `n` more of `what`.
pub(crate) fn add(what: Count, n: u64) {
    COUNTED[what as usize].fetch_add(n, Relaxed);
}

/// How many of `what` were counted since the start.
pub(crate) fn counted(what: Count) -> u64 {
    COUNTED[what as usize].load(Relaxed)
}

/// Whether the lines are written.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_FRAME_WATCH").is_ok_and(|v| v.trim() == "1"))
}

/// The numbers of a line: those of [`Count`], then the ones kept elsewhere.
const NUMBERS: usize = COUNTS + 20;

/// Where the numbers kept elsewhere are, after those of [`Count`].
mod at {
    use super::COUNTS;
    pub(super) const DRAWS: usize = COUNTS;
    pub(super) const DISPATCHES: usize = COUNTS + 1;
    pub(super) const INDIRECT: usize = COUNTS + 2;
    pub(super) const RUN: usize = COUNTS + 3;
    pub(super) const STRIDES_OUT: usize = COUNTS + 4;
    pub(super) const STRIDES_WITHHELD: usize = COUNTS + 5;
    pub(super) const CLEARS_HELD: usize = COUNTS + 6;
    pub(super) const CLEARS_WRITTEN: usize = COUNTS + 7;
    pub(super) const VIEWS_HELD: usize = COUNTS + 8;
    pub(super) const VIEWS_MADE: usize = COUNTS + 9;
    pub(super) const UNWRITTEN: usize = COUNTS + 10;
    pub(super) const OTHER_KIND: usize = COUNTS + 11;
    pub(super) const NEW_LATER: usize = COUNTS + 12;
    pub(super) const TEXTURE_NULL: usize = COUNTS + 13;
    pub(super) const TEXTURE_KEPT: usize = COUNTS + 14;
    pub(super) const TEXTURE_NEUTRAL: usize = COUNTS + 15;
    pub(super) const TEXTURE_STALE: usize = COUNTS + 16;
    pub(super) const TEXTURE_UNMAPPED: usize = COUNTS + 17;
    pub(super) const SETTLES: usize = COUNTS + 18;
    pub(super) const LANDINGS: usize = COUNTS + 19;
}

/// The numbers as they are now, all since the start.
fn numbers() -> [u64; NUMBERS] {
    use crate::descriptor::host as slots;
    let mut n = [0u64; NUMBERS];
    for (to, from) in n.iter_mut().zip(&COUNTED) {
        *to = from.load(Relaxed);
    }
    let [draws, dispatches, indirect] = crate::list::totals();
    n[at::DRAWS] = draws;
    n[at::DISPATCHES] = dispatches;
    n[at::INDIRECT] = indirect;
    n[at::RUN] = crate::draw::drawn();
    n[at::STRIDES_OUT] = slots::STRIDES_HIDDEN[0].load(Relaxed);
    n[at::STRIDES_WITHHELD] = slots::STRIDES_HIDDEN[1].load(Relaxed);
    n[at::CLEARS_HELD] = slots::HELD_CLEARS[0].load(Relaxed);
    n[at::CLEARS_WRITTEN] = slots::HELD_CLEARS[2].load(Relaxed);
    n[at::VIEWS_HELD] = slots::DEFERRED_VIEWS[0].load(Relaxed);
    n[at::VIEWS_MADE] = slots::DEFERRED_VIEWS[1].load(Relaxed);
    n[at::UNWRITTEN] = slots::UNWRITTEN[..4].iter().map(|c| c.load(Relaxed)).sum();
    n[at::OTHER_KIND] = slots::KIND_MISMATCHES.iter().map(|c| c.load(Relaxed)).sum();
    n[at::NEW_LATER] = slots::NEW_LATER.load(Relaxed);
    let [null, kept, neutral, stale] = slots::pop_causes();
    n[at::TEXTURE_NULL] = null;
    n[at::TEXTURE_KEPT] = kept;
    n[at::TEXTURE_NEUTRAL] = neutral;
    n[at::TEXTURE_STALE] = stale;
    n[at::TEXTURE_UNMAPPED] = slots::STALE[4].load(Relaxed);
    let [settles, landings] = crate::host::settled_so_far();
    n[at::SETTLES] = settles;
    n[at::LANDINGS] = landings;
    n
}

/// The line of a frame: what `now` has more than `was`.
fn line(frame: u64, ms: u64, now: &[u64; NUMBERS], was: &[u64; NUMBERS]) -> String {
    let d = |i: usize| now[i].saturating_sub(was[i]);
    let c = |what: Count| d(what as usize);
    format!(
        "frame watch: frame {frame} at {:.3} s: {} draws ({} run), {} dispatches, {} indirect executions ({} dropped); not \
         run: {} without a host pipeline, {} of the structure builder, {} others ({} tables found no slots); the card's clamps, noted since: {} mesh groups asked, {} records refused; \
         heap records: {} mirrored at once ({} over another view), {} put to wait, {} mirrored after the wait ({} over \
         another view), {} slots made null for a buffer view that waits; strides: {} taken out, {} withheld; clears: {} \
         held back, {} written; new views: {} held back, {} made; unwritten: {}; other kind: {}; new textures later: {}; \
         texture slots: {} null for another, {} kept, {} neutral, {} stale, {} unmapped; {} settles, {} landings",
        ms as f64 / 1e3,
        d(at::DRAWS),
        d(at::RUN),
        d(at::DISPATCHES),
        d(at::INDIRECT),
        c(Count::IndirectDropped),
        c(Count::Dropped),
        c(Count::DroppedBuilder),
        c(Count::DroppedOther),
        c(Count::TablesRefused),
        c(Count::MeshGroups),
        c(Count::Refused),
        c(Count::AtOnce),
        c(Count::AtOnceOver),
        c(Count::Waits),
        c(Count::Waited),
        c(Count::WaitedOver),
        c(Count::WaitNull),
        d(at::STRIDES_OUT),
        d(at::STRIDES_WITHHELD),
        d(at::CLEARS_HELD),
        d(at::CLEARS_WRITTEN),
        d(at::VIEWS_HELD),
        d(at::VIEWS_MADE),
        d(at::UNWRITTEN),
        d(at::OTHER_KIND),
        d(at::NEW_LATER),
        d(at::TEXTURE_NULL),
        d(at::TEXTURE_KEPT),
        d(at::TEXTURE_NEUTRAL),
        d(at::TEXTURE_STALE),
        d(at::TEXTURE_UNMAPPED),
        d(at::SETTLES),
        d(at::LANDINGS),
    )
}

/// Frame `frame` is presented: its line, if they are written
/// ([`on`]). The first frame has none (nothing to take it from).
pub(crate) fn presented(frame: u64) {
    if !on() {
        return;
    }
    static WAS: Mutex<Option<[u64; NUMBERS]>> = Mutex::new(None);
    let now = numbers();
    let was = WAS.lock().unwrap_or_else(|e| e.into_inner()).replace(now);
    if let Some(was) = was {
        tracing::info!("{}", line(frame, crate::host::now_ms(), &now, &was));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line tells what was counted between two frames, each number in its
    /// place, and a count that was taken back in between (it never is, but
    /// two of them are sums) tells nothing rather than wrapping.
    #[test]
    fn a_line_tells_what_was_counted_since_the_frame_before() {
        let mut was = [0u64; NUMBERS];
        let mut now = [0u64; NUMBERS];
        for (i, (w, n)) in was.iter_mut().zip(now.iter_mut()).enumerate() {
            *w = 1000 * i as u64;
            *n = *w + 1 + i as u64;
        }
        now[at::LANDINGS] = 0;
        let text = line(7, 12_345, &now, &was);
        assert!(text.starts_with("frame watch: frame 7 at 12.345 s: "), "{text}");
        let draws = 1 + at::DRAWS as u64;
        let dropped = 1 + Count::Dropped as u64;
        let run = 1 + at::RUN as u64;
        assert!(text.contains(&format!("{draws} draws ({run} run), ")), "{text}");
        let (builder, others) = (1 + Count::DroppedBuilder as u64, 1 + Count::DroppedOther as u64);
        let refused = 1 + Count::TablesRefused as u64;
        let not_run = format!(
            "not run: {dropped} without a host pipeline, {builder} of the structure builder, {others} others ({refused} tables found no slots)"
        );
        assert!(text.contains(&not_run), "{text}");
        let waited = 1 + Count::Waited as u64;
        let over = 1 + Count::WaitedOver as u64;
        assert!(text.contains(&format!("{waited} mirrored after the wait ({over} over another view)")), "{text}");
        let groups = 1 + Count::MeshGroups as u64;
        assert!(text.contains(&format!("{groups} mesh groups asked")), "{text}");
        assert!(text.ends_with(&format!("{} settles, 0 landings", 1 + at::SETTLES as u64)), "{text}");
    }

    /// Counting goes on whether or not the lines are written, and adds up.
    #[test]
    fn counts_add_up() {
        let before = COUNTED[Count::WaitNull as usize].load(Relaxed);
        add(Count::WaitNull, 2);
        add(Count::WaitNull, 3);
        assert!(COUNTED[Count::WaitNull as usize].load(Relaxed) >= before + 5);
    }
}
