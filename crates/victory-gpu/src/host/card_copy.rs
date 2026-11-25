//! Title buffer copies whose source flights in the air may still write,
//! done on the card behind those flights instead of on the CPU after a
//! wait for them (`VICTORY_CARD_COPY=1`, see [`on`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::*;
use crate::shadow::Shadow;

/// Whether a title buffer copy that reads title memory flights in the air
/// may write is done on the card, behind them, instead of on the CPU after
/// they have landed (`VICTORY_CARD_COPY=1`; off by default). Only with
/// flights sent ahead ([`late`]).
///
/// Why: in the boat scene of runs auto-b1 and b5 the submitting thread
/// stood 55-78 ms a frame, once a frame, for "a buffer copy at 2048 bytes
/// in 0xe011e0000+64K": the title copies 2 KiB of a 14 KiB buffer a
/// dispatch writes (0xe011ea300 and 0xe011eab00, one frame and the other)
/// into a small buffer of its own, and the copy waited until that dispatch's
/// flight had landed. Fills that read ahead of flights (`fill_ahead`) took
/// the bytes from before for it some 950 times in eight minutes of run b3,
/// so neither reading ahead nor waiting is the answer for this copy.
///
/// What is done: the copy goes to the card in a list of ours, sent on its
/// own between the send of the submission's title bytes to copies in the
/// card's memory and the submission's lists. It copies the source's bytes
/// from the host copy the flights write into the host copy of the
/// destination, and the destination's bytes are declared written by
/// shaders of this submission, exactly the bytes copied, after a refresh of
/// them: from there on they are what any shader's write is. The landing
/// compares and puts what changed into title memory before the title is
/// told anything of this flight; until then no refresh of that host copy
/// puts title memory over them.
///
/// Why that is what the CPU would have copied after the wait. After the
/// wait, title memory holds for every byte of the source what the title
/// wrote, or what the last flight that wrote it wrote. The source's host
/// copy holds the same when our list runs: every flight that may write a
/// byte of the source writes it through that one host copy ([`holder`]),
/// they are all ahead of our list on the queue, nothing else of theirs
/// writes title memory there (a bottom-of-pipe write, a read-back), and
/// the title's own bytes are brought up to date by a refresh, prepared as
/// any list's (also under flights' declared writes, `stale_under_flights`,
/// which settles if it cannot). Nothing of this submission writes the
/// source or the destination after the copy, or reads the destination on
/// the CPU after it, and no other host copy of the destination is brought
/// up to date or written in it ([`order`]): those are refused, and the
/// submission waits as before.
///
/// Later submissions, while the copy is in the air: its destination is a
/// range flights may write, so a copy into it, a fill or a texture that
/// reads it waits as for any flight's write, and a fill or copy that reads
/// it is never read ahead (`fill_ahead`), since it is known to change; a
/// copy out of it may be done here in turn. A refresh of another host copy
/// of those bytes waits for the landing (`blocked_by`).
///
/// What is not seen: code of ours that reads title memory away from a
/// submission's steps (`Step::Host` work, a look at title memory while a
/// list is recorded) finds the destination's bytes once the flight has
/// landed, not at once.
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| late() && std::env::var_os("VICTORY_CARD_COPY").is_some_and(|v| v == "1"))
}

/// The most copies of one submission done this way.
const MOST: usize = 1024;

/// Why a copy was not done on the card ([`COUNT`]'s places from 2 on).
const SOURCE: usize = 0;
const TITLE: usize = 1;
const DESTINATION: usize = 2;
const ORDER: usize = 3;
const ELSEWHERE: usize = 4;
const MANY: usize = 5;
const WHY: [&str; 6] = [
    "its source is not written through one host copy by flights seen",
    "flights write title memory there outside host copies",
    "its destination has no host copy of its own",
    "a later step of the submission reads its destination or writes there",
    "another host copy of its destination is brought up to date or written in the submission",
    "too many in one submission",
];

