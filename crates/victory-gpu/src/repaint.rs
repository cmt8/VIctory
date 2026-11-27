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
//!
//! The audit (third form). Runs fr1 and fr2 showed the head textures
//! uploaded again every frame or two while the fault was on screen, which
//! the lines above cannot tell from "the host copy is wrong": they count
//! uploads, not what was uploaded. And in fr2 another face was speckled in
//! patches a minute after anything was followed. So every texture the pass
//! over textures picked by index makes ready is audited: the digest of the
//! bytes its staging buffer was last filled from (`Staging::detile`, the one
//! place) is kept by address, and in turns ([`AUDIT_BYTES`] of title memory
//! a frame) the texture's memory is read whole and compared with it. A
//! texture found other than what was detiled is looked at again
//! [`AUDIT_AGAIN`] frames later; if it is still other, and was not detiled in
//! between, a line "host copy stale" says so, once, with what is known of
//! it: whether draws read it through a table and how many frames ago a list
//! last used it (one nobody draws is not refreshed, and that is no fault),
//! whether shaders' writes landed in its memory since it was detiled, and
//! whether all of it is mapped. "host copy right again" follows when a later
//! look finds it equal. A line "audit" every [`AUDIT_LINE`] frames has the
//! counts. A run in which a face is wrong and no texture of its sizes is
//! stale has the fault in title memory, not in what is uploaded from it.
//!
//! The bakes (fourth form). Runs fr2 and fr3 showed that the textures
//! followed above are the planes of a video the title decodes (two sets of
//! 2048x1088 and twice 1024x544 bytes), written by its own threads and
//! uploaded while drawn: not a head's. What the faulty faces look like (the
//! detail of the skin kept, its colour gone: one flat green, or coarse
//! blocks of every colour, for one character and a whole shot, another
//! character in another run) fits a small colour texture of zeros or of
//! other bytes, and the title makes such textures itself: its decoration
//! bake draws a character's skin and decorations from sources picked by
//! index and copies the result into a texture of its own
//! (`crate::draw::BAKE_PIXEL_SHADERS`, `crate::list` `copy_texture_region`).
//! A bake that reads a host copy that is not what title memory holds bakes
//! that, once; and the audit found 13 to 24 such host copies at any time in
//! the frames the bakes were drawn in. So the bakes are told too:
//! - "bake pass": every run of the texture pass made for a bake draw: its
//!   frame, the bake draws so far (and those that got no pass: nothing was
//!   new, or the frame's milliseconds were spent), whether it served all
//!   that was due, what it uploaded and how long it took, the queue's
//!   counts, and how many host copies the audit holds stale at that moment;
//! - "host copy stale" also says how many bake passes ran between the
//!   detiling and the line;
//! - "first copy on the card into a title texture" and, a frame at a time,
//!   "copies on the card into a title texture": which texture (its title
//!   memory, size and format), how many copies, into which levels, the
//!   bounds of those into the first level, from what (a texture, a buffer
//!   with shaders' writes at their place in the list, a buffer filled from
//!   title memory), and what the texture held before the first one (zeros,
//!   the title's bytes). Two runs' lists of these can be laid side by side:
//!   the scene is the same, the faulty character is not;
//! - "a slot shows a texture copies on the card wrote": which slots;
//! - "the title's bytes are uploaded into a texture a copy on the card
//!   wrote": at a list's submission, when its prelude is given such an
//!   upload and another list recorded the copies (and whether that list was
//!   submitted before: then the title's bytes land over the baked texels);
//! - "bake sources": for every bake draw, what the slots it can pick from
//!   show when it is recorded (which slots: from the draw's own constants
//!   and decoration records, after readmet3xt-lab b3d49db): how many are
//!   right, and how many are null or the neutral texture, a kept view of
//!   another texture, without a host copy, or a host copy of other bytes
//!   than title memory holds (read whole), and how many have a new view
//!   waiting to be shown (`VICTORY_SHOW_NEW_LATER`, `VICTORY_SHOW_AFTER`).
//!   The card reads the heap when the bake's list runs, not when it is
//!   recorded: a slot counted here as waiting is at risk, no more.
//!
//! `VICTORY_REPAINT_WATCH=2` adds the exact form of the question, at the
//! cost of a pause in every frame with a bake: before the frame's first
//! bake pass every audited texture of up to 4 MiB is read whole, and two
//! frames later a line "bake audit" says how many were other than their
//! host copy at that moment, how many of those the pass and the frame's
//! lists detiled again in that frame, and names up to twelve of the rest.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use crate::descriptor::View;
use crate::object::This;

