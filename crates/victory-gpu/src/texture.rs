//! Texture contents the title writes itself (decision 0007, "Texture
//! memory"): streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture.
//!
//! "Changed" means a page of the memory was written since the last detiling
//! ([`crate::watch`]), in write-watched memory; in physical-page views,
//! which cannot be watched, a content hash of the whole memory at every
//! submission that samples it.
//!
//! shortcut: a change detiles the whole texture, not the pages written.
//! Only textures read through descriptor tables are brought up to date
//! (directly indexed views are not known per draw); plane 0 is uploaded
//! (every mip and array slice), depth and stencil planes are not.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::object::This;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A texture's upload state: the staging buffer laid out as the host
/// texture's subresources, and what was last detiled.
pub(crate) struct Staging {
    buffer: ID3D12Resource,
    /// The size class it was taken for from the kept buffers
    /// ([`staging_pool`]), to which it goes back; 0: made for it alone, or
    /// a region of an upload slab.
    class: u64,
    /// Where in `buffer` its bytes start: 0, or the region's offset in an
    /// upload slab ([`crate::slab`]), which goes back to the slab with it.
    base: u64,
    region: Option<crate::slab::Region>,
    /// Its bytes.
    #[cfg(feature = "diagnostics")]
    size: u64,
    data: *mut u8,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: victory_formats::texture::Layout,
    /// Mips per array slice.
    mips: u32,
    /// The staging buffer's bytes (counted in `STAGED` while it lives).
    bytes: u64,
    memory: *const u8,
    last: Mutex<Last>,
    /// Whether the staging buffer was detiled since a submitted list last
    /// uploaded it, and whether one has uploaded it at all: a list records
    /// the upload only then. (Every list that sampled a title texture
    /// uploaded all of it again, changed or not: 5,411 subresource copies
    /// of 1,120 textures in one frame of run 18.)
    dirty: std::sync::atomic::AtomicBool,
    uploaded: std::sync::atomic::AtomicBool,
    /// When a list last recorded a use of it (`crate::host::moment`).
    used: std::sync::atomic::AtomicU64,
    /// Levels on demand (`crate::tiles`): the texture's host copy was made
    /// without memory of its own when the staging buffer was made.
    on_demand: bool,
    /// For such a texture: the levels that were the title's at the last
    /// detiling (a bit a level; the staging buffer holds zeros for the
    /// others), and what that was judged by ([`Staging::judgement`]).
    detiled: std::sync::atomic::AtomicU32,
    judged: [std::sync::atomic::AtomicU64; 2],
    /// For such a texture: when an upload last left a held level without
    /// tiles (`crate::host::moment`, never 0; 0: none did). The upload is
    /// recorded again about a second later if the card has room by then
    /// ([`upload`]).
    waiting: std::sync::atomic::AtomicU64,
}

/// What a texture's staging buffer holds: the hash of the memory detiled
/// (memory not watched), or the write-watch asking it was detiled after.
#[derive(Default)]
struct Last {
    hash: Option<blake3::Hash>,
    asked: u64,
    /// The frame a partly mapped texture was last read in.
    partial: Option<u64>,
    /// Until this frame the texture is read at every refresh although the
    /// guards saw no write to it ([`Last::passes_over`]).
    blind_until: u64,
}

/// A texture in view memory the guards saw no write to is read only every
/// this many frames, by its address, so that the textures take turns.
const HASH_EVERY: u64 = 8;

/// How long a texture found changed behind the guards is read at every
/// refresh: about four seconds at the title's 30 frames a second, and
/// again from each change found.
const BLIND_FRAMES: u64 = 120;

/// Textures found changed behind the guards since the last frame-rate line
/// ([`Last::read_behind_guards`]).
pub(crate) static BLIND: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether a texture found changed behind the guards is read at every
/// refresh for a while (`VICTORY_BLIND_TEXTURES=0`: only on its turn, as
/// before).
fn blind_textures() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BLIND_TEXTURES").is_none_or(|v| v != "0"))
}

impl Last {
    /// Whether a refresh of the texture at `start` in frame `frame`, whose
    /// memory the guards saw no write to, is passed over.
    ///
    /// The guards see the title's writes through the view the texture is
    /// in, not writes to the same pages through another view of them (the
    /// title maps physical pages where it likes, more than once if it
    /// likes), nor any other write that takes no fault of ours. Those were
    /// found only by the hash taken on the texture's turn, one frame in
    /// [`HASH_EVERY`]: the boot logo's video, decoded into one of its three
    /// sets of planes every frame, then changed one frame in eight and
    /// showed its sets as they were at their last hash, out of order
    /// (runs under Proton: 1.6 MB of texture reads a frame while it
    /// played, an eighth of a set's 12.7 MB, where VIctory main read a set
    /// every frame and the video was smooth). A texture whose hash changed
    /// with no write seen is read at every refresh for [`BLIND_FRAMES`]
    /// after.
    fn passes_over(&self, frame: u64, start: usize) -> bool {
        let turn = frame.wrapping_add((start >> 16) as u64).is_multiple_of(HASH_EVERY);
        !turn && frame >= self.blind_until
    }

    /// A refresh of guarded view memory in frame `frame`: `read` reads the
    /// texture at `start` (and sets its hash) unless the guards saw no
    /// write (`unwritten`) and [`Last::passes_over`] it. Returns what
    /// `read` returns, or 0.
    fn refresh_guarded(
        &mut self,
        frame: u64,
        start: usize,
        unwritten: bool,
        read: impl FnOnce(&mut Last) -> usize,
    ) -> usize {
        if !unwritten {
            return read(self);
        }
        if self.passes_over(frame, start) {
            return 0;
        }
        let before = self.hash;
        let n = read(self);
        self.read_behind_guards(frame, before);
        n
    }

    /// After a read that [`Last::passes_over`] let through though the
    /// guards saw no write: the texture's hash was `before`; if the read
    /// found it changed, it is read at every refresh for a while.
    fn read_behind_guards(&mut self, frame: u64, before: Option<blake3::Hash>) {
        if before.is_some() && self.hash != before && blind_textures() {
            if frame >= self.blind_until {
                BLIND.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            self.blind_until = frame + BLIND_FRAMES;
        }
    }
}

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock; the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

static STAGING: Mutex<Option<HashMap<usize, Arc<Staging>>>> = Mutex::new(None);

/// The buffer a texture the host writes is copied through into the texture
/// of a view the title placed in its memory in another format
/// ([`alias`]): every level of every layer as each of the two formats lays
/// it out for a copy (the same bytes: the texels are as wide), the written
/// texture's layout first.
struct Through {
    buffer: ID3D12Resource,
    /// For a source the host copied into rather than rendered to: its
    /// write ([`host_write_tick`]) the last copy was recorded after (0:
    /// none).
    copied: std::sync::atomic::AtomicU64,
    parts: Vec<(
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    )>,
}

/// The most levels times layers [`alias`] copies (a cube of nine levels).
const ALIAS_PARTS: u32 = 54;

// SAFETY: a device-local buffer only lists record copies with.
unsafe impl Send for Through {}
// SAFETY: as above.
unsafe impl Sync for Through {}

/// The copy buffers of [`alias`], by the placed view's texture.
static THROUGH: Mutex<Option<HashMap<usize, Arc<Through>>>> = Mutex::new(None);

/// Brings `placed`, the texture of a view the title placed in the memory
/// of `source` in another format (`crate::resource::alias_of`), up to date
/// with what the host wrote to `source`: the texels of every level and
/// layer are copied as they are, through a buffer (the host copies between
/// two formats of one texel width in no other way). Recorded the first time
/// the list reads the view
/// and again whenever the list used `source` in another way since (wrote
/// it, or read it itself). Leaves `placed` in the copy destination state,
/// as [`upload`] does. False if the copy cannot be made: the caller fills
/// the texture from title memory, as before.
pub(crate) fn alias(list: &HostList, placed: This, source: This) -> bool {
    const TARGET_OR_UNORDERED: u32 = 0x4 | 0x8;
    const COPY_DEST: u32 = 0x400;
    const COPY_SOURCE: u32 = 0x800;
    let (Some(to), Some(from)) = (
        crate::resource::host_resource(placed),
        crate::resource::host_resource(source),
    ) else {
        return false;
    };
    let Some(host) = host() else { return false };
    let through = {
        let mut map = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
        let map = map.get_or_insert_with(HashMap::new);
        match map.get(&(placed as usize)) {
            Some(through) => through.clone(),
            None => {
                // SAFETY: live textures.
                let (of_from, of_to) = unsafe { (from.GetDesc(), to.GetDesc()) };
                // Levels times layers (a volume's slices are one part).
                let count = |d: &D3D12_RESOURCE_DESC| {
                    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
                        1
                    } else {
                        u32::from(d.DepthOrArraySize)
                    };
                    u32::from(d.MipLevels) * layers
                };
                let parts = count(&of_from);
                if parts == 0 || parts > ALIAS_PARTS || parts != count(&of_to) {
                    return false;
                }
                let mut rows = [(); 2]
                    .map(|()| vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); parts as usize]);
                let mut bytes = [0u64; 2];
                // SAFETY: plain queries; the out pointers are ours and
                // hold `parts` layouts each.
                unsafe {
                    host.device.GetCopyableFootprints(
                        &of_from,
                        0,
                        parts,
                        0,
                        Some(rows[0].as_mut_ptr()),
                        None,
                        None,
                        Some(&mut bytes[0]),
                    );
                    host.device.GetCopyableFootprints(
                        &of_to,
                        0,
                        parts,
                        0,
                        Some(rows[1].as_mut_ptr()),
                        None,
                        None,
                        Some(&mut bytes[1]),
                    );
                }
                let alike = rows[0].iter().zip(&rows[1]).all(|(a, b)| {
                    let (fa, fb) = (a.Footprint, b.Footprint);
                    a.Offset == b.Offset
                        && (fa.Width, fa.Height, fa.Depth, fa.RowPitch)
                            == (fb.Width, fb.Height, fb.Depth, fb.RowPitch)
                });
                if bytes[0] == 0 || bytes[0] != bytes[1] || !alike {
                    return false;
                }
                let Some(buffer) = host.uav_buffer(bytes[0]) else {
                    return false;
                };
                crate::host::name(&buffer, || {
                    format!(
                        "alias copy {}x{} f{} to f{}",
                        of_from.Width, of_from.Height, of_from.Format.0, of_to.Format.0
                    )
                });
                tracing::info!(
                    view = ?placed,
                    texture = ?source,
                    width = of_from.Width,
                    height = of_from.Height,
                    parts,
                    written_as = of_from.Format.0,
                    read_as = of_to.Format.0,
                    "a view placed in the memory of a texture the host writes, in another format: read from that texture by a copy on the host"
                );
                let [of_written, of_read] = rows;
                let through = Arc::new(Through {
                    buffer,
                    copied: std::sync::atomic::AtomicU64::new(0),
                    parts: of_written.into_iter().zip(of_read).collect(),
                });
                map.insert(placed as usize, through.clone());
                through
            }
        }
    };
    // A source the host copied into (not one it renders to or writes in
    // shaders): copied once per write of it, not once per list. The
    // placed view's host copy keeps what the last copy put there.
    let tick = title_written(from).then(|| host_write_tick(source)).flatten();
    if let Some(tick) = tick
        && through.copied.load(std::sync::atomic::Ordering::Relaxed) == tick
    {
        return true;
    }
    let first = list.first_use(placed as usize) || tick.is_some();
    // The source's first use in the list records nothing (its prelude
    // brings it to the state); a use in another way since the last copy
    // shows as a change of state.
    let before = list.transition(source, COPY_SOURCE).map(|(texture, before)| {
        let state = crate::host::legal_state(texture, COPY_SOURCE);
        let barrier = crate::host::transition(
            texture,
            D3D12_RESOURCE_STATES(before as i32),
            D3D12_RESOURCE_STATES(state as i32),
        );
        // SAFETY: an open list and a live texture.
        unsafe { list.list.ResourceBarrier(&[barrier]) };
        before
    });
    if !first && before.is_none() {
        return true;
    }
    card_writes(list, placed, to, None);
    crate::list::ensure_state(list, placed, COPY_DEST);
    let part = |texture: &ID3D12Resource, index: usize| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index as u32,
        },
    };
    let rows = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(&through.buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: *row,
        },
    };
    let (common, read, written) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    #[cfg(feature = "diagnostics")]
    list.note_op(|| format!("alias copy of {source:?} into {placed:?}"));
    // SAFETY: an open list and live resources; the buffer is as big as
    // the footprints and rests in the common state between copies.
    unsafe {
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, common, written)]);
        for (index, (of_written, _)) in through.parts.iter().enumerate() {
            list.list
                .CopyTextureRegion(&rows(of_written), 0, 0, 0, &part(from, index), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, written, read)]);
        for (index, (_, of_read)) in through.parts.iter().enumerate() {
            list.list
                .CopyTextureRegion(&part(to, index), 0, 0, 0, &rows(of_read), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, read, common)]);
    }
    list.keep(&through.buffer);
    if let Some(tick) = tick {
        through.copied.store(tick, std::sync::atomic::Ordering::Relaxed);
    }
    // A draw that also writes the source (a view of it handled before
    // this one) needs it in that state again; the next read of the alias
    // then copies once more, as it must after a write.
    if let Some(before) = before
        && before & TARGET_OR_UNORDERED != 0
    {
        crate::list::ensure_state(list, source, before);
    }
    true
}

