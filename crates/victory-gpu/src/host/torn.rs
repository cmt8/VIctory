//! Pieces of card copies that came down while a later flight was writing
//! them (`VICTORY_FETCH_CHECK`, with `VICTORY_RELAY_COPY`: both on unless
//! `=0`).
//!
//! On the copy queue a fetch runs beside later submissions (`relay_copy`),
//! so what comes down of a piece may be what it held in the middle of one:
//! a counter cleared and not counted up again, a list half filled. Such
//! bytes went into title memory and the copy's CPU side, to be put right by
//! that submission's own landing, which fetches what its compare flagged.
//! But the compare flags a piece only if it is not, at the submission's end,
//! what it was at the end of the one before. A piece cleared and filled
//! again with the same is not flagged, is not fetched, and the CPU's side
//! keeps the bytes from in between: until they go up with a refresh of
//! their 64 KiB, over the right ones.
//!
//! So the compare gives each piece's hash with its flag, and what came down
//! is hashed here too. The same hash: it is what the flight ended with. Not
//! the same: the piece is suspect (another flight was at it, or the title's
//! bytes went up to it meanwhile), and the next flight that may write it
//! fetches it whatever its compare says, until a fetch is found whole.
//!
//! Every suspect a flight was at has a flight to fetch it: only a flight
//! that declared a piece writes it, a flight that declared it has a range
//! over it when it lands, and that landing fetches a suspect whatever its
//! compare says. So the last flight at a piece fetches it with none behind
//! it, whole. A suspect no flight comes for is one the title's bytes went
//! up to behind the fetch: those bytes the merge left out
//! (`Shadow::land_card`), and the CPU's side holds them as the card does.
//! Nothing is fetched outside a landing's declared ranges, then, and
//! nothing could be merged there: the refresh writes the CPU's side
//! wherever no range is declared.
//!
//! A piece that came down under a range the card did not compare (no room
//! for its results) has no hash to be checked against: it is suspect too,
//! or a later flight at it whose compare flags nothing left it as it was.
//!
//! What it costs: pieces are fetched whole where a flight declared a part
//! of one, and 64 KiB of what came down are hashed (some 4 us) just before
//! they are merged, on the thread that merges them (`crate::land`).
//!
//! The frame-rate line tells what was found: how many pieces a frame were
//! not what their flight ended with, in how many of those title bytes had
//! been sent up behind the flight, how many were what a later flight ended
//! with (whole, as a fetch on the drawing queue brings them too:
//! `crate::land::Had`), how many suspects there are and how many of them no
//! flight has fetched for [`OLD`] landings, and which pieces of title memory
//! it was most often. What is left of the first number when the next two
//! are taken off is the most that came down from the middle of a flight.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64};

use super::*;

/// Whether fetches over the copy queue are checked: with
/// `VICTORY_RELAY_COPY` (on unless `=0`), on unless `VICTORY_FETCH_CHECK=0`.
/// Not only a count: a suspect is fetched again by the next flight that may
/// write it, whatever its compare says ([`Suspects::wanted`]), and without
/// that title memory may keep bytes from the middle of a flight (see above).
/// The hashes cost 3-4 ms for 96 MB beside the merge, spread over the
/// threads that land; the runs that made the copy queue the default all had
/// the check on.
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        if !relay_copy() {
            return false;
        }
        let on = said(std::env::var("VICTORY_FETCH_CHECK").ok().as_deref());
        if on {
            tracing::info!(
                "what comes down over the copy queue is hashed, and a piece caught while a later flight was at it is fetched again (VICTORY_FETCH_CHECK: on; =0 leaves it unchecked)"
            );
        } else {
            tracing::warn!(
                "what comes down over the copy queue is not checked (VICTORY_FETCH_CHECK=0): a piece caught while a later flight was at it may keep the bytes from in between"
            );
        }
        on
    })
}

/// What `VICTORY_FETCH_CHECK` says: on unless it is `0`.
fn said(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.trim() != "0")
}

