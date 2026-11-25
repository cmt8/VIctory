//! Upload memory carved from a few large upload-heap buffers kept for the
//! run ("slabs"), instead of an upload buffer made and freed per use.
//!
//! Every upload buffer is an allocation of the host's: under Proton's
//! vkd3d-proton a buffer of 2 MiB or more is a `vkAllocateMemory` of its
//! own, a smaller one a sub-allocation of 16 MiB chunks
//! (libs/vkd3d/memory.c `vkd3d_memory_info_allow_suballocate`,
//! `VKD3D_MEMORY_CHUNK_SIZE`), and all of them count against a budget of
//! the card's memory that the processor can see, a sixteenth of the card's
//! largest heap (`vkd3d_memory_info_init_budgets`: 1.5 GB on a 24 GB card).
//! Past that budget each allocation falls back to system memory
//! (`vkd3d_try_allocate_device_memory` passes over the memory types under
//! the budget, and `vkd3d_allocate_device_memory` tries again without
//! `DEVICE_LOCAL`, "falling back to system memory"), which costs about
//! 0.55 ms a buffer whatever its size (`membench` `staging_bench`), with a
//! lock error from that path printed under Proton each time. Texture staging buffers, the lists' upload
//! buffers and their scratch chunks were all made so.
//!
//! Here a region of a slab is handed out instead, and given back when the
//! one that took it lets go of it ([`Region`]'s drop). Who holds a region
//! holds it as long as a buffer of its own was held before: a texture's
//! staging until no texture or list holds it (lists hold what they read
//! until they are reset, after every submission of theirs ran), a list's
//! upload memory until the list is reset. So a region is taken again only
//! after the card is done reading it, as a buffer was freed only then.
//!
//! The slabs are made on first need, `VICTORY_UPLOAD_SLAB_MB` (128) each,
//! up to `VICTORY_UPLOAD_SLABS_MB` (768) in all, so that they fit in the
//! budget beside the host's own upload memory; a request a slab cannot
//! serve (larger than a quarter of a slab, or all slabs full) is made as
//! before. `VICTORY_UPLOAD_SLABS` turns them on (`1`) or off (`0`); on by
//! default under Wine, off elsewhere.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

/// Regions are whole multiples of this, at offsets that are too: enough
/// for a placed texture footprint (512 bytes), a constant buffer view
/// (256) and a buffer copy.
const GRAIN: u64 = 4 << 10;

/// Whether upload memory comes from the slabs (`VICTORY_UPLOAD_SLABS`; on
/// under Wine).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_UPLOAD_SLABS").as_deref().map(str::trim) {
        Ok("0") => false,
        Ok("1") => true,
        _ => crate::host::on_wine(),
    })
}

fn megabytes(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(default) << 20
}

/// The bytes of a slab (`VICTORY_UPLOAD_SLAB_MB`, 128; at least 4 MiB).
fn slab_bytes() -> u64 {
    static BYTES: OnceLock<u64> = OnceLock::new();
    *BYTES.get_or_init(|| megabytes("VICTORY_UPLOAD_SLAB_MB", 128).max(4 << 20).next_multiple_of(GRAIN))
}

/// The most bytes of slabs made (`VICTORY_UPLOAD_SLABS_MB`, 768).
fn most_bytes() -> u64 {
    static BYTES: OnceLock<u64> = OnceLock::new();
    *BYTES.get_or_init(|| megabytes("VICTORY_UPLOAD_SLABS_MB", 768))
}

/// Since the start: regions handed out and their bytes; requests made as
/// before because they were too big, and because the slabs were full;
/// slabs made. And now: bytes held in regions, and the most ever.
pub(crate) static COUNTS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

/// The free space of the slabs: by size (for the best fit) and by place
/// (to join a region given back with free neighbours).
#[derive(Default)]
struct Free {
    /// (bytes, slab, offset) of every free run.
    by_size: BTreeSet<(u64, u32, u64)>,
    /// Per slab: offset to bytes of every free run.
    by_place: Vec<BTreeMap<u64, u64>>,
}

impl Free {
    /// A new slab of `bytes`, all free: its number.
    fn add(&mut self, bytes: u64) -> u32 {
        let slab = self.by_place.len() as u32;
        self.by_place.push(BTreeMap::from([(0, bytes)]));
        self.by_size.insert((bytes, slab, 0));
        slab
    }