/// Whether the lines are written.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_REPAINT_WATCH").is_ok_and(|v| matches!(v.trim(), "1" | "2")))
}

/// Whether every audited texture is read before a frame's first bake pass
/// (`VICTORY_REPAINT_WATCH=2`).
fn exact() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_REPAINT_WATCH").is_ok_and(|v| v.trim() == "2"))
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
    audited(heap, slot, at, len);
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
    copies_frame(frame);
    bake_audit_done(frame);
    audit_frame(heap, frame);
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
                    shaders_writes_landed_there_in_two_frames = crate::watch::landed_lately(f.at as usize, f.len as usize, 2),
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

/// Title memory read for the audit in a frame; the frames after which a
/// texture found other than what was detiled is looked at again; the frames
/// between lines of counts; and the largest texture audited.
const AUDIT_BYTES: u64 = 24 << 20;
const AUDIT_AGAIN: u64 = 8;
const AUDIT_LINE: u64 = 256;
const AUDIT_LARGEST: u64 = 48 << 20;
/// Lines "host copy stale" written at most.
const AUDIT_TOLD: u32 = 400;

/// What a staging buffer was last filled from: the bytes read, their
/// digest, and the frame.
struct Detiled {
    bytes: u64,
    digest: u128,
    frame: u64,
}

/// By the address of the texture's memory.
static DETILED: Mutex<Option<HashMap<u64, Detiled>>> = Mutex::new(None);

/// A staging buffer is filled from `tiled`, the texture's memory at
/// `memory` (`Staging::detile`).
pub(crate) fn detiled(memory: usize, tiled: &[u8]) {
    if !on() {
        return;
    }
    let digest = crate::texture::digest(tiled).0;
    let frame = crate::frame::current();
    DETILED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(memory as u64, Detiled { bytes: tiled.len() as u64, digest, frame });
}

/// What a slot's view was of when it was taken into the audit: a placed
/// texture (1) by its address, a texture object (2) by its resource.
fn key(heap: &crate::descriptor::Heap, slot: u32) -> Option<(u8, u64)> {
    match heap.handle(slot).and_then(crate::descriptor::view)? {
        View::Placed(false, _, desc) => Some((1, u64::from(desc[10]) | u64::from(desc[11]) << 32)),
        View::Shader(resource, _) => Some((2, resource as u64)),
        _ => None,
    }
}

struct Audited {
    slot: u32,
    at: u64,
    /// 0: let go (the slot shows something else).
    len: u64,
    key: (u8, u64),
    /// Found other than what was detiled in this frame (0: not), and the
    /// frame of that detiling.
    other_since: u64,
    other_than: u64,
    /// The line "host copy stale" is written.
    told: bool,
}

#[derive(Default)]
struct Audit {
    slots: Vec<Audited>,
    by_slot: HashMap<u32, usize>,
    next: usize,
    /// Slots to look at again, and from which frame on: found other, or
    /// told stale.
    again: Vec<(u32, u64)>,
    frame: u64,
    /// Since the last line of counts: textures and bytes read, found equal,
    /// found other, never detiled (or at another size), written by a copy
    /// on the card, let go.
    counts: [u64; 7],
    told: u32,
    stale: u32,
    rounds: u64,
}

static AUDIT: Mutex<Option<Audit>> = Mutex::new(None);

/// The pass made the texture of title slot `slot` at `[at, at + len)`
/// ready, or looked at it: audited from now on.
pub(crate) fn audited(heap: &crate::descriptor::Heap, slot: u32, at: u64, len: u64) {
    if !on() || len == 0 || len > AUDIT_LARGEST {
        return;
    }
    let Some(key) = key(heap, slot) else {
        return;
    };
    let mut a = AUDIT.lock().unwrap_or_else(|e| e.into_inner());
    let a = a.get_or_insert_with(Audit::default);
    let new = Audited { slot, at, len, key, other_since: 0, other_than: 0, told: false };
    match a.by_slot.get(&slot) {
        Some(&i) => {
            let old = &a.slots[i];
            if old.at != at || old.len != len || old.key != key {
                if old.told {
                    a.stale = a.stale.saturating_sub(1);
                }
                a.slots[i] = new;
            }
        }
        None => {
            a.by_slot.insert(slot, a.slots.len());
            a.slots.push(new);
        }
    }
}

