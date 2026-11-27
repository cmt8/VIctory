//! The holes of the title's reservations of 64 KiB pages, from the runtime's
//! own records (`VICTORY_HOLE_RECORDS`, on by default; `=0` asks Windows
//! every time, as before; `=check` asks both, goes by what Windows says and
//! says where the two differ).
//!
//! The title's views lie in placeholders the runtime reserved for it
//! (`victory-hle` xmem, `XMemVirtualAlloc`), and in such a reservation a
//! page comes to be only by the runtime's mapping a view of a section there
//! (`XMemMapPhysicalPages`, the GPU's page-mapping copies) and goes only by
//! its unmapping it. So for the blocks of such a reservation the runtime
//! knows what [`super::uncommitted_end`] asks: which of them are holes.
//!
//! Under Wine nothing else knew: Windows cannot be asked there whether a
//! reservation is a placeholder (`super::placeholder`, which the known holes
//! of `VICTORY_KNOWN_HOLES` rest on), so every asking about a range of views
//! with holes put a question to it for each hole, a walk through the
//! process's views behind its one address-space lock each time: the largest
//! single cost left in a profile of the world (run belo-water, 6.6% of all
//! threads' time in `get_basic_memory_info`, 14 ms a frame).
//!
//! Two sets of blocks, a bit each ([`Bits`]), read without a lock:
//!
//! * `reserved`: the whole blocks of a placeholder reservation the runtime
//!   made ([`super::title_memory_reserved`]), of which nothing but its own
//!   mapping calls has been told since. Taken out, before the memory
//!   changes, by the title's freeing or decommitting ([`super::title_memory_freed`]);
//!   and when the runtime allocates there in another way
//!   ([`super::title_memory_allocated`]: private memory in place of a
//!   placeholder), and where a view does not begin or end with a block.
//! * `shown`: the blocks a view of the runtime's is mapped over, and
//!   committed: set when the runtime tells of it, which is after it mapped
//!   and committed ([`super::title_memory_remapped`]), and cleared when it
//!   tells that it is about to unmap ([`super::title_memory_unmapping`]),
//!   which is before. So a block that is `shown` is there.
//!
//! A reserved block that is not shown is a hole, or a view on its way: mapped
//! and not yet told of, or told of and not yet unmapped. Called a hole then,
//! it is what Windows would have said a moment before or after, and what the
//! runtime tells next takes what was noted of the block away as it did before
//! (`super::title_memory_changed`).
//!
//! The records never make a hole of a block: they say that one is a hole
//! still. A block is first noted a hole by Windows' word ([`found_asked`]: the
//! guards ask what is at a block before they guard anything there, and
//! Windows says nothing is committed; [`found`]: a view could not be guarded,
//! and Windows says so then), as before;
//! the questions asked at every asking after that, whether the hole has been
//! filled ([`end`]), are the ones the records answer. So a block is taken for
//! a hole only if Windows said it was one, and the runtime has told of no
//! view, no freeing and no other allocation there since.
//!
//! Nor do they make a view of one. The guards make read-only only what
//! Windows said was a view; what the records add is how long that answer may
//! be kept: while the runtime shows its view there ([`Records::shows`],
//! `super::kept_view`). Of all other memory Windows is asked each time.
//!
//! What the records do not know, and Windows is asked about as before: all
//! memory that is not such a reservation (the title's own `VirtualAlloc`,
//! which the runtime does not see; ordinary reservations, which an ordinary
//! commit fills with no word to anyone; memory of 2 MiB pages; file
//! mappings, our own views, free memory), and what was such a reservation
//! and was told of in another way since. What they cannot know: a hole
//! filled, or a view taken away, by a call that is not the runtime's (the
//! title mapping into a placeholder itself, which no title we know does).
//! For that one answer in [`EVERY`] is compared with Windows', as is every
//! answer of Windows' about a hole first found, and where they differ the
//! blocks are the records' no more; [`super::look_at_holes`] asks Windows
//! always.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::{BLOCK, Bits, in_word};

/// How holes are found (`VICTORY_HOLE_RECORDS`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Mode {
    /// `0`: Windows is asked, as before.
    Asked,
    /// The records where they know, Windows where they do not.
    Records,
    /// `check`: both; Windows' answer is the one that counts.
    Check,
}

pub(super) fn mode() -> Mode {
    static MODE: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| mode_of(std::env::var("VICTORY_HOLE_RECORDS").ok().as_deref()))
}

fn mode_of(setting: Option<&str>) -> Mode {
    match setting.map(str::trim) {
        Some("0") => Mode::Asked,
        Some("check") => Mode::Check,
        _ => Mode::Records,
    }
}

/// Of the answers the records give, one in so many is compared with
/// Windows' (a prime: no divisor of what a frame asks). A comparison is as
/// many questions to Windows as the hole has regions, up to [`WALK`]: a
/// hundredth or so of the questions the records save.
const EVERY: u64 = 251;
/// Regions of Windows' such a comparison walks at most (with `check`, and
/// for a hole first found: the one the caller asks about, every time).
const WALK: usize = 8;

/// Since the start: questions about holes the records answered; questions
/// put to Windows for want of a record; questions put to Windows about a hole
/// first found; answers of the records compared with Windows'; of those, the
/// ones that differed; and the ones that differed while a mapping changed,
/// or the records did, which say nothing.
static COUNTS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
const KNOWN: usize = 0;
const ASKED: usize = 1;
const COMPARED: usize = 2;
const DIFFERED: usize = 3;
const UNSETTLED: usize = 4;
const FOUND: usize = 5;

/// [`COUNTS`], for the log.
pub(crate) fn line() -> String {
    let [known, asked, found, compared, differed, unsettled] =
        [KNOWN, ASKED, FOUND, COMPARED, DIFFERED, UNSETTLED].map(|i| COUNTS[i].load(Relaxed));
    format!(
        "holes in the title's view memory, since the start: {known} questions answered from the runtime's own records, {asked} \
         put to Windows for want of a record and {found} about holes first found, {compared} answers compared with Windows', \
         {differed} disagreements ({unsettled} more while a mapping changed, not counted) (VICTORY_HOLE_RECORDS: {})",
        match mode() {
            Mode::Asked => "0, Windows is asked every time",
            Mode::Records => "on; 0 asks Windows every time, check asks both",
            Mode::Check => "check, both are asked and Windows' answer is used",
        }
    )
}

