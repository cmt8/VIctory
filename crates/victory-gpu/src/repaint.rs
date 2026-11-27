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

use std::collections::HashMap;
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
