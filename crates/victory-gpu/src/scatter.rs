//! Short runs of bytes sent up to a copy in the card's memory in one
//! compute pass instead of a `CopyBufferRegion` each.
//!
//! Before a submission, what changed on a card copy's CPU side goes to its
//! card side (`Host::card_before`). Where flights' shaders write the copy,
//! what changed is the title's bytes put in around the shaders' (the merge
//! under writes), in runs of a few bytes: run 232629 sent 207,525 ranges a
//! frame in one stretch, 219,176 runs of about 6 bytes, and `membench`'s
//! `small_copies_bench` has 200,000 copies of 8 bytes at 21.6 ms to record
//! and 20.4 ms to run under vkd3d-proton (RX 7900 XTX), against 0.1 ms each
//! for the same bytes as one copy.
//!
//! Here each such run becomes records of one dword of the card copy each:
//! (dword, mask of the run's bytes in it, the CPU side's dword), written
//! into upload memory, and one dispatch applies them: a whole dword is
//! stored, a part of one is cleared and set under its mask with two
//! atomics, so that bytes of the same dword that are not the run's (a
//! neighbouring run's, or bytes a flight's shader wrote on the card) stay
//! as they are. Runs are disjoint (the dirty ranges are joined), so no two
//! records set the same byte. The bytes are taken from the CPU side when
//! the records are made, at the send, where a copy took them when the card
//! ran it: what changes on the CPU side in between is dirty again and goes
//! up with the next send either way.
//!
//! Wine's HLSL compiler has no methods on read-write byte-address buffers
//! and puts no root signature in the blob: the shader uses structured
//! buffers of dwords and free-function atomics, and the root signature is
//! made here. Every root parameter is bound at every dispatch; the shader
//! reads no record past `count` (which the caller keeps within the records
//! it wrote) and writes no dword past `dwords` (the card copy's size).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use windows::Win32::Graphics::Direct3D12::*;

/// The shader: one record a thread, `count` records from `first` on.
const SCATTER: &str = r#"
cbuffer Span : register(b0) {
    uint first;   // the first record of this dispatch
    uint count;   // the records of this dispatch
    uint dwords;  // the card copy's dwords
};
// Three dwords a record: the card copy's dword, the mask of the bytes to
// set in it, their value (the CPU side's dword).
StructuredBuffer<uint> records : register(t0);
RWStructuredBuffer<uint> card : register(u0);
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= count) return;
    uint r = (first + id.x) * 3u;
    uint at = records[r];
    uint mask = records[r + 1u];
    uint value = records[r + 2u];
    if (at >= dwords) return;
    [branch] if (mask == 0xffffffffu) {
        card[at] = value;
    } else {
        InterlockedAnd(card[at], ~mask);
        InterlockedOr(card[at], value & mask);
    }
}
"#;

/// Bytes of a record.
pub(crate) const RECORD: u64 = 12;

/// Whether short runs go up to card copies by [`dispatch`]
/// (`VICTORY_CARD_SCATTER`: on under Wine, `=0` a copy each as before,
/// `=1` on elsewhere).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_CARD_SCATTER").as_deref().map(str::trim) {
        Ok("0") => false,
        Ok("1") => true,
        _ => crate::host::on_wine(),
    })
}

/// Runs shorter than this many bytes go by records
/// (`VICTORY_CARD_SCATTER_BYTES`, 256): a copy costs about 0.2 us to record
/// and run, a record 12 bytes of upload memory a dword.
pub(crate) fn shorter_than() -> usize {
    static BYTES: OnceLock<usize> = OnceLock::new();
    *BYTES.get_or_init(|| {
        std::env::var("VICTORY_CARD_SCATTER_BYTES").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(256)
    })
}

/// Since the last status line: runs sent by records, their bytes, the
/// records, and the dispatches.
pub(crate) static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Since the start: dispatches of the pass.
pub(crate) static DISPATCHES: AtomicU64 = AtomicU64::new(0);

/// The records of the run `[a, b)` of a buffer: how many.
pub(crate) fn records_of(a: usize, b: usize) -> usize {
    if a >= b { 0 } else { (b - 1) / 4 - a / 4 + 1 }
}