/// The digest of title memory `[at, at + len)` read whole through the
/// kernel (pages not there count as zeros).
fn whole(at: u64, len: u64) -> u128 {
    thread_local! {
        static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    READ.with(|read| {
        let mut read = read.borrow_mut();
        read.clear();
        read.resize(len as usize, 0);
        crate::host::copy_mapped(read.as_mut_ptr(), at as usize as *const u8, len as usize, false);
        crate::texture::digest(&read).0
    })
}

/// One audited texture is looked at. Returns the bytes read.
fn audit_one(heap: &crate::descriptor::Heap, a: &mut Audit, i: usize, frame: u64) -> u64 {
    let Audit { slots, counts, stale, told, again, .. } = a;
    let s = &mut slots[i];
    let (slot, at, len, was) = (s.slot, s.at, s.len, s.key);
    if key(heap, slot) != Some(was) {
        // The slot shows something else now: no longer this texture's.
        counts[6] += 1;
        if s.told {
            *stale = stale.saturating_sub(1);
        }
        s.len = 0;
        s.told = false;
        s.other_since = 0;
        return 0;
    }
    let (view_of, texture, tabled, card_copied) = shown(heap, slot);
    if card_copied {
        counts[5] += 1;
        if let Some(texture) = texture {
            slot_of_copied(slot, texture, at, len, frame);
        }
        return 0;
    }
    let held = {
        let map = DETILED.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref().and_then(|map| map.get(&at)).filter(|d| d.bytes == len).map(|d| (d.digest, d.frame))
    };
    let Some((digest, detiled_at)) = held else {
        counts[4] += 1;
        return 0;
    };
    let now = whole(at, len);
    counts[0] += 1;
    counts[1] += len;
    if now == digest {
        counts[2] += 1;
        if s.told {
            *stale = stale.saturating_sub(1);
            tracing::info!(
                slot,
                address = format_args!("{at:#x}"),
                bytes = len,
                frame,
                detiled_in_frame = detiled_at,
                found_other_in_frame = s.other_since,
                "repaint watch: host copy right again"
            );
        }
        s.told = false;
        s.other_since = 0;
        return len;
    }
    counts[3] += 1;
    if s.other_since == 0 || s.other_than != detiled_at {
        // Other for the first time, or detiled meanwhile and other again
        // (a texture the title goes on writing): looked at again soon.
        if s.told {
            *stale = stale.saturating_sub(1);
            s.told = false;
        }
        s.other_since = frame;
        s.other_than = detiled_at;
        again.push((slot, frame + AUDIT_AGAIN));
        return len;
    }
    if !s.told && frame >= s.other_since + AUDIT_AGAIN {
        s.told = true;
        *stale += 1;
        *told += 1;
        if *told <= AUDIT_TOLD {
            let elsewhere = crate::inplace::shown_elsewhere(at as usize & !(BLOCK - 1), (at + len) as usize);
            let used = texture.and_then(crate::texture::staged_used);
            tracing::info!(
                slot,
                address = format_args!("{at:#x}"),
                bytes = len,
                frame,
                detiled_in_frame = detiled_at,
                found_other_in_frame = s.other_since,
                view_of,
                tabled,
                a_list_used_it_frames_ago = used.map_or(-1, |at| frame.saturating_sub(at) as i64),
                blocks_shown_elsewhere = elsewhere.len(),
                shaders_writes_landed_there_since =
                    crate::watch::landed_lately(at as usize, len as usize, frame.saturating_sub(detiled_at)),
                all_mapped = crate::texture::committed(at as usize as *const u8, len as usize),
                bake_passes_since_detiled = bake_passes_between(detiled_at, frame),
                stale_now = *stale,
                "repaint watch: host copy stale"
            );
        }
    }
    // Looked at again: soon until the line is written, from time to time
    // after (for "right again").
    let wait = if s.told { 8 * AUDIT_AGAIN } else { AUDIT_AGAIN };
    again.push((slot, frame + wait));
    len
}

/// Once a frame: the textures due again, then the others in turn, until
/// [`AUDIT_BYTES`] are read.
fn audit_frame(heap: &crate::descriptor::Heap, frame: u64) {
    let mut a = AUDIT.lock().unwrap_or_else(|e| e.into_inner());
    let Some(a) = a.as_mut() else {
        return;
    };
    if a.frame == frame || a.slots.is_empty() {
        return;
    }
    a.frame = frame;
    let mut read = 0;
    let again = std::mem::take(&mut a.again);
    for (slot, due) in again {
        if due > frame {
            a.again.push((slot, due));
            continue;
        }
        if let Some(&i) = a.by_slot.get(&slot)
            && a.slots[i].len != 0
            && a.slots[i].other_since != 0
        {
            read += audit_one(heap, a, i, frame);
        }
    }
    let mut looked = 0;
    while read < AUDIT_BYTES && looked < a.slots.len() {
        let i = a.next % a.slots.len();
        a.next = i + 1;
        if a.next >= a.slots.len() {
            a.next = 0;
            a.rounds += 1;
        }
        looked += 1;
        // (Those found other have their own turns.)
        if a.slots[i].len != 0 && a.slots[i].other_since == 0 {
            read += audit_one(heap, a, i, frame);
        }
    }
    if frame.is_multiple_of(AUDIT_LINE) {
        let c = std::mem::take(&mut a.counts);
        tracing::info!(
            frame,
            slots = a.slots.len(),
            rounds = a.rounds,
            looked_at = c[0],
            megabytes = c[1] >> 20,
            equal = c[2],
            other = c[3],
            never_detiled_at_that_size = c[4],
            written_by_a_copy_on_the_card = c[5],
            let_go = c[6],
            stale_now = a.stale,
            stale_told = a.told,
            "repaint watch: audit"
        );
    }
}

/// Lines "bake pass" written at most; lines of copies on the card, in all
/// and for one texture; the textures a "bake audit" line names; and the
/// largest texture a bake audit reads.
const BAKE_LINES: u32 = 800;
const COPY_LINES: u32 = 1500;
const COPY_LINES_EACH: u32 = 48;
const BAKE_NAMED: usize = 12;
const BAKE_LARGEST: u64 = 4 << 20;

/// What a bake audit found other than its host copy: slot, address, bytes,
/// the frame it was last detiled in, and whether draws read it through a
/// table.
type Other = (u32, u64, u64, u64, bool);

/// A bake audit waiting for its line: the frame, the textures and megabytes
/// read, the milliseconds, and those found other.
struct BakeAudit {
    frame: u64,
    looked: u64,
    megabytes: u64,
    ms: u64,
    other: Vec<Other>,
}

#[derive(Default)]
struct Bakes {
    /// The frames of the passes made for bake draws, in order.
    passes: Vec<u64>,
    /// Bake draws: those a pass was made for, those that needed none
    /// (nothing new since one served all), and those whose frame had spent
    /// its milliseconds.
    draws: [u64; 3],
    lines: u32,
    audit: Option<BakeAudit>,
    /// The frame the last bake audit was made in, plus one.
    audited_in: u64,
}

static BAKES: Mutex<Option<Bakes>> = Mutex::new(None);

/// A draw of the title's bake is recorded (`State::early_textures`): a pass
/// follows (0), none is needed (1), or the frame's time for them is spent
/// (2).
pub(crate) fn bake_draw(what: usize) {
    if !on() {
        return;
    }
    let mut b = BAKES.lock().unwrap_or_else(|e| e.into_inner());
    b.get_or_insert_with(Bakes::default).draws[what.min(2)] += 1;
}

/// How many bake passes ran in frames `(after, until]`.
fn bake_passes_between(after: u64, until: u64) -> usize {
    let b = BAKES.lock().unwrap_or_else(|e| e.into_inner());
    b.as_ref().map_or(0, |b| b.passes.iter().filter(|&&f| f > after && f <= until).count())
}

/// Before a pass made for a bake draw (`VICTORY_REPAINT_WATCH=2`, the
/// frame's first one): every audited texture is read whole and compared
/// with what its staging buffer was last filled from. The line follows two
/// frames later ([`bake_audit_done`]).
pub(crate) fn before_bake(heap: &crate::descriptor::Heap, frame: u64) {
    if !on() || !exact() {
        return;
    }
    {
        let mut b = BAKES.lock().unwrap_or_else(|e| e.into_inner());
        let b = b.get_or_insert_with(Bakes::default);
        if b.audited_in == frame + 1 || b.audit.is_some() {
            return;
        }
        b.audited_in = frame + 1;
    }
    let began = std::time::Instant::now();
    let (mut looked, mut bytes, mut other) = (0u64, 0u64, Vec::new());
    {
        let a = AUDIT.lock().unwrap_or_else(|e| e.into_inner());
        let Some(a) = a.as_ref() else {
            return;
        };
        let mut seen = std::collections::HashSet::new();
        for s in &a.slots {
            if s.len == 0 || s.len > BAKE_LARGEST || !seen.insert(s.at) || key(heap, s.slot) != Some(s.key) {
                continue;
            }
            let (_, _, tabled, card_copied) = shown(heap, s.slot);
            if card_copied {
                continue;
            }
            let held = {
                let map = DETILED.lock().unwrap_or_else(|e| e.into_inner());
                map.as_ref().and_then(|map| map.get(&s.at)).filter(|d| d.bytes == s.len).map(|d| (d.digest, d.frame))
            };
            let Some((digest, detiled_at)) = held else {
                continue;
            };
            looked += 1;
            bytes += s.len;
            if whole(s.at, s.len) != digest {
                other.push((s.slot, s.at, s.len, detiled_at, tabled));
            }
        }
    }
    let ms = began.elapsed().as_millis() as u64;
    let mut b = BAKES.lock().unwrap_or_else(|e| e.into_inner());
    b.get_or_insert_with(Bakes::default).audit = Some(BakeAudit { frame, looked, megabytes: bytes >> 20, ms, other });
}

/// Once a frame: the line of a bake audit made two frames ago or more.
fn bake_audit_done(frame: u64) {
    let audit = {
        let mut b = BAKES.lock().unwrap_or_else(|e| e.into_inner());
        let Some(b) = b.as_mut() else {
            return;
        };
        if b.audit.as_ref().is_none_or(|a| frame < a.frame + 2) {
            return;
        }
        b.audit.take()
    };
    let Some(audit) = audit else {
        return;
    };
    // Detiled again in the bake's frame (the pass before it, or a list of
    // that frame), in the next one, or left as they were.
    let (mut same_frame, mut next_frame) = (0u64, 0u64);
    let mut left: Vec<Other> = Vec::new();
    {
        let map = DETILED.lock().unwrap_or_else(|e| e.into_inner());
        for &(slot, at, len, detiled_at, tabled) in &audit.other {
            match map.as_ref().and_then(|map| map.get(&at)).map(|d| d.frame) {
                Some(f) if f != detiled_at && f == audit.frame => same_frame += 1,
                Some(f) if f != detiled_at && f == audit.frame + 1 => next_frame += 1,
                _ => left.push((slot, at, len, detiled_at, tabled)),
            }
        }
    }
    let named: Vec<String> = left
        .iter()
        .take(BAKE_NAMED)
        .map(|&(slot, at, len, detiled_at, tabled)| {
            format!("slot {slot} at {at:#x} of {len} bytes, detiled in frame {detiled_at}, tabled {tabled}")
        })
        .collect();
    tracing::info!(
        frame = audit.frame,
        looked_at = audit.looked,
        megabytes = audit.megabytes,
        ms = audit.ms,
        other_than_the_host_copy = audit.other.len(),
        detiled_again_in_that_frame = same_frame,
        detiled_again_in_the_next = next_frame,
        left_as_they_were = left.len(),
        kilobytes_left = left.iter().map(|o| o.2).sum::<u64>() >> 10,
        the_first = ?named,
        "repaint watch: bake audit"
    );
}

/// A pass made for a bake draw has run: its number, whether it served all
/// that was due, the textures and bytes it uploaded, its milliseconds, and
/// the queue's counts as it found them
/// (`crate::descriptor::host::TextureQueue::census`).
pub(crate) fn bake_pass(frame: u64, pass: u64, served: bool, uploaded: u64, bytes: u64, ms: u64, queue: [u64; 5]) {
    if !on() {
        return;
    }
    let stale = AUDIT.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, |a| a.stale);
    let mut b = BAKES.lock().unwrap_or_else(|e| e.into_inner());
    let b = b.get_or_insert_with(Bakes::default);
    b.passes.push(frame);
    b.lines += 1;
    if b.lines > BAKE_LINES {
        return;
    }
    tracing::info!(
        frame,
        pass,
        bake_draws_with_a_pass = b.draws[0],
        with_nothing_new = b.draws[1],
        with_the_frames_time_spent = b.draws[2],
        served_all_that_was_due = served,
        uploaded,
        kilobytes = bytes >> 10,
        ms,
        queued = queue[0],
        due = queue[1],
        waiting_for_memory = queue[2],
        uploaded_in_part = queue[3],
        oldest_due_waited_frames = queue[4],
        host_copies_stale_now = stale,
        "repaint watch: bake pass"
    );
}