/// Whether a texture refresh that finds the runtime remapping title memory
/// goes on without waiting for it (`VICTORY_TEXTURE_NO_WAIT=1`; off by
/// default).
///
/// A texture whose memory is all mapped is read in place, under a lock
/// the runtime takes alone while it changes a mapping
/// (`crate::watch::title_memory_reading`), so that no page goes away under
/// the read. While the world streams in the runtime remaps all the time,
/// and each of the frame's 150-360 refreshes waited for it: runs 99-101
/// have 4, 15 and 16 frame lines with over 300 ms of "textures", up to
/// 3,708 ms, and on every one of them the wait for this lock is the whole
/// of it (1,345 of 1,381 ms; 3,630 of 3,708; the write-watch askings 3-12
/// ms, the texture's own lock 0).
///
/// With the switch the refresh asks for the lock without waiting. If the
/// runtime has it, the texture is read through the kernel instead, which
/// needs no lock: it fails if a page is not there, and then the texture is
/// left as it is and read at its next refresh, a frame later. A read
/// through the kernel costs about 0.6 ms a megabyte against 0.3 in place.
///
/// What changes for the picture: a texture that arrives while the runtime
/// is remapping may show a frame later. It never shows partly read.
fn no_wait() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_NO_WAIT").is_some_and(|v| v == "1"))
}

/// Since the last frame-rate line, with [`no_wait`]: refreshes that read
/// through the kernel because title memory was being remapped, and those
/// left for the next refresh because a page was not there; with
/// [`copy_first`]: refreshes read through the kernel's copy first, and
/// those that went on to the read under the lock (a page was not there).
/// With [`partly_no_wait`]: refreshes read as partly mapped without
/// waiting while the runtime remapped, and refreshes of partly mapped
/// textures passed over before the kernel copy (see `refresh_timed`).
pub(crate) static UNLOCKED: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];

/// Whether a texture the kernel's copy could not read whole
/// ([`copy_first`]) is read as a partly mapped one (zeros where a page is
/// not there, at most every 30 frames) when the runtime is remapping title
/// memory, instead of waiting for it to finish: on whenever [`copy_first`]
/// is (by default under Wine); `VICTORY_TEXTURE_PARTLY_WAIT=1` waits, as
/// before. When the runtime is not remapping, the read goes on as before.
///
/// Run 212937 (world entry and play, Proton): of the texture refreshes'
/// time summed over its 180 lines, 61.4 of 70.0 s (88%) was this wait;
/// lines of 140-209 ms a frame of it while 348-388 refreshes a frame found
/// a page not there. Most of those then returned at once: a partly mapped
/// texture is read at most every 30 frames, and that rule was looked at
/// only after the wait. The read is through the kernel's copy, which a
/// page that goes or comes meanwhile cannot fault; a texture caught while
/// the runtime remaps it is read again once it has (its guards are reset
/// by the remapping), so at worst it shows its holes for a frame.
fn partly_no_wait() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| copy_first() && std::env::var_os("VICTORY_TEXTURE_PARTLY_WAIT").is_none_or(|v| v != "1"))
}

/// Whether a texture refresh reads the texture's memory through the
/// kernel's copy first, without the lock the runtime takes to remap
/// (`crate::watch::title_memory_reading`) and without asking whether the
/// memory is all mapped (`committed`); only if a page is not there does it
/// go on as before. On by default under Wine, off elsewhere;
/// `VICTORY_TEXTURE_COPY_FIRST=1` or `=0` says.
///
/// Under Wine the runtime's remapping holds that lock across wineserver
/// requests for every view it maps and unmaps, each of which walks the
/// server's list of the process's views (Valve's Wine 11.0,
/// `server/mapping.c`: `map_view` checks the new view against every view,
/// `unmap_view` looks its view up in the list): 67-112 us a view with
/// 40,000 views (`membench`), so the waits for the lock were the frame
/// line's "textures" (run 13: 2,947 of 3,587 ms a frame). The question
/// whether the memory is mapped (`QueryWorkingSetEx`) takes Wine's lock on
/// the memory map, which the same requests hold: 2 us idle, 10-160 us for
/// 16-256 pages while one thread remaps, milliseconds with two. The kernel
/// copy is a copy in our own process there (1.1 us for 64 KiB, the same
/// while another thread remaps), safe from a page that goes away: it fails
/// instead of faulting. A texture read whole that way is the one the read
/// in place would have seen; one with a page not there is read as before.
/// On Windows the copy costs twice the read in place (see
/// [`through_copy`]), so it stays off there.
fn copy_first() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var_os("VICTORY_TEXTURE_COPY_FIRST") {
        Some(v) => v == "1",
        None => crate::host::on_wine(),
    })
}

/// Since the last frame-rate line, of texture refreshes on every thread, in
/// microseconds: waiting for the texture's own lock (another thread is
/// refreshing the same texture), asking the write watch and the guards,
/// waiting for title memory to hold still (the lock the runtime takes
/// alone while it remaps, `crate::watch::title_memory_reading`), and the
/// refreshes in all.
///
/// While the world streams in, the frame line's "textures" is seconds a
/// frame in some runs and tenths in others for the same megabytes (2,566
/// ms for 110 MB in run 93, 314 ms for 132 MB in run 91; before these
/// counters, runs 81, 83, 85 and 94 each had lines of 1.5-2.9 s), and the
/// submitting thread is not running while it lasts (run 93: 240-310 ms of
/// its own time a frame beside 1,100-1,800 ms of "textures"): it waits.
/// These say for what.
pub(crate) static REFRESH_WAITS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// `f`'s result, its time added to [`REFRESH_WAITS`]' place `slot`.
fn waited<T>(slot: usize, f: impl FnOnce() -> T) -> T {
    let started = std::time::Instant::now();
    let out = f();
    let us = started.elapsed().as_micros() as u64;
    REFRESH_WAITS[slot].fetch_add(us, std::sync::atomic::Ordering::Relaxed);
    if slot == 2 {
        crate::framelog::add(crate::framelog::T::RefreshStillUs, us);
    }
    out
}

thread_local! {
    /// Room to read a texture's memory into: one buffer per submitting
    /// thread. Each texture kept its own, a second copy in memory of every
    /// write-watched or partly mapped texture ever sampled, for the run.
    static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Calls `f` with `size` bytes of this thread's read buffer (their
/// contents are whatever the last texture left).
fn with_read<R>(size: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    READ.with(|read| {
        let mut read = read.borrow_mut();
        if read.len() < size {
            read.resize(size, 0);
        }
        f(&mut read[..size])
    })
}

/// Whether a texture all of whose memory is mapped is read through the
/// kernel's copy instead of in place (`VICTORY_TEXTURE_COPY=1`; off by
/// default). Round 3 of the memory work read every such texture that way
/// to be safe from pages taken away under the read; it cost 0.6 ms per MB
/// against 0.3 in place (90-105 ms a frame while the world streams in, runs
/// 72 and 73, against 20-40), and both runs with it lost the graphics
/// device, for a reason not found. The read in place is safe now for
/// another reason (`crate::watch::title_memory_reading`); this switch is
/// left to tell whether the copy was what lost the device.
fn through_copy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_COPY").is_some_and(|v| v == "1"))
}

/// Calls `f` with the `size` bytes of title memory at `memory`, read
/// through the kernel into this thread's read buffer; pages that are not
/// there read as zeros. The bytes are those a read in place would see.
fn with_title<R>(memory: *const u8, size: usize, f: impl FnOnce(&[u8]) -> R) -> R {
    with_read(size, |read| {
        crate::host::copy_mapped(read.as_mut_ptr(), memory, size, false);
        f(read)
    })
}