/// Since the last frame line: copies done on the card, their bytes,
/// copies refused by why ([`WHY`]), and waits for a copy's destination
/// done on the card (a fill or a copy that reads it, a refresh of another
/// host copy of it).
static COUNT: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

/// A title buffer copy of a submission done on the card: where it stands
/// (its list's place in the submission, its step's in the list), what it
/// copies, the host copy every flight that may write its source writes
/// through, and the host copy of its destination.
pub(super) struct Taken {
    pub(super) list: usize,
    pub(super) step: usize,
    pub(super) dst: usize,
    pub(super) src: usize,
    pub(super) len: usize,
    pub(super) from: Arc<Shadow>,
    pub(super) to: Arc<Shadow>,
}

/// What the flights in the air write, as [`take`] needs it: gathered under
/// the line's lock ([`Air::of`]), looked at outside it.
#[derive(Default)]
pub(super) struct Air {
    /// The ranges flights may write ([`Line::written`]).
    written: Vec<(usize, usize)>,
    /// Per host copy, the ranges flights in the line and flights being
    /// landed declared.
    back: Vec<Declared>,
    /// Title memory those flights write outside host copies when they land:
    /// bottom-of-pipe writes and read-backs.
    title: Vec<(usize, usize)>,
    /// One of them copies a texture back; where to is not looked at here.
    textures: bool,
    /// Copies done on the card in the air: the destination's host copy (by
    /// address), from, to.
    copied: Vec<(usize, usize, usize)>,
}

/// The title memory a flight writes when it lands outside host copies:
/// its lists' read-backs; whether it copies a texture back.
fn landing_title(flight: &Flight, out: &mut Vec<(usize, usize)>) -> bool {
    let mut textures = false;
    for &i in &flight.order {
        let deferred = flight.lists[i].host().deferred();
        out.extend(deferred.backs.iter().map(|&(_, at, [row, rows, pitch])| (at, at + rows.saturating_sub(1) * pitch + row)));
        textures |= !deferred.texture_backs.is_empty();
    }
    textures
}

impl Air {
    pub(super) fn of(line: &Line) -> Air {
        let mut air = Air {
            written: line.written.clone(),
            back: line.in_hand_back.clone(),
            title: line.in_hand_title.clone(),
            textures: line.in_hand_textures,
            copied: line.copied.clone(),
        };
        air.title.extend(line.in_hand.iter().map(|&(at, _, n)| (at, at + n)));
        for item in &line.line {
            match item {
                Late::Run(flight) => {
                    air.back.extend(flight.back.iter().cloned());
                    air.textures |= landing_title(flight, &mut air.title);
                }
                &Late::Write(at, _, n) => air.title.push((at, at + n)),
                Late::Signal(..) => {}
            }
        }
        air
    }

    /// Whether `[from, to)` touches the destination of a copy done on the
    /// card that is in the air.
    pub(super) fn copied_touches(&self, from: usize, to: usize) -> bool {
        self.copied.iter().any(|&(_, a, b)| a < to && from < b)
    }

    /// Whether a refresh of `shadow`'s `[from, from + len)` touches the
    /// destination of a copy done on the card through another host copy.
    pub(super) fn copied_elsewhere(&self, shadow: &Arc<Shadow>, from: usize, len: usize) -> bool {
        let to = from.saturating_add(len);
        let ours = Arc::as_ptr(shadow) as usize;
        self.copied.iter().any(|&(s, a, b)| s != ours && a < to && from < b)
    }
}

/// What the line notes of flights being landed, for [`Air::of`]: their
/// declared ranges, the title memory they write outside host copies, and
/// whether one copies a texture back. Called by the thread that lands
/// flights as it takes them, under the line's lock.
pub(super) fn in_hand(line: &mut Line, items: &[Late]) {
    line.in_hand_back.clear();
    line.in_hand_title.clear();
    line.in_hand_textures = false;
    if !on() {
        return;
    }
    for item in items {
        if let Late::Run(flight) = item {
            line.in_hand_back.extend(flight.back.iter().cloned());
            let mut title = Vec::new();
            line.in_hand_textures |= landing_title(flight, &mut title);
            line.in_hand_title.extend(title);
        }
    }
}