/// A draw of the title's bake is recorded, and `slots` are the title slots
/// it can pick textures from (`State::watch_bake_sources`): a line of what
/// they show at this moment, after the pass made for the draw. Each slot
/// counts once, by the first of: its memory's first page is not mapped; the
/// host slot is null or the neutral texture; it shows a view made from
/// another record than the title's present one (a kept view); it is not
/// made from the present record otherwise; its texture is one copies on the
/// card wrote; its texture has no host copy; the host copy was not filled
/// from the bytes title memory holds now (read whole); else it is right.
/// Apart from that: how many have a new view waiting to be shown.
pub(crate) fn bake_sources(
    heap: &crate::descriptor::Heap,
    host: &crate::descriptor::host::HostHeap,
    frame: u64,
    slots: &[u32],
) {
    if !on() {
        return;
    }
    static LINES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let line = LINES.fetch_add(1, Relaxed) + 1;
    let mut n = [0u64; 9];
    let mut named = Vec::new();
    for &slot in slots {
        let Some(handle) = heap.handle(slot) else {
            continue;
        };
        let view = crate::descriptor::view(handle);
        let (address, first) = match &view {
            Some(View::Placed(false, _, desc)) => {
                (u64::from(desc[10]) | u64::from(desc[11]) << 32, crate::resource::first_mip(desc))
            }
            _ => (0, 0),
        };
        let (_, texture, _, card_copied) = shown(heap, slot);
        if host.flip_waits(slot) {
            n[8] += 1;
        }
        let why = if address != 0 && !crate::texture::committed(address as usize as *const u8, 1) {
            0
        } else if host.shows_none(slot) {
            1
        } else if host.shows_another(slot, handle) {
            2
        } else if !host.texture_slot_made_from(slot, handle) {
            3
        } else if card_copied {
            4
        } else {
            match texture {
                Some(texture) if crate::resource::made_copy(texture).is_none() => 5,
                Some(texture) if !crate::texture::unchanged(texture, first) => 6,
                Some(_) => 7,
                None => 5,
            }
        };
        n[why] += 1;
        if !matches!(why, 0 | 4 | 7) && named.len() < 8 {
            let words = [
                "",
                "null or neutral",
                "a kept view",
                "not made",
                "",
                "no host copy",
                "host copy of other bytes",
            ][why];
            named.push(format!("slot {slot} at {address:#x}: {words}"));
        }
    }
    if line > 600 {
        return;
    }
    tracing::info!(
        frame,
        bake_draw = line,
        sources = slots.len(),
        right = n[7],
        written_by_the_card = n[4],
        not_mapped = n[0],
        on_null_or_the_neutral_texture = n[1],
        on_a_kept_view = n[2],
        not_made_otherwise = n[3],
        no_host_copy = n[5],
        host_copy_of_other_bytes = n[6],
        with_a_new_view_waiting = n[8],
        the_first = ?named,
        "repaint watch: bake sources"
    );
}