/// A piece: its copy's number, and the piece.
pub(super) type Piece = (u64, u32);

/// Since the last frame-rate line: see [`stat`].
pub(super) static STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// What [`STATS`] count.
mod stat {
    /// Pieces fetched and hashed.
    pub(super) const CHECKED: usize = 0;
    /// Of them, those that were not what their flight ended with.
    pub(super) const SUSPECT: usize = 1;
    /// Suspects found whole when fetched again.
    pub(super) const WHOLE: usize = 2;
    /// Suspects that had come down as a later flight ended with: early,
    /// but whole. (At least: of the last [`super::HELD`] fetches of one,
    /// and only as long as it is suspect.)
    pub(super) const LATER: usize = 3;
    /// Pieces fetched with no hash to check them against.
    pub(super) const BLIND: usize = 4;
    /// Of the suspects, those in whose piece title bytes were sent up behind
    /// the flight (a send may be behind the fetch as well: a flight may have
    /// been at such a piece too).
    pub(super) const TITLE: usize = 5;
}

/// As the last landing left them: the suspects, and those of them that no
/// flight has fetched for [`OLD`] landings.
static NOW: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// A piece for the log: its title address, and its copy's address and
/// bytes.
type Place = (u64, (u64, u64));

/// Since the last frame-rate line, by piece: the times it was not what its
/// flight ended with.
static OFTEN: std::sync::Mutex<Option<HashMap<Place, u32>>> = std::sync::Mutex::new(None);

/// The landings (those with a fetch) after which a suspect that no flight
/// fetched again is counted as left.
const OLD: u64 = 64;

/// The hashes kept of what came down of a suspect.
const HELD: usize = 4;

/// What is known of a suspect.
#[derive(Default)]
struct Suspect {
    /// What came down of it and was not what its flight ended with, by
    /// hash: the last few fetches, less those a later flight is known to
    /// have ended with.
    held: Vec<u32>,
    /// The landing that fetched it last.
    at: u64,
}

/// The pieces whose last fetch was not what a flight ended with, or could
/// not be checked.
#[derive(Default)]
pub(super) struct Suspects {
    all: HashMap<Piece, Suspect>,
    /// The landings with a fetch so far.
    landings: u64,
    /// For [`STATS`], since the last landing.
    counted: [u64; 6],
}

impl Suspects {
    /// Whether a flight that may write `piece` fetches it, its compare
    /// having flagged it or not.
    pub(super) fn wanted(&self, piece: Piece, flagged: bool) -> bool {
        flagged || (!self.all.is_empty() && self.all.contains_key(&piece))
    }

    /// A flight that is landed ended with `hash` at `piece`: what came down
    /// of the piece before with that hash was this flight's, whole.
    pub(super) fn ended(&mut self, piece: Piece, hash: u32) {
        if self.all.is_empty() {
            return;
        }
        if let Some(suspect) = self.all.get_mut(&piece) {
            let before = suspect.held.len();
            suspect.held.retain(|&held| held != hash);
            self.counted[stat::LATER] += (before - suspect.held.len()) as u64;
        }
    }

    /// `piece` was fetched and came down with hash `held`; `same`: that is
    /// what the last of the flights landed ended with.
    pub(super) fn fetched(&mut self, piece: Piece, same: bool, held: u32) {
        self.counted[stat::CHECKED] += 1;
        if same {
            if self.all.remove(&piece).is_some() {
                self.counted[stat::WHOLE] += 1;
            }
            return;
        }
        self.counted[stat::SUSPECT] += 1;
        let suspect = self.all.entry(piece).or_default();
        suspect.at = self.landings;
        if suspect.held.len() == HELD {
            suspect.held.remove(0);
        }
        suspect.held.push(held);
    }

    /// Title bytes were sent up to the piece just found suspect, behind its
    /// flight.
    pub(super) fn title_sent(&mut self) {
        self.counted[stat::TITLE] += 1;
    }