/// Staging buffers alive (also those only a list still holds) and their
/// bytes.
static STAGED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

impl Drop for Staging {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        STAGED[0].fetch_sub(1, Relaxed);
        STAGED[1].fetch_sub(self.bytes, Relaxed);
        // A region of a slab goes back to it as `region` drops.
        if self.class != 0 && self.region.is_none() {
            // Nothing uses it any more (lists that uploaded from it held it
            // until they had run): kept for the next texture of its class.
            give_staging(self.buffer.clone(), self.data, self.class);
        }
    }
}

/// Whether texture staging buffers are kept when their texture lets go of
/// them, by size class, for the next texture that needs one of that class
/// (`VICTORY_STAGING_POOL=0`: each made and freed with its texture, as
/// before). On by default (fxceless f85101b6 turned its own pool of the
/// same name on everywhere: 200 to 500 uploads a frame in a streaming
/// burst there, each buffer a quarter of a millisecond to make; this pool
/// takes its place, and off Wine keeps that pool's limits: buffers up to
/// 8 MiB, 256 MB spare).
///
/// Textures picked by index let go of their staging buffer right after
/// the upload, so each upload made an upload-heap buffer, mapped it and
/// wrote its fresh pages: run 212937 (Proton) uploaded 108,659 such
/// textures, 42 GB, and vkd3d-proton logged 388 times that the card's
/// upload memory was full ("falling back to system memory"), with 12,292
/// lock errors on that path. `membench`'s `staging_bench` on Proton's
/// vkd3d-proton (RX 7900 XTX): a buffer made, mapped, written and freed
/// per upload costs 33 us for 64 KiB, 74 us for 256 KiB, 223 us for
/// 1 MiB and 618 us for 4 MiB; written again, 3, 12, 46 and 185 us.
/// A class is the size rounded up to a quarter step between powers of two
/// (at most a quarter more), from 64 KiB; buffers past 64 MiB are not kept,
/// nor more than [`staging_spare_most`] bytes of them.
fn staging_pool() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_STAGING_POOL").as_deref().map(str::trim) {
        Ok("0") => false,
        Ok("1") => true,
        _ => true,
    })
}

/// The most bytes of staging buffers kept while no texture holds them
/// (`VICTORY_STAGING_SPARE_MB`: 512 under Wine, 256 elsewhere as in
/// fxceless f85101b6).
///
/// A buffer let go past it is freed, and the next texture of its class
/// makes one again. Under Proton's vkd3d-proton an upload buffer made once
/// the card's upload budget (a sixteenth of its memory) is spent falls
/// back to system memory (libs/vkd3d/memory.c
/// `vkd3d_allocate_device_memory`), and that path costs about 0.55 ms a
/// buffer whatever its size (`membench` `staging_bench` with
/// `VKD3D_CONFIG=no_upload_hvv`, which takes it for every buffer: 64 KiB
/// 0.54 ms, 4 MiB 0.57 ms, against 0.03-0.6 ms in the card's upload
/// memory); while the world streamed in, run 232629 made 400-550 staging
/// buffers a frame. A larger spare keeps more for the next textures; it
/// is memory the PC holds meanwhile. For A/B runs.
fn staging_spare_most() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_STAGING_SPARE_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(if crate::host::on_wine() { 512 } else { 256 })
            << 20
    })
}

/// The size class of a staging buffer of `size` bytes ([`staging_pool`]):
/// 0 if it is not kept (past 64 MiB under Wine, past 8 MiB elsewhere).
fn staging_class(size: u64) -> u64 {
    staging_class_upto(size, if crate::host::on_wine() { 64 << 20 } else { 8 << 20 })
}

/// [`staging_class`] with buffers past `most` bytes not kept.
fn staging_class_upto(size: u64, most: u64) -> u64 {
    const LEAST: u64 = 64 << 10;
    if size > most {
        return 0;
    }
    if size <= LEAST {
        return LEAST;
    }
    // Quarter steps between powers of two: 64, 80, 96, 112, 128, 160, ... KiB.
    let step = (1u64 << (63 - (size - 1).leading_zeros())) / 4;
    size.div_ceil(step) * step
}

/// Staging buffers no texture holds, by class, and their bytes in all.
struct SpareStaging {
    kept: HashMap<u64, Vec<(ID3D12Resource, usize)>>,
    bytes: u64,
}

// SAFETY: D3D12 resources are free-threaded; the mappings are plain
// addresses of persistently mapped buffers.
unsafe impl Send for SpareStaging {}

static SPARE_STAGING: Mutex<Option<SpareStaging>> = Mutex::new(None);

/// Since the start: staging buffers taken from those kept, made, and
/// taken from the upload slabs.
pub(crate) static STAGING_TAKEN: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Where a staging buffer's bytes are: a buffer and the offset in it, the
/// mapping of those bytes, the size class it goes back to (0: none), and
/// the slab region it is.
type StagingMemory = (ID3D12Resource, u64, *mut u8, u64, Option<crate::slab::Region>);

/// Staging memory of at least `size` bytes, mapped and zeroed: a region of
/// an upload slab ([`crate::slab`]), else a kept buffer of its class, or a
/// new one.
fn take_staging(host: &crate::host::Host, size: u64) -> Option<StagingMemory> {
    use std::sync::atomic::Ordering::Relaxed;
    if let Some(region) = crate::slab::take(host, size) {
        STAGING_TAKEN[2].fetch_add(1, Relaxed);
        // As a new buffer is: zeros (see below).
        // SAFETY: the region's mapping holds at least `size` bytes, and
        // nothing else holds the region.
        unsafe { region.data().write_bytes(0, size as usize) };
        return Some((region.buffer().clone(), region.offset(), region.data(), 0, Some(region)));
    }
    let class = if staging_pool() { staging_class(size) } else { 0 };
    if class != 0 {
        let kept = {
            let mut spare = SPARE_STAGING.lock().unwrap_or_else(|e| e.into_inner());
            spare.as_mut().and_then(|spare| {
                let kept = spare.kept.get_mut(&class).and_then(Vec::pop)?;
                spare.bytes -= class;
                Some(kept)
            })
        };
        if let Some((buffer, data)) = kept {
            STAGING_TAKEN[0].fetch_add(1, Relaxed);
            // As a new buffer is: zeros. A refresh may upload it without
            // detiling into it (nothing of the texture mapped, a read that
            // failed), and it holds another texture's bytes.
            // SAFETY: the mapping holds `class` >= `size` bytes, and no
            // texture or list holds the buffer any more.
            unsafe { (data as *mut u8).write_bytes(0, size as usize) };
            return Some((buffer, 0, data as *mut u8, class, None));
        }
    }
    STAGING_TAKEN[1].fetch_add(1, Relaxed);
    let (buffer, data) = host.upload_buffer(if class != 0 { class } else { size })?;
    Some((buffer, 0, data, class, None))
}

/// A staging buffer of class `class` no texture holds any more: kept, or
/// freed past [`staging_spare_most`].
fn give_staging(buffer: ID3D12Resource, data: *mut u8, class: u64) {
    let mut spare = SPARE_STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let spare = spare.get_or_insert_with(|| SpareStaging { kept: HashMap::new(), bytes: 0 });
    if spare.bytes + class > staging_spare_most() {
        return;
    }
    spare.bytes += class;
    spare.kept.entry(class).or_default().push((buffer, data as usize));
}

/// For the memory line: staging buffers alive, their bytes, and how many
/// of them their texture still has (the others wait for a list's reset).
#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 3] {
    use std::sync::atomic::Ordering::Relaxed;
    let kept = STAGING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |map| map.len() as u64);
    [STAGED[0].load(Relaxed), STAGED[1].load(Relaxed), kept]
}

/// Staging buffers let go as idle since the start, and their bytes.
static LET_GO: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// For the memory line: staging buffers let go as idle, and their bytes.
#[cfg(feature = "diagnostics")]
pub(crate) fn let_go() -> [u64; 2] {
    use std::sync::atomic::Ordering::Relaxed;
    [LET_GO[0].load(Relaxed), LET_GO[1].load(Relaxed)]
}

/// Lets go of the staging buffers no list recorded a use of for `IDLE_MS`
/// and `IDLE_FRAMES` frames and no list holds: a staging buffer is system memory the size of its
/// texture, kept so that an unchanged texture is not uploaded again, and
/// those of textures no longer sampled were kept for the run (2,246 of
/// them, 2.1 GB, at the end of run 24). The next list that samples such a
/// texture makes a staging buffer anew, detiles and uploads once more.
/// Called before a submission starts, by the thread that submits.
pub(crate) fn let_go_idle() {
    use std::sync::atomic::Ordering::Relaxed;
    const IDLE_FRAMES: u64 = 4;
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = map.as_mut() else {
        return;
    };
    if map.is_empty() {
        return;
    }
    let idle_ms = crate::host::ram_idle_ms(crate::host::available_physical_ram());
    // A list that recorded an upload or a check holds the staging until it
    // is reset: one only the table holds is named by no list, recorded or
    // running.
    let (mut went, mut bytes) = (0u64, 0u64);
    map.retain(|_, s| {
        let keep = Arc::strong_count(s) > 1
            || !crate::host::long_ago(s.used.load(Relaxed), idle_ms, IDLE_FRAMES);
        if !keep {
            LET_GO[0].fetch_add(1, Relaxed);
            LET_GO[1].fetch_add(s.bytes, Relaxed);
            went += 1;
            bytes += s.bytes;
        }
        keep
    });
    if went > 0 {
        crate::host::trail(|| format!("sweep: {went} staging buffers of textures let go ({} KiB), held by no list", bytes >> 10));
    }
}