/// A title texture copies on the card write into.
struct Copied {
    first_frame: u64,
    copies: u64,
    /// The frame of the copies not yet told, and of them: how many, a bit
    /// for each level written, the bounds of those into the first level
    /// (left, top, right, bottom), how many had no box, and how many came
    /// from a texture, from a buffer with shaders' writes at their place in
    /// the list, and from a buffer filled from title memory.
    frame: u64,
    in_frame: u32,
    levels: u64,
    bounds: [u32; 4],
    whole: u32,
    sources: [u32; 3],
    lines: u32,
    address: u64,
    /// The lists that recorded copies into it since the last first one, by
    /// their epochs (the last sixteen).
    epochs: Vec<u64>,
}

#[derive(Default)]
struct Copies {
    by_texture: HashMap<usize, Copied>,
    lines: u32,
    /// Textures a slot was found to show, with a line written.
    shown: std::collections::HashSet<usize>,
}

static COPIES: Mutex<Option<Copies>> = Mutex::new(None);

/// The line of one texture's copies of a frame, if there are any.
fn copies_line(texture: usize, c: &mut Copied, lines: &mut u32) {
    if c.in_frame == 0 {
        return;
    }
    *lines += 1;
    c.lines += 1;
    if *lines <= COPY_LINES && c.lines <= COPY_LINES_EACH {
        tracing::info!(
            texture = format_args!("{texture:#x}"),
            address = format_args!("{:#x}", c.address),
            frame = c.frame,
            copies = c.in_frame,
            levels = format_args!("{:#x}", c.levels),
            first_level_bounds = ?c.bounds,
            without_a_box = c.whole,
            from_textures = c.sources[0],
            from_buffers_with_shaders_writes_placed = c.sources[1],
            from_buffers_filled_from_title_memory = c.sources[2],
            copies_so_far = c.copies,
            first_copy_in_frame = c.first_frame,
            "repaint watch: copies on the card into a title texture"
        );
    }
    c.in_frame = 0;
    c.levels = 0;
    c.bounds = [u32::MAX, u32::MAX, 0, 0];
    c.whole = 0;
    c.sources = [0; 3];
}

