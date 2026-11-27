//! `VICTORY_REPAINT_WATCH=1` (diagnostics builds; off unless set): textures
//! picked by index that the title paints through another view of their
//! pages are followed frame by frame for a while, and a line says when their
//! title memory changes and whether their host copy holds what title memory
//! holds. Metadata only: slots, addresses, sizes, frames, counts of 64 KiB
//! blocks; no byte of a texture is written anywhere.
//!
//! What it is for: in the opening scene a character's face is at times one
//! flat hue (violet, green) or speckled for a whole shot, in the runs in
//! which the title fills that character's three head textures (one of
//! 2,228,224 bytes and two of 557,056, all their blocks shown at other
//! addresses too) while the shot is on screen (runs fg0 and fh1; not fg1 and
//! fg2, where it filled them seconds before). The texture pass looks at such
//! a texture again 4, 12, 40, 120 and 400 frames after an upload, by a
//! fingerprint of 48 KiB of it. These lines say which of two things happens
//! in a run that shows the fault: title memory goes on changing and the host
//! copy stays behind (the pass is what is late), or the host copy holds what
//! title memory holds all along (what is wrong is in title memory, or the
//! face reads other slots).
//!
//! Followed: a texture found changed by a look after its upload, and a
//! texture of one of those two sizes when it is uploaded, if all its blocks
//! are shown elsewhere. At most [`MOST`] at a time, each for [`FRAMES`]
//! frames. Every frame each one's memory is read once through the kernel
//! (0.3-0.6 ms a megabyte): a few milliseconds a frame while any is followed.
//!
//! Lines, all beginning "repaint watch:":
//! - "followed": a texture is taken up (why, how many of its blocks are
//!   shown elsewhere and at which address the first one is);
//! - "title memory changed": in how many of its blocks, whether in the 48
//!   KiB the pass's look reads, and how many blocks are all zeros now;
//! - "host copy behind" / "host copy holds title memory again": when what
//!   the host copy was filled from stops and starts being what title memory
//!   holds (with how the slot's texture is kept up: by the pass, or by the
//!   draws that read it through a table; and whether a copy on the card
//!   wrote into it);
//! - "done": after [`FRAMES`] frames, the changes seen, the frames the host
//!   copy was behind and the longest such stretch.

use std::sync::{Mutex, OnceLock};

use crate::descriptor::View;

/// Whether the lines are written.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_REPAINT_WATCH").is_ok_and(|v| v.trim() == "1"))
}

/// Textures followed at a time, and the frames each is followed for.
const MOST: usize = 24;
const FRAMES: u64 = 240;
/// Followed in all: the lines of a run stay a few hundred.
const EVER: usize = 96;
const BLOCK: usize = 64 << 10;
/// The sizes of the head textures' first levels (see the module's text).
const SIZES: [u64; 2] = [2_228_224, 557_056];

struct Followed {
    slot: u32,
    at: u64,
    len: u64,
    since: u64,
    /// A digest of every 64 KiB block of the memory, as last read.
    blocks: Vec<u128>,
    /// What the pass's look would have read then.
    sampled: u64,
    /// Whether the host copy held title memory's bytes at the last frame.
    held: Option<bool>,
    behind_since: u64,
    changes: u32,
    behind: u64,
    longest: u64,
}

struct Watch {
    now: Vec<Followed>,
    ever: usize,
    frame: u64,
}

static WATCH: Mutex<Watch> = Mutex::new(Watch { now: Vec::new(), ever: 0, frame: u64::MAX });

