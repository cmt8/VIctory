//! What the waits for flights before title memory is read are for, and what
//! would be left of them if small reads in pages no shader's write went into
//! lately were taken as the title's (`VICTORY_FILL_TITLE=2`). A report: no
//! submission is prepared otherwise for it.
//!
//! With the card's steps at their first stage about one submission a frame
//! still waits, 17-22 ms of a 60 ms frame on the boat (run pb12): the card's
//! steps refuse it for a title buffer copy, and it stands until every flight
//! that is declared to write the title's 174 MB heap has landed. Its fills of
//! upload memory, some thousands, are then done on the CPU. Done on the card
//! instead (the second stage, which takes the copy) they were 1,700-2,900
//! descriptor slots a frame pointed at the card's bytes where the first stage
//! in the same scene has 20-36 (run pb11 against pb10 and pb12), and a wait of the same
//! length was left for another reason.
//!
//! The other way is not to ask the card: the bytes are the title's own (its
//! ring of constants), and only the declaration, which is of the whole heap,
//! says a shader may write them. `VICTORY_FILL_TITLE=2` takes them so where
//! no shader's write went into their pages in the last 600 frames, and the
//! CPU prepares the submission as it does with nothing in the air: no slot is
//! pointed anywhere else and no view is made. Whether that is so for the
//! reads that wait in a scene, and what else of such a submission would wait
//! all the same, is counted here before the switch is ever set; with it set,
//! the same line says what the waits that are left are for, and
//! `crate::watch::taken_as_titles` counts the reads that were taken wrongly.

use super::*;

/// The most bytes of a read that is taken as the title's.
pub(super) const SMALL: usize = 64 << 10;

/// Whether a read is one `VICTORY_FILL_TITLE=2` takes as the title's,
/// whatever the switch says.
fn quiet(src: usize, len: usize) -> bool {
    len <= SMALL && !crate::watch::landed_lately(src, len, 600)
}

/// A step of a submission, as far as this looks at it.
#[derive(Clone, Copy, Debug)]
enum Seen {
    Copy { dst: usize, src: usize, len: usize },
    /// `to`: the card can be given its bytes ([`card_steps`]).
    Fill { src: usize, len: usize, to: bool },
    /// Of a host copy, of a texture, or by a step of ours: title memory read
    /// on the CPU.
    Refresh { from: usize, to: usize },
    Texture { from: usize, to: usize },
    Host { from: usize, to: usize },
    Run,
    /// A copy at its place that waits whatever reads it ([`place::Waits`]).
    PlacedWaits,
}

/// What is counted, since the last frame-rate line.
#[derive(Clone, Copy)]
#[repr(usize)]
enum Count {
    /// Waits looked at.
    Looked,
    /// Of them: nothing would have waited, and nothing gone to the card.
    Free,
    /// Only fills in pages shaders wrote lately, which the card's steps take.
    Card,
    /// What would have waited all the same, each once a wait.
    Source,
    Destination,
    Fill,
    Texture,
    Placed,
    Readers,
    Run,
    /// In those submissions: fills that read what flights are declared to
    /// write, and those of them in quiet pages.
    Fills,
    FillsQuiet,
    /// Buffer copies that read or write it, their bytes, and those with a
    /// source in quiet pages.
    Copies,
    CopyBytes,
    CopiesQuiet,
    /// Behind such a copy in the submission, reading its destination on the
    /// CPU: fills, refreshes of host copies, other steps.
    ReadByFills,
    ReadByRefreshes,
    ReadByOthers,
    /// (How many there are.)
    All,
}

static STATS: [std::sync::atomic::AtomicU64; Count::All as usize] = [const { std::sync::atomic::AtomicU64::new(0) }; Count::All as usize];