/// Textures the host GPU wrote by a copy or resolve: their contents are on
/// the host, not in title memory, which stays as it was.
static HOST_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// A host copy or resolve writes `resource`.
pub(crate) fn host_written(resource: This) {
    let new = {
        let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
        set.get_or_insert_with(Default::default).insert(resource as usize)
    };
    // When the host last wrote it ([`host_write_tick`]).
    let tick = WRITE_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    HOST_WRITES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(resource as usize, tick);
    // Diagnostics (run 177): where its title memory is, for placed views
    // the title reads that memory through ([`placed_in_host_written`]).
    let Some((at, desc)) = crate::resource::texture_memory(resource) else {
        return;
    };
    // SAFETY: 56 bytes in the description's layout.
    let words = unsafe { std::ptr::from_ref(&desc).cast::<[u32; 14]>().read_unaligned() };
    let bytes = crate::resource::placed_bytes(&words).unwrap_or(0);
    if at.is_null() || bytes == 0 {
        return;
    }
    if new {
        HOST_WRITTEN_AT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(at as u64, (bytes, words[4], words[6], words[7], words[8], resource as usize));
    }
    // Views the title placed in this memory before the copy: the lead
    // above looks only at views made after it, and the character bake's
    // textures are viewed (their slots made ready by the texture pass run
    // just before the bake draws) before the bake copies into them.
    let placed = crate::resource::placed_in(at as u64, at as u64 + bytes);
    if placed.is_empty() {
        return;
    }
    if new {
        static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
            tracing::warn!(
                copied_address = format_args!("{:#x}", at as u64),
                copied_bytes = bytes,
                copied_width = words[4],
                copied_height = words[6],
                copied_layers_levels = format_args!("{:#x}", words[7]),
                copied_format = words[8],
                views = placed.len(),
                first_view_offset = placed[0].0 - at as u64,
                same_address = placed.iter().any(|&(a, _)| a == at as u64),
                "HERO LEAD (view made before the copy): the host copied into a texture whose memory holds views the title placed earlier; \
                 one at the same address and of the same size and texel width reads the copied texture (VICTORY_COPIED_VIEWS=0: not)"
            );
        }
    }
    // Their slots are due again: a view that reads the copied texture
    // copies it again ([`alias`]), and a slot the texture pass made from
    // title memory before the copy is made again.
    crate::descriptor::host::remapped(at as usize, bytes as usize);
}

/// Since the start, writes by the host of title textures (copies and
/// resolves into them), and the last one into each ([`host_write_tick`]).
static WRITE_TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HOST_WRITES: Mutex<Option<HashMap<usize, u64>>> = Mutex::new(None);

/// When the host last copied or resolved into `resource` (a number that
/// only grows), if it ever did.
fn host_write_tick(resource: This) -> Option<u64> {
    HOST_WRITES.lock().unwrap_or_else(|e| e.into_inner()).as_ref()?.get(&(resource as usize)).copied()
}

/// The title texture the host copied into whose memory starts at `address`
/// ([`host_written`]), if `fits` says it may be read for a view placed
/// there (asked under the table's lock: a texture leaves the table when
/// the title releases it).
pub(crate) fn host_copied_at(address: u64, fits: impl FnOnce(This) -> bool) -> Option<This> {
    let map = HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
    let resource = map.get(&address)?.5 as This;
    fits(resource).then_some(resource)
}

/// The title memory of textures the host copied into, by address: bytes
/// and description words 4, 6, 7 and 8 (width, height, layers and levels,
/// format).
#[allow(clippy::type_complexity)]
static HOST_WRITTEN_AT: Mutex<std::collections::BTreeMap<u64, (u64, u32, u32, u32, u32, usize)>> =
    Mutex::new(std::collections::BTreeMap::new());

/// A texture the title placed at `address` (description words `desc`) was
/// made: says so once if it lies in the memory of a texture the host
/// copied into ([`host_written`]). Its own host copy is filled from title
/// memory, which the copy never reached: the copied texels are on the
/// card in the other texture, where no shader that reads this view finds
/// them.
pub(crate) fn placed_in_host_written(address: u64, desc: &[u32; 14]) {
    let hit = {
        let map = HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
        map.range(..=address)
            .next_back()
            .filter(|(at, d)| address < **at + d.0)
            .map(|(at, d)| (*at, *d))
    };
    if let Some((at, (bytes, width, height, layers_levels, format, _))) = hit {
        static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
            tracing::warn!(
                view_address = format_args!("{address:#x}"),
                view_offset = address - at,
                view_width = desc[4],
                view_height = desc[6],
                view_layers_levels = format_args!("{:#x}", desc[7]),
                view_format = desc[8],
                copied_address = format_args!("{at:#x}"),
                copied_bytes = bytes,
                copied_width = width,
                copied_height = height,
                copied_layers_levels = format_args!("{layers_levels:#x}"),
                copied_format = format,
                "HERO LEAD: a placed texture view lies in the memory of a texture the host copied into: it is filled from title memory, which the copy never reached"
            );
        }
    }
}

/// Whether the host GPU wrote `resource` by a copy or resolve
/// ([`host_written`]): its host copy cannot be made again from title
/// memory.
pub(crate) fn host_wrote(resource: usize) -> bool {
    HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&resource))
}

/// The title released a resource.
pub(crate) fn forget(resource: usize) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&resource);
    }
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = set.as_mut() {
        set.remove(&resource);
    }
    if let Some(writes) = HOST_WRITES.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        writes.remove(&resource);
    }
    // Its memory may be given to other textures now (run 177: most of the
    // "HERO LEAD" lines were views in memory a released texture had held).
    HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, d| d.5 != resource);
    let mut through = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(through) = through.as_mut() {
        through.remove(&resource);
    }
}

/// Whether `resource`'s host copy takes its contents from title memory:
/// a texture the host GPU does not render to or write.
pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    const TARGET_DEPTH_UNORDERED: i32 = 0x7;
    // SAFETY: a live resource.
    unsafe { texture.GetDesc() }.Flags.0 & TARGET_DEPTH_UNORDERED == 0
}

/// Records, once per list, the copy of `resource`'s staging buffer into
/// its host copy, and the detiling that refreshes the staging buffer at
/// submission if the memory changed. Leaves the texture in the copy
/// destination state.
pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) {
    let host_written = HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&(resource as usize)));
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    if host_written || !list.first_use(resource as usize) {
        return;
    }
    let Some(staging) = staging(resource, texture) else {
        return;
    };
    use std::sync::atomic::Ordering::Relaxed;
    staging.used.store(crate::host::moment(), Relaxed);
    // Levels on demand: a view of the texture now starts at another
    // level, or another texture now starts inside its span, with no byte
    // of its memory changed. The levels are judged anew: detiled again at
    // this list's submission, and uploaded by this list.
    if staging.on_demand && staging.judgement() != staging.judged() {
        staging.forget();
        staging.dirty.store(true, Relaxed);
    }
    // Levels on demand: the last upload left a held level without tiles
    // (the card had no room, or no heap could be had), and nothing else
    // would record an upload again until the title writes the texture: the
    // level stayed empty, the texture blurred or black. Asked again about
    // once a second, and only when the card has room; the staging buffer
    // still holds the level's bytes.
    if staging.on_demand {
        let since = staging.waiting.load(Relaxed);
        if since != 0 && crate::host::long_ago(since, 1000, 2) {
            if crate::resource::room_lately() {
                staging.waiting.store(0, Relaxed);
                staging.dirty.store(true, Relaxed);
                crate::tiles::STATS[10].fetch_add(1, Relaxed);
            } else {
                staging.waiting.store(crate::host::moment().max(1), Relaxed);
            }
        }
    }
    // The host texture has the staging buffer's contents: only the check
    // at submission is left, and what it finds changed the next list that
    // samples the texture uploads (a frame late where every list of the
    // frame was recorded before that check).
    if staging.uploaded.load(Relaxed) && !staging.dirty.load(Relaxed) {
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexSkipped, 1);
        list.defer_texture(staging);
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::TexUploads, 1);
    crate::framelog::add(crate::framelog::T::Uploads, 1);
    const COPY_DEST: u32 = 0x400;
    crate::list::ensure_state(list, resource, COPY_DEST);
    // The staging buffer lives as long as `staging`, which the deferred
    // refresh below holds until the list is reset.
    for (i, footprint) in staging.footprints.iter().enumerate() {
        #[cfg(feature = "diagnostics")]
        {
            let f = footprint.Footprint;
            // SAFETY: live resources.
            let (gpu, d) = unsafe { (staging.buffer.GetGPUVirtualAddress() + staging.base, texture.GetDesc()) };
            let len = u64::from(f.RowPitch) * u64::from(staging.rows[i]) * u64::from(f.Depth);
            list.note_read(
                || {
                    format!(
                        "texture upload {resource:?} ({}x{}x{} f{} mips {}) sub {i} of {} from staging {gpu:#x}+{:#x}: offset {:#x} {}x{}x{} pitch {}",
                        d.Width,
                        d.Height,
                        d.DepthOrArraySize,
                        d.Format.0,
                        d.MipLevels,
                        staging.footprints.len(),
                        staging.size,
                        footprint.Offset,
                        f.Width,
                        f.Height,
                        f.Depth,
                        f.RowPitch
                    )
                },
                (gpu + footprint.Offset, len),
            );
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&staging.buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                // The footprints are laid out from the staging memory's
                // start, which is `base` into the buffer (a multiple of
                // 4 KiB, so the placement stays 512-byte aligned).
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: footprint.Offset + staging.base,
                    Footprint: footprint.Footprint,
                },
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    // When the list is submitted, its copy takes what the staging buffer
    // holds by then: noted before the check below, which sets `dirty`
    // again if it detiles (the next list then uploads once more).
    {
        let staging = staging.clone();
        list.defer_host(move || {
            staging.uploaded.store(true, Relaxed);
            staging.dirty.store(false, Relaxed);
        });
    }
    // Levels on demand (`crate::tiles`): after the check below, in the
    // order of this list's steps, and before the list is executed, the
    // levels the staging buffer holds as the title's are given the card's
    // memory if they have none. The list copies every level, so each one
    // given here is written whole by it (the rest are copies into no
    // memory, which the card drops).
    let on_demand = if staging.on_demand { crate::resource::host_tiled(resource) } else { None };
    let Some(on_demand) = on_demand else {
        // Once per submission however many of its lists sample the texture.
        list.defer_texture(staging);
        return;
    };
    list.defer_texture(staging.clone());
    list.defer_host(move || {
        let waiting = on_demand.uploaded(staging.detiled.load(Relaxed), staging.memory as u64);
        staging.waiting.store(if waiting { crate::host::moment().max(1) } else { 0 }, Relaxed);
        // A texture picked by index has no staging buffer after this
        // upload: its slots are made due again by the texture pass.
        if waiting {
            crate::descriptor::host::levels_waiting(staging.memory as u64);
        }
    });
}

/// Levels on demand: the levels a list claimed for its zeros
/// (`crate::tiles::Tiled::claim`), held by the list's submission-time
/// step. A list that is reset without having been submitted gives the
/// claim up, or the next list that writes those levels would send no zeros
/// and what its write does not cover would be what the tiles' last owner
/// left.
struct Claimed {
    on_demand: crate::resource::OnDemand,
    fresh: u32,
    ran: std::sync::atomic::AtomicBool,
}