    /// `piece` was fetched and there is no hash to check it against (the
    /// card did not compare the range it came down under): a later flight
    /// may have been at it, so the next one that may write it fetches it.
    pub(super) fn blind(&mut self, piece: Piece) {
        self.counted[stat::BLIND] += 1;
        self.all.entry(piece).or_default().at = self.landings;
    }

    /// The suspects that no flight has fetched for [`OLD`] landings.
    fn left(&self) -> usize {
        self.all.values().filter(|s| self.landings - s.at >= OLD).count()
    }

    /// A landing's fetch is done with: what was counted goes to the line.
    pub(super) fn landed(&mut self) {
        for (stat, counted) in STATS.iter().zip(&mut self.counted) {
            stat.fetch_add(std::mem::take(counted), Ordering::Relaxed);
        }
        self.landings += 1;
        NOW[0].store(self.all.len() as u64, Ordering::Relaxed);
        NOW[1].store(self.left() as u64, Ordering::Relaxed);
    }
}

/// The `most` pieces of `often` with the highest counts, those first (and
/// by address where the counts are the same).
fn most_often(often: HashMap<Place, u32>, most: usize) -> Vec<(Place, u32)> {
    let mut all: Vec<_> = often.into_iter().collect();
    all.sort_unstable_by_key(|&(piece, times)| (std::cmp::Reverse(times), piece));
    all.truncate(most);
    all
}

/// For the log, over `frames` frames.
pub(super) fn line(frames: f64) {
    let t: Vec<f64> = STATS.iter().map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    let often = OFTEN.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    if t[stat::CHECKED] <= 0.0 {
        return;
    }
    let often: Vec<String> = most_often(often, 6)
        .into_iter()
        .map(|((at, (start, len)), times)| format!("{at:#x} (of {start:#x}+{len:#x}) {:.2}", f64::from(times) / frames))
        .collect();
    tracing::info!(
        "fetched pieces checked per frame (VICTORY_FETCH_CHECK): {:.1}, of which {:.2} were not what their flight ended with \
         ({:.2} of them with title bytes sent up to the piece behind the flight, which the merge leaves out; at least {:.2} what \
         a later flight ended with, whole; the others caught while a flight was at them); {:.2} were found whole when fetched \
         again; {:.2} had no hash to be checked against; suspect now: {} pieces, {} of them fetched by no flight for {OLD} \
         landings; most often not what their flight ended with, per frame: {}",
        t[stat::CHECKED],
        t[stat::SUSPECT],
        t[stat::TITLE],
        t[stat::LATER],
        t[stat::WHOLE],
        t[stat::BLIND],
        NOW[0].load(Ordering::Relaxed),
        NOW[1].load(Ordering::Relaxed),
        if often.is_empty() { "none".to_string() } else { often.join(", ") },
    );
}

/// What is landed: its copy, its offsets from and to, and where it comes
/// down.
pub(super) type Landed = (std::sync::Arc<crate::shadow::Shadow>, usize, usize, u64);

/// The pieces of a landing's fetch, whole, and what is known of them.
pub(super) struct Check {
    /// What is copied down: whole pieces, as the compare hashes them.
    pub(super) copies: Vec<Landed>,
    /// The bytes of them.
    pub(super) total: u64,
    /// Of each: the piece, and the hash the last flight that may have
    /// written it ended with.
    of: Vec<(Piece, Option<u32>)>,
    /// Of what is landed, the piece (in `copies`) it is of.
    within: Vec<usize>,
    /// Of each piece: 0 not looked at yet, 1 as that flight ended with, 2
    /// not so.
    found: Vec<AtomicU8>,
    /// Of each piece that was looked at: the hash of what came down.
    held: Vec<AtomicU32>,
}