/// What the runtime reserved for views, and where it shows one.
pub(super) struct Records {
    reserved: Bits,
    shown: Bits,
}

pub(super) static RECORDS: Records = Records::new();

impl Records {
    pub(super) const fn new() -> Records {
        Records { reserved: Bits::new(), shown: Bits::new() }
    }

    /// The runtime reserved `[at, at + len)` as a placeholder, just now:
    /// nothing is mapped in it. Its whole blocks are the records' to answer
    /// for.
    pub(super) fn reserve(&self, at: usize, len: usize) {
        let (first, last) = (at.div_ceil(BLOCK), at.saturating_add(len) / BLOCK);
        if first >= last {
            return;
        }
        // (What an earlier reservation at these addresses left behind.)
        self.shown.clear_all(first, last);
        self.reserved.set_all(first, last);
    }

    /// `[at, at + len)` is, or is about to be, something the records do not
    /// follow: Windows is asked about every block it touches from now on.
    pub(super) fn forget(&self, at: usize, len: usize) {
        if len != 0 {
            self.forget_blocks(at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
        }
    }

    pub(super) fn forget_blocks(&self, first: usize, last: usize) {
        self.reserved.clear_all(first, last);
    }

    /// Whether a block of `[at, at + len)` is the records' to answer for.
    pub(super) fn any(&self, at: usize, len: usize) -> bool {
        len != 0 && self.reserved.any(at / BLOCK, at.saturating_add(len).div_ceil(BLOCK))
    }

    /// The blocks `[at, at + len)` touches; and what is left of the records'
    /// for a view that does not begin or end with a block: such a block may
    /// hold another view, or a hole, beside it.
    fn touched(&self, at: usize, len: usize) -> (usize, usize) {
        let end = at.saturating_add(len);
        let (first, last) = (at / BLOCK, end.div_ceil(BLOCK));
        if !at.is_multiple_of(BLOCK) {
            self.reserved.clear(first);
        }
        if !end.is_multiple_of(BLOCK) {
            self.reserved.clear(last - 1);
        }
        (first, last)
    }

    /// The runtime mapped a view over `[at, at + len)`, and committed it.
    pub(super) fn show(&self, at: usize, len: usize) {
        if len == 0 {
            return;
        }
        let (first, last) = self.touched(at, len);
        self.shown.set_all(first, last);
    }

    /// The runtime is about to unmap what `[at, at + len)` shows.
    pub(super) fn hide(&self, at: usize, len: usize) {
        if len == 0 {
            return;
        }
        let (first, last) = self.touched(at, len);
        self.shown.clear_all(first, last);
    }

    /// Whether the runtime shows a view over `block` now, in a reservation
    /// of its own: mapped before it told of it, and told of before it is
    /// unmapped. For `super::kept_view`, which also wants Windows' word.
    pub(super) fn shows(&self, block: usize) -> bool {
        let bit = 1u64 << (block % 64);
        self.reserved.word(block / 64) & self.shown.word(block / 64) & bit != 0
    }

    /// What [`super::uncommitted_end`] asks of `block`, if the block is the
    /// records': `block` itself if a view is mapped over it, else the end of
    /// the holes from it on, at most `limit` (the caller cares for no more).
    /// `None`: not the records' to say.
    ///
    /// Each block is looked at by itself, and the answer is what each was at
    /// some moment of the call: as much as an answer of Windows' is worth by
    /// the time its asker reads it.
    pub(super) fn end(&self, block: usize, limit: usize) -> Option<usize> {
        let limit = limit.max(block.saturating_add(1));
        let mut word = block / 64;
        loop {
            let (reserved, shown) = (self.reserved.word(word), self.shown.word(word));
            if word == block / 64 {
                let bit = 1u64 << (block % 64);
                if reserved & bit == 0 {
                    return None;
                }
                if shown & bit != 0 {
                    return Some(block);
                }
            }
            // The first block from `block` on that is no hole of the records'
            // (past `block`: that one is, in these same words).
            let other = !(reserved & !shown) & in_word(word * 64, block, limit);
            if other != 0 {
                return Some(word * 64 + other.trailing_zeros() as usize);
            }
            word += 1;
            if word * 64 >= limit {
                return Some(limit);
            }
        }
    }
}

/// [`super::uncommitted_end`] as `mode` has it: for a block noted a hole
/// before, whether it is one still.
pub(super) fn end(mode: Mode, block: usize, limit: usize) -> usize {
    if mode == Mode::Asked {
        return super::uncommitted_end_asked(block);
    }
    let Some(known) = RECORDS.end(block, limit) else {
        COUNTS[ASKED].fetch_add(1, Relaxed);
        return super::uncommitted_end_asked(block);
    };
    let n = COUNTS[KNOWN].fetch_add(1, Relaxed);
    answer(mode, block, known, limit, n.is_multiple_of(EVERY))
}

/// [`super::uncommitted_end_found`] as `mode` has it: Windows' answer about
/// its region from `block` on, for a block that is about to be noted a hole
/// for the first time. Where the records answer for the block the two are
/// compared, and blocks they differ about are the records' no more (with
/// `check` they stay, and it is said).
pub(super) fn found(mode: Mode, block: usize, limit: usize) -> usize {
    found_given(mode, block, limit, None)
}

/// [`found`] for a caller that has just asked Windows about `block` itself
/// and was told `asked` (what [`super::uncommitted_end_asked`] says of it:
/// `super::protect_here`, which asks what is at a block before it guards
/// anything there). That question is not put again: where the records do not
/// answer for the block `asked` is the answer, and where they do it is the
/// first of the two answers of Windows' they are compared with; the second,
/// after which alone a difference counts, is asked anew ([`differs`]).
pub(super) fn found_asked(mode: Mode, block: usize, limit: usize, asked: usize) -> usize {
    found_given(mode, block, limit, Some(asked))
}

fn found_given(mode: Mode, block: usize, limit: usize, given: Option<usize>) -> usize {
    let ask = || given.unwrap_or_else(|| super::uncommitted_end_asked(block));
    if mode == Mode::Asked {
        return ask();
    }
    COUNTS[FOUND].fetch_add(1, Relaxed);
    let Some(known) = RECORDS.end(block, limit) else {
        return ask();
    };
    match differs_given(&RECORDS, block, known, limit, 1, given) {
        (Some(at), asked) if mode == Mode::Records => {
            RECORDS.forget_blocks(block, known.max(at + 1));
            asked
        }
        (_, asked) => asked,
    }
}

/// What is made of the records' answer `known` about `block`; `compare`:
/// this is one of those compared with Windows'.
fn answer(mode: Mode, block: usize, known: usize, limit: usize, compare: bool) -> usize {
    match mode {
        Mode::Asked => super::uncommitted_end_asked(block),
        // Windows' region from the block on, no further: the caller asks
        // about the next as it always did, and that is compared then.
        Mode::Check => differs(&RECORDS, block, known, limit, 1).1,
        Mode::Records if compare => match differs(&RECORDS, block, known, limit, WALK) {
            (Some(at), asked) => {
                // Filled, or emptied, with nobody telling: not the records'
                // any more, the holes they had and the block they were wrong
                // about.
                RECORDS.forget_blocks(block, known.max(at + 1));
                asked
            }
            (None, _) => known,
        },
        Mode::Records => known,
    }
}

/// For the runtime's tests, and for whoever doubts the records: of the
/// blocks of `[at, at + len)`, how many the records answer for, and the
/// first of them at which Windows says otherwise (as an address), if any
/// does with no mapping changing meanwhile.
pub fn title_memory_holes_compared(at: usize, len: usize) -> (usize, Option<usize>) {
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    let (mut known, mut differing) = (0, None);
    for block in first..last {
        if let Some(end) = RECORDS.end(block, last) {
            known += 1;
            if differing.is_none() {
                differing = differs(&RECORDS, block, end, last, usize::MAX).0.map(|block| block * BLOCK);
            }
        }
    }
    (known, differing)
}

/// Whether Windows says otherwise of the blocks from `block` on than the
/// records, which said `known` ([`Records::end`]): the first block they
/// differ at, with no mapping changing meanwhile and the records saying the
/// same after as before, twice over. Counted, and said at the 1st, 2nd, 4th..
/// time. And what Windows said of `block` itself ([`super::uncommitted_end_asked`]),
/// last. At most `most` of Windows' regions are walked for the holes.
pub(super) fn differs(records: &Records, block: usize, known: usize, limit: usize, most: usize) -> (Option<usize>, usize) {
    differs_given(records, block, known, limit, most, None)
}

/// [`differs`]; `given`: what Windows said of `block` itself a moment ago,
/// to the caller, which stands for the first question about it. Only for
/// the first of the two rounds: an answer taken before this began says
/// nothing of whether a mapping changed meanwhile, and none is needed of it.
/// If it agrees with the records, they were right a moment ago, which is all
/// an agreement ever says; if it does not, the second round asks Windows
/// anew, with the changes counted around the asking as before, and only what
/// that one finds, standing still, is a difference.
fn differs_given(
    records: &Records,
    block: usize,
    known: usize,
    limit: usize,
    most: usize,
    given: Option<usize>,
) -> (Option<usize>, usize) {
    COUNTS[COMPARED].fetch_add(1, Relaxed);
    let of_block = std::cell::Cell::new(block);
    let given = std::cell::Cell::new(given);
    let ask = |at: usize| {
        // (The first question is always the one about `block`.)
        let end = match given.take() {
            Some(end) if at == block => end,
            _ => super::uncommitted_end_asked(at),
        };
        if at == block {
            of_block.set(end);
        }
        end
    };
    let mut found = None;
    for again in [false, true] {
        if again {
            // (The runtime tells of memory it allocated a moment after it
            // has it: `super::title_memory_allocated`.)
            std::thread::yield_now();
        }
        let before = super::changes();
        // Where the records' holes end with a view, Windows has one too.
        let view = known > block && known < limit && records.end(known, limit) == Some(known);
        let Some(at) = differs_from(block, known, view, most.max(1), &ask) else {
            return (None, of_block.get());
        };
        // (In this order: the records still say it, and no change of mapping
        // began since before Windows was asked, so both stood still all the
        // while. The runtime changes a mapping and tells of it within one
        // change: `super::title_memory_remapping`.)
        if records.end(block, limit) != Some(known) || !before.is_multiple_of(2) || super::changes() != before {
            COUNTS[UNSETTLED].fetch_add(1, Relaxed);
            return (None, of_block.get());
        }
        found = Some(at);
    }
    let Some(at) = found else {
        return (None, of_block.get());
    };
    let n = COUNTS[DIFFERED].fetch_add(1, Relaxed) + 1;
    if n.is_power_of_two() {
        tracing::warn!(
            n,
            from = format_args!("{:#x}", block * BLOCK),
            records = format_args!("{:#x}", known * BLOCK),
            differs_at = format_args!("{:#x}", at * BLOCK),
            windows = format_args!("{:#x}", super::uncommitted_end_asked(at) * BLOCK),
            "the runtime's records of the title's reservations and Windows differ about a hole: memory there was mapped, \
             allocated or freed with nobody telling (Windows is asked about these blocks from now on; \
             VICTORY_HOLE_RECORDS=0 asks it every time)"
        );
    }
    (Some(at), of_block.get())
}

/// The first block at which `ask` (Windows: the end of the uncommitted
/// blocks from a block on, the block itself if it is not one) says otherwise
/// than the records, which said `known` of `block`; `view`: and that a view
/// is at `known`. At most `most` questions for the holes; the first question
/// is always about `block`.
fn differs_from(block: usize, known: usize, view: bool, most: usize, ask: impl Fn(usize) -> usize) -> Option<usize> {
    if known <= block {
        // A view, the records say.
        return (ask(block) != block).then_some(block);
    }
    let mut at = block;
    let mut asked = 0;
    while at < known {
        if asked == most {
            return None;
        }
        asked += 1;
        let end = ask(at);
        if end <= at {
            // Something is there.
            return Some(at);
        }
        at = end;
    }
    // (A hole that goes on past what the records answered for is not theirs
    // to know; one that goes on where they have a view is.)
    (view && (at > known || ask(known) != known)).then_some(known)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records() -> Box<Records> {
        Box::new(Records::new())
    }

    #[test]
    fn the_switch_is_on_unless_0_or_check() {
        assert_eq!(mode_of(None), Mode::Records);
        assert_eq!(mode_of(Some("1")), Mode::Records);
        assert_eq!(mode_of(Some("")), Mode::Records);
        assert_eq!(mode_of(Some("0")), Mode::Asked);
        assert_eq!(mode_of(Some(" 0 ")), Mode::Asked);
        assert_eq!(mode_of(Some("check")), Mode::Check);
    }

    /// Nothing is known of memory the runtime did not reserve; in what it
    /// did, a hole ends at the next view, at the reservation's end, or where
    /// the caller stops caring, and a view is no hole.
    #[test]
    fn holes_end_at_the_next_view_or_with_the_reservation() {
        let r = records();
        let base = 0x12_3400_0000 / BLOCK;
        assert_eq!(r.end(base, base + 100), None);
        r.reserve(base * BLOCK, 100 * BLOCK);
        assert_eq!(r.end(base - 1, base + 100), None);
        assert_eq!(r.end(base + 100, base + 200), None);
        // All of it a hole: to its end, or the limit.
        assert_eq!(r.end(base, base + 200), Some(base + 100));
        assert_eq!(r.end(base + 99, base + 200), Some(base + 100));
        assert_eq!(r.end(base, base + 7), Some(base + 7));
        assert_eq!(r.end(base + 3, base + 3), Some(base + 4), "a limit that is none");
        assert_eq!(r.end(base + 3, 0), Some(base + 4));
        // Views of one block and of several.
        r.show((base + 10) * BLOCK, BLOCK);
        r.show((base + 70) * BLOCK, 5 * BLOCK);
        assert_eq!(r.end(base, base + 100), Some(base + 10));
        assert_eq!(r.end(base + 9, base + 100), Some(base + 10));
        assert_eq!(r.end(base + 10, base + 100), Some(base + 10), "a view");
        assert_eq!(r.end(base + 11, base + 100), Some(base + 70), "over the end of a word of bits");
        for block in base + 70..base + 75 {
            assert_eq!(r.end(block, base + 100), Some(block));
        }
        assert_eq!(r.end(base + 75, base + 100), Some(base + 100));
        // Unmapped: a hole again, joined with those around it.
        r.hide((base + 10) * BLOCK, BLOCK);
        assert_eq!(r.end(base, base + 100), Some(base + 70));
        r.hide((base + 72) * BLOCK, BLOCK);
        assert_eq!(r.end(base + 72, base + 100), Some(base + 73));
        assert_eq!(r.end(base + 71, base + 100), Some(base + 71));
    }

    /// Only whole blocks of a reservation are known, and a view that does
    /// not begin and end with a block takes the blocks at its ends out of
    /// what is known: another view or a hole may share them.
    #[test]
    fn blocks_held_in_part_are_not_the_records() {
        let r = records();
        let base = 0x7_0000_0000usize;
        r.reserve(base + 4096, 10 * BLOCK);
        let first = base / BLOCK;
        assert_eq!(r.end(first, first + 20), None, "its first block is not all the reservation's");
        assert_eq!(r.end(first + 1, first + 20), Some(first + 10));
        assert_eq!(r.end(first + 10, first + 20), None, "nor its last");
        r.reserve(base + 4096, BLOCK - 4096);
        r.reserve(base + 4096, 0);
        assert_eq!(r.end(first, first + 20), None, "less than a block");
        // A view from the middle of block 3 to the middle of block 5.
        r.show(base + 3 * BLOCK + 4096, 2 * BLOCK);
        assert_eq!(r.end(first + 1, first + 20), Some(first + 3));
        assert_eq!(r.end(first + 3, first + 20), None);
        assert_eq!(r.end(first + 4, first + 20), Some(first + 4), "the block it holds whole is a view");
        assert_eq!(r.end(first + 5, first + 20), None);
        assert_eq!(r.end(first + 6, first + 20), Some(first + 10));
        // Unmapped: the blocks at its ends stay unknown.
        r.hide(base + 3 * BLOCK + 4096, 2 * BLOCK);
        assert_eq!(r.end(first + 3, first + 20), None);
        assert_eq!(r.end(first + 4, first + 20), Some(first + 5));
        assert_eq!(r.end(first + 5, first + 20), None);
        // Nothing at all changes for no bytes.
        r.show(base + 7 * BLOCK, 0);
        r.hide(base + 7 * BLOCK + 5, 0);
        r.forget(base + 7 * BLOCK, 0);
        assert_eq!(r.end(first + 6, first + 20), Some(first + 10));
    }

    /// Freed, decommitted or allocated in another way: Windows is asked about
    /// every block that touches, and about no other; a view the runtime maps
    /// in what is left ends the hole before it as ever.
    #[test]
    fn what_is_told_of_in_another_way_is_forgotten() {
        let r = records();
        let base = 0x3_0000_0000usize;
        let first = base / BLOCK;
        r.reserve(base, 64 * BLOCK);
        assert!(r.any(base, 1) && r.any(base + 63 * BLOCK, BLOCK) && !r.any(base + 64 * BLOCK, BLOCK) && !r.any(base, 0));
        // A piece freed, from the middle of a block to the middle of another.
        r.forget(base + 20 * BLOCK + 1, 2 * BLOCK);
        assert_eq!(r.end(first, first + 64), Some(first + 20));
        for block in first + 20..first + 23 {
            assert_eq!(r.end(block, first + 64), None);
        }
        assert_eq!(r.end(first + 23, first + 64), Some(first + 64));
        assert!(!r.any(base + 20 * BLOCK, 3 * BLOCK) && r.any(base + 19 * BLOCK, 2 * BLOCK));
        r.show(base + 30 * BLOCK, BLOCK);
        assert_eq!(r.end(first + 23, first + 64), Some(first + 30));
        // The runtime tells of a view where the records know nothing: a view
        // there makes nothing known.
        r.show(base + 21 * BLOCK, BLOCK);
        assert_eq!(r.end(first + 21, first + 64), None);
        r.hide(base + 21 * BLOCK, BLOCK);
        assert_eq!(r.end(first + 21, first + 64), None);
        // Reserved anew (the address given out again): known again, and what
        // was shown there before is not.
        r.show(base + 100 * BLOCK, 4 * BLOCK);
        r.reserve(base + 98 * BLOCK, 10 * BLOCK);
        assert_eq!(r.end(first + 98, first + 200), Some(first + 108));
        assert_eq!(r.end(first + 101, first + 200), Some(first + 108));
    }

    /// The sets are in pieces of 4 GiB made when first needed: a reservation
    /// over several of them, a hole that runs from one into the next, and an
    /// address past all of them.
    #[test]
    fn a_reservation_of_many_gib_is_one() {
        let r = records();
        const PIECE: usize = (4usize << 30) / BLOCK;
        let first = 5 * PIECE - 3;
        r.reserve(first * BLOCK, (2 * PIECE + 6) * BLOCK);
        assert_eq!(r.end(first, usize::MAX), Some(first + 2 * PIECE + 6));
        assert_eq!(r.end(first + PIECE, first + PIECE + 5), Some(first + PIECE + 5));
        r.show((6 * PIECE) * BLOCK, BLOCK);
        assert_eq!(r.end(first, usize::MAX), Some(6 * PIECE));
        assert_eq!(r.end(6 * PIECE + 1, usize::MAX), Some(first + 2 * PIECE + 6));
        r.forget(6 * PIECE * BLOCK - BLOCK, 2 * BLOCK);
        assert_eq!(r.end(first, usize::MAX), Some(6 * PIECE - 1));
        assert_eq!(r.end(6 * PIECE, usize::MAX), None);
        // Past the address space: nothing is noted, nothing known.
        let past = (1usize << 47) / BLOCK;
        r.reserve(past * BLOCK, 4 * BLOCK);
        r.show(past * BLOCK, BLOCK);
        assert_eq!(r.end(past, past + 4), None);
        assert_eq!(r.end(usize::MAX / BLOCK, usize::MAX), None);
        r.forget(usize::MAX - BLOCK, BLOCK);
        r.hide(usize::MAX - BLOCK, BLOCK);
    }

    /// The comparison with Windows: its regions end where they like (a
    /// placeholder cut in pieces is as many), and what the records call a
    /// hole must be uncommitted in all of them, what they call a view not.
    #[test]
    fn the_records_differ_from_windows_where_something_is_or_is_not_there() {
        // Windows: holes 100..103, 103..110, a view at 110, a hole 111..120,
        // something at 120.
        let windows = |block: usize| match block {
            100..=102 => 103,
            103..=109 => 110,
            111..=119 => 120,
            _ => block,
        };
        // The same.
        assert_eq!(differs_from(100, 110, true, usize::MAX, windows), None);
        assert_eq!(differs_from(105, 110, true, usize::MAX, windows), None);
        assert_eq!(differs_from(110, 110, false, usize::MAX, windows), None);
        assert_eq!(differs_from(111, 120, true, usize::MAX, windows), None);
        // Less than Windows knows (the caller's limit, the reservation's
        // end): no difference.
        assert_eq!(differs_from(100, 102, false, usize::MAX, windows), None);
        assert_eq!(differs_from(111, 115, false, usize::MAX, windows), None);
        // A hole of the records' with something in it.
        assert_eq!(differs_from(100, 115, false, usize::MAX, windows), Some(110));
        assert_eq!(differs_from(110, 115, false, usize::MAX, windows), Some(110));
        assert_eq!(differs_from(119, 125, true, usize::MAX, windows), Some(120));
        // A view of the records' where Windows has none: the block itself,
        // and the one their holes end with.
        assert_eq!(differs_from(105, 105, false, usize::MAX, windows), Some(105));
        assert_eq!(differs_from(100, 105, true, usize::MAX, windows), Some(105));
        assert_eq!(differs_from(111, 115, true, usize::MAX, windows), Some(115));
        // No more questions than allowed: what was not reached is not judged.
        assert_eq!(differs_from(100, 115, false, 2, windows), None);
        assert_eq!(differs_from(100, 115, false, 3, windows), Some(110));
        let asked = std::cell::Cell::new(0);
        let counted = |block: usize| {
            asked.set(asked.get() + 1);
            windows(block)
        };
        assert_eq!(differs_from(100, 110, false, 1, counted), None);
        assert_eq!(asked.get(), 1);
    }

    /// Readers and the runtime's telling at once: whatever a reader is told,
    /// each block of it was so at some moment. Here a view comes and goes at
    /// one block of a reservation; the blocks around it never change.
    #[test]
    fn an_answer_is_true_of_each_block_while_others_change() {
        let r = records();
        let base = 0x9_0000_0000usize;
        let first = base / BLOCK;
        r.reserve(base, 200 * BLOCK);
        r.show(base + 150 * BLOCK, BLOCK);
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            for _ in 0..3 {
                s.spawn(|| {
                    while !stop.load(Relaxed) {
                        // From before the block that changes: a hole to it,
                        // or to the view that stays.
                        let end = r.end(first + 10, first + 200).expect("reserved all the time");
                        assert!(end == first + 100 || end == first + 150, "{}", end - first);
                        // The block itself: a view, or a hole to the next.
                        let end = r.end(first + 100, first + 200);
                        assert!(end == Some(first + 100) || end == Some(first + 150), "{end:?}");
                        // After it: never anything but the view that stays.
                        assert_eq!(r.end(first + 101, first + 200), Some(first + 150));
                    }
                });
            }
            for _ in 0..20_000 {
                r.show(base + 100 * BLOCK, BLOCK);
                r.hide(base + 100 * BLOCK, BLOCK);
            }
            stop.store(true, Relaxed);
        });
    }
}