impl Drop for Claimed {
    fn drop(&mut self) {
        if self.fresh != 0
            && !*self.ran.get_mut()
            && let Some(tiled) = self.on_demand.tiled()
        {
            tiled.unclaim(self.fresh);
        }
    }
}

/// Levels on demand (`crate::tiles`): the card itself is about to write
/// title texture `resource` in `list` (a copy or a resolve into level
/// `level`, or into every level). If its host copy `texture` was made
/// without memory, the levels written get theirs when the list is
/// submitted, ahead of it on the queue; the ones that have none yet are
/// first written with zeros, here in the same list, so that what the
/// card's write does not cover is not what the tiles' last owner left.
/// To be called before the list is brought to the state of that write.
/// shortcut: two lists that write the same new level, submitted in the
/// other order than recorded, lose the first one's write under the zeros
/// of the other; so does an upload recorded and not yet submitted.
pub(crate) fn card_writes(list: &HostList, resource: This, texture: &ID3D12Resource, level: Option<u32>) {
    let Some(on_demand) = crate::resource::host_tiled(resource) else {
        return;
    };
    let Some(tiled) = on_demand.tiled() else {
        return;
    };
    let levels = match level {
        Some(level) => tiled.with_level(level),
        None => tiled.every_level(),
    };
    if levels == 0 {
        return;
    }
    let fresh = tiled.claim(levels);
    // Of them, the ones whose zeros could not be recorded.
    let mut unzeroed = if host().is_none() { fresh } else { 0 };
    if fresh != 0
        && let Some(host) = host()
    {
        const COPY_DEST: u32 = 0x400;
        crate::list::ensure_state(list, resource, COPY_DEST);
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        for level in (0..u32::from(d.MipLevels).min(32)).filter(|&level| fresh & (1u32 << level) != 0) {
            let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let mut bytes = 0u64;
            // SAFETY: outputs for one subresource.
            unsafe {
                host.device
                    .GetCopyableFootprints(&d, level, 1, 0, Some(&mut footprint), None, None, Some(&mut bytes));
            }
            let Some((buffer, offset, data)) = list.upload(host, bytes) else {
                // (With the levels that share its tiles, if it is one of
                // the small ones.)
                unzeroed |= tiled.with_level(level);
                continue;
            };
            // SAFETY: a mapped buffer of `bytes` bytes, just made.
            unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: a borrowed pointer the location never releases.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: level },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above; the list holds the buffer until it is reset.
                pResource: unsafe { std::mem::transmute_copy(&buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                // Laid out from the memory's start, `offset` into the buffer.
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT { Offset: footprint.Offset + offset, ..footprint },
                },
            };
            // SAFETY: an open list; both locations name live resources.
            unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
    }
    // A new level without its zeros gets no tiles from this list: the
    // card's write into it is lost (the level reads zeros, as before the
    // write), which is less wrong than a part write over what the tiles'
    // last owner left. The next list that writes it claims it again.
    unzeroed &= fresh;
    if unzeroed != 0 {
        tiled.unclaim(unzeroed);
    }
    let levels = levels & !unzeroed;
    if levels == 0 {
        return;
    }
    let memory = crate::resource::texture_memory(resource).map_or(0, |(memory, _)| memory as u64);
    let step = Claimed {
        on_demand,
        fresh: fresh & !unzeroed,
        ran: std::sync::atomic::AtomicBool::new(false),
    };
    list.defer_host(move || {
        step.ran.store(true, std::sync::atomic::Ordering::Relaxed);
        step.on_demand.card_wrote(levels, memory);
    });
}

impl Staging {
    /// Levels on demand: what says, beside the mapping, which levels of
    /// the texture are the title's now. First the lowest level a live view
    /// of the texture shows (0 without a view that says): the title's own
    /// word, a texture it holds from level 3 on is viewed from level 3.
    /// Then where the next texture placed in this one's span starts, in
    /// bytes from this one's start (`u64::MAX`: none does): for a texture
    /// viewed from level 0 although a neighbour sits where its large
    /// levels would be.
    fn judgement(&self) -> [u64; 2] {
        let start = self.memory as u64;
        let first = crate::descriptor::host::first_viewed(start).map_or(0, u64::from);
        let limit = crate::descriptor::host::next_start(start, start.saturating_add(self.layout.size))
            .map_or(u64::MAX, |at| at - start);
        [first, limit]
    }

    /// What [`Staging::judgement`] said at the last detiling.
    fn judged(&self) -> [u64; 2] {
        use std::sync::atomic::Ordering::Relaxed;
        [self.judged[0].load(Relaxed), self.judged[1].load(Relaxed)]
    }

    /// Levels on demand: the levels that are the title's now, a bit a
    /// level. A level is held if no view starts above it, its bytes end
    /// before the next texture starts, and all of them are mapped. The
    /// smallest levels share the texture's first block and are held
    /// together. (The console layout: that block first, then the other
    /// levels from the smallest up, level 0 last:
    /// `victory_formats::texture::Layout`.) A texture this cannot tell
    /// the levels of holds them all.
    fn held_levels(&self, [first, limit]: [u64; 2]) -> u32 {
        let mips = &self.layout.mips;
        if self.layout.slices != 1 || mips.is_empty() || mips.len() > 32 {
            return u32::MAX;
        }
        let tail = (self.layout.first_tail_mip as usize).min(mips.len());
        let there = |from: u64, to: u64| {
            to > from && to <= limit && committed(self.memory.wrapping_add(from as usize), (to - from) as usize)
        };
        let mut held = 0u32;
        for (level, m) in mips.iter().enumerate().take(tail) {
            let end = if level == 0 { self.layout.slice_size } else { mips[level - 1].offset };
            if level as u64 >= first && there(m.offset, end) {
                held |= 1 << level;
            }
        }
        if tail < mips.len() && there(0, self.layout.alignment.min(self.layout.slice_size)) {
            for level in tail..mips.len() {
                if level as u64 >= first {
                    held |= 1 << level;
                }
            }
        }
        held
    }

    /// Forgets what the staging buffer was last filled from: the next
    /// refresh detiles anew, whether the memory changed or not.
    fn forget(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Last::default();
    }

    /// Whether its levels get the card's memory on demand (`crate::tiles`).
    pub(crate) fn on_demand(&self) -> bool {
        self.on_demand
    }