impl Check {
    /// For `landed`, parts of pieces that `plans` may have written, whose
    /// compares' results are in `differs`: the parts are given their places
    /// in the pieces that come down. `suspects` are told what the flights
    /// ended with.
    pub(super) fn new<'a>(
        plans: impl Iterator<Item = &'a CardPlan>,
        differs: &[u32],
        landed: &mut [Landed],
        suspects: &mut Suspects,
    ) -> Check {
        let mut index: HashMap<Piece, usize> = HashMap::new();
        let (mut copies, mut of, mut within, mut total) = (Vec::new(), Vec::new(), Vec::new(), 0u64);
        for (shadow, lo, _, at) in landed {
            let number = shadow.card_piece_of(*lo);
            let piece = (shadow.id(), number);
            let i = *index.entry(piece).or_insert_with(|| {
                let (from, to) = shadow.card_hashed(number);
                copies.push((shadow.clone(), from, to, total));
                of.push((piece, None));
                total += (to - from) as u64;
                copies.len() - 1
            });
            *at = copies[i].3 + (*lo - copies[i].1) as u64;
            within.push(i);
        }
        // (The last flight's stands.)
        for r in plans.flat_map(|plan| &plan.ranges).filter(|r| r.compared && !r.picked) {
            for p in 0..r.span.pieces {
                let piece = (r.shadow.id(), r.span.first + p);
                let Some(&d) = differs.get((r.at + p) as usize) else {
                    continue;
                };
                suspects.ended(piece, folded(d));
                if let Some(&i) = index.get(&piece) {
                    of[i].1 = Some(folded(d));
                }
            }
        }
        let found = of.iter().map(|_| AtomicU8::new(0)).collect();
        let held = of.iter().map(|_| AtomicU32::new(0)).collect();
        Check { copies, total, of, within, found, held }
    }

    /// Looks at the piece that part `i` of what is landed is of, if none of
    /// its parts was looked at: on the thread that lands it, just before, so
    /// that its bytes are near.
    ///
    /// # Safety
    /// What was fetched is at `data`, and stays as it is during the call.
    pub(super) unsafe fn look(&self, i: usize, data: usize) {
        let i = self.within[i];
        if self.found[i].load(Ordering::Relaxed) != 0 {
            return;
        }
        let Some(hash) = self.of[i].1 else {
            return;
        };
        let (_, from, to, at) = &self.copies[i];
        // SAFETY: the caller's.
        let bytes = unsafe { std::slice::from_raw_parts((data + *at as usize) as *const u8, to - from) };
        let held = self::hash(bytes);
        self.held[i].store(held, Ordering::Relaxed);
        self.found[i].store(if held == hash { 1 } else { 2 }, Ordering::Relaxed);
    }

    /// All is landed: what was found is kept for later flights. `after`:
    /// the send the fetch is known to have run behind.
    pub(super) fn done(&self, suspects: &mut Suspects, after: u64) {
        let mut caught: Vec<Place> = Vec::new();
        for (i, ((piece, _), found)) in self.of.iter().zip(&self.found).enumerate() {
            match found.load(Ordering::Relaxed) {
                // Not compared on the card: nothing says what it should be.
                0 => suspects.blind(*piece),
                found => {
                    suspects.fetched(*piece, found == 1, self.held[i].load(Ordering::Relaxed));
                    if found == 2 {
                        let (shadow, from, to, _) = &self.copies[i];
                        if shadow.card_sent_behind(after, *from, *to) {
                            suspects.title_sent();
                        }
                        let span = shadow.span();
                        caught.push((span.0 + *from as u64, span));
                    }
                }
            }
        }
        if !caught.is_empty() {
            let mut often = OFTEN.lock().unwrap_or_else(|e| e.into_inner());
            let often = often.get_or_insert_with(Default::default);
            for place in caught {
                *often.entry(place).or_default() += 1;
            }
        }
        suspects.landed();
    }
}

/// Of a result of the compare: whether the piece differs.
pub(super) fn flagged(result: u32) -> bool {
    result & 1 != 0
}

/// Of a result of the compare: the piece's hash, 31 bits of it.
pub(super) fn folded(result: u32) -> u32 {
    result >> 1
}