/// The host copy every flight that may write `[src, end)` writes it
/// through, as the index of its entry in `back` (per host copy, by a key,
/// the declared ranges): one such copy, whose ranges hold every byte of
/// `written` there.
fn holder<K: PartialEq>(back: &[(K, &[(usize, usize)])], written: &[(usize, usize)], src: usize, end: usize) -> Result<usize, usize> {
    let mut found: Option<usize> = None;
    let mut held: Vec<(usize, usize)> = Vec::new();
    for (i, (key, ranges)) in back.iter().enumerate() {
        for &(a, b) in ranges.iter() {
            if a < end && src < b {
                match found {
                    Some(f) if back[f].0 != *key => return Err(SOURCE),
                    Some(_) => {}
                    None => found = Some(i),
                }
                held.push((a.max(src), b.min(end)));
            }
        }
    }
    let Some(found) = found else {
        return Err(SOURCE);
    };
    join_ranges(&mut held, std::iter::empty());
    let first = written.partition_point(|r| r.1 <= src);
    for &(a, b) in written[first..].iter().take_while(|r| r.0 < end) {
        let (a, b) = (a.max(src), b.min(end));
        let i = held.partition_point(|h| h.1 <= a);
        if !held.get(i).is_some_and(|h| h.0 <= a && b <= h.1) {
            return Err(SOURCE);
        }
    }
    Ok(found)
}

/// A step of a submission as [`order`] sees it.
#[derive(Clone, Copy, Debug)]
enum Seen {
    /// A title buffer copy: destination, source, bytes.
    Copy(usize, usize, usize),
    /// A read of title memory on the CPU (a fill, a texture): from, to.
    Read(usize, usize),
    /// A refresh of a host copy, or shaders' writes declared there: the
    /// copy (by a key), from, to.
    Copy2(usize, usize, usize),
    /// Anything, anywhere.
    Run,
    Other,
}

/// Whether the copy at `at` (list, step) of `steps` (each with its place)
/// may be done on the card in front of the submission's lists, into the
/// host copy `to` (by its key): no later step writes its source or its
/// destination, or reads its destination on the CPU, and no step at all
/// brings another host copy of the destination up to date or has shaders
/// write one. Returns why not.
fn order(steps: &[((usize, usize), Seen)], at: (usize, usize), (dst, src, len): (usize, usize, usize), to: usize) -> Result<(), usize> {
    let meet = |a: usize, b: usize, c: usize, d: usize| a < d && c < b;
    let (end, dend) = (src.saturating_add(len), dst.saturating_add(len));
    for &(place, seen) in steps {
        if place == at {
            continue;
        }
        let later = place > at;
        match seen {
            Seen::Copy(d, s, n) => {
                let (de, se) = (d.saturating_add(n), s.saturating_add(n));
                if later && (meet(d, de, src, end) || meet(d, de, dst, dend) || meet(s, se, dst, dend)) {
                    return Err(ORDER);
                }
            }
            Seen::Read(a, b) => {
                if later && meet(a, b, dst, dend) {
                    return Err(ORDER);
                }
            }
            Seen::Copy2(key, a, b) => {
                if key != to && meet(a, b, dst, dend) {
                    return Err(ELSEWHERE);
                }
            }
            Seen::Run => return Err(ORDER),
            Seen::Other => {}
        }
    }
    Ok(())
}

