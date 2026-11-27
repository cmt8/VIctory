//! Breadcrumbs of what goes to the card (`VICTORY_CRUMBS=1`, diagnostics
//! builds; off by default): every list handed to a queue and every fence
//! signalled behind it, and for each list what it holds that reaches memory
//! by an address of ours, with the address, the offsets and the buffer's
//! bytes (`victory_core::crumbs` has the records). They go into a file
//! mapped into the process, `victory-crumbs.bin` in the working directory
//! (another value than `1` names the file; `VICTORY_CRUMBS_MB` its size,
//! 256 by default): a page fault on the card takes the device and often the
//! process, and the log's last second with them, but what was written into
//! the mapping is the system's from that moment on.
//!
//! A thread of ours watches the main fence: each value it completes is a
//! record, so the file says which submissions the card had not finished,
//! and a lost device is one too. `cargo xtask crumbs <file>` reads it.
//!
//! With the switch off every call here is one load of a flag.

use std::os::windows::io::AsRawHandle;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use victory_core::crumbs::{Ring, WORDS, kind};
pub(crate) use victory_core::crumbs::{exec as sent, view as views};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;
use windows_sys::Win32::System::Memory as mem;

struct Crumbs {
    ring: Ring,
    began: std::time::Instant,
    /// The mapping, and the file under it.
    at: usize,
    file: std::fs::File,
}

static ON: OnceLock<Option<Crumbs>> = OnceLock::new();

fn crumbs() -> Option<&'static Crumbs> {
    ON.get_or_init(|| {
        let name = std::env::var_os("VICTORY_CRUMBS").filter(|v| !v.is_empty() && v != "0")?;
        let path = if name == "1" { std::path::PathBuf::from("victory-crumbs.bin") } else { std::path::PathBuf::from(name) };
        let megabytes = std::env::var("VICTORY_CRUMBS_MB").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(256).clamp(1, 4096);
        match open(&path, (megabytes << 20) as usize) {
            Ok(made) => {
                tracing::warn!(
                    "VICTORY_CRUMBS: what goes to the card is noted in {} ({} records, the oldest overwritten); read it with \
                     `cargo xtask crumbs <file>`",
                    path.display(),
                    made.ring.capacity()
                );
                Some(made)
            }
            Err(e) => {
                tracing::error!(error = %e, "VICTORY_CRUMBS: no file {}: nothing is noted", path.display());
                None
            }
        }
    })
    .as_ref()
}

/// A ring in a file of `bytes` bytes at `path`, made anew and mapped.
fn open(path: &std::path::Path, bytes: usize) -> std::io::Result<Crumbs> {
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
    file.set_len(bytes as u64)?;
    // SAFETY: a mapping of the whole file just made, read and written, kept
    // for as long as the process lives (the handle of the mapping too).
    let at = unsafe {
        let mapping = mem::CreateFileMappingW(file.as_raw_handle().cast(), std::ptr::null(), mem::PAGE_READWRITE, 0, 0, std::ptr::null());
        if mapping.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let view = mem::MapViewOfFile(mapping, mem::FILE_MAP_WRITE | mem::FILE_MAP_READ, 0, 0, bytes).Value;
        if view.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        view as usize
    };
    let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    // SAFETY: the view: page aligned, `bytes` long, ours alone and for good.
    let ring = unsafe { Ring::new(at as *mut u8, bytes, started) }.ok_or_else(|| std::io::Error::other("the file is too small for a record"))?;
    Ok(Crumbs {
        ring,
        began: std::time::Instant::now(),
        at,
        file,
    })
}

/// Whether anything is noted.
pub(crate) fn on() -> bool {
    static IS: OnceLock<bool> = OnceLock::new();
    *IS.get_or_init(|| crumbs().is_some())
}

fn put(kind: u8, words: [u64; WORDS]) {
    if let Some(c) = crumbs() {
        c.ring.put(kind, c.began.elapsed().as_micros() as u64, words);
    }
}