/// The hash the compare on the card ([`CARD_COMPARE`]) makes of a piece
/// that holds `bytes` (a multiple of 16 of them), as [`folded`].
pub(super) fn hash(bytes: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        // SAFETY: the feature is there.
        return unsafe { hash_avx2(bytes) };
    }
    hash_plain(bytes)
}

const MUL: [u32; 4] = [0x0100_0193, 0x9E37_79B1, 0x85EB_CA77, 0xC2B2_AE3D];
const SEED: [u32; 4] = [0x9E37_79B9, 0x85EB_CA6B, 0xC2B2_AE35, 0x27D4_EB2F];

fn step(h: &mut [u32; 4], with: [u32; 4]) {
    let x: [u32; 4] = std::array::from_fn(|i| (h[i] ^ with[i]).wrapping_mul(MUL[i]));
    *h = std::array::from_fn(|i| x[(i + 1) % 4] ^ (x[i] >> 15));
}

/// The 64 threads' hashes as one, folded to 31 bits.
fn finish(parts: &[[u32; 4]; 64]) -> u32 {
    let mut h = parts[0];
    for part in &parts[1..] {
        step(&mut h, *part);
    }
    let f = h[0] ^ h[1].wrapping_mul(3) ^ h[2].wrapping_mul(5) ^ h[3].wrapping_mul(7);
    f & 0x7FFF_FFFF
}

fn seeds() -> [[u32; 4]; 64] {
    std::array::from_fn(|thread| SEED.map(|s| s.wrapping_add(thread as u32)))
}

fn tail(parts: &mut [[u32; 4]; 64], bytes: &[u8]) {
    for (thread, chunk) in bytes.chunks_exact(16).enumerate() {
        let with = std::array::from_fn(|i| u32::from_le_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]));
        step(&mut parts[thread], with);
    }
}