    /// The smallest free run of at least `bytes` (a multiple of [`GRAIN`]),
    /// its front taken: slab and offset.
    fn take(&mut self, bytes: u64) -> Option<(u32, u64)> {
        let &(run, slab, offset) = self.by_size.range((bytes, 0, 0)..).next()?;
        self.by_size.remove(&(run, slab, offset));
        let place = &mut self.by_place[slab as usize];
        place.remove(&offset);
        if run > bytes {
            place.insert(offset + bytes, run - bytes);
            self.by_size.insert((run - bytes, slab, offset + bytes));
        }
        Some((slab, offset))
    }

    /// `bytes` at `offset` of `slab` are free again, joined with the free
    /// runs on either side.
    fn give(&mut self, slab: u32, offset: u64, bytes: u64) {
        let place = &mut self.by_place[slab as usize];
        let (mut start, mut len) = (offset, bytes);
        if let Some((&before, &run)) = place.range(..offset).next_back()
            && before + run == offset
        {
            debug_assert!(before + run <= offset);
            place.remove(&before);
            self.by_size.remove(&(run, slab, before));
            start = before;
            len += run;
        }
        if let Some(&run) = place.get(&(offset + bytes)) {
            place.remove(&(offset + bytes));
            self.by_size.remove(&(run, slab, offset + bytes));
            len += run;
        }
        debug_assert!(place.range(start..start + len).next().is_none(), "a region given back twice");
        place.insert(start, len);
        self.by_size.insert((len, slab, start));
    }
}

/// A slab: its buffer, mapping and host GPU address.
struct Slab {
    buffer: ID3D12Resource,
    data: usize,
    gpu: u64,
}

struct Arena {
    free: Free,
    slabs: Vec<Slab>,
    /// Whether a slab could not be made: none is tried again.
    failed: bool,
}

// SAFETY: D3D12 resources are free-threaded; the mappings are persistent
// mappings of upload buffers kept for the run.
unsafe impl Send for Arena {}

static ARENA: Mutex<Arena> = Mutex::new(Arena {
    free: Free { by_size: BTreeSet::new(), by_place: Vec::new() },
    slabs: Vec::new(),
    failed: false,
});

/// Held while a slab is made, so that two threads do not make one each.
static GROWING: Mutex<()> = Mutex::new(());

/// A region of a slab, mapped, given back when dropped.
pub(crate) struct Region {
    buffer: ID3D12Resource,
    slab: u32,
    offset: u64,
    bytes: u64,
    data: *mut u8,
    gpu: u64,
}

// SAFETY: the buffer is free-threaded; the mapping is the region's alone
// while it is held.
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Region {
    /// The slab's buffer, which the region lies in.
    pub(crate) fn buffer(&self) -> &ID3D12Resource {
        &self.buffer
    }

    /// Where in [`Region::buffer`] the region starts (a multiple of 4 KiB).
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    /// The region's mapping: as many bytes as asked for, or more.
    pub(crate) fn data(&self) -> *mut u8 {
        self.data
    }

    /// The region's host GPU address.
    pub(crate) fn gpu(&self) -> u64 {
        self.gpu
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        ARENA.lock().unwrap_or_else(|e| e.into_inner()).free.give(self.slab, self.offset, self.bytes);
        COUNTS[5].fetch_sub(self.bytes, Relaxed);
    }
}

/// A region of at least `size` bytes (not zeroed: it may hold what its
/// last holder wrote), or `None` where the slabs are off, the request is
/// too big for them or they are full: the caller makes its own buffer as
/// before.
pub(crate) fn take(host: &crate::host::Host, size: u64) -> Option<Region> {
    if !on() {
        return None;
    }
    let bytes = size.max(1).next_multiple_of(GRAIN);
    if bytes > slab_bytes() / 4 {
        COUNTS[2].fetch_add(1, Relaxed);
        return None;
    }
    let mut grown = false;
    loop {
        let count;
        {
            let mut arena = ARENA.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((slab, offset)) = arena.free.take(bytes) {
                let s = &arena.slabs[slab as usize];
                COUNTS[0].fetch_add(1, Relaxed);
                COUNTS[1].fetch_add(bytes, Relaxed);
                let now = COUNTS[5].fetch_add(bytes, Relaxed) + bytes;
                COUNTS[6].fetch_max(now, Relaxed);
                return Some(Region {
                    buffer: s.buffer.clone(),
                    slab,
                    offset,
                    bytes,
                    data: (s.data + offset as usize) as *mut u8,
                    gpu: s.gpu + offset,
                });
            }
            if grown || arena.failed || (arena.slabs.len() as u64 + 1) * slab_bytes() > most_bytes() {
                COUNTS[3].fetch_add(1, Relaxed);
                return None;
            }
            count = arena.slabs.len();
        }
        grown = grow(host, count);
    }
}