/// Once a frame: the lines of the copies of earlier frames.
fn copies_frame(frame: u64) {
    let mut c = COPIES.lock().unwrap_or_else(|e| e.into_inner());
    let Some(c) = c.as_mut() else {
        return;
    };
    let Copies { by_texture, lines, .. } = c;
    for (&texture, copied) in by_texture.iter_mut() {
        if copied.frame < frame {
            copies_line(texture, copied, lines);
        }
    }
}

/// A list records a copy on the card into title texture `dst` (host texture
/// `to`), subresource `sub`, at `x`, `y`, of `boxed` of the source (left,
/// top, front, right, bottom, back; none: all of it). `source`: a texture
/// (0), a buffer with shaders' writes at their place in the list (1), a
/// buffer filled from title memory (2). `first`: what was done before it:
/// nothing, the host wrote the texture before (0); zeros (1); the title's
/// bytes uploaded (2); nothing, and it is the first (3).
#[allow(clippy::too_many_arguments)] // what a copy is
pub(crate) fn card_copy(
    dst: This,
    to: &windows::Win32::Graphics::Direct3D12::ID3D12Resource,
    sub: u32,
    x: u32,
    y: u32,
    boxed: Option<[u32; 6]>,
    source: usize,
    first: u8,
    epoch: u64,
) {
    if !on() {
        return;
    }
    let frame = crate::frame::current();
    // SAFETY: a live resource.
    let d = unsafe { to.GetDesc() };
    let level = sub % u32::from(d.MipLevels.max(1));
    let mut c = COPIES.lock().unwrap_or_else(|e| e.into_inner());
    let Copies { by_texture, lines, .. } = c.get_or_insert_with(Copies::default);
    let new = !by_texture.contains_key(&(dst as usize));
    let copied = by_texture.entry(dst as usize).or_insert_with(|| Copied {
        first_frame: frame,
        copies: 0,
        frame,
        in_frame: 0,
        levels: 0,
        bounds: [u32::MAX, u32::MAX, 0, 0],
        whole: 0,
        sources: [0; 3],
        lines: 0,
        address: crate::resource::texture_memory(dst).map_or(0, |(at, _)| at as u64),
        epochs: Vec::new(),
    });
    if first != 0 {
        // A texture made anew at this place: the copies of the one before
        // are not its own.
        copied.epochs.clear();
    }
    if copied.epochs.last() != Some(&epoch) {
        if copied.epochs.len() >= 16 {
            copied.epochs.remove(0);
        }
        copied.epochs.push(epoch);
    }
    if new || first != 0 {
        *lines += 1;
        if *lines <= COPY_LINES {
            tracing::info!(
                texture = format_args!("{:#x}", dst as usize),
                address = format_args!("{:#x}", copied.address),
                width = d.Width,
                height = d.Height,
                layers = d.DepthOrArraySize,
                levels = d.MipLevels,
                format = d.Format.0,
                frame,
                held_before = match first {
                    0 => "what the card wrote before the watch saw it",
                    1 => "zeros",
                    2 => "the title's bytes, uploaded",
                    _ => "what its host copy held",
                },
                draws_read_it_through_a_table = crate::texture::is_staged(dst),
                "repaint watch: first copy on the card into a title texture"
            );
        }
    }
    if copied.frame != frame {
        copies_line(dst as usize, copied, lines);
        copied.frame = frame;
    }
    copied.copies += 1;
    copied.in_frame += 1;
    copied.levels |= 1 << level.min(63);
    copied.sources[source.min(2)] += 1;
    match boxed {
        Some([left, top, _, right, bottom, _]) if level == 0 => {
            let b = &mut copied.bounds;
            *b = [
                b[0].min(x),
                b[1].min(y),
                b[2].max(x + right.saturating_sub(left)),
                b[3].max(y + bottom.saturating_sub(top)),
            ];
        }
        Some(_) => {}
        None => copied.whole += 1,
    }
}