fn hash_plain(bytes: &[u8]) -> u32 {
    let mut parts = seeds();
    for row in bytes.chunks(1024) {
        tail(&mut parts, row);
    }
    finish(&parts)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn hash_avx2(bytes: &[u8]) -> u32 {
    use std::arch::x86_64::*;
    let mut parts = seeds();
    let rows = bytes.chunks_exact(1024);
    let rest = rows.remainder();
    let mul = _mm256_setr_epi32(
        MUL[0] as i32, MUL[1] as i32, MUL[2] as i32, MUL[3] as i32, MUL[0] as i32, MUL[1] as i32, MUL[2] as i32, MUL[3] as i32,
    );
    let state = parts.as_mut_ptr().cast::<__m256i>();
    for row in rows {
        let from = row.as_ptr().cast::<__m256i>();
        for k in 0..32 {
            // SAFETY: two threads' hashes of 16 bytes each, and their 32 of
            // the row's 1,024 bytes.
            unsafe {
                let h = _mm256_xor_si256(_mm256_loadu_si256(state.add(k)), _mm256_loadu_si256(from.add(k)));
                let x = _mm256_mullo_epi32(h, mul);
                // y, z, w, x of each.
                let turned = _mm256_shuffle_epi32::<0b00_11_10_01>(x);
                _mm256_storeu_si256(state.add(k), _mm256_xor_si256(turned, _mm256_srli_epi32::<15>(x)));
            }
        }
    }
    tail(&mut parts, rest);
    finish(&parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The check is the default with the copy queue: unset is on, only `0`
    /// turns it off.
    #[test]
    fn the_check_is_on_unless_told_0() {
        assert!(said(None));
        assert!(said(Some("1")));
        assert!(!said(Some("0")));
        assert!(!said(Some(" 0")));
    }

    #[test]
    fn the_wide_hash_is_the_plain_one() {
        let bytes: Vec<u8> = (0..65_536u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        for n in [0usize, 16, 32, 1008, 1024, 1040, 5000 & !15, 65_536] {
            assert_eq!(hash(&bytes[..n]), hash_plain(&bytes[..n]), "{n} bytes");
        }
        let mut other = bytes.clone();
        other[40_000] ^= 1;
        assert_ne!(hash(&bytes), hash(&other));
    }

    /// A model of one piece. The card runs flights in order; each writes the
    /// piece several times and its compare flags it if it ends otherwise
    /// than the flight before. Each landing fetches what is wanted, on a
    /// queue beside the drawing one: the fetch sees the piece as it is while
    /// a later flight is at some step. Returns what the CPU's side holds in
    /// the end, and the fetches.
    fn landed(flights: &[&[u32]], seen: &[(usize, usize)], check: bool) -> (u32, u32) {
        let (held, fetches, _) = landed_counted(flights, seen, check);
        (held, fetches)
    }

    /// [`landed`], and what the suspects counted.
    fn landed_counted(flights: &[&[u32]], seen: &[(usize, usize)], check: bool) -> (u32, u32, [u64; 6]) {
        let piece: Piece = (1, 0);
        let mut suspects = Suspects::default();
        let (mut held, mut ended, mut fetches) = (0, 0, 0);
        for (f, steps) in flights.iter().enumerate() {
            let before = ended;
            ended = *steps.last().expect("a step");
            let flagged = ended != before;
            suspects.ended(piece, ended);
            if if check { !suspects.wanted(piece, flagged) } else { !flagged } {
                continue;
            }
            fetches += 1;
            // While flight `at` is after its step `step`.
            let (at, step) = seen[f];
            assert!(at >= f);
            held = flights[at][step];
            suspects.fetched(piece, held == ended, held);
        }
        (held, fetches, suspects.counted)
    }

    /// A counter that each flight clears and counts up to the same: the
    /// first flight's fetch sees it cleared by the second, whose compare
    /// flags nothing.
    #[test]
    fn a_piece_caught_in_between_is_fetched_again() {
        let count: &[u32] = &[0, 3, 5];
        // The first is fetched while the second has cleared it.
        assert_eq!(landed(&[count, count], &[(1, 0), (1, 2)], false), (0, 1), "the CPU's side keeps what was in between");
        assert_eq!(landed(&[count, count], &[(1, 0), (1, 2)], true), (5, 2));
        // And again, while the third was at it; the fourth finds it whole.
        let four = [count; 4];
        assert_eq!(landed(&four, &[(1, 0), (2, 1), (3, 0), (3, 2)], true), (5, 4));
        // Fetched whole: no more fetches than without the check.
        assert_eq!(landed(&four, &[(0, 2), (1, 2), (2, 2), (3, 2)], true), (5, 1));
        assert_eq!(landed(&four, &[(0, 2), (1, 2), (2, 2), (3, 2)], false), (5, 1));
    }

    /// Two flights that end otherwise: the first's fetch runs behind the
    /// second, or while the second has cleared the piece.
    #[test]
    fn what_a_later_flight_ended_with_is_told_from_what_was_in_between() {
        let (first, second): (&[u32], &[u32]) = (&[0, 3, 5], &[0, 4, 7]);
        let (held, fetches, counted) = landed_counted(&[first, second], &[(1, 2), (1, 2)], true);
        assert_eq!((held, fetches), (7, 2));
        assert_eq!((counted[stat::SUSPECT], counted[stat::LATER], counted[stat::WHOLE]), (1, 1, 1));
        let (held, fetches, counted) = landed_counted(&[first, second], &[(1, 0), (1, 2)], true);
        assert_eq!((held, fetches), (7, 2));
        assert_eq!((counted[stat::SUSPECT], counted[stat::LATER], counted[stat::WHOLE]), (1, 0, 1));
        // Behind the third, of four: the second's landing does not know
        // the hash, the third's does.
        let third: &[u32] = &[0, 6, 9];
        let (held, fetches, counted) = landed_counted(&[first, second, third, third], &[(2, 2), (2, 2), (2, 2), (3, 2)], true);
        assert_eq!((held, fetches), (9, 3));
        assert_eq!((counted[stat::SUSPECT], counted[stat::LATER], counted[stat::WHOLE]), (2, 2, 1));
    }

    /// A piece that came down under a range the card did not compare: the
    /// next flight that may write it fetches it, flagged or not, and what
    /// it finds whole is suspect no more.
    #[test]
    fn a_piece_with_no_hash_is_fetched_by_the_next_flight() {
        let (piece, other): (Piece, Piece) = ((7, 3), (7, 4));
        let mut suspects = Suspects::default();
        assert!(!suspects.wanted(piece, false));
        suspects.blind(piece);
        assert!(suspects.wanted(piece, false) && !suspects.wanted(other, false) && suspects.wanted(other, true));
        // Not compared again: it stays.
        suspects.blind(piece);
        assert!(suspects.wanted(piece, false));
        suspects.ended(piece, 5);
        suspects.fetched(piece, true, 5);
        assert!(!suspects.wanted(piece, false));
        assert_eq!(suspects.counted, [1, 0, 1, 0, 2, 0]);
    }

    /// A suspect that no flight comes for (title bytes went up to it, and
    /// no shader writes it again) is counted, and stays for the flight that
    /// does come.
    #[test]
    fn a_suspect_no_flight_fetches_is_counted_as_left() {
        let (left, kept): (Piece, Piece) = ((1, 0), (1, 1));
        let mut suspects = Suspects::default();
        suspects.fetched(left, false, 9);
        suspects.fetched(kept, false, 9);
        for _ in 0..OLD - 1 {
            suspects.landed();
        }
        assert_eq!((suspects.all.len(), suspects.left()), (2, 0));
        suspects.fetched(kept, false, 8);
        suspects.landed();
        assert_eq!((suspects.all.len(), suspects.left()), (2, 1));
        assert!(suspects.wanted(left, false));
        suspects.fetched(left, true, 5);
        assert_eq!((suspects.all.len(), suspects.left()), (1, 0));
        // No more hashes are kept of one than the last few.
        for held in 0..10 {
            suspects.fetched(kept, false, held);
        }
        assert_eq!(suspects.all[&kept].held, [6, 7, 8, 9]);
    }

    #[test]
    fn the_pieces_caught_most_often_come_first() {
        let copy = (0x1000_0000, 0x40_0000);
        let often: HashMap<Place, u32> =
            [(0x1001_0000, 2), (0x1002_0000, 9), (0x1003_0000, 2), (0x1004_0000, 1)].into_iter().map(|(at, n)| ((at, copy), n)).collect();
        let most: Vec<(u64, u32)> = most_often(often, 3).into_iter().map(|((at, _), n)| (at, n)).collect();
        assert_eq!(most, [(0x1002_0000, 9), (0x1001_0000, 2), (0x1003_0000, 2)]);
    }

    /// What the hash costs (ignored: a measurement): pieces that just came
    /// down, hashed alone, compared with what the CPU's side holds (as the
    /// merge begins), and both.
    #[test]
    #[ignore]
    fn hash_costs() {
        let (bytes, held) = (vec![0x5Au8; 96 << 20], vec![0x5Au8; 96 << 20]);
        let pieces = || bytes.chunks(64 << 10).zip(held.chunks(64 << 10));
        let compare = |a: &[u8], b: &[u8]| a.chunks(4096).zip(b.chunks(4096)).filter(|(a, b)| a != b).count() as u32;
        for _ in 0..5 {
            let mut all = 0u32;
            let mut time = |f: &dyn Fn(&[u8], &[u8]) -> u32| {
                let t = std::time::Instant::now();
                for (a, b) in pieces() {
                    all ^= f(a, b);
                }
                t.elapsed().as_secs_f64() * 1e3
            };
            let (hashed, compared, both) = (
                time(&|a, _| hash(a)),
                time(&compare),
                time(&|a, b| hash(a) ^ compare(a, b)),
            );
            println!("96 MB: hashed {hashed:.2} ms, compared {compared:.2} ms, both {both:.2} ms ({all:#x})");
        }
    }
}