    /// The title memory the texture is read from: address and bytes.
    pub(crate) fn memory(&self) -> (usize, usize) {
        (self.memory as usize, self.layout.size as usize)
    }

    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all).
    pub(crate) fn refresh(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let read = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
    }

    /// Returns the bytes of title memory read.
    fn refresh_now(&self) -> usize {
        let whole = std::time::Instant::now();
        let read = self.refresh_timed();
        let us = whole.elapsed().as_micros() as u64;
        REFRESH_WAITS[3].fetch_add(us, std::sync::atomic::Ordering::Relaxed);
        crate::framelog::add(crate::framelog::T::RefreshUs, us);
        read
    }

    fn refresh_timed(&self) -> usize {
        let size = self.layout.size as usize;
        let start = self.memory as usize;
        let mut last = waited(0, || self.last.lock().unwrap_or_else(|e| e.into_inner()));
        let last = &mut *last;
        if let Some((now, written)) = waited(1, || crate::watch::writes(start, start + size, false)) {
            if last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked) {
                return 0;
            }
            last.asked = now;
            // Read through the copy, which stands unmapped pages: memory of
            // big pools is too slow to ask page by page.
            with_read(size, |read| {
                crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
                self.detile(read);
            });
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, size as u64);
            return size;
        }
        // View memory has no write-watch, and a texture there was hashed
        // whole at every check: 229 MB and 51 ms a frame in a gameplay view
        // where two textures change (run 58). It is guarded now, as buffers
        // in view memory are: a write of the title's to it faults once and
        // is noted, a mapping is announced by the runtime. While neither
        // happened the texture is as it was. What the guard cannot see (a
        // write to the same pages through another view) is still found by
        // the hash, which is taken every eighth frame only, by the
        // texture's address, so the textures take turns; and at every
        // refresh for a while once it found one (`Last::passes_over`).
        // `VICTORY_TEXTURE_GUARD=0` hashes at every check, as before.
        static GUARD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *GUARD.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_GUARD").is_none_or(|v| v != "0"))
            && let Some((now, written)) = waited(1, || crate::watch::writes(start, start + size, true))
        {
            let unwritten = last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked);
            last.asked = now;
            let frame = crate::frame::current();
            return last.refresh_guarded(frame, start, unwritten, |last| {
                // A partly mapped texture within its 30 frames, on its hash
                // turn with no write or mapping seen: what the read would
                // come to (`read_partly` reads nothing then), without the
                // kernel copy that fails at its first missing page (2 us,
                // and ~50 us while the runtime remaps: `membench`). A
                // mapping under it resets its guards, so it is read then.
                if unwritten && partly_no_wait() && last.partial.is_some_and(|at| frame < at + 30) {
                    UNLOCKED[5].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return 0;
                }
                self.read_now(last, size)
            });
        }
        self.read_now(last, size)
    }

    /// Reads the texture's memory, and detiles it if its hash changed (or
    /// if it is watched no other way). Returns the bytes read.
    fn read_now(&self, last: &mut Last, size: usize) -> usize {
        // Read through the kernel's copy, which needs no lock: whole, or on
        // to the read under the lock below (`copy_first`).
        if copy_first() {
            use std::sync::atomic::Ordering::Relaxed;
            // What of it is mapped, from the runtime's own word where the
            // texture lies in its placeholders (`crate::inplace::view_books`):
            // one known to have a page not there is not read through the
            // kernel up to that page, which fails there, nor asked about
            // page by page; it is read as a partly mapped one at once.
            if partly_no_wait()
                && let Some(parts) = crate::inplace::mapped_parts(self.memory as usize, size)
                && parts.iter().map(|p| p.1).sum::<usize>() < size
            {
                UNLOCKED[3].fetch_add(1, Relaxed);
                BOOKS[1].fetch_add(1, Relaxed);
                return self.read_partly_from(last, size, Some(&parts));
            }
            let read = with_read(size, |read| {
                if !crate::host::read_into(self.memory, read) {
                    return false;
                }
                // All of it is there now (`refresh_timed` passes over a
                // partly mapped one on its turns).
                if partly_no_wait() {
                    last.partial = None;
                }
                let hash = blake3::hash(read);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
                if last.hash != Some(hash) {
                    last.hash = Some(hash);
                    self.detile(read);
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
                }
                true
            });
            UNLOCKED[if read { 2 } else { 3 }].fetch_add(1, Relaxed);
            if read {
                return size;
            }
            // A page was not there: a texture streaming in, or one the
            // runtime is remapping right now. While it remaps, the texture
            // is read as a partly mapped one, without waiting for it
            // (`partly_no_wait`).
            if partly_no_wait() {
                match crate::watch::title_memory_reading_now() {
                    Some(mapped) => return self.read_locked(last, size, mapped),
                    None => {
                        UNLOCKED[4].fetch_add(1, Relaxed);
                        return self.read_partly(last, size);
                    }
                }
            }
        }
        // From here on the texture's memory may be read in place. Held until
        // that read is done: the runtime changes what title memory shows
        // only with this taken alone, so what `committed` finds mapped
        // stays mapped (see `crate::watch::title_memory_reading`; without
        // it a page went between the asking and the read in runs 71 and
        // 74).
        let mapped = if no_wait() {
            // Not waited for: while the runtime remaps title memory the
            // texture is read through the kernel instead, whole or not at
            // all (`no_wait`).
            let Some(mapped) = crate::watch::title_memory_reading_now() else {
                return self.refresh_unlocked(last, size);
            };
            mapped
        } else {
            waited(2, crate::watch::title_memory_reading)
        };
        self.read_locked(last, size, mapped)
    }

    /// [`Staging::read_now`] holding `_mapped` (the runtime remaps nothing
    /// meanwhile): partly mapped, or read in place.
    fn read_locked(&self, last: &mut Last, size: usize, _mapped: crate::watch::Reading) -> usize {
        if !committed(self.memory, size) {
            return self.read_partly(last, size);
        }
        if through_copy() {
            return with_title(self.memory, size, |tiled| {
                let hash = blake3::hash(tiled);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
                if last.hash == Some(hash) {
                    return size;
                }
                last.hash = Some(hash);
                self.detile(tiled);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
                size
            });
        }
        // SAFETY: committed memory of the texture's layout size, and it
        // stays committed while `_mapped` is held.
        let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
        let hash = blake3::hash(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        if last.hash == Some(hash) {
            return size;
        }
        last.hash = Some(hash);
        self.detile(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        size
    }

    /// Partly mapped: a streamed texture with only some mips' pages there,
    /// the usual state while it streams in. Read through the copy, which
    /// stands unmapped pages (zeros) and needs no lock, and upload what is
    /// there; nothing at all mapped changes nothing. Returns the bytes read.
    ///
    /// shortcut: read again at most every 30 frames (reading around
    /// unmapped pages is slow, and nothing tells us when pages arrive): a
    /// mip that streams in shows up to 30 frames late.
    fn read_partly(&self, last: &mut Last, size: usize) -> usize {
        self.read_partly_from(last, size, None)
    }

    /// [`Staging::read_partly`]; with `parts`, the parts of the memory that
    /// are mapped ([`crate::inplace::mapped_parts`]): those are read and
    /// the rest is zeros, as the kernel copy around holes reads it, without
    /// asking the kernel where the holes are. A part that cannot be read
    /// (the runtime remapping it this moment) reads as zeros too; its
    /// remapping makes the texture look again.
    fn read_partly_from(&self, last: &mut Last, size: usize, parts: Option<&[(usize, usize)]>) -> usize {
        let frame = crate::frame::current();
        if last.partial.is_some_and(|at| frame < at + 30) {
            return 0;
        }
        last.partial = Some(frame);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        let changed = with_read(size, |read| {
            match parts {
                Some(parts) => {
                    read.fill(0);
                    let start = self.memory as usize;
                    for &(at, n) in parts {
                        let piece = &mut read[at - start..at - start + n];
                        if !crate::host::read_into(at as *const u8, piece) {
                            piece.fill(0);
                        }
                    }
                }
                None => crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false),
            }
            let hash = blake3::hash(read);
            if last.hash == Some(hash) {
                return false;
            }
            last.hash = Some(hash);
            if read.iter().all(|&b| b == 0) {
                return false;
            }
            self.detile(read);
            true
        });
        #[cfg(feature = "diagnostics")]
        if changed {
            crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        }
        let _ = changed;
        size
    }

    /// A refresh while the runtime is remapping title memory ([`no_wait`]):
    /// the texture is read through the kernel, which fails if a page of it
    /// is not there at this moment. Then nothing is taken and the texture
    /// is looked at again at its next refresh (what was learnt about writes
    /// to it is forgotten, so that it is not passed over as unwritten).
    /// Returns the bytes read.
    fn refresh_unlocked(&self, last: &mut Last, size: usize) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let read = with_read(size, |read| {
            if !crate::host::read_into(self.memory, read) {
                return false;
            }
            let hash = blake3::hash(read);
            if last.hash != Some(hash) {
                last.hash = Some(hash);
                self.detile(read);
            }
            true
        });
        if read {
            UNLOCKED[0].fetch_add(1, Relaxed);
            size
        } else {
            UNLOCKED[1].fetch_add(1, Relaxed);
            last.asked = 0;
            0
        }
    }

    /// Detiles `tiled` (the texture's memory) into the staging buffer.
    fn detile(&self, tiled: &[u8]) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        // Levels on demand: which levels are the title's is judged here,
        // once, for what the staging buffer holds from now on. The step
        // that gives levels their memory goes by this answer.
        let held = if self.on_demand {
            use std::sync::atomic::Ordering::Relaxed;
            let judged = self.judgement();
            self.judged[0].store(judged[0], Relaxed);
            self.judged[1].store(judged[1], Relaxed);
            let held = self.held_levels(judged);
            self.detiled.store(held, Relaxed);
            held
        } else {
            u32::MAX
        };
        #[cfg(feature = "diagnostics")]
        if tiled.iter().all(|&b| b == 0) {
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.desc,
                "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        for (i, (footprint, &rows)) in self.footprints.iter().zip(&self.rows).enumerate() {
            let (mip, slice) = (i as u32 % self.mips, i as u32 / self.mips);
            let f = footprint.Footprint;
            let pitch = f.RowPitch as usize;
            let len = pitch * rows as usize * f.Depth as usize;
            // SAFETY: the subresource's part of the mapped staging buffer.
            let linear = unsafe {
                std::slice::from_raw_parts_mut(self.data.add(footprint.Offset as usize), len)
            };
            if mip < 32 && held & (1 << mip) == 0 {
                // Not the title's: zeros, not what lies in that memory.
                linear.fill(0);
                continue;
            }
            if let Err(e) =
                self.layout
                    .detile(tiled, mip, slice, linear, pitch, pitch * rows as usize)
            {
                tracing::warn!(?e, mip, slice, "texture not detiled");
                return;
            }
        }
    }
}

/// Lets go of a title texture's staging buffer (lists that recorded an
/// upload from it hold it until they have run); the next upload makes one
/// anew.
pub(crate) fn unstage(resource: This) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&(resource as usize));
    }
}

/// Where a staged title texture's memory is: address and bytes.
pub(crate) fn staged_memory(resource: This) -> Option<(u64, u64)> {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let (at, len) = map.as_ref()?.get(&(resource as usize))?.memory();
    Some((at as u64, len as u64))
}

/// A fingerprint of the title memory `[at, at + len)`, never 0: of all of
/// it up to 48 KiB, else of 16 KiB at its start, its middle and its end
/// (a texture streams in whole mips; the small ones lie first). Read
/// through the kernel, so pages not there count as zeros.
pub(crate) fn fingerprint(at: u64, len: u64) -> u64 {
    const WINDOW: usize = 16 << 10;
    thread_local! {
        static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let len = len as usize;
    let mut hasher = blake3::Hasher::new();
    READ.with(|read| {
        let mut read = read.borrow_mut();
        read.resize(3 * WINDOW, 0);
        let mut take = |from: usize, bytes: usize| {
            read[..bytes].fill(0);
            crate::host::copy_mapped(read.as_mut_ptr(), (at as usize + from) as *const u8, bytes, false);
            hasher.update(&read[..bytes]);
        };
        if len <= 3 * WINDOW {
            take(0, len);
        } else {
            take(0, WINDOW);
            take((len / 2) & !0xfff, WINDOW);
            take(len - WINDOW, WINDOW);
        }
    });
    let hash = hasher.finalize();
    let mut first = [0u8; 8];
    first.copy_from_slice(&hash.as_bytes()[..8]);
    u64::from_le_bytes(first) | 1
}

/// Whether a title texture has a staging buffer: a list recorded its
/// upload, and the lists that sample it check it at submission.
/// For the frame listing: whether a list has uploaded `resource`'s staging
/// buffer, and whether it was detiled since. None: it has none.
#[cfg(feature = "diagnostics")]
pub(crate) fn staged_state(resource: This) -> Option<(bool, bool)> {
    use std::sync::atomic::Ordering::Relaxed;
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let staging = map.as_ref()?.get(&(resource as usize))?;
    Some((staging.uploaded.load(Relaxed), staging.dirty.load(Relaxed)))
}

pub(crate) fn is_staged(resource: This) -> bool {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref().is_some_and(|map| map.contains_key(&(resource as usize)))
}

/// The bytes of a title texture's memory, and whether all of it is mapped,
/// if the texture has been staged for upload.
pub(crate) fn staged(resource: This) -> Option<(u64, bool)> {
    let s = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref()?.get(&(resource as usize))?.clone()
    };
    let size = s.layout.size as usize;
    let whole = match crate::inplace::mapped_bytes(s.memory as usize, size) {
        Some(mapped) => {
            BOOKS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            mapped == size as u64
        }
        None => committed(s.memory, size),
    };
    Some((size as u64, whole))
}

/// Since the last frame-rate line, questions about what of a texture's
/// memory is mapped answered from the runtime's own word
/// (`crate::inplace::view_books`) instead of the kernel's: the texture
/// pass's looks at a slot ([`mapped_bytes`]), refreshes of a texture known
/// to have a page not there, and the pass's looks whether a staged texture
/// is whole ([`staged`]).
pub(crate) static BOOKS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Whether `[at, at + len)` is committed memory (streamed textures may be
/// unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    if by_query() {
        return committed_by_query(at, len);
    }
    if len == 0 {
        return true;
    }
    let (there, of) = readable(at as usize, len, true);
    there == of
}

