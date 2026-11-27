//! `VICTORY_REPAINT_WATCH=1` (diagnostics builds; off unless set): textures
//! picked by index that the title paints through another view of their
//! pages are followed frame by frame, and lines say when their title memory
//! changes and when an upload of them is recorded. Metadata only: slots,
//! addresses, sizes, frames, counts of 64 KiB blocks; no byte of a texture
//! is written anywhere.
//!
//! What it is for: in the opening scene a character's face is at times one
//! flat hue (violet, green) or speckled for a whole shot. The runs that had
//! it are the ones in which the title was still filling that character's
//! three head textures (one of 2,228,224 bytes and two of 557,056, all
//! their blocks shown at other addresses too) while the shot was on screen
//! (runs fg0, fh1, fr1; not fg1 and fg2). Run fr1, with the first form of
//! this watch, showed how the title fills them: all of their 64 KiB blocks
//! change in nearly every frame for a hundred frames and more, then fewer
//! and fewer (one set from 61 s until past 79.6 s, the shot being 74-80 s).
//! That form could not say what the host copy held meanwhile (it asked a
//! record that the slots of these textures do not have). This one counts
//! the uploads themselves.
//!
//! The lines say which of two things happens: title memory goes on
//! changing and no upload follows (the host copy is behind: the texture
//! pass or the draws' check is what is late), or an upload follows every
//! change (the host copy shows what title memory holds, so what is wrong is
//! in title memory: what the shaders' stores into the buffers over that
//! memory bring back to it, or the face reads other slots).
//!
//! Followed: a texture found changed by a look after its upload, and a
//! texture of one of those two sizes when it is uploaded, if all its blocks
//! are shown elsewhere. At most [`MOST`] at a time, each until [`QUIET`]
//! frames after its last change, [`LONGEST`] frames at most. Every frame
//! each one's memory is read once through the kernel (0.3-0.6 ms a
//! megabyte): a few milliseconds a frame while any is followed.
//!
//! Lines, all beginning "repaint watch:":
//! - "followed": a texture is taken up: why, how many of its blocks are
//!   shown elsewhere and at which address the first one is, what the slot's
//!   view is of (a placed texture, a texture object), whether draws read it
//!   through a table (it has a staging buffer), and whether a copy on the
//!   card wrote into it (then it is never uploaded from title memory again);
//! - "title memory changed": the first three changes and every sixteenth:
//!   in how many of its blocks, whether in the 48 KiB the pass's look
//!   reads, how many blocks are all zeros now, and the uploads so far;
//! - "behind": the first frame in which title memory has changed and no
//!   upload was recorded in that frame or the one before; "caught up": the
//!   first upload after that, and how many frames it took;
//! - "done": the changes seen (and those outside what a look reads), the
//!   first and last frame of them, the uploads recorded and the last one's
//!   frame, the frames behind and the longest such stretch, and what the
//!   slot's view was of at the end.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use crate::descriptor::View;
use crate::object::This;

/// Whether the lines are written.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_REPAINT_WATCH").is_ok_and(|v| v.trim() == "1"))
}

/// Textures followed at a time; the frames after its last change one is
/// still followed for; and the frames one is followed for at most.
const MOST: usize = 24;
const QUIET: u64 = 120;
const LONGEST: u64 = 900;
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
    changes: u32,
    unseen: u32,
    first_change: u64,
    last_change: u64,
    uploads: u32,
    last_upload: u64,
    /// Behind since this frame (0: not).
    behind_since: u64,
    behind: u64,
    longest: u64,
}

struct Watch {
    now: Vec<Followed>,
    ever: usize,
    frame: u64,
}

static WATCH: Mutex<Watch> = Mutex::new(Watch { now: Vec::new(), ever: 0, frame: u64::MAX });
/// Whether any texture is followed: [`uploaded`] is called for every
/// upload of the run.
static ANY: AtomicBool = AtomicBool::new(false);
/// Uploads recorded since the last frame's look: the texture's memory
/// (address, bytes) and the frame.
static UPLOADS: Mutex<Vec<(u64, u64, u64)>> = Mutex::new(Vec::new());

/// A list's prelude records an upload of the title texture whose memory is
/// `[memory, memory + bytes)` (`crate::texture::PreludeUpload`).
pub(crate) fn uploaded(memory: usize, bytes: usize) {
    if !ANY.load(Relaxed) {
        return;
    }
    let mut uploads = UPLOADS.lock().unwrap_or_else(|e| e.into_inner());
    if uploads.len() < 1 << 16 {
        uploads.push((memory as u64, bytes as u64, crate::frame::current()));
    }
}

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