/// Writes the records of the run `[a, b)` of a buffer whose CPU side's
/// bytes are at `cpu` (`len` of them, a multiple of 4 that holds the run)
/// into `out`, from its start: how many ([`records_of`]). `out` holds at
/// least that many records' dwords.
///
/// # Safety
/// `cpu` is readable for `len` bytes.
pub(crate) unsafe fn write_records(cpu: *const u8, len: usize, (a, b): (usize, usize), out: &mut [u32]) -> usize {
    debug_assert!(b <= len && len.is_multiple_of(4));
    let mut n = 0;
    let mut at = a;
    while at < b {
        let word = at / 4;
        let lo = at % 4;
        let hi = (b - word * 4).min(4);
        let mask = if hi - lo == 4 { u32::MAX } else { (u32::MAX >> (32 - 8 * (hi - lo))) << (8 * lo) };
        // SAFETY: the dword lies inside the CPU side (`b <= len`, a
        // multiple of 4).
        let value = unsafe { cpu.add(word * 4).cast::<u32>().read_unaligned() };
        out[n * 3..n * 3 + 3].copy_from_slice(&[word as u32, mask, value]);
        n += 1;
        at = word * 4 + hi;
    }
    n
}

struct Pipeline {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Pipeline {}
unsafe impl Sync for Pipeline {}

/// The pass, made on first use; `None` if the host cannot make it (the
/// runs then go by copies).
fn pipeline(device: &ID3D12Device) -> Option<&'static Pipeline> {
    static MADE: OnceLock<Option<Pipeline>> = OnceLock::new();
    MADE.get_or_init(|| match make(device) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "no scatter pass for card copies: short runs go up by a copy each");
            None
        }
    })
    .as_ref()
}