/// Whether the two questions above and below are put to `VirtualQuery`, as
/// they were (`VICTORY_COMMITTED_QUERY=1`). `VirtualQuery` answers with the
/// length of the run of pages in the same state, which it finds by walking
/// the page tables to the run's end: 8.8 ms for one question in the title's
/// 1.1 GB block (round 6). The pass over the heap's views asks three or
/// more of them for every texture slot that is due, 3,000 slots a frame
/// when the card has no room (run 75): that pass then ran 208-264 ms, and
/// while it ran the submitting thread's write-watch askings waited behind
/// the walks (run 76: 8-22 askings a frame of 6-21 ms each, 46-172 ms).
fn by_query() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COMMITTED_QUERY").is_some_and(|v| v == "1"))
}

/// Of the kernel's pages that hold `[at, at + len)` (`len > 0`): how many
/// can be read, and how many there are; with `all`, it stops at the first
/// that cannot. One question to the kernel for up to 256 pages at once
/// (`QueryWorkingSetEx`: the state of each page asked about, no walk); a
/// page not in the working set may still be there (never touched, or
/// trimmed), and a one-byte read through the kernel says.
/// A page that is committed and may not be read counts as not there, which
/// is what the callers ask for: they read or write the memory next.
fn readable(at: usize, len: usize, all: bool) -> (usize, usize) {
    use windows_sys::Win32::System::ProcessStatus::{K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const KERNEL_PAGE: usize = 4096;
    let (first, last) = (at / KERNEL_PAGE, at.saturating_add(len - 1) / KERNEL_PAGE);
    let of = last - first + 1;
    let mut there = 0;
    if by_reading() {
        for page in first..=last {
            let mut byte = [0u8];
            if crate::host::read_into((page * KERNEL_PAGE) as *const u8, &mut byte) {
                there += 1;
            } else if all {
                break;
            }
        }
        return (there, of);
    }
    // SAFETY: plain data.
    let mut info: [PSAPI_WORKING_SET_EX_INFORMATION; 256] = unsafe { std::mem::zeroed() };
    let mut page = first;
    while page <= last {
        let n = (last - page + 1).min(info.len());
        for (i, entry) in info[..n].iter_mut().enumerate() {
            entry.VirtualAddress = ((page + i) * KERNEL_PAGE) as *mut _;
        }
        let timed = crate::vmstat::time(crate::vmstat::Site::WorkingSet);
        // SAFETY: `n` entries of the size given; our own process.
        let asked = unsafe {
            K32QueryWorkingSetEx(
                GetCurrentProcess(),
                info.as_mut_ptr().cast(),
                (n * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
            )
        } != 0;
        drop(timed);
        for (i, entry) in info[..n].iter().enumerate() {
            // Bit 0 of the answer: the page is in the working set.
            // SAFETY: plain data the query filled in.
            let valid = asked && unsafe { entry.VirtualAttributes.Flags } & 1 != 0;
            let mut byte = [0u8];
            if valid || crate::host::read_into(((page + i) * KERNEL_PAGE) as *const u8, &mut byte) {
                there += 1;
            } else if all {
                return (there, of);
            }
        }
        page += n;
    }
    (there, of)
}

/// Whether [`readable`] asks each page with a one-byte read through the
/// kernel and not `QueryWorkingSetEx` first: on by default under Wine, off
/// elsewhere; `VICTORY_PROBE_READ=1` or `=0` says.
///
/// The answer is the same: a page counts as there if it is in the working
/// set or a one-byte read of it succeeds, and under Wine a page in the
/// working set (committed, not a guard page, present) can be read. But
/// there `QueryWorkingSetEx` takes ntdll's lock on the memory map
/// (dlls/ntdll/unix/virtual.c get_working_set_ex), which the runtime's
/// remapping holds across wineserver requests, while a read that succeeds
/// is a copy in our own process that takes nothing. Measured with the
/// membench test under Wine 11.22 at 40,000 views, 16 pages:
/// `QueryWorkingSetEx` 2 us idle, 10-28 us with one thread remapping,
/// 10-41 ms with two; sixteen one-byte reads 1.0-1.3 us in all three. A
/// read that fails still takes the lock (a fault), as the fallback read
/// after the question did. On Windows the question is one kernel call for
/// up to 256 pages and stays.
fn by_reading() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var_os("VICTORY_PROBE_READ") {
        Some(v) => v == "1",
        None => crate::host::on_wine(),
    })
}

fn committed_by_query(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut p = at as usize;
    let end = p + len;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = {
            let _timed = crate::vmstat::time(crate::vmstat::Site::QueryTexture);
            unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) }
        };
        if n == 0 || info.State != MEM_COMMIT {
            return false;
        }
        p = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// How many bytes of `[at, at + len)` are committed memory: all of a
/// texture that is there, some of one streaming in (its smaller mips lie
/// at the start of its memory, and their pages are mapped first), none of
/// one not there. With neither the first nor the last page committed the
/// answer is none without a look at the pages between (two queries for the
/// slots whose texture is away, which are asked again and again).
pub(crate) fn mapped_bytes(at: *const u8, len: usize) -> u64 {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    if len == 0 {
        return 0;
    }
    let start = at as usize;
    let end = start.saturating_add(len);
    // From the runtime's own word, without a question to the kernel that
    // faults for every block not there (`crate::inplace::view_books`).
    if let Some(mapped) = crate::inplace::mapped_bytes(start, len) {
        BOOKS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return mapped;
    }
    if !committed(at, 1) && !committed((end - 1) as *const u8, 1) {
        return 0;
    }
    if !by_query() {
        static BY_PAGE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *BY_PAGE.get_or_init(|| std::env::var_os("VICTORY_MAPPED_BY_PAGE").is_some_and(|v| v == "1")) {
            // By page, without the walk: whole pages, cut to the range.
            let (there, _) = readable(start, len, false);
            return (there as u64 * 4096).min(len as u64);
        }
        // By 64 KiB block, one page asked of each: the title's memory is
        // mapped in blocks of that size (the console's page), and the
        // answer is a measure of how much of a texture has arrived, not a
        // promise that every byte can be read (the upload reads through the
        // kernel). By page, a texture half there cost a failed kernel read
        // for each of its missing pages, every frame it stayed due: 1,663
        // textures "uploaded in part" kept the pass over the heap's views at
        // 90-160 ms a frame (run 81). `VICTORY_MAPPED_BY_PAGE=1`: by page.
        const BLOCK: usize = 64 << 10;
        let mut there = 0u64;
        let mut block = start & !(BLOCK - 1);
        while block < end {
            let (lo, hi) = (block.max(start), block.saturating_add(BLOCK).min(end));
            if readable(lo, 1, true).0 == 1 {
                there += (hi - lo) as u64;
            }
            block = block.saturating_add(BLOCK);
        }
        return there;
    }
    let (mut p, mut there) = (start, 0u64);
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = {
            let _timed = crate::vmstat::time(crate::vmstat::Site::QueryTexture);
            unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) }
        };
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize).min(end);
        if n == 0 || next <= p {
            break;
        }
        if info.State == MEM_COMMIT {
            there += (next - p) as u64;
        }
        p = next;
    }
    there
}

/// The staging state of a title texture, made on first use.
fn staging(resource: This, texture: &ID3D12Resource) -> Option<Arc<Staging>> {
    // Asked before the table is locked: the sweep lets go of a staging
    // buffer with its texture's host side locked, in that order.
    let on_demand = crate::resource::host_tiled(resource).is_some();
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(s) = map.get(&(resource as usize)) {
        return Some(s.clone());
    }
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        u32::from(d.MipLevels)
    } else {
        u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize)
    };
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    let (buffer, base, data, class, region) = take_staging(host, total)?;
    // (A slab keeps its own name.)
    if region.is_none() {
        crate::host::name(&buffer, || {
            format!(
                "staging of {resource:?} {}x{}x{} f{}",
                d.Width, d.Height, d.DepthOrArraySize, d.Format.0
            )
        });
    }
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    STAGED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    STAGED[1].fetch_add(total, std::sync::atomic::Ordering::Relaxed);
    let s = Arc::new(Staging {
        buffer,
        class,
        base,
        region,
        #[cfg(feature = "diagnostics")]
        size: total,
        data,
        footprints,
        rows,
        layout,
        mips: u32::from(d.MipLevels),
        bytes: total,
        memory,
        last: Mutex::default(),
        dirty: std::sync::atomic::AtomicBool::new(true),
        uploaded: std::sync::atomic::AtomicBool::new(false),
        used: std::sync::atomic::AtomicU64::new(crate::host::moment()),
        on_demand,
        detiled: std::sync::atomic::AtomicU32::new(0),
        judged: [const { std::sync::atomic::AtomicU64::new(0) }; 2],
        waiting: std::sync::atomic::AtomicU64::new(0),
    });
    map.insert(resource as usize, s.clone());
    Some(s)
}

#[cfg(test)]
mod title_memory_tests {
    use super::with_title;