/// The copy `(dst, src, len)` at `at` (list, step) of `runs`, done on the
/// card behind the flights in `air`, if it can be ([`on`] says why it is
/// right); else why not. `taken`: the copies of the submission taken so far.
/// Not called with a list's steps locked.
pub(super) fn take(
    air: &Air,
    runs: &[&HostList],
    at: (usize, usize),
    (dst, src, len): (usize, usize, usize),
    taken: &[Taken],
) -> Result<Taken, usize> {
    let (Some(end), Some(dend)) = (src.checked_add(len), dst.checked_add(len)) else {
        return Err(SOURCE);
    };
    if taken.len() >= MOST {
        return Err(MANY);
    }
    let back: Vec<(usize, &[(usize, usize)])> =
        air.back.iter().map(|(shadow, ranges)| (Arc::as_ptr(shadow) as usize, &ranges[..])).collect();
    let from = &air.back[holder(&back, &air.written, src, end)?].0;
    if from.in_place() || from.offset_in(src as u64).is_none() || from.left(src as u64) < len as u64 {
        return Err(SOURCE);
    }
    if air.textures || air.title.iter().any(|&(a, b)| a < end && src < b) {
        return Err(TITLE);
    }
    // The submission's steps, and the host copies it brings up to date or
    // has shaders write over the destination.
    let mut steps = Vec::new();
    let mut over: Vec<Arc<Shadow>> = Vec::new();
    for (i, list) in runs.iter().enumerate() {
        for (j, step) in list.deferred().copies.iter().enumerate() {
            let seen = match step {
                &Step::Copy(d, s, n) => Seen::Copy(d, s, n),
                &Step::Fill(_, s, n) => Seen::Read(s, s.saturating_add(n)),
                Step::Texture(staging) => {
                    let (a, n) = staging.memory();
                    Seen::Read(a, a.saturating_add(n))
                }
                Step::Refresh(shadow, a, n) | Step::GpuWrite(shadow, a, n) => {
                    let b = a.saturating_add(*n);
                    if *a < dend && dst < b && !over.iter().any(|o| Arc::ptr_eq(o, shadow)) {
                        over.push(shadow.clone());
                    }
                    Seen::Copy2(Arc::as_ptr(shadow) as usize, *a, b)
                }
                Step::Run(_) => Seen::Run,
                Step::Host(_) => Seen::Other,
            };
            steps.push(((i, j), seen));
        }
    }
    // The destination's host copy: the one the submission reads it through,
    // else the copy of the title buffer that holds it, if one is made.
    let to = match &over[..] {
        [] => crate::shadow::made(dst as u64, len as u64).ok_or(DESTINATION)?,
        [one] => one.clone(),
        _ => return Err(ELSEWHERE),
    };
    if to.in_place() || Arc::ptr_eq(&to, from) || to.offset_in(dst as u64).is_none() || to.left(dst as u64) < len as u64 {
        return Err(DESTINATION);
    }
    order(&steps, at, (dst, src, len), Arc::as_ptr(&to) as usize)?;
    // Copies taken before it (in front of it in the submission) are on the
    // card in the same list, in order: one that writes this one's source or
    // destination, or reads its destination, was refused by its own look at
    // this one (a later step). Nothing more to check.
    let _ = taken;
    Ok(Taken {
        list: at.0,
        step: at.1,
        dst,
        src,
        len,
        from: from.clone(),
        to,
    })
}

/// A copy was refused for `why` (one of the reasons in [`WHY`]).
pub(super) fn refused(why: usize) {
    COUNT[2 + why.min(WHY.len() - 1)].fetch_add(1, Relaxed);
}

/// A step waited for a copy's destination done on the card.
pub(super) fn waited_for_copy() {
    COUNT[8].fetch_add(1, Relaxed);
}

/// The host copies and ranges the copies taken bring up to date before
/// they are done: the source's and the destination's.
pub(super) fn refreshed(taken: &[Taken]) -> Vec<(Arc<Shadow>, usize, usize)> {
    taken
        .iter()
        .flat_map(|t| [(t.from.clone(), t.src, t.len), (t.to.clone(), t.dst, t.len)])
        .collect()
}