/// What the mapping holds goes to the file now (the system writes it in
/// its own time otherwise, which a machine that has to be switched off
/// does not give it).
fn flush() {
    if let Some(c) = crumbs() {
        c.flush();
    }
}

impl Crumbs {
    fn flush(&self) {
        // SAFETY: the whole view.
        unsafe { mem::FlushViewOfFile(self.at as *const _, 0) };
        let _ = self.file.sync_data();
    }
}

fn raw(object: &impl Interface) -> u64 {
    object.as_raw() as usize as u64
}

/// `lists` are handed to `queue` now ([`sent`] says as what; `submission`:
/// its number, or 0).
pub(crate) fn exec(queue: &ID3D12CommandQueue, what: u64, submission: u64, lists: &[Option<ID3D12CommandList>]) {
    if !on() {
        return;
    }
    // By the address draws and passes name a list by.
    let ids: Vec<u64> = lists
        .iter()
        .flatten()
        .map(|l| l.cast::<ID3D12GraphicsCommandList>().map_or_else(|_| raw(l), |g| raw(&g)))
        .collect();
    let chunks: Vec<&[u64]> = if ids.is_empty() { vec![&[][..]] } else { ids.chunks(3).collect() };
    for (k, chunk) in chunks.iter().enumerate() {
        let more = u64::from(k + 1 < chunks.len());
        let mut words = [raw(queue), what | ((chunk.len() as u64) << 8) | (more << 16), submission, 0, 0, 0];
        words[3..3 + chunk.len()].copy_from_slice(chunk);
        put(kind::EXEC, words);
    }
}

/// `fence` is signalled with `value` behind what `queue` was handed.
pub(crate) fn signal(queue: &ID3D12CommandQueue, fence: &ID3D12Fence, value: u64) {
    if on() {
        put(kind::SIGNAL, [raw(queue), raw(fence), value, 0, 0, 0]);
    }
}

/// A wait for `value` of `fence` came back.
pub(crate) fn waited(fence: &ID3D12Fence, value: u64) {
    if on() {
        put(kind::WAITED, [raw(fence), value, 0, 0, 0, 0]);
    }
}

/// A root view of a pass of ours in `list`: of `buffer`, from its
/// beginning.
pub(crate) fn view(list: &ID3D12GraphicsCommandList, what: u64, writes: bool, buffer: &ID3D12Resource) {
    if on() {
        // SAFETY: plain queries of a live buffer.
        let (address, width) = unsafe { (buffer.GetGPUVirtualAddress(), buffer.GetDesc().Width) };
        view_at(list, what, writes, address, width);
    }
}

/// A root view at `address`, with `bytes` of its buffer from there on (0:
/// not known here).
pub(crate) fn view_at(list: &ID3D12GraphicsCommandList, what: u64, writes: bool, address: u64, bytes: u64) {
    if on() {
        put(kind::VIEW, [raw(list), what | (u64::from(writes) << 8), address, bytes, 0, 0]);
    }
}

/// A root view that every list binds at the same address (title memory's
/// directory, the empty-page notes): noted without a list when the address
/// is another than the last one said, and again every second (the ring's
/// oldest records go).
pub(crate) fn steady_view(what: u64, writes: bool, address: u64) {
    use std::sync::atomic::AtomicU64;
    static LAST: [[AtomicU64; 2]; 16] = [const { [const { AtomicU64::new(0) }; 2] }; 16];
    let Some(c) = crumbs() else {
        return;
    };
    let last = &LAST[what as usize % 16];
    let now = c.began.elapsed().as_millis() as u64;
    let was = last[0].swap(address, Ordering::Relaxed);
    if was != address || now.saturating_sub(last[1].load(Ordering::Relaxed)) >= 1000 {
        last[1].store(now, Ordering::Relaxed);
        put(kind::VIEW, [0, what | (u64::from(writes) << 8), address, 0, 0, 0]);
    }
}