/// Makes a slab unless another thread made one since there were `count`;
/// whether there is a new one to try.
fn grow(host: &crate::host::Host, count: usize) -> bool {
    let _growing = GROWING.lock().unwrap_or_else(|e| e.into_inner());
    {
        let arena = ARENA.lock().unwrap_or_else(|e| e.into_inner());
        if arena.slabs.len() != count {
            return true;
        }
        if arena.failed {
            return false;
        }
    }
    // Made outside the arena's lock: regions are taken and given back
    // meanwhile.
    let started = std::time::Instant::now();
    let made = host.upload_buffer(slab_bytes());
    let ms = started.elapsed().as_secs_f64() * 1e3;
    let mut arena = ARENA.lock().unwrap_or_else(|e| e.into_inner());
    let Some((buffer, data)) = made else {
        arena.failed = true;
        tracing::warn!(bytes = slab_bytes(), "no upload slab: upload memory is made per use from now on");
        return false;
    };
    crate::host::name(&buffer, || format!("upload slab {}", arena.slabs.len()));
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    arena.free.add(slab_bytes());
    arena.slabs.push(Slab { buffer, data: data as usize, gpu });
    COUNTS[4].fetch_add(1, Relaxed);
    tracing::info!(
        slabs = arena.slabs.len(),
        bytes = slab_bytes(),
        ms = format_args!("{ms:.1}"),
        "an upload slab was made (VICTORY_UPLOAD_SLABS=0: upload buffers made per use)"
    );
    true
}