/// What title slot `slot`'s view is of now: in words, the texture if one
/// was made for it, whether draws read that one through a table, and
/// whether a copy on the card wrote into it.
fn shown(heap: &crate::descriptor::Heap, slot: u32) -> (&'static str, Option<This>, bool, bool) {
    let (what, texture) = match heap.handle(slot).and_then(crate::descriptor::view) {
        Some(View::Placed(false, resource, desc)) => {
            let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            match crate::resource::texture_at_made(address, &resource) {
                Some(texture) => ("a placed texture", Some(texture)),
                None => ("a placed texture no resource was made for", None),
            }
        }
        Some(View::Shader(resource, _)) => ("a texture object", Some(resource as This)),
        Some(_) => ("another kind of view", None),
        None => ("nothing", None),
    };
    match texture {
        Some(texture) => {
            (what, Some(texture), crate::texture::is_staged(texture), crate::texture::host_wrote(texture as usize))
        }
        None => (what, None, false, false),
    }
}

/// Takes up the texture of title slot `slot` at `[at, at + len)`: one a look
/// found changed after its upload (`changed`), or one just uploaded.
pub(crate) fn found(heap: &crate::descriptor::Heap, slot: u32, at: u64, len: u64, frame: u64, changed: bool) {
    if !on() || len == 0 {
        return;
    }
    if !changed && !SIZES.contains(&len) {
        return;
    }
    let mut w = WATCH.lock().unwrap_or_else(|e| e.into_inner());
    if w.now.iter().any(|f| f.slot == slot && f.at == at) {
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
    let (view_of, _, tabled, card_copied) = shown(heap, slot);
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
        view_of,
        tabled,
        card_copied,
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
        changes: 0,
        unseen: 0,
        first_change: 0,
        last_change: 0,
        uploads: 0,
        last_upload: 0,
        behind_since: 0,
        behind: 0,
        longest: 0,
    });
    ANY.store(true, Relaxed);
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
    let uploads = std::mem::take(&mut *UPLOADS.lock().unwrap_or_else(|e| e.into_inner()));
    for f in &mut w.now {
        // The uploads of it since the last look.
        for &(memory, bytes, at_frame) in &uploads {
            if memory < f.at + f.len && f.at < memory + bytes.max(1) {
                f.uploads += 1;
                f.last_upload = f.last_upload.max(at_frame);
            }
        }
        let (digests, zeros) = blocks(f.at, f.len);
        let changed = digests.iter().zip(&f.blocks).filter(|(a, b)| a != b).count();
        if changed != 0 {
            let sampled = crate::texture::fingerprint(f.at, f.len);
            let seen = sampled != f.sampled;
            f.changes += 1;
            f.unseen += u32::from(!seen);
            if f.first_change == 0 {
                f.first_change = frame;
            }
            f.last_change = frame;
            if f.changes <= 3 || f.changes.is_multiple_of(16) {
                tracing::info!(
                    slot = f.slot,
                    frame,
                    change = f.changes,
                    blocks_changed = changed,
                    of = digests.len(),
                    in_what_a_look_reads = seen,
                    blocks_all_zeros = zeros,
                    uploads = f.uploads,
                    last_upload = f.last_upload,
                    "repaint watch: title memory changed"
                );
            }
            f.blocks = digests;
            f.sampled = sampled;
        }
        // Behind: title memory changed (between the last frame's look and
        // this one) and no upload was recorded in the last frame or since.
        // (One recorded in the last frame may have read it after the
        // change: not counted as behind.)
        let behind = f.last_change != 0 && f.last_upload + 1 < f.last_change;
        if behind {
            f.behind += 1;
            if f.behind_since == 0 {
                f.behind_since = frame;
                let (view_of, _, tabled, card_copied) = shown(heap, f.slot);
                tracing::info!(
                    slot = f.slot,
                    frame,
                    changes = f.changes,
                    uploads = f.uploads,
                    last_upload = f.last_upload,
                    view_of,
                    tabled,
                    card_copied,
                    "repaint watch: behind"
                );
            }
        } else if f.behind_since != 0 {
            let stretch = frame.saturating_sub(f.behind_since);
            f.longest = f.longest.max(stretch);
            f.behind_since = 0;
            tracing::info!(slot = f.slot, frame, frames_behind = stretch, uploads = f.uploads, "repaint watch: caught up");
        }
    }
    w.now.retain(|f| {
        let quiet = frame.saturating_sub(f.last_change.max(f.since));
        if quiet < QUIET && frame.saturating_sub(f.since) < LONGEST {
            return true;
        }
        let open = if f.behind_since != 0 { frame.saturating_sub(f.behind_since) } else { 0 };
        let (view_of, _, tabled, card_copied) = shown(heap, f.slot);
        tracing::info!(
            slot = f.slot,
            address = format_args!("{:#x}", f.at),
            bytes = f.len,
            frame,
            followed_from = f.since,
            changes = f.changes,
            changes_outside_what_a_look_reads = f.unseen,
            first_change = f.first_change,
            last_change = f.last_change,
            uploads = f.uploads,
            last_upload = f.last_upload,
            frames_behind = f.behind,
            longest_stretch_behind = f.longest.max(open),
            behind_at_the_end = f.behind_since != 0,
            view_of,
            tabled,
            card_copied,
            "repaint watch: done"
        );
        false
    });
    if w.now.is_empty() {
        ANY.store(false, Relaxed);
    }
}