/// With memory of the host's: placeholders, sections and views as the runtime
/// makes them, told of as it tells.
#[cfg(test)]
mod host_tests {
    use super::*;
    use crate::watch::{
        title_memory_allocated, title_memory_freed, title_memory_remapped, title_memory_remapping, title_memory_reserved,
        title_memory_unmapping, uncommitted_end, uncommitted_end_asked, writes,
    };
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::System::Memory::*;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    /// A placeholder of `blocks` blocks, told of as the runtime tells of the
    /// title's, and a section as big (both stay): the first block's number,
    /// and the section.
    fn reservation(blocks: usize) -> (usize, usize) {
        // SAFETY: a fresh placeholder and a pagefile section of our own.
        unsafe {
            let at = VirtualAlloc2(
                GetCurrentProcess(),
                null(),
                blocks * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                null_mut(),
                0,
            ) as usize;
            assert_ne!(at, 0);
            let section = CreateFileMappingW(-1isize as _, null(), PAGE_READWRITE, 0, (blocks * BLOCK) as u32, null());
            assert!(!section.is_null());
            title_memory_reserved(at, blocks * BLOCK);
            (at / BLOCK, section as usize)
        }
    }

    /// A view of `n` blocks at block `block`, in place of a placeholder cut
    /// to fit, of which nobody is told.
    fn map_untold(section: usize, first: usize, block: usize, n: usize) {
        let to = (first + block) * BLOCK;
        // SAFETY: our placeholder and our section.
        unsafe {
            VirtualFree(to as *mut _, n * BLOCK, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
            let view = MapViewOfFile3(
                section as _,
                GetCurrentProcess(),
                to as *const _,
                (block * BLOCK) as u64,
                n * BLOCK,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                null_mut(),
                0,
            );
            assert_eq!(view.Value as usize, to, "{}", std::io::Error::last_os_error());
        }
    }

    /// The view at `block` becomes a placeholder again, of which nobody is
    /// told.
    fn unmap_untold(first: usize, block: usize) {
        let view = MEMORY_MAPPED_VIEW_ADDRESS { Value: ((first + block) * BLOCK) as *mut _ };
        // SAFETY: a view of ours.
        assert_ne!(unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) }, 0);
    }

    /// As the runtime maps (`victory-hle` xmem, `map`): the view, then the
    /// telling.
    fn map(section: usize, first: usize, block: usize, n: usize) {
        map_untold(section, first, block, n);
        title_memory_remapped((first + block) * BLOCK, n * BLOCK);
    }

    /// As the runtime unmaps (xmem, `unmap`): the telling, then the view.
    fn unmap(first: usize, block: usize, n: usize) {
        title_memory_unmapping((first + block) * BLOCK, n * BLOCK);
        unmap_untold(first, block);
    }

    /// What `compare` finds, once it finds something: another test's change
    /// of a mapping meanwhile leaves a comparison unsettled, with nothing
    /// found.
    fn settled<T>(mut compare: impl FnMut() -> Option<T>) -> Option<T> {
        for _ in 0..100_000 {
            if let Some(found) = compare() {
                return Some(found);
            }
            std::thread::yield_now();
        }
        None
    }

    /// Every block of `[first, first + blocks)` the records answer for is
    /// what Windows says it is, a hole or not, and its holes end where
    /// Windows' regions say; how many blocks that was.
    fn as_windows_says(first: usize, blocks: usize) -> usize {
        let limit = first + blocks;
        let mut known = 0;
        for block in first..limit {
            let Some(end) = RECORDS.end(block, limit) else {
                continue;
            };
            known += 1;
            let hole = uncommitted_end_asked(block) > block;
            assert_eq!(end > block, hole, "block {}: the records say {}", block - first, end - first);
            assert_eq!(differs(&RECORDS, block, end, limit, usize::MAX).0, None, "block {}", block - first);
        }
        known
    }

    /// Through all the runtime does with a reservation, and what it is told
    /// the title does, the records say what Windows says, block for block;
    /// and of what was freed or allocated in another way they say nothing.
    #[test]
    fn the_records_say_of_a_reservation_what_windows_says() {
        const BLOCKS: usize = 40;
        let (first, section) = reservation(BLOCKS);
        let limit = first + BLOCKS;
        let end = |block: usize| RECORDS.end(first + block, limit).map(|end| end - first);
        assert_eq!(end(0), Some(BLOCKS));
        assert_eq!(as_windows_says(first, BLOCKS), BLOCKS);
        // Views: of one block, of several, at the end.
        map(section, first, 3, 1);
        map(section, first, 10, 4);
        map(section, first, 39, 1);
        assert_eq!([end(0), end(3), end(4), end(10), end(13), end(14), end(39)].map(Option::unwrap), [3, 3, 10, 10, 13, 39, 39]);
        assert_eq!(as_windows_says(first, BLOCKS), BLOCKS);
        // Unmapped, and a block of the placeholder it leaves mapped anew.
        unmap(first, 10, 4);
        assert_eq!(end(4), Some(39));
        assert_eq!(as_windows_says(first, BLOCKS), BLOCKS);
        map(section, first, 11, 1);
        assert_eq!([end(4), end(11), end(12)], [Some(11), Some(11), Some(39)]);
        assert_eq!(as_windows_says(first, BLOCKS), BLOCKS);
        // The same page at the same place again, as a title maps over what
        // it has: unmapped and mapped.
        unmap(first, 11, 1);
        map(section, first, 11, 1);
        assert_eq!(as_windows_says(first, BLOCKS), BLOCKS);
        // The title cuts a block out of the placeholder and frees it (the
        // runtime tells before the host is asked): Windows is asked about it
        // from then on, whatever comes to be there; the rest is as it was.
        let cut = (first + 20) * BLOCK;
        // SAFETY: our placeholder.
        unsafe {
            assert_ne!(VirtualFree(cut as *mut _, BLOCK, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER), 0);
            title_memory_freed(cut, 0);
            assert_ne!(VirtualFree(cut as *mut _, 0, MEM_RELEASE), 0);
        }
        assert_eq!([end(12), end(19), end(20), end(21)], [Some(20), Some(20), None, Some(39)]);
        assert_eq!(as_windows_says(first, BLOCKS), BLOCKS - 1);
        // A range the title decommits or frees by its length.
        title_memory_freed((first + 30) * BLOCK + 1, BLOCK);
        assert_eq!([end(29), end(30), end(31), end(32)], [Some(30), None, None, Some(39)]);
        // Private memory in place of a block of the placeholder, as the
        // runtime's allocator puts it there and then tells of.
        let private = (first + 25) * BLOCK;
        // SAFETY: our placeholder.
        unsafe {
            assert_ne!(VirtualFree(private as *mut _, BLOCK, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER), 0);
            let got = VirtualAlloc2(
                GetCurrentProcess(),
                private as *const _,
                BLOCK,
                MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                null_mut(),
                0,
            );
            assert_eq!(got as usize, private);
        }
        title_memory_allocated(private, BLOCK);
        assert_eq!([end(21), end(25), end(26)], [Some(25), None, Some(30)]);
        assert_eq!(uncommitted_end(first + 25, limit), first + 25, "Windows says it is there");
        assert_eq!(as_windows_says(first, BLOCKS), BLOCKS - 4);
        assert_eq!(title_memory_holes_compared(first * BLOCK, BLOCKS * BLOCK), (BLOCKS - 4, None));
    }

    /// What the records cannot know: a view mapped into a hole, and one taken
    /// away, by a call that is not the runtime's. An answer that is compared
    /// with Windows' finds it, is Windows', and has Windows asked about those
    /// blocks from then on; with `check` Windows' answer is the one given, and
    /// the records stay as they are.
    #[test]
    fn what_changes_with_nobody_telling_is_found_and_asked_about_from_then_on() {
        const BLOCKS: usize = 16;
        let (first, section) = reservation(BLOCKS);
        let limit = first + BLOCKS;
        map(section, first, 2, 1);
        // A view at block 6 the records know nothing of.
        map_untold(section, first, 6, 1);
        let known = RECORDS.end(first + 3, limit).unwrap();
        assert_eq!(known, limit, "a hole to the end, as far as the records go");
        assert_eq!(uncommitted_end_asked(first + 3), first + 6);
        assert_eq!(settled(|| differs(&RECORDS, first + 3, known, limit, usize::MAX).0), Some(first + 6));
        assert_eq!(title_memory_holes_compared((first + 3) * BLOCK, 3 * BLOCK), (3, None), "as far as it was asked");
        assert_eq!(settled(|| title_memory_holes_compared((first + 3) * BLOCK, 4 * BLOCK).1), Some((first + 6) * BLOCK));
        // `check`: Windows' answer for its region, each time; nothing forgotten.
        assert_eq!(answer(Mode::Check, first + 3, known, limit, false), first + 6);
        assert_eq!(answer(Mode::Check, first + 6, RECORDS.end(first + 6, limit).unwrap(), limit, false), first + 6);
        assert_eq!(RECORDS.end(first + 3, limit), Some(limit));
        // Not compared: the records' answer, as it is.
        assert_eq!(answer(Mode::Records, first + 3, known, limit, false), limit);
        // Compared: Windows' answer, and the blocks are the records' no more.
        let compared = || Some(answer(Mode::Records, first + 3, known, limit, true)).filter(|&end| end != known);
        assert_eq!(settled(compared), Some(first + 6));
        assert!((first + 3..limit).all(|block| RECORDS.end(block, limit).is_none()));
        assert_eq!(end(Mode::Records, first + 3, limit), first + 6);
        assert_eq!(end(Mode::Records, first + 6, limit), first + 6);
        assert_eq!(end(Mode::Asked, first + 3, limit), first + 6);
        // The other way: the view at block 2 goes with nobody telling. The
        // records have the holes before it end with a view; Windows has none.
        unmap_untold(first, 2);
        assert_eq!(RECORDS.end(first, limit), Some(first + 2));
        assert_eq!(RECORDS.end(first + 2, limit), Some(first + 2));
        assert_eq!(settled(|| differs(&RECORDS, first + 2, first + 2, limit, usize::MAX).0), Some(first + 2));
        // (Windows' holes before it end where the records' did: what is
        // given is the same, and the records are forgotten.)
        let forgotten = || {
            let end = answer(Mode::Records, first, first + 2, limit, true);
            assert_eq!(end, first + 2);
            RECORDS.end(first, limit).is_none().then_some(())
        };
        assert_eq!(settled(forgotten), Some(()));
        assert!((first..limit).all(|block| RECORDS.end(block, limit).is_none()), "the block they were wrong about too");
        assert_eq!(title_memory_holes_compared(first * BLOCK, BLOCKS * BLOCK), (0, None));
    }

    /// The guards ask about holes as they did, and are told the same: a range
    /// of a reservation with views in it, written, mapped into and unmapped
    /// from, is told block for block as Windows had it told, and the
    /// questions about its holes go to the records.
    #[test]
    fn the_guards_hear_of_holes_from_the_records() {
        const BLOCKS: usize = 8;
        let (first, section) = reservation(BLOCKS);
        let (at, end) = (first * BLOCK, (first + BLOCKS) * BLOCK);
        map(section, first, 1, 1);
        map(section, first, 5, 1);
        let ask = |since: u64| -> (u64, Vec<usize>) {
            let (now, written) = writes(at, end, true).expect("a view");
            (now, written.into_iter().filter(|w| w.1 > since).map(|w| w.0 - first).collect())
        };
        let count = |which: usize| COUNTS[which].load(Relaxed);
        let (known, found) = (count(KNOWN), count(FOUND));
        let (mut last, all) = ask(0);
        assert_eq!(all, (0..BLOCKS).collect::<Vec<_>>());
        if mode() != Mode::Asked {
            assert!(count(FOUND) > found, "the holes were first found with no question to Windows");
        }
        let mut next = |expected: &[usize]| {
            let (now, written) = ask(last);
            assert_eq!(written, expected);
            last = now;
        };
        next(&[]);
        if mode() != Mode::Asked {
            assert!(count(KNOWN) > known, "the holes were asked about again with a question to Windows");
        }
        // Written; mapped into; written there; unmapped.
        // SAFETY: mapped read-write (a guarded block is opened by the fault).
        unsafe { (((first + 5) * BLOCK) as *mut u8).write_volatile(1) };
        next(&[5]);
        map(section, first, 2, 1);
        next(&[2]);
        // SAFETY: as above.
        unsafe { (((first + 2) * BLOCK) as *mut u8).write_volatile(1) };
        next(&[2]);
        unmap(first, 5, 1);
        next(&[5]);
        next(&[]);
        assert_eq!(title_memory_holes_compared(at, end - at), (BLOCKS, None));
        // Told of memory there, though there is none: Windows is asked about
        // it from now on, and says what the records said.
        title_memory_allocated(at, BLOCK);
        assert_eq!(RECORDS.end(first, first + BLOCKS), None);
        next(&[]);
        title_memory_unmapping(at, BLOCKS * BLOCK);
    }

    /// A view the records know nothing of, mapped by a call that is not the
    /// runtime's, is not taken for a hole when its range is first guarded:
    /// Windows is asked then, whatever the records say. Its blocks are
    /// guarded as any view's, and a write to them is told.
    #[test]
    fn a_hole_is_first_found_by_what_windows_says() {
        const BLOCKS: usize = 8;
        let (first, section) = reservation(BLOCKS);
        let limit = first + BLOCKS;
        let (at, end) = (first * BLOCK, limit * BLOCK);
        map(section, first, 1, 1);
        map_untold(section, first, 4, 2);
        assert_eq!(RECORDS.end(first + 2, limit), Some(limit), "holes to the end, as far as the records go");
        let ask = |since: u64| -> (u64, Vec<usize>) {
            let (now, written) = writes(at, end, true).expect("a view");
            (now, written.into_iter().filter(|w| w.1 > since).map(|w| w.0 - first).collect())
        };
        let (last, all) = ask(0);
        assert_eq!(all, (0..BLOCKS).collect::<Vec<_>>());
        // The question itself: Windows' region from the block on, in every
        // mode; and what the records had wrong is theirs no more, unless
        // they are only being checked. (Before the range is asked about
        // again: an answer of the records' that is compared then would find
        // the same.)
        for mode in [Mode::Asked, Mode::Check, Mode::Records] {
            assert_eq!(found(mode, first + 2, limit), first + 4, "{mode:?}");
            assert_eq!(found(mode, first, limit), first + 1, "{mode:?}");
            assert_eq!(found(mode, first + 1, limit), first + 1, "{mode:?}");
        }
        assert_eq!(found(Mode::Check, first + 4, limit), first + 4);
        assert_eq!(RECORDS.end(first + 4, limit), Some(limit));
        let forgotten = || {
            assert_eq!(found(Mode::Records, first + 4, limit), first + 4);
            RECORDS.end(first + 4, limit).is_none().then_some(())
        };
        assert_eq!(settled(forgotten), Some(()));
        assert_eq!(RECORDS.end(first + 2, limit), Some(first + 4), "the holes before it are the records' still");
        assert_eq!(RECORDS.end(first, limit), Some(first + 1));
        // The view was guarded when the range was, not left for holes: what
        // is written there is told, and nothing else is.
        // SAFETY: mapped read-write (a guarded block is opened by the fault).
        unsafe {
            (((first + 4) * BLOCK) as *mut u8).write_volatile(1);
            (((first + 5) * BLOCK + 100) as *mut u8).write_volatile(1);
        }
        let (last, written) = ask(last);
        assert_eq!(written, [4, 5]);
        assert_eq!(ask(last).1, [0usize; 0]);
        title_memory_unmapping(at, BLOCKS * BLOCK);
    }

    /// The caller that has Windows' answer about a block in hand
    /// ([`found_asked`]: the guards, which ask what is at a block before
    /// they guard anything) is told what [`found`] tells, and the records are
    /// compared as there. An answer that has grown old meanwhile makes no
    /// difference of itself: Windows is asked anew before one counts.
    #[test]
    fn a_hole_found_with_windows_answer_in_hand_is_compared_the_same() {
        const BLOCKS: usize = 8;
        let (first, section) = reservation(BLOCKS);
        let limit = first + BLOCKS;
        map(section, first, 1, 1);
        map_untold(section, first, 4, 1);
        let asked = uncommitted_end_asked;
        for mode in [Mode::Asked, Mode::Check, Mode::Records] {
            assert_eq!(found_asked(mode, first + 2, limit, asked(first + 2)), first + 4, "{mode:?}");
            assert_eq!(found_asked(mode, first, limit, asked(first)), first + 1, "{mode:?}");
            assert_eq!(found_asked(mode, first + 5, limit, asked(first + 5)), limit, "{mode:?}");
        }
        assert_eq!(RECORDS.end(first, limit), Some(first + 1), "nothing was forgotten");
        assert_eq!(RECORDS.end(first + 5, limit), Some(limit));
        // Windows' answer from before the runtime mapped and told of a view:
        // a hole, where the records have a view now, and so has Windows. No
        // difference (unless another test's mapping left it unsettled, when
        // the old answer is all there is), and the records stay.
        let hole = asked(first + 6);
        assert_eq!(hole, limit);
        map(section, first, 6, 1);
        let anew = || Some(found_asked(Mode::Records, first + 6, limit, hole)).filter(|&end| end != hole);
        assert_eq!(settled(anew), Some(first + 6), "asked anew");
        assert_eq!(RECORDS.end(first + 6, limit), Some(first + 6), "the records were right");
        // The view nobody told of, with a true answer in hand: what the
        // records had wrong is theirs no more.
        let forgotten = || {
            assert_eq!(found_asked(Mode::Records, first + 4, limit, asked(first + 4)), first + 4);
            RECORDS.end(first + 4, limit).is_none().then_some(())
        };
        assert_eq!(settled(forgotten), Some(()));
        assert_eq!(RECORDS.end(first + 2, limit), Some(first + 4), "the holes before it are the records' still");
        title_memory_unmapping(first * BLOCK, BLOCKS * BLOCK);
    }

    /// Threads that map and unmap as the runtime does, each its own blocks,
    /// while others compare every block's record with Windows: a view on its
    /// way in or out is not taken for a difference, and when all is still
    /// the two say the same.
    #[test]
    fn no_difference_is_found_while_the_runtime_maps() {
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        const BLOCKS: usize = 24;
        let (first, section) = reservation(BLOCKS);
        let limit = first + BLOCKS;
        let (stop, compared) = (AtomicBool::new(false), AtomicUsize::new(0));
        std::thread::scope(|s| {
            let mappers: Vec<_> = (0..3usize)
                .map(|t| {
                    s.spawn(move || {
                        for round in 0..150 {
                            let block = t * 8 + round % 7;
                            {
                                let _changing = title_memory_remapping();
                                map(section, first, block, 1);
                            }
                            assert_eq!(RECORDS.end(first + block, limit), Some(first + block));
                            {
                                let _changing = title_memory_remapping();
                                unmap(first, block, 1);
                            }
                        }
                    })
                })
                .collect();
            for _ in 0..3 {
                s.spawn(|| {
                    while !stop.load(Relaxed) {
                        for block in first..limit {
                            let known = RECORDS.end(block, limit).expect("the records' own all the time");
                            assert_eq!(differs(&RECORDS, block, known, limit, usize::MAX).0, None, "block {}", block - first);
                            // And as the guards ask.
                            let end = uncommitted_end(block, limit);
                            assert!(end >= block && end <= limit);
                            compared.fetch_add(1, Relaxed);
                        }
                    }
                });
            }
            let failed = mappers.into_iter().map(|m| m.join()).filter(Result::is_err).count();
            stop.store(true, Relaxed);
            assert_eq!(failed, 0);
        });
        assert!(compared.into_inner() > 0);
        assert_eq!(as_windows_says(first, BLOCKS), BLOCKS, "nothing was forgotten, and all of it is holes");
        assert_eq!(RECORDS.end(first, limit), Some(limit));
    }
}