/// For the status line, per `frames` since the last: regions handed out
/// and their megabytes, requests made as before (too big, slabs full);
/// slabs and their megabytes; megabytes held now, and the most ever.
pub(crate) fn line(frames: f64) -> Option<String> {
    if !on() {
        return None;
    }
    let [taken, bytes, big, full] = [0, 1, 2, 3].map(|i| COUNTS[i].swap(0, Relaxed) as f64 / frames);
    if taken + big + full == 0.0 {
        return None;
    }
    let slabs = COUNTS[4].load(Relaxed);
    Some(format!(
        "upload memory per frame from the slabs (VICTORY_UPLOAD_SLABS=0: a buffer made per use): {taken:.1} regions \
         ({:.1} MB); made as before: {big:.1} too big, {full:.1} with the slabs full; {slabs} slabs of {} MB, {} MB \
         held now, {} MB at most",
        bytes / 1e6,
        slab_bytes() >> 20,
        COUNTS[5].load(Relaxed) >> 20,
        COUNTS[6].load(Relaxed) >> 20,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On the host GPU: a texture copied from a placed footprint in a slab
    /// region that does not start the slab, and a buffer copied from such a
    /// region, hold the bytes written there; regions held at once do not
    /// overlap.
    #[test]
    fn copies_from_regions_at_their_offsets_hold_their_bytes() {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        if !on() {
            return;
        }
        const SIDE: u32 = 64;
        const BYTES: u64 = SIDE as u64 * SIDE as u64 * 4;
        // A few regions, so that at least one starts past its slab's start.
        let regions: Vec<Region> = (0..4).map(|_| take(host, BYTES).expect("a region")).collect();
        for (i, a) in regions.iter().enumerate() {
            assert_eq!(a.offset() % GRAIN, 0);
            for b in &regions[i + 1..] {
                let same = a.buffer().clone() == b.buffer().clone();
                assert!(!same || a.offset() + BYTES <= b.offset() || b.offset() + BYTES <= a.offset());
            }
        }
        let region = regions.iter().find(|r| r.offset() > 0).expect("a region past the start");
        let pattern: Vec<u8> = (0..BYTES).map(|i| (i * 7 + i / 251) as u8).collect();
        // SAFETY: the region's mapping holds `BYTES` bytes.
        unsafe { std::ptr::copy_nonoverlapping(pattern.as_ptr(), region.data(), BYTES as usize) };
        let heap = |kind| D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
        let mut desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(SIDE),
            Height: SIDE,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let mut texture: Option<ID3D12Resource> = None;
        let mut copied: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of resources of the test's own.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &heap(D3D12_HEAP_TYPE_DEFAULT),
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut texture,
                )
                .unwrap();
            desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: BYTES,
                Height: 1,
                Format: Default::default(),
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..desc
            };
            host.device
                .CreateCommittedResource(
                    &heap(D3D12_HEAP_TYPE_DEFAULT),
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut copied,
                )
                .unwrap();
        }
        let back_bytes = 2 * BYTES;
        let back = host.read_back_buffer(back_bytes).unwrap();
        let (texture, copied) = (texture.unwrap(), copied.unwrap());
        let footprint = |offset| D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: offset,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                Width: SIDE,
                Height: SIDE,
                Depth: 1,
                RowPitch: SIDE * 4,
            },
        };
        let placed = |buffer: &ID3D12Resource, offset| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint(offset) },
        };
        let whole = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        let list = host.new_list().unwrap();
        // SAFETY: an open list and live resources; every range lies inside
        // its buffer.
        unsafe {
            let l = &list.list;
            l.CopyTextureRegion(&whole, 0, 0, 0, &placed(region.buffer(), region.offset()), None);
            l.CopyBufferRegion(&copied, 0, region.buffer(), region.offset(), BYTES);
            l.ResourceBarrier(&[
                crate::host::transition(&texture, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE),
                crate::host::transition(&copied, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE),
            ]);
            l.CopyTextureRegion(&placed(&back, 0), 0, 0, 0, &whole, None);
            l.CopyBufferRegion(&back, BYTES, &copied, 0, BYTES);
        }
        list.close();
        host.execute(&[&list]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copies ran (execute waits); the buffer holds
        // `back_bytes`.
        let read = unsafe {
            back.Map(0, Some(&D3D12_RANGE { Begin: 0, End: back_bytes as usize }), Some(&mut mapped)).unwrap();
            std::slice::from_raw_parts(mapped.cast::<u8>(), back_bytes as usize).to_vec()
        };
        assert!(read[..BYTES as usize] == pattern[..], "the texture copied from the region");
        assert!(read[BYTES as usize..] == pattern[..], "the buffer copied from the region");
    }

    #[test]
    fn free_runs_are_taken_best_fit_and_joined_when_given_back() {
        let mut free = Free::default();
        let a = free.add(64 * GRAIN);
        let b = free.add(8 * GRAIN);
        // The smaller slab fits best.
        assert_eq!(free.take(4 * GRAIN), Some((b, 0)));
        assert_eq!(free.take(4 * GRAIN), Some((b, 4 * GRAIN)));
        assert_eq!(free.take(4 * GRAIN), Some((a, 0)));
        assert_eq!(free.take(GRAIN), Some((a, 4 * GRAIN)));
        assert_eq!(free.take(2 * GRAIN), Some((a, 5 * GRAIN)));
        // Given back out of order, they join into the slab's one run.
        free.give(a, 4 * GRAIN, GRAIN);
        free.give(a, 0, 4 * GRAIN);
        assert_eq!(free.by_place[a as usize].get(&0), Some(&(5 * GRAIN)));
        free.give(a, 5 * GRAIN, 2 * GRAIN);
        assert_eq!(free.by_place[a as usize].len(), 1);
        assert_eq!(free.by_place[a as usize].get(&0), Some(&(64 * GRAIN)));
        free.give(b, 4 * GRAIN, 4 * GRAIN);
        free.give(b, 0, 4 * GRAIN);
        assert_eq!(free.by_size.len(), 2);
        assert_eq!(free.take(65 * GRAIN), None);
        assert_eq!(free.take(64 * GRAIN), Some((a, 0)));
    }

    #[test]
    fn many_regions_taken_and_given_back_leave_the_slabs_whole() {
        let mut free = Free::default();
        for _ in 0..3 {
            free.add(1024 * GRAIN);
        }
        let mut held = Vec::new();
        let mut seed = 12345u64;
        for round in 0..20_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            if held.is_empty() || (seed >> 33) % 3 != 0 {
                let bytes = ((seed >> 40) % 64 + 1) * GRAIN;
                if let Some((slab, offset)) = free.take(bytes) {
                    // No overlap with anything held.
                    assert!(
                        held.iter().all(|&(s, o, b)| s != slab || o + b <= offset || offset + bytes <= o),
                        "round {round}"
                    );
                    held.push((slab, offset, bytes));
                }
            } else {
                let at = (seed >> 20) as usize % held.len();
                let (slab, offset, bytes) = held.swap_remove(at);
                free.give(slab, offset, bytes);
            }
        }
        for (slab, offset, bytes) in held.drain(..) {
            free.give(slab, offset, bytes);
        }
        assert_eq!(free.by_size.len(), 3);
        assert!(free.by_place.iter().all(|p| p.len() == 1 && p.get(&0) == Some(&(1024 * GRAIN))));
    }
}