/// The shader writes the copies taken declare, per host copy, joined with
/// `back` (as `declared_writes` gives them).
pub(super) fn declare(back: &mut Vec<Declared>, taken: &[Taken]) {
    for t in taken {
        let range = (t.dst, t.dst + t.len);
        match back.iter_mut().find(|(s, _)| Arc::ptr_eq(s, &t.to)) {
            Some((_, ranges)) => {
                if !ranges.contains(&range) {
                    ranges.push(range);
                }
            }
            None => back.push((t.to.clone(), vec![range])),
        }
    }
}

/// A list of ours with the copies of one submission, kept until its flight
/// has landed: then it is free for another.
struct Kept(Prelude);

// SAFETY: D3D12 objects (free-threaded); a list is recorded by one thread
// at a time (the submitting one, under the submission lock).
unsafe impl Send for Kept {}

static FREE: Mutex<Vec<Kept>> = Mutex::new(Vec::new());

/// What a submission's copies done on the card hold until its flight has
/// landed ([`landed`]).
pub(super) struct Out {
    list: Kept,
    _copies: Vec<Arc<Shadow>>,
    /// The destinations: their host copy (by address), from, to.
    pub(super) copied: Vec<(usize, usize, usize)>,
}

/// Sends `taken` to the card on the host's queue: after what was sent to
/// it before (the flights, and this submission's title bytes for copies in
/// the card's memory), before the submission's lists. `None` if there is
/// nothing to send.
pub(super) fn send(host: &Host, taken: &[Taken]) -> Option<Out> {
    if taken.is_empty() {
        return None;
    }
    let free = FREE.lock().unwrap_or_else(|e| e.into_inner()).pop();
    let Some(Kept(list)) = free.or_else(|| host.make_prelude().map(Kept)) else {
        fail(format_args!("no host list for title buffer copies done on the card (VICTORY_CARD_COPY=1)"));
    };
    let (allocator, commands) = &list;
    // SAFETY: the list's last run has finished (its flight landed before it
    // came back); live buffers, and the ranges lie in them ([`take`]).
    let sent = unsafe {
        (|| -> windows::core::Result<()> {
            allocator.Reset()?;
            commands.Reset(allocator, None)?;
            for t in taken {
                commands.CopyBufferRegion(
                    &t.to.buffer,
                    t.to.offset(t.dst as u64),
                    &t.from.buffer,
                    t.from.offset(t.src as u64),
                    t.len as u64,
                );
            }
            commands.Close()?;
            host.queue.ExecuteCommandLists(&[Some(commands.cast()?)]);
            Ok(())
        })()
    };
    if let Err(e) = sent {
        fail(format_args!("title buffer copies done on the card could not be sent ({e}, VICTORY_CARD_COPY=1)"));
    }
    COUNT[0].fetch_add(taken.len() as u64, Relaxed);
    COUNT[1].fetch_add(taken.iter().map(|t| t.len as u64).sum(), Relaxed);
    let mut copies: Vec<Arc<Shadow>> = Vec::with_capacity(2 * taken.len());
    for t in taken {
        copies.push(t.from.clone());
        copies.push(t.to.clone());
    }
    Some(Out {
        list: Kept(list),
        _copies: copies,
        copied: taken.iter().map(|t| (Arc::as_ptr(&t.to) as usize, t.dst, t.dst + t.len)).collect(),
    })
}

/// The flight that carried `out` has landed: its list is free.
pub(super) fn landed(out: Option<Out>) {
    if let Some(out) = out {
        FREE.lock().unwrap_or_else(|e| e.into_inner()).push(out.list);
    }
}