/// Changed words stored into a copy: `words` pairs at `pairs`, into the
/// copy at `card` of `bytes` bytes; `end`: where the last of them ends.
pub(crate) fn scatter(list: &ID3D12GraphicsCommandList, pairs: u64, words: u64, card: u64, bytes: u64, end: u64) {
    if on() {
        put(kind::SCATTER, [raw(list), pairs, words, card, bytes, end]);
    }
}

/// A batched clamp: its table, the clamped records' buffer, the first
/// place in it, the budget.
pub(crate) fn batch(list: &ID3D12GraphicsCommandList, table: u64, dst: &ID3D12Resource, base: u64, budget: u64) {
    if on() {
        // SAFETY: plain queries of a live buffer.
        let (address, width) = unsafe { (dst.GetGPUVirtualAddress(), dst.GetDesc().Width) };
        put(kind::BATCH, [raw(list), table, address, width, base, budget]);
    }
}

/// A clamp of one execution: `written` bytes of records into `dst`.
pub(crate) fn clamp(list: &ID3D12GraphicsCommandList, dst: &ID3D12Resource, written: u64, source: u64, budget: u64) {
    if on() {
        // SAFETY: plain queries of a live buffer.
        let (address, width) = unsafe { (dst.GetGPUVirtualAddress(), dst.GetDesc().Width) };
        put(kind::CLAMP, [raw(list), address, width, written, source, budget]);
    }
}

/// A card-step list and its buffer between: its steps, levels, and the
/// bytes of the buffer used.
pub(crate) fn step_list(list: &ID3D12GraphicsCommandList, between: &ID3D12Resource, [steps, levels, used]: [u64; 3]) {
    if on() {
        // SAFETY: plain queries of a live buffer.
        let (address, width) = unsafe { (between.GetGPUVirtualAddress(), between.GetDesc().Width) };
        put(kind::STEP_LIST, [raw(list), address, width, steps, levels, used]);
    }
}

/// A copy of a card-step list: `len` bytes between `offset` of `buffer`
/// (`width` bytes) and `at` of the buffer between; out of it with `write`;
/// not recorded unless `kept`.
pub(crate) fn step_copy(
    list: &ID3D12GraphicsCommandList,
    (write, kept): (bool, bool),
    buffer: &ID3D12Resource,
    width: u64,
    [offset, at, len]: [u64; 3],
) {
    if on() {
        let kind = match (write, kept) {
            (false, true) => kind::STEP_READ,
            (true, true) => kind::STEP_WRITE,
            (false, false) => kind::STEP_READ_LEFT,
            (true, false) => kind::STEP_WRITE_LEFT,
        };
        // SAFETY: a plain query of a live buffer.
        let address = unsafe { buffer.GetGPUVirtualAddress() };
        put(kind, [raw(list), address, width, offset, at, len]);
    }
}

/// A descriptor slot pointed at `len` bytes at `address` of a card-step
/// list's buffer between.
pub(crate) fn step_slot(list: &ID3D12GraphicsCommandList, heap: u64, id: u64, slot: u64, address: u64, len: u64) {
    if on() {
        put(kind::STEP_SLOT, [raw(list), heap, id, slot, address, len]);
    }
}

/// A buffer `list` uploads from, of `size` bytes.
pub(crate) fn upload_made(list: &ID3D12GraphicsCommandList, buffer: &ID3D12Resource, size: u64, stage: u64) {
    if on() {
        // SAFETY: a plain query of a live buffer.
        put(kind::UPLOAD_MADE, [raw(list), unsafe { buffer.GetGPUVirtualAddress() }, size, stage, 0, 0]);
    }
}

/// The main fence, for [`upload_gone`].
static MAIN: OnceLock<ID3D12Fence> = OnceLock::new();