/// The last buffer copy counted: destination, source, bytes, whether flights
/// are declared to write the destination, and what holds the destination on
/// the card.
static LAST: Mutex<Option<(usize, usize, usize, bool, &'static str)>> = Mutex::new(None);

/// What [`judge`] found: the counts, and the last copy (destination, source,
/// bytes, whether its destination is declared written).
struct Judged {
    counts: [u64; Count::All as usize],
    last: Option<(usize, usize, usize, bool)>,
}

/// `seen`, the steps of a submission in their order, against `written`, what
/// flights in the air are declared to write (in order, joined). `readers`:
/// whether a step reads on the CPU what a copy at its place writes
/// ([`place::read_by`]).
fn judge(written: &[(usize, usize)], seen: &[Seen], quiet: &dyn Fn(usize, usize) -> bool, readers: bool) -> Judged {
    let counts = std::cell::Cell::new([0u64; Count::All as usize]);
    let add = |c: Count, n: usize| {
        let mut all = counts.get();
        all[c as usize] += n as u64;
        counts.set(all);
    };
    let mut left = [false; Count::All as usize];
    let mut card = false;
    // The destinations of the copies counted so far.
    let mut copied: Vec<(usize, usize)> = Vec::new();
    let mut last = None;
    for step in seen {
        match *step {
            Seen::Copy { dst, src, len } => {
                if touches(&copied, src, src + len) {
                    add(Count::ReadByOthers, 1);
                }
                let read = touches(written, src, src + len);
                let write = touches(written, dst, dst + len);
                if !read && !write {
                    continue;
                }
                add(Count::Copies, 1);
                add(Count::CopyBytes, len);
                if read && quiet(src, len) {
                    add(Count::CopiesQuiet, 1);
                } else if read {
                    left[Count::Source as usize] = true;
                }
                left[Count::Destination as usize] |= write;
                join_ranges(&mut copied, [(dst, dst + len)]);
                last = Some((dst, src, len, write));
            }
            Seen::Fill { src, len, to } => {
                if touches(&copied, src, src + len) {
                    add(Count::ReadByFills, 1);
                }
                if !touches(written, src, src + len) {
                    continue;
                }
                add(Count::Fills, 1);
                if quiet(src, len) {
                    add(Count::FillsQuiet, 1);
                } else if to {
                    card = true;
                } else {
                    left[Count::Fill as usize] = true;
                }
            }
            Seen::Refresh { from, to } => {
                if touches(&copied, from, to) {
                    add(Count::ReadByRefreshes, 1);
                }
            }
            Seen::Texture { from, to } => {
                if touches(&copied, from, to) {
                    add(Count::ReadByOthers, 1);
                }
                left[Count::Texture as usize] |= touches(written, from, to);
            }
            Seen::Host { from, to } => {
                if touches(&copied, from, to) {
                    add(Count::ReadByOthers, 1);
                }
            }
            Seen::Run => left[Count::Run as usize] = true,
            Seen::PlacedWaits => left[Count::Placed as usize] = true,
        }
    }
    left[Count::Readers as usize] = readers;
    add(Count::Looked, 1);
    if !left.iter().any(|l| *l) {
        add(if card { Count::Card } else { Count::Free }, 1);
    }
    let mut counts = counts.get();
    for (c, _) in left.iter().enumerate().filter(|(_, l)| **l) {
        counts[c] += 1;
    }
    Judged { counts, last }
}

/// `runs` are about to wait for flights because title memory is read
/// ([`late_blocked`]): counted, by what the wait is for.
pub(super) fn look(runs: &[&HostList]) {
    let written = line().written.clone();
    if written.is_empty() {
        return;
    }
    let mut seen = Vec::new();
    let mut placed = false;
    for list in runs {
        for step in &list.deferred().copies {
            seen.push(match step {
                &Step::Copy(dst, src, len) => Seen::Copy { dst, src, len },
                Step::Fill(_, src, len, to) => Seen::Fill { src: *src, len: *len, to: to.is_some() },
                Step::Refresh(_, at, len) => Seen::Refresh { from: *at, to: *at + *len },
                Step::Texture(staging) => {
                    let (at, len) = staging.memory();
                    Seen::Texture { from: at, to: at + len }
                }
                &Step::Host(_, (from, to)) => Seen::Host { from, to },
                Step::Run(_) => Seen::Run,
                Step::Placed(p) => match p.waits(&written) {
                    place::Waits::Yes => Seen::PlacedWaits,
                    place::Waits::ForReaders => {
                        placed = true;
                        continue;
                    }
                    place::Waits::No => continue,
                },
                Step::GpuWrite(..) => continue,
            });
        }
    }
    let readers = placed && place::read_by(runs, &written);
    let judged = judge(&written, &seen, &quiet, readers);
    for (stat, n) in STATS.iter().zip(judged.counts) {
        if n > 0 {
            stat.fetch_add(n, Ordering::Relaxed);
        }
    }
    if let Some((dst, src, len, declared)) = judged.last {
        let held = match crate::shadow::peek(dst as u64, len as u64) {
            None => "no host copy holds its destination",
            Some(s) if s.in_place() => "its destination's host copy is title memory itself",
            Some(_) => "its destination has a host copy of its own",
        };
        *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some((dst, src, len, declared, held));
    }
}

/// For the frame-rate line.
pub(super) fn log(frames: f64) {
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    let n = |c: Count| s[c as usize];
    let mode = fill_title();
    if n(Count::Looked) > 0.0 {
        tracing::info!(
            "waits for flights before title memory is read, per frame: {:.2} looked at; with small reads in pages no shader's \
             write went into in 600 frames taken as the title's (VICTORY_FILL_TITLE=2, now {}) {:.2} would not have waited and \
             {:.2} only for fills the card's steps take (VICTORY_CARD_STEPS); waiting all the same: {:.2} for a buffer copy's \
             source, {:.2} for its destination, {:.2} for a fill the card cannot be given, {:.2} for a texture, {:.2} for a copy \
             at its place out of upload memory, {:.2} for what a copy at its place writes, {:.2} for a step of ours; in them \
             {:.1} fills read what flights are declared to write, {:.1} of them in such pages, and {:.2} buffer copies read or \
             write it ({:.1} KB), {:.2} of them with their source in such pages; behind those copies {:.1} fills, {:.1} \
             refreshes of host copies and {:.1} other steps of the submission read their destinations",
            n(Count::Looked),
            if mode == 2 { "on" } else { "off" },
            n(Count::Free),
            n(Count::Card),
            n(Count::Source),
            n(Count::Destination),
            n(Count::Fill),
            n(Count::Texture),
            n(Count::Placed),
            n(Count::Readers),
            n(Count::Run),
            n(Count::Fills),
            n(Count::FillsQuiet),
            n(Count::Copies),
            n(Count::CopyBytes) / 1024.0,
            n(Count::CopiesQuiet),
            n(Count::ReadByFills),
            n(Count::ReadByRefreshes),
            n(Count::ReadByOthers),
        );
        if let Some((dst, src, len, declared, held)) = LAST.lock().unwrap_or_else(|e| e.into_inner()).take() {
            tracing::info!(
                "the last buffer copy of a submission that waited so: {len:#x} bytes from {src:#x} to {dst:#x}; {held}{}",
                if declared { ", and flights are declared to write it" } else { "" },
            );
        }
    }
    let [looks, wrong] = crate::watch::taken_as_titles_stats().map(|n| n as f64 / frames);
    if mode != 0 || looks > 0.0 {
        tracing::info!(
            "small reads taken as the title's though flights are declared to write there, per frame (VICTORY_FILL_TITLE={mode}): \
             {looks:.1} looks (a read is looked at several times); {wrong:.2} pages of them had a shader's write landed in them \
             within a frame (read too early, unless that flight was sent after the read)",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The title's heap, declared written as a whole, and a place outside.
    const HEAP: (usize, usize) = (0x2_51a0_0000, 0x2_51a0_0000 + 0xae0_0000);
    const OUT: usize = 0x7_0000_0000;

    fn n(j: &Judged, c: Count) -> u64 {
        j.counts[c as usize]
    }

    /// The ring's constants and the copy out of it, none in a page shaders
    /// wrote: nothing would wait, and nothing go to the card.
    #[test]
    fn reads_of_the_ring_alone_would_not_wait() {
        let ring = HEAP.0 + 0x1e_0000;
        let seen = [
            Seen::Fill { src: ring + 0x1e4c0, len: 0x1e0, to: true },
            Seen::Copy { dst: OUT, src: ring + 0x1e940, len: 0xd60 },
            Seen::Fill { src: OUT + 0x100, len: 0x40, to: true },
            Seen::Refresh { from: OUT, to: OUT + 0x1000 },
            Seen::Fill { src: OUT + 0x10_0000, len: 0x40, to: true },
        ];
        let j = judge(&[HEAP], &seen, &|_, _| true, false);
        assert_eq!((n(&j, Count::Looked), n(&j, Count::Free), n(&j, Count::Card)), (1, 1, 0));
        assert_eq!((n(&j, Count::Fills), n(&j, Count::FillsQuiet)), (1, 1), "the fills outside the heap read nothing declared");
        assert_eq!((n(&j, Count::Copies), n(&j, Count::CopyBytes), n(&j, Count::CopiesQuiet)), (1, 0xd60, 1));
        assert_eq!((n(&j, Count::ReadByFills), n(&j, Count::ReadByRefreshes), n(&j, Count::ReadByOthers)), (1, 1, 0));
        assert_eq!(j.last, Some((OUT, ring + 0x1e940, 0xd60, false)));
    }

    /// A fill in a page a shader wrote lately is the card's still; one the
    /// card cannot be given, a copy's source there, a destination flights
    /// are declared to write, a texture and what a copy at its place writes
    /// wait as before, each counted once a wait.
    #[test]
    fn what_shaders_wrote_lately_is_left_to_the_card_or_the_wait() {
        let landed = HEAP.0 + 0x40_0000;
        let quiet = |src: usize, _: usize| !(landed..landed + 0x1000).contains(&src);
        let fill = |to| Seen::Fill { src: landed + 0x10, len: 0x230, to };
        let j = judge(&[HEAP], &[fill(true), fill(true)], &quiet, false);
        assert_eq!((n(&j, Count::Free), n(&j, Count::Card), n(&j, Count::Fills), n(&j, Count::FillsQuiet)), (0, 1, 2, 0));
        let j = judge(&[HEAP], &[fill(true), fill(false), fill(false)], &quiet, false);
        assert_eq!((n(&j, Count::Free), n(&j, Count::Card), n(&j, Count::Fill)), (0, 0, 1));
        let seen = [
            Seen::Copy { dst: OUT, src: landed, len: 0x100 },
            Seen::Copy { dst: HEAP.0 + 0x1000, src: OUT + 0x2000, len: 0x100 },
            Seen::Texture { from: HEAP.0, to: HEAP.0 + 0x1_0000 },
        ];
        let j = judge(&[HEAP], &seen, &quiet, true);
        assert_eq!((n(&j, Count::Free), n(&j, Count::Card)), (0, 0));
        assert_eq!(
            [Count::Source, Count::Destination, Count::Texture, Count::Readers, Count::Placed, Count::Run].map(|c| n(&j, c)),
            [1, 1, 1, 1, 0, 0]
        );
        assert_eq!(j.last, Some((HEAP.0 + 0x1000, OUT + 0x2000, 0x100, true)));
        // (What reads a destination ahead of its copy is not behind it.)
        let seen = [Seen::Fill { src: OUT, len: 0x40, to: true }, Seen::Copy { dst: OUT, src: HEAP.0, len: 0x40 }];
        assert_eq!(n(&judge(&[HEAP], &seen, &quiet, false), Count::ReadByFills), 0);
        let j = judge(&[HEAP], &[Seen::Run, Seen::PlacedWaits], &quiet, false);
        assert_eq!((n(&j, Count::Run), n(&j, Count::Placed), n(&j, Count::Free)), (1, 1, 0));
    }

    /// A read of more than 64 KiB is never taken as the title's.
    #[test]
    fn only_small_reads_are_quiet() {
        assert!(!quiet(0x7fe0_0000_0000, SMALL + 1));
    }
}