/// With the frame line, when there was anything to say.
pub(super) fn line(frames: f64) {
    if !on() {
        return;
    }
    let n: Vec<u64> = COUNT.iter().map(|c| c.swap(0, Relaxed)).collect();
    if n.iter().all(|&v| v == 0) {
        return;
    }
    let refused: Vec<String> = WHY
        .iter()
        .enumerate()
        .filter(|&(i, _)| n[2 + i] > 0)
        .map(|(i, why)| format!("{why} {:.2}", n[2 + i] as f64 / frames))
        .collect();
    tracing::info!(
        "title buffer copies done on the card behind flights (VICTORY_CARD_COPY=1), per frame: {:.2} ({:.0} bytes); refused, \
         and waited for: {}; waits for a destination of one still in the air: {:.2}",
        n[0] as f64 / frames,
        n[1] as f64 / frames,
        if refused.is_empty() { "none".to_owned() } else { refused.join(", ") },
        n[8] as f64 / frames
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The source's host copy is the one whose ranges hold every byte
    /// flights may write there; two copies, or a byte no flight seen
    /// declared, and nothing is.
    #[test]
    fn the_source_has_one_holder_or_none() {
        let a: &[(usize, usize)] = &[(100, 200), (300, 400)];
        let b: &[(usize, usize)] = &[(150, 160), (500, 600)];
        // Copy 1 holds 100..200; copy 2 writes 150..160 too.
        let back = [(1usize, a), (2, b)];
        let written = [(100, 200), (300, 400), (500, 600)];
        assert_eq!(holder(&back, &written, 300, 350), Ok(0));
        assert_eq!(holder(&back, &written, 510, 520), Ok(1));
        assert_eq!(holder(&back, &written, 120, 170), Err(SOURCE), "two copies write it");
        assert_eq!(holder(&back, &written, 120, 140), Ok(0));
        // The same copy declared twice (two flights) is one holder.
        let back2 = [(1usize, a), (1, &[(380usize, 420usize)][..])];
        assert_eq!(holder(&back2, &[(100, 200), (300, 420)], 390, 410), Ok(0));
        // Written there by a flight not in `back` (being landed, unseen).
        assert_eq!(holder(&back, &[(100, 200), (300, 450)], 390, 440), Err(SOURCE));
        // Nothing declared there at all.
        assert_eq!(holder(&back, &written, 700, 800), Err(SOURCE));
    }

    /// A later step that writes the source or the destination, or reads the
    /// destination on the CPU, keeps the copy off the card; an earlier one
    /// does not. Another host copy of the destination anywhere does.
    #[test]
    fn later_steps_that_touch_its_bytes_keep_a_copy_off_the_card() {
        let at = (1, 5);
        let copy = (1000, 5000, 64);
        let to = 7;
        let check = |seen: Seen, place: (usize, usize)| order(&[(at, Seen::Copy(1000, 5000, 64)), (place, seen)], at, copy, to);
        let later = (2, 0);
        let earlier = (0, 9);
        // Copies into the source or the destination, or out of the destination.
        for seen in [Seen::Copy(5032, 9000, 8), Seen::Copy(990, 9000, 16), Seen::Copy(9000, 1060, 8)] {
            assert_eq!(check(seen, later), Err(ORDER), "{seen:?} later");
            assert_eq!(check(seen, earlier), Ok(()), "{seen:?} earlier");
        }
        // A copy out of the source, or beside both, is no reason.
        assert_eq!(check(Seen::Copy(9000, 5000, 64), later), Ok(()));
        assert_eq!(check(Seen::Copy(1064, 5064, 64), later), Ok(()));
        // Reads of the destination on the CPU after it.
        assert_eq!(check(Seen::Read(1063, 1100), later), Err(ORDER));
        assert_eq!(check(Seen::Read(1064, 1100), later), Ok(()));
        assert_eq!(check(Seen::Read(1000, 1001), (1, 6)), Err(ORDER));
        assert_eq!(check(Seen::Read(1000, 1001), (1, 4)), Ok(()));
        // Host copies over the destination: its own is fine, another not.
        assert_eq!(check(Seen::Copy2(to, 0, 4096), later), Ok(()));
        assert_eq!(check(Seen::Copy2(8, 0, 4096), earlier), Err(ELSEWHERE));
        assert_eq!(check(Seen::Copy2(8, 4096, 8192), later), Ok(()));
        // Work that may write anything.
        assert_eq!(check(Seen::Run, earlier), Err(ORDER));
        assert_eq!(check(Seen::Other, later), Ok(()));
    }
}