/// The digests of the 64 KiB blocks of title memory `[at, at + len)`, read
/// through the kernel (pages not there count as zeros), and how many of the
/// blocks are all zeros.
fn blocks(at: u64, len: u64) -> (Vec<u128>, usize) {
    thread_local! {
        static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    READ.with(|read| {
        let mut read = read.borrow_mut();
        read.resize(BLOCK, 0);
        let mut out = Vec::with_capacity((len as usize).div_ceil(BLOCK));
        let mut zeros = 0;
        let mut from = 0usize;
        while from < len as usize {
            let bytes = BLOCK.min(len as usize - from);
            read[..bytes].fill(0);
            crate::host::copy_mapped(read.as_mut_ptr(), (at as usize + from) as *const u8, bytes, false);
            zeros += usize::from(read[..bytes].iter().all(|&b| b == 0));
            out.push(crate::texture::digest(&read[..bytes]).0);
            from += bytes;
        }
        (out, zeros)
    })
}

/// Takes up the texture of title slot `slot` at `[at, at + len)`: one a look
/// found changed after its upload (`changed`), or one just uploaded.
pub(crate) fn found(slot: u32, at: u64, len: u64, frame: u64, changed: bool) {
    if !on() || len == 0 {
        return;
    }
    if !changed && !SIZES.contains(&len) {
        return;
    }
    let mut w = WATCH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(f) = w.now.iter_mut().find(|f| f.slot == slot && f.at == at) {
        // Followed already: for as long again.
        f.since = f.since.max(frame.saturating_sub(FRAMES / 2));
        return;
    }
    if w.now.len() >= MOST || w.ever >= EVER {
        return;
    }
    let elsewhere = crate::inplace::shown_elsewhere(at as usize & !(BLOCK - 1), (at + len) as usize);
    let all = (at + len).div_ceil(BLOCK as u64) - at / BLOCK as u64;
    if !changed && (elsewhere.len() as u64) < all {
        return;
    }
    let (digests, zeros) = blocks(at, len);
    tracing::info!(
        slot,
        address = format_args!("{at:#x}"),
        bytes = len,
        frame,
        why = if changed { "a look found it changed after its upload" } else { "uploaded, and all its blocks are shown elsewhere" },
        blocks = digests.len(),
        blocks_shown_elsewhere = elsewhere.len(),
        first_other_address = format_args!("{:#x}", elsewhere.first().and_then(|(_, others)| others.first()).copied().unwrap_or(0)),
        blocks_all_zeros = zeros,
        "repaint watch: followed"
    );
    w.ever += 1;
    w.now.push(Followed {
        slot,
        at,
        len,
        since: frame,
        blocks: digests,
        sampled: crate::texture::fingerprint(at, len),
        held: None,
        behind_since: frame,
        changes: 0,
        behind: 0,
        longest: 0,
    });
}

/// Once a frame, from the texture pass: looks at every texture followed.
pub(crate) fn frame(heap: &crate::descriptor::Heap, frame: u64) {
    if !on() {
        return;
    }
    let mut w = WATCH.lock().unwrap_or_else(|e| e.into_inner());
    if w.frame == frame || w.now.is_empty() {
        return;
    }
    w.frame = frame;
    for f in &mut w.now {
        let (digests, zeros) = blocks(f.at, f.len);
        let changed = digests.iter().zip(&f.blocks).filter(|(a, b)| a != b).count();
        if changed != 0 {
            let sampled = crate::texture::fingerprint(f.at, f.len);
            f.changes += 1;
            tracing::info!(
                slot = f.slot,
                frame,
                frames_followed = frame.saturating_sub(f.since),
                blocks_changed = changed,
                of = digests.len(),
                in_what_a_look_reads = sampled != f.sampled,
                blocks_all_zeros = zeros,
                "repaint watch: title memory changed"
            );
            f.blocks = digests;
            f.sampled = sampled;
        }
        // The texture the slot's view is of now, and whether its host copy
        // was filled from the bytes title memory holds.
        let state = heap.handle(f.slot).and_then(crate::descriptor::view).and_then(|view| match view {
            View::Placed(false, resource, desc) => {
                let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                let texture = crate::resource::texture_at_made(address, &resource)?;
                let first = crate::resource::first_mip(&desc);
                Some((
                    address,
                    crate::texture::unchanged(texture, first),
                    crate::texture::is_staged(texture),
                    crate::texture::host_wrote(texture as usize),
                ))
            }
            _ => None,
        });
        let Some((address, held, tabled, card_copied)) = state else {
            if f.held.is_some() {
                tracing::info!(slot = f.slot, frame, "repaint watch: the slot's view is no longer of a placed texture that was made");
                f.held = None;
            }
            continue;
        };
        if !held {
            f.behind += 1;
        }
        if f.held != Some(held) {
            if held {
                let stretch = frame.saturating_sub(f.behind_since);
                if f.held == Some(false) {
                    f.longest = f.longest.max(stretch);
                }
                tracing::info!(
                    slot = f.slot,
                    frame,
                    frames_followed = frame.saturating_sub(f.since),
                    frames_behind = if f.held == Some(false) { stretch } else { 0 },
                    view_address = format_args!("{address:#x}"),
                    kept_up_by = if tabled { "the draws that read it through a table" } else { "the pass" },
                    card_copied,
                    "repaint watch: host copy holds title memory again"
                );
            } else {
                f.behind_since = frame;
                tracing::info!(
                    slot = f.slot,
                    frame,
                    frames_followed = frame.saturating_sub(f.since),
                    view_address = format_args!("{address:#x}"),
                    kept_up_by = if tabled { "the draws that read it through a table" } else { "the pass" },
                    card_copied,
                    "repaint watch: host copy behind"
                );
            }
            f.held = Some(held);
        }
    }
    w.now.retain(|f| {
        if frame.saturating_sub(f.since) < FRAMES {
            return true;
        }
        let open = if f.held == Some(false) { frame.saturating_sub(f.behind_since) } else { 0 };
        tracing::info!(
            slot = f.slot,
            address = format_args!("{:#x}", f.at),
            bytes = f.len,
            frame,
            changes = f.changes,
            frames_behind = f.behind,
            longest_stretch_behind = f.longest.max(open),
            behind_at_the_end = f.held == Some(false),
            "repaint watch: done"
        );
        false
    });
}