/// The epochs of the lists submitted lately, in order (the last 4,096).
static SUBMITTED: Mutex<std::collections::VecDeque<u64>> = Mutex::new(std::collections::VecDeque::new());

/// A list is submitted (`HostList::run_deferred`).
pub(crate) fn submitted(epoch: u64) {
    if !on() {
        return;
    }
    let mut s = SUBMITTED.lock().unwrap_or_else(|e| e.into_inner());
    if s.len() >= 4096 {
        s.pop_front();
    }
    s.push_back(epoch);
}

/// At a list's submission its prelude is given an upload of title memory
/// `memory` (address, bytes) into title texture `resource`. If a copy on the
/// card wrote that texture already, a line says so, unless only this list
/// recorded such copies (its prelude runs before them). The line says how
/// many of the lists that recorded copies were submitted before this one:
/// then the title's bytes land over what the card wrote, and stay (the
/// texture is never uploaded again, and its memory never held the copy).
pub(crate) fn upload_after_copy(resource: usize, memory: (usize, usize), epoch: u64, first_upload: bool) {
    if !on() || !crate::texture::host_wrote(resource) {
        return;
    }
    static COUNTS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];
    let c = COPIES.lock().unwrap_or_else(|e| e.into_inner());
    let copied = c.as_ref().and_then(|c| c.by_texture.get(&resource));
    let own = copied.is_some_and(|c| c.epochs.contains(&epoch));
    let others = copied.map_or(0, |c| c.epochs.iter().filter(|&&e| e != epoch).count());
    let before = copied.map_or(0, |c| {
        let s = SUBMITTED.lock().unwrap_or_else(|e| e.into_inner());
        c.epochs.iter().filter(|&&e| e != epoch && s.contains(&e)).count()
    });
    let kind = if before > 0 {
        2
    } else if others > 0 || !own {
        1
    } else {
        0
    };
    let n = COUNTS[kind].fetch_add(1, Relaxed) + 1;
    if kind == 0 || n > 200 {
        return;
    }
    tracing::info!(
        texture = format_args!("{resource:#x}"),
        address = format_args!("{:#x}", memory.0),
        bytes = memory.1,
        frame = crate::frame::current(),
        first_upload,
        this_list_recorded_copies_too = own,
        other_lists_that_recorded_copies = others,
        of_them_submitted_before_this_one = before,
        copies_so_far = copied.map_or(0, |c| c.copies),
        last_copy_in_frame = copied.map_or(0, |c| c.frame),
        uploads_by_the_copying_list_alone_so_far = COUNTS[0].load(Relaxed),
        with_copies_by_lists_not_submitted_yet_so_far = COUNTS[1].load(Relaxed),
        with_copies_by_lists_submitted_before_so_far = COUNTS[2].load(Relaxed),
        "repaint watch: the title's bytes are uploaded into a texture a copy on the card wrote"
    );
}

/// The audit met title slot `slot` showing `texture`, which copies on the
/// card wrote (its memory at `[at, at + len)`): said once for a texture.
fn slot_of_copied(slot: u32, texture: This, at: u64, len: u64, frame: u64) {
    let mut c = COPIES.lock().unwrap_or_else(|e| e.into_inner());
    let c = c.get_or_insert_with(Copies::default);
    if c.shown.len() >= 300 || !c.shown.insert(texture as usize) {
        return;
    }
    let copied = c.by_texture.get(&(texture as usize));
    tracing::info!(
        slot,
        texture = format_args!("{:#x}", texture as usize),
        address = format_args!("{at:#x}"),
        bytes = len,
        frame,
        copies_so_far = copied.map_or(0, |c| c.copies),
        first_copy_in_frame = copied.map_or(0, |c| c.first_frame),
        "repaint watch: a slot shows a texture copies on the card wrote"
    );
}