    /// Title memory whose tail is reserved and not committed reads through
    /// the copy as its bytes and then zeros, without a fault; so does
    /// memory a page of which is taken away between two reads, and the
    /// zeros are zeros whatever the read buffer held before.
    #[test]
    fn an_uncommitted_tail_reads_as_zeros() {
        use windows_sys::Win32::System::Memory::*;
        const HEAD: usize = 0xb_0000;
        const SIZE: usize = 0x20_0000;
        // SAFETY: a private reservation of our own; only committed pages
        // are written in place, and nothing else reads them in place.
        unsafe {
            // The read buffer holds something else first.
            let other = vec![0x77u8; SIZE];
            with_title(other.as_ptr(), SIZE, |bytes| assert!(bytes.iter().all(|&b| b == 0x77)));
            let base = VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(VirtualAlloc(base.cast(), HEAD, MEM_COMMIT, PAGE_READWRITE), base.cast());
            base.write_bytes(0x5a, HEAD);
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..HEAD].iter().all(|&b| b == 0x5a), "what is mapped");
                assert!(bytes[HEAD..].iter().all(|&b| b == 0), "what is not: zeros");
            });
            // The title takes a page in the middle away: zeros from then
            // on, the rest as before.
            assert_ne!(VirtualFree(base.add(0x4_0000).cast(), 0x1_0000, MEM_DECOMMIT), 0);
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..0x4_0000].iter().all(|&b| b == 0x5a));
                assert!(bytes[0x4_0000..0x5_0000].iter().all(|&b| b == 0));
                assert!(bytes[0x5_0000..HEAD].iter().all(|&b| b == 0x5a));
                assert!(bytes[HEAD..].iter().all(|&b| b == 0));
            });
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    /// The texture both device-loss runs named last (16 texels wide, 4 or 8
    /// high, format 10, layout 0x100, console flags 0x300000; its memory
    /// began 0x300 into a page in run 73 and 0x700 in run 72, and here also
    /// 0xf00, so that a page line runs through it): what the detiling is
    /// given and what it writes are the same read in place and read through
    /// the copy, and the rows it writes end where the staging buffer the
    /// host sizes for it ends (896 bytes for 4 rows of 128 bytes 256 apart,
    /// 1,920 for 8).
    #[test]
    fn the_small_texture_of_runs_72_and_73_detiles_the_same_both_ways() {
        use windows_sys::Win32::System::Memory::*;
        for (height, within) in [(4u32, 0x300usize), (8, 0x700), (4, 0xf00), (8, 0xf00)] {
            // The description as the log printed it, word for word.
            let words: [u32; 14] = [3, 0, 0, 0, 16, 0, height, 65537, 10, 1, 0, 256, 3_145_728, 0];
            // SAFETY: fourteen words are a resource description (56 bytes,
            // plain numbers).
            let desc: crate::resource::ResourceDesc = unsafe { std::mem::transmute(words) };
            let layout = crate::resource::texture_layout(&desc)
                .expect("a console layout")
                .planes[0]
                .layout
                .clone();
            let size = layout.size as usize;
            let (row, pitch) = (16 * 8, 256usize);
            assert!(size >= row * height as usize, "the layout holds every texel");
            // Committed pages and a reserved one after them; the texture
            // begins `within` bytes into the first, as the title placed it.
            // SAFETY: our own reservation; the committed part is written
            // and read in place, by this thread alone.
            unsafe {
                let pages = size.div_ceil(4096) + 1;
                let base = VirtualAlloc(std::ptr::null(), (pages + 1) * 4096, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
                assert!(!base.is_null());
                assert_eq!(VirtualAlloc(base.cast(), pages * 4096, MEM_COMMIT, PAGE_READWRITE), base.cast());
                for i in 0..pages * 4096 {
                    base.add(i).write((i * 7 + i / 251) as u8);
                }
                let memory = base.add(within);
                let in_place = std::slice::from_raw_parts(memory, size);
                // The bytes: the same in place and through the copy.
                with_title(memory, size, |copied| assert_eq!(copied, in_place));
                // What the detiling writes: the same, and inside the bytes
                // the host's staging buffer for this texture has.
                let staged = (height as usize - 1) * pitch + row;
                assert_eq!(staged, if height == 4 { 896 } else { 1920 });
                let detile = |tiled: &[u8]| {
                    // The slice `Staging::detile` hands over: whole rows.
                    let mut linear = vec![0xeeu8; pitch * height as usize];
                    layout
                        .detile(tiled, 0, 0, &mut linear, pitch, pitch * height as usize)
                        .expect("detiled");
                    linear
                };
                let direct = detile(in_place);
                let through = with_title(memory, size, |copied| detile(copied));
                assert_eq!(direct, through);
                for y in 0..height as usize {
                    assert!(direct[y * pitch..][..row].iter().any(|&b| b != 0xee), "row {y} written");
                    assert!(direct[y * pitch + row..(y + 1) * pitch].iter().all(|&b| b == 0xee), "nothing past row {y}");
                }
                // A buffer of exactly the staged bytes is enough.
                let mut exact = vec![0u8; staged];
                layout
                    .detile(in_place, 0, 0, &mut exact, pitch, pitch * height as usize)
                    .expect("the staging buffer's bytes are enough");
                assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
            }
        }
    }
}

#[cfg(test)]
mod guard_tests {
    use super::{BLIND_FRAMES, HASH_EVERY, Last};
    use crate::watch::PAGE;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
        PAGE_READWRITE, UnmapViewOfFile,
    };

    const SIZE: usize = 2 * PAGE;

    /// Two views of one pagefile section: the texture's, which the guards
    /// watch, and the decoder's, through which it is written.
    struct Views([MEMORY_MAPPED_VIEW_ADDRESS; 2]);

    impl Drop for Views {
        fn drop(&mut self) {
            crate::watch::title_memory_remapped(self.0[0].Value as usize, SIZE);
            for view in self.0 {
                // SAFETY: our views, still mapped.
                unsafe { UnmapViewOfFile(view) };
            }
        }
    }

    /// Writes `byte` over the texture through `at` on a thread of its
    /// own, as the title's decoder threads do, and waits for it.
    fn write_from_another_thread(at: usize, byte: u8) {
        std::thread::spawn(move || {
            // SAFETY: `SIZE` committed read-write bytes of a view, which
            // nothing reads meanwhile (the caller waits).
            unsafe { std::ptr::write_bytes(at as *mut u8, byte, SIZE) };
        })
        .join()
        .unwrap();
    }

    /// A texture whose memory another thread rewrites every frame through
    /// another view of the same pages (the boot logo's video planes): the
    /// guards see none of it, and the texture was read one frame in
    /// eight. Once a read on its turn finds it changed it is read, and
    /// shows the frame's contents, at every frame; once the writes stop,
    /// one frame in eight again after `BLIND_FRAMES`. A write through the
    /// texture's own view is seen by the guards and read at once.
    #[test]
    fn a_texture_rewritten_through_another_view_is_read_every_frame() {
        // SAFETY: a new pagefile section mapped twice, read-write; the
        // views hold it after the handle is closed.
        let views = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                SIZE as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let views = [0, 1].map(|_| MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, SIZE));
            CloseHandle(section);
            assert!(views.iter().all(|v| !v.Value.is_null()));
            Views(views)
        };
        let [texture, decoder] = views.0.map(|v| v.Value as usize);
        let mut last = Last::default();
        // One refresh as `Staging::refresh_timed` makes it for guarded
        // memory; the read hashes what the texture's view shows, as
        // `Staging::read_now` does. Returns the byte read, if it read.
        let refresh = |last: &mut Last, frame: u64| -> (bool, Option<u8>) {
            let (now, written) = crate::watch::writes(texture, texture + SIZE, true).expect("view memory is guarded");
            let unwritten = last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked);
            last.asked = now;
            let mut seen = None;
            last.refresh_guarded(frame, texture, unwritten, |last| {
                // SAFETY: the texture's view: `SIZE` committed bytes, read
                // only while no thread writes them.
                let bytes = unsafe { std::slice::from_raw_parts(texture as *const u8, SIZE) };
                last.hash = Some(blake3::hash(bytes));
                seen = Some(bytes[0]);
                SIZE
            });
            (unwritten, seen)
        };
        // The first look reads it whole.
        assert_eq!(refresh(&mut last, 0), (false, Some(0)));
        // Every frame the decoder writes the frame's picture first.
        let mut first_read = None;
        for frame in 1..=40u64 {
            let picture = frame as u8;
            write_from_another_thread(decoder, picture);
            let (unwritten, seen) = refresh(&mut last, frame);
            assert!(unwritten, "frame {frame}: the guards do not see writes through another view");
            match first_read {
                None => {
                    if let Some(byte) = seen {
                        assert_eq!(byte, picture);
                        first_read = Some(frame);
                    }
                }
                Some(_) => assert_eq!(seen, Some(picture), "frame {frame}: read with its picture"),
            }
        }
        let first_read = first_read.expect("read on its turn");
        assert!(first_read <= HASH_EVERY, "found within {HASH_EVERY} frames, not at {first_read}");
        // The writes stop: read every frame until `BLIND_FRAMES` after
        // the last change, then on its turn only.
        let mut reads = 0;
        for frame in 41..41 + BLIND_FRAMES + 8 * HASH_EVERY {
            let (unwritten, seen) = refresh(&mut last, frame);
            assert!(unwritten);
            if frame < 40 + BLIND_FRAMES {
                assert_eq!(seen, Some(40), "frame {frame}: still read every frame");
            } else if seen.is_some() {
                reads += 1;
            }
        }
        assert!((7..=9).contains(&reads), "one frame in eight afterwards, read {reads} times in {} frames", 8 * HASH_EVERY);
        // A write through the texture's own view, from another thread: the
        // guards see it and the texture is read off its turn.
        let frame = (41 + BLIND_FRAMES + 8 * HASH_EVERY..)
            .find(|&f| !f.wrapping_add((texture >> 16) as u64).is_multiple_of(HASH_EVERY))
            .unwrap();
        write_from_another_thread(texture, 0xaa);
        assert_eq!(refresh(&mut last, frame), (false, Some(0xaa)));
    }
}

#[cfg(test)]
mod staging_pool_tests {
    use super::staging_class_upto;

    /// Under Wine: buffers up to 64 MiB are kept.
    fn staging_class(size: u64) -> u64 {
        staging_class_upto(size, 64 << 20)
    }

    /// A class holds its size, wastes at most a quarter of it (past the
    /// smallest class), grows with it, and is 0 (not kept) past 64 MiB.
    #[test]
    fn staging_classes_hold_their_sizes_within_a_quarter() {
        let mut last = 0;
        for size in (1..=(64u64 << 20)).step_by(4093).chain([64 << 10, (64 << 10) + 1, 1 << 20, (1 << 20) + 1, 64 << 20]) {
            let class = staging_class(size);
            assert!(class >= size, "{size}: {class}");
            if size > 64 << 10 {
                assert!(class * 4 <= size * 5 + 4, "{size}: {class}");
            }
            assert_eq!(class % 512, 0, "{size}: {class}");
            assert!(size < last || class >= staging_class(last), "{size}: {class}");
            last = last.max(size);
        }
        assert_eq!(staging_class(64 << 10), 64 << 10);
        assert_eq!(staging_class((64 << 10) + 1), 80 << 10);
        assert_eq!(staging_class(1 << 20), 1 << 20);
        assert_eq!(staging_class((1 << 20) + 1), 1280 << 10);
        assert_eq!(staging_class((64 << 20) + 1), 0);
        let classes: Vec<u64> = (1..1600u64).map(|i| staging_class(i * 40_000)).collect();
        assert!(classes.windows(2).all(|w| w[0] <= w[1]));
    }

    /// Off Wine buffers past 8 MiB are not kept (fxceless f85101b6's limit);
    /// the classes below it are the same.
    #[test]
    fn off_wine_staging_classes_stop_at_8_mib() {
        assert_eq!(staging_class_upto(8 << 20, 8 << 20), 8 << 20);
        assert_eq!(staging_class_upto((8 << 20) + 1, 8 << 20), 0);
        for size in (1..=(8u64 << 20)).step_by(65_521) {
            assert_eq!(staging_class_upto(size, 8 << 20), staging_class(size), "{size}");
        }
    }
}