fn make(device: &ID3D12Device) -> windows::core::Result<Pipeline> {
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; the out pointers
    // are ours.
    let compiled = unsafe {
        D3DCompile(
            SCATTER.as_ptr().cast(),
            SCATTER.len(),
            windows::core::s!("card scatter"),
            None,
            None,
            windows::core::s!("main"),
            windows::core::s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe { std::slice::from_raw_parts(errors.GetBufferPointer().cast::<u8>(), errors.GetBufferSize()) };
            tracing::warn!("card scatter: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // The span (b0), the records (t0), the card copy (u0).
    let root = crate::host::root_signature(
        device,
        &[
            crate::host::root_constants(0, 3),
            crate::host::root_view(D3D12_ROOT_PARAMETER_TYPE_SRV, 0),
            crate::host::root_view(D3D12_ROOT_PARAMETER_TYPE_UAV, 0),
        ],
        "card scatter",
    )?;
    // SAFETY: the blob's own bytes, and a live root signature.
    let pso = unsafe {
        let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?
    };
    Ok(Pipeline { root, pso })
}

/// Whether the pass can be recorded on `device`.
pub(crate) fn ready(device: &ID3D12Device) -> bool {
    pipeline(device).is_some()
}

/// Records into `list` the pass that applies `count` records at the host
/// GPU address `records` (upload memory holding at least `count` records)
/// to `card`, a buffer of `bytes` bytes in the unordered-access state. The
/// pass leaves the list's compute root signature and pipeline its own.
///
/// # Safety
/// `list` is open; `card` and the records are live until it has run.
pub(crate) unsafe fn dispatch(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    card: &ID3D12Resource,
    bytes: u64,
    records: u64,
    count: u32,
) -> bool {
    let Some(p) = pipeline(device) else {
        return false;
    };
    if count == 0 {
        return true;
    }
    const GROUP: u32 = 64;
    const MOST: u32 = 65_535 * GROUP;
    let dwords = u32::try_from(bytes / 4).unwrap_or(u32::MAX);
    // SAFETY: as the caller says; every root parameter is bound before each
    // dispatch.
    unsafe {
        list.SetComputeRootSignature(&p.root);
        list.SetPipelineState(&p.pso);
        list.SetComputeRootShaderResourceView(1, records);
        list.SetComputeRootUnorderedAccessView(2, card.GetGPUVirtualAddress());
        let mut first = 0;
        while first < count {
            let n = (count - first).min(MOST);
            let span = [first, n, dwords];
            list.SetComputeRoot32BitConstants(0, 3, span.as_ptr().cast(), 0);
            list.Dispatch(n.div_ceil(GROUP), 1, 1);
            STATS[3].fetch_add(1, Relaxed);
            DISPATCHES.fetch_add(1, Relaxed);
            first += n;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_cover_exactly_the_runs_bytes() {
        let cpu: Vec<u8> = (0..64u32).map(|i| (i * 13 + 1) as u8).collect();
        for (a, b) in [(0usize, 1usize), (1, 2), (3, 5), (0, 4), (2, 11), (4, 12), (5, 64), (63, 64), (8, 9)] {
            let mut out = vec![0u32; 3 * records_of(a, b)];
            // SAFETY: 64 readable bytes.
            let n = unsafe { write_records(cpu.as_ptr(), cpu.len(), (a, b), &mut out) };
            assert_eq!(n, records_of(a, b));
            let mut card = vec![0xeeu8; 64];
            for r in out.chunks(3) {
                let (word, mask, value) = (r[0] as usize, r[1], r[2]);
                let was = u32::from_le_bytes(card[word * 4..word * 4 + 4].try_into().unwrap());
                card[word * 4..word * 4 + 4].copy_from_slice(&((was & !mask) | (value & mask)).to_le_bytes());
            }
            for (i, &byte) in card.iter().enumerate() {
                assert_eq!(byte, if (a..b).contains(&i) { cpu[i] } else { 0xee }, "run {a}..{b} byte {i}");
            }
        }
    }

    /// On the host GPU: runs sent up by the pass leave the card buffer as
    /// copies of the same runs do, byte for byte, on a small buffer first
    /// and then a larger one with many runs that share dwords.
    #[test]
    fn the_pass_leaves_the_card_copy_as_copies_do() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        if !ready(&host.device) {
            panic!("the scatter pass could not be made");
        }
        for (bytes, runs) in [(256usize, 8usize), (1 << 20, 50_000)] {
            // The card side before: one pattern; the CPU side: another.
            let before: Vec<u8> = (0..bytes).map(|i| (i * 7 + 3) as u8).collect();
            let cpu: Vec<u8> = (0..bytes).map(|i| (i * 31 + 11) as u8 ^ 0x5a).collect();
            // Disjoint runs of 1-13 bytes, a gap of 1-9 bytes between.
            let mut ranges = Vec::new();
            let mut seed = 0x1234_5678u64 ^ bytes as u64;
            let mut at = 1;
            while ranges.len() < runs {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let len = 1 + (seed >> 40) as usize % 13;
                if at + len > bytes {
                    break;
                }
                ranges.push((at, at + len));
                at += len + 1 + (seed >> 50) as usize % 9;
            }
            let count: usize = ranges.iter().map(|&(a, b)| records_of(a, b)).sum();
            let (upload, data) = host.upload_buffer((2 * bytes + count * RECORD as usize) as u64).unwrap();
            let mut records = vec![0u32; 3 * count];
            let mut n = 0;
            for &r in &ranges {
                // SAFETY: the CPU side's `bytes` bytes.
                n += unsafe { write_records(cpu.as_ptr(), bytes, r, &mut records[3 * n..]) };
            }
            assert_eq!(n, count);
            // SAFETY: the upload buffer holds the two images and the records.
            unsafe {
                std::ptr::copy_nonoverlapping(before.as_ptr(), data, bytes);
                std::ptr::copy_nonoverlapping(cpu.as_ptr(), data.add(bytes), bytes);
                std::ptr::copy_nonoverlapping(records.as_ptr().cast::<u8>(), data.add(2 * bytes), count * RECORD as usize);
            }
            let card = |_: ()| host.uav_buffer(bytes as u64).unwrap();
            let (by_copies, by_pass) = (card(()), card(()));
            let back = host.read_back_buffer(2 * bytes as u64).unwrap();
            let list = host.new_list().unwrap();
            let moved = crate::host::transition;
            // SAFETY: an open list; live buffers, every range inside them;
            // the records lie in the upload buffer after the two images.
            unsafe {
                let l = &list.list;
                for c in [&by_copies, &by_pass] {
                    l.CopyBufferRegion(c, 0, &upload, 0, bytes as u64);
                }
                for &(a, b) in &ranges {
                    l.CopyBufferRegion(&by_copies, a as u64, &upload, (bytes + a) as u64, (b - a) as u64);
                }
                l.ResourceBarrier(&[moved(&by_pass, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
                let records_at = upload.GetGPUVirtualAddress() + 2 * bytes as u64;
                assert!(dispatch(&host.device, l, &by_pass, bytes as u64, records_at, count as u32));
                l.ResourceBarrier(&[
                    moved(&by_pass, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE),
                    moved(&by_copies, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE),
                ]);
                l.CopyBufferRegion(&back, 0, &by_copies, 0, bytes as u64);
                l.CopyBufferRegion(&back, bytes as u64, &by_pass, 0, bytes as u64);
            }
            list.close();
            host.execute(&[&list]);
            let mut mapped = std::ptr::null_mut();
            // SAFETY: the copies ran (execute waits); the buffer holds both.
            let read = unsafe {
                back.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 2 * bytes }), Some(&mut mapped)).unwrap();
                std::slice::from_raw_parts(mapped.cast::<u8>(), 2 * bytes).to_vec()
            };
            let mut expected = before.clone();
            for &(a, b) in &ranges {
                expected[a..b].copy_from_slice(&cpu[a..b]);
            }
            assert!(read[..bytes] == expected[..], "the copies ({bytes} bytes, {} runs)", ranges.len());
            assert!(read[bytes..] == expected[..], "the pass ({bytes} bytes, {} runs)", ranges.len());
        }
    }
}
