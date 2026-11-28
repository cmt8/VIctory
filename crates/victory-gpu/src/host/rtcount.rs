//! Diagnostics: what the programs that walk ray-tracing structures do, counted
//! by the programs themselves (`VICTORY_BVH_COUNT=1`, diagnostics builds;
//! `victory_shader::emit::bvh_count`). Each such program adds, at its end,
//! each lane's counts to its own run of a buffer in card memory the CPU maps
//! (`victory_shader::emit::SLOT_COUNTS`): node tests by kind and by what they
//! found, lanes, lanes stopped at the translation's loop limit, and a
//! histogram of lanes by the nodes they tested. The counts only grow; they
//! are read once a frame (as 32-bit numbers that wrap, so a frame's growth
//! is what is added up) and said with the frame-rate line.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use victory_shader::emit::{COUNT_DWORDS, COUNT_HISTOGRAM, COUNT_NAMES, COUNT_SLOTS};
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

const DWORDS: usize = (COUNT_SLOTS * COUNT_DWORDS) as usize;
/// The dword of the most nodes a lane tested: a maximum, not a sum.
const MOST: usize = 17;

struct Counts {
    _buffer: ID3D12Resource,
    data: *const u32,
    gpu: u64,
    /// What was read last, the frame it was read in, and the sums since the
    /// last line (the maximum for [`MOST`]).
    last: Vec<u32>,
    frame: u64,
    sums: Vec<u64>,
}

// SAFETY: a free-threaded buffer and its mapping, used behind the mutex.
unsafe impl Send for Counts {}

static COUNTS: Mutex<Option<Option<Counts>>> = Mutex::new(None);

fn make() -> Option<Counts> {
    let host = super::host()?;
    let bytes = 4 * DWORDS as u64;
    let Some((buffer, data)) = host.mapped_card_buffer(bytes, true) else {
        tracing::warn!("renderer port RT: no buffer in card memory the CPU maps: the node tests are not counted (VICTORY_BVH_COUNT=1) and the programs that walk structures are dropped");
        return None;
    };
    crate::host::name(&buffer, || "counts of ray-tracing node tests".into());
    // SAFETY: a fresh mapping of that many bytes.
    unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    tracing::info!("renderer port RT: programs that walk ray-tracing structures count their node tests (VICTORY_BVH_COUNT=1)");
    Some(Counts {
        _buffer: buffer,
        data: data.cast::<u32>().cast_const(),
        gpu,
        last: vec![0; DWORDS],
        frame: 0,
        sums: vec![0; DWORDS],
    })
}

impl Counts {
    /// Adds what the counts of the programs known to count grew by since
    /// they were last read (a read of the mapping crosses the bus: only
    /// their runs, each at once).
    fn read(&mut self) {
        let slots: Vec<u32> = NAMES.lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
        let mut now = [0u32; COUNT_DWORDS as usize];
        for slot in slots {
            let first = (slot * COUNT_DWORDS) as usize;
            // SAFETY: a run inside the mapping; the card adds to whole dwords.
            unsafe { std::ptr::copy_nonoverlapping(self.data.add(first), now.as_mut_ptr(), now.len()) };
            for (k, &v) in now.iter().enumerate() {
                let i = first + k;
                if k == MOST {
                    self.sums[i] = self.sums[i].max(u64::from(v));
                } else {
                    self.sums[i] += u64::from(v.wrapping_sub(self.last[i]));
                }
                self.last[i] = v;
            }
        }
    }
}

/// The counts' GPU address for a program to count into, the counts read at
/// most once a frame. None without the buffer.
pub(crate) fn address() -> Option<u64> {
    let mut guard = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let c = guard.get_or_insert_with(make).as_mut()?;
    let frame = crate::frame::current();
    if c.frame != frame {
        c.frame = frame;
        c.read();
    }
    Some(c.gpu)
}

/// Notes that the program `name` counts into run `slot`.
pub(crate) fn named(slot: u32, name: &str) {
    NAMES.lock().unwrap_or_else(|e| e.into_inner()).entry(slot).or_default().insert(name.to_owned());
}

/// Programs that count, by their run.
static NAMES: Mutex<BTreeMap<u32, BTreeSet<String>>> = Mutex::new(BTreeMap::new());

/// For the frame-rate line: per run of a program that tested nodes, the
/// counts per frame since the last line, and the histogram of lanes by the
/// nodes they tested.
pub(crate) fn line(frames: f64) {
    let mut guard = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(Some(c)) = guard.as_mut() else {
        return;
    };
    c.read();
    let frames = frames.max(1.0);
    let mut all = vec![0u64; COUNT_DWORDS as usize];
    for slot in 0..COUNT_SLOTS {
        let run = &mut c.sums[(slot * COUNT_DWORDS) as usize..][..COUNT_DWORDS as usize];
        if run[0] == 0 {
            run.fill(0);
            continue;
        }
        for (k, (a, &v)) in all.iter_mut().zip(run.iter()).enumerate() {
            *a = if k == MOST { (*a).max(v) } else { *a + v };
        }
        let names = NAMES.lock().unwrap_or_else(|e| e.into_inner()).get(&slot).cloned();
        let names = names.map_or_else(|| "?".to_owned(), |n| n.iter().cloned().collect::<Vec<_>>().join("+"));
        say(&format!("run {slot} ({names})"), run, frames);
        run.fill(0);
    }
    if all[0] > 0 {
        say("all", &all, frames);
    }
}

fn say(what: &str, run: &[u64], frames: f64) {
    let per = |v: u64| v as f64 / frames;
    let counts: Vec<String> = COUNT_NAMES
        .iter()
        .enumerate()
        .map(|(k, name)| if k == MOST { format!("{name} {}", run[k]) } else { format!("{name} {:.0}", per(run[k])) })
        .collect();
    let lanes = run[14].max(1) as f64;
    let histogram: Vec<String> = (1..32)
        .filter(|&b| run[(COUNT_HISTOGRAM + b) as usize] > 0)
        .map(|b| format!("<{}:{:.0}", 1u64 << b, per(run[(COUNT_HISTOGRAM + b) as usize])))
        .collect();
    tracing::info!(
        "renderer port RT: node tests per frame (VICTORY_BVH_COUNT=1), {what}: {:.1} a lane that tested; {}; lanes by nodes tested {}",
        run[0] as f64 / lanes,
        counts.join(", "),
        histogram.join(" "),
    );
}