/// A buffer a list uploaded from is released.
pub(crate) fn upload_gone(buffer: &ID3D12Resource) {
    if on() {
        // SAFETY: plain queries of live objects.
        let (address, completed) = unsafe { (buffer.GetGPUVirtualAddress(), MAIN.get().map_or(0, |f| f.GetCompletedValue())) };
        put(kind::UPLOAD_GONE, [address, completed, 0, 0, 0, 0]);
    }
}

/// Starts the thread that notes what the main fence has completed, and a
/// lost device; once.
pub(crate) fn watch(queue: &ID3D12CommandQueue, fence: &ID3D12Fence, device: &ID3D12Device) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if !on() || STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = MAIN.set(fence.clone());
    let (queue, fence, device) = (raw(queue), fence.clone(), device.clone());
    let made = std::thread::Builder::new().name("victory-crumbs".into()).spawn(move || {
        let mut last = None;
        let mut said = std::time::Instant::now();
        loop {
            // SAFETY: a plain query.
            let value = unsafe { fence.GetCompletedValue() };
            // (Said again every second, changed or not: the ring's oldest
            // records go, and the reader takes the main queue from these.)
            let again = said.elapsed() >= std::time::Duration::from_secs(1);
            if last != Some(value) || again {
                put(kind::FENCE, [queue, raw(&fence), value, 0, 0, 0]);
            }
            if value == u64::MAX {
                if last != Some(value) {
                    // SAFETY: a plain query.
                    let reason = unsafe { device.GetDeviceRemovedReason() }.err().map_or(0, |e| e.code().0 as u32);
                    put(kind::REMOVED, [u64::from(reason), 0, 0, 0, 0, 0]);
                    flush();
                    tracing::error!("VICTORY_CRUMBS: the device is lost (reason {reason:#x}); the breadcrumbs are in the file");
                }
                last = Some(value);
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            last = Some(value);
            if again {
                flush();
                said = std::time::Instant::now();
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    });
    if let Err(e) = made {
        tracing::error!(error = %e, "VICTORY_CRUMBS: no thread to watch the fence: which submissions had finished is not noted");
    }
}

#[cfg(test)]
mod tests {
    use victory_core::crumbs::{HEADER, RECORD, Report, kind, parse};

    /// What is put into the ring is in the file at once, without a flush:
    /// the mapping is the file's own pages (a process that ends at a lost
    /// device flushes nothing), and the reader finds it there.
    #[test]
    fn records_are_in_the_file_as_they_are_put() {
        let path = std::env::temp_dir().join("victory-crumbs-test.bin");
        let bytes = 1 << 20;
        let crumbs = super::open(&path, bytes).expect("a breadcrumb file in the temporary folder");
        assert_eq!(crumbs.ring.capacity(), ((bytes - HEADER) / RECORD) as u64);
        crumbs.ring.put(kind::FENCE, 1, [0x10, 0x20, 6, 0, 0, 0]);
        crumbs.ring.put(kind::EXEC, 2, [0x10, 1 | (1 << 8), 7, 0xa1, 0, 0]);
        crumbs.ring.put(kind::SIGNAL, 3, [0x10, 0x20, 7, 0, 0, 0]);
        let read = || {
            let file = std::fs::read(&path).expect("the file, read beside its mapping");
            assert_eq!(file.len(), bytes);
            parse(&file).expect("a breadcrumb file")
        };
        let parsed = read();
        assert_eq!(parsed.records.len(), 3, "not yet flushed");
        assert_eq!(parsed.records[1].words, [0x10, 1 | (1 << 8), 7, 0xa1, 0, 0]);
        let report = Report::of(&parsed);
        assert_eq!((report.completed.map(|c| c.0), report.in_flight.len()), (Some(6), 1));
        crumbs.flush();
        crumbs.ring.put(kind::REMOVED, 4, [0x887a_0006, 0, 0, 0, 0, 0]);
        let parsed = read();
        assert_eq!(parsed.records.len(), 4, "after a flush, and one more record");
        // (The file stays: it cannot be removed while it is mapped, and the
        // mapping is for good. The next run begins it anew.)
    }
}
