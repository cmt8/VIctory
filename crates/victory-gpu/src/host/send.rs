//! What goes up to copies in the card's memory goes up as it was when its
//! submission was prepared (`VICTORY_SEND_OWN`, on by default; `0`: as
//! before).
//!
//! The sends were copies out of the copies' CPU sides, which the card makes
//! when it comes to the submission. By then later submissions are prepared
//! (two frames of them with `VICTORY_RELAY`), and their refreshes have
//! written the same CPU sides: the earlier submission's shaders then read a
//! later one's bytes. For what the title's CPU writes that is as on the
//! console (it does not write what the card still reads). But the title's
//! own buffer copies are commands in its lists, and those are done by us
//! when a submission is prepared: "a value to a buffer, a draw that reads
//! it" in one submission and the same with another value in the next is
//! ordinary, and both draws read the second value.
//!
//! So each submission's bytes are taken out of the CPU sides into memory of
//! the submission's own when they are taken from the dirty lists, and the
//! card copies them from there. Runs of a few words, which a refresh that
//! sends only what changed makes many of, go as pairs of offset and value,
//! stored by one dispatch per copy; the others as a copy each.

use super::*;
use crate::shadow::Shadow;

pub(super) fn own() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SEND_OWN").is_none_or(|v| v != "0"))
}

/// Runs up to this many bytes go as pairs.
const SHORT: usize = 256;

/// Whether a run of `len` bytes at offset `at` of a copy goes as pairs.
fn short(at: usize, len: usize) -> bool {
    len <= SHORT && at.is_multiple_of(4) && len.is_multiple_of(4)
}

/// Whether a run of `len` bytes at offset `at` of a copy in the card's memory
/// is no copy command of its own, but pairs of one dispatch.
pub(crate) fn goes_as_words(at: usize, len: usize) -> bool {
    own() && short(at, len)
}

/// Whether the title's bytes that go in under flights' writes
/// (`Shadow::merge_under_writes`) go in by whole words
/// (`VICTORY_UNDER_WORDS=0`: by the bytes that changed, as before).
///
/// By bytes nearly every run (a float's three low bytes) was a copy command
/// of its own, and a submission with more than 4,096 of them stood until the
/// flights had landed: 4 to 14 ms of a steady frame (session play-1125-0203:
/// "more changed runs than a submission sends up"), for some 20 KB. Whole
/// words are pairs of one dispatch, and are not counted.
///
/// A word is taken to be the title's or the shaders', not shared: where the
/// title changed a byte of a word of which a flight's shader writes another
/// byte, that byte is as it was before the flight. The landing rests on the
/// same ([`crate::shadow::exact_back`]).
pub(crate) fn words_under() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| own() && std::env::var_os("VICTORY_UNDER_WORDS").is_none_or(|v| v != "0"))
}

/// The card's copy is bound by its address alone (a root view), so nothing
/// but the pass itself keeps a store inside it: an offset past the copy's
/// end would be written wherever it points, and where no memory is mapped
/// the card stops with a page fault and the device is lost. So the pass is
/// told the copy's bytes and stores only inside them; a pair outside is left
/// out and counted ([`refused`]). The offsets are made by us from ranges of
/// the copy, so none should be: the bound costs each thread a subtraction
/// and two compares, and nothing else unless it holds.
const SCATTER: &str = r#"
#define RS "RootConstants(num32BitConstants=3, b0), SRV(t0), UAV(u0), UAV(u1)"
cbuffer Part : register(b0) { uint at; uint count; uint bytes; };
ByteAddressBuffer pairs : register(t0);
RWByteAddressBuffer card : register(u0);
// In words (Wine's compiler has Interlocked* only as functions, not as
// methods of a byte address buffer): 0 the pairs left out.
RWStructuredBuffer<uint> notes : register(u1);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= count) return;
    uint2 p = pairs.Load2(at + id.x * 8u);
    if (p.x >= bytes || bytes - p.x < 4u) {
        uint was;
        InterlockedAdd(notes[0], 1u, was);
        return;
    }
    card.Store(p.x, p.y);
}
"#;

struct Pass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    /// The pass's notes (word 0: the pairs it left out, since the start),
    /// and where the CPU reads them.
    notes: ID3D12Resource,
    noted: usize,
}

/// The bytes of a copy the pass may store in: its buffer's, as far as a
/// pair's offset reaches.
fn bound(width: u64) -> u32 {
    u32::try_from(width).unwrap_or(u32::MAX)
}

/// The pairs the pass left out since the start, for want of a place inside
/// their copy ([`SCATTER`]); 0 while the pass is not made.
pub(super) fn refused() -> u64 {
    let Some(pass) = PASS.get().and_then(Option::as_ref) else {
        return 0;
    };
    // SAFETY: the notes' mapping, which lives as long as the pass (for
    // good); the card adds to the word, which is read whole.
    u64::from(unsafe { (pass.noted as *const u32).read_volatile() })
}

static PASS: OnceLock<Option<Pass>> = OnceLock::new();

fn pass(host: &Host) -> Option<&'static Pass> {
    PASS.get_or_init(|| {
        let code = compile(SCATTER, windows::core::s!("main"), windows::core::s!("cs_5_1")).ok()?;
        // The root signature of `RS`, made in code (Wine's compiler,
        // `D3DCompile` under Wine, ignores `[RootSignature]`): the part's two
        // constants and the copy's bytes, the pairs as a root SRV, the card's
        // copy and the notes as root UAVs.
        let params = [
            super::root_constants(0, 3),
            super::root_view(D3D12_ROOT_PARAMETER_TYPE_SRV, 0),
            super::root_view(D3D12_ROOT_PARAMETER_TYPE_UAV, 0),
            super::root_view(D3D12_ROOT_PARAMETER_TYPE_UAV, 1),
        ];
        let root = super::root_of(&host.device, &code, &params, "changed words sent to the card").ok()?;
        let (notes, noted) = host.shared_buffer(256)?;
        name(&notes, || "notes of the changed words sent to the card".into());
        // SAFETY: the buffer's own mapping, before anything reads it.
        unsafe { std::ptr::write_bytes(noted, 0, 256) };
        // SAFETY: the code lives through the calls.
        unsafe {
            let pso: ID3D12PipelineState = host
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                })
                .ok()?;
            name(&pso, || "victory: changed words sent to the card".into());
            Some(Pass {
                root,
                pso,
                notes,
                noted: noted as usize,
            })
        }
    })
    .as_ref()
}

/// Memory a submission's sends come from, mapped.
pub(super) struct Memory {
    buffer: ID3D12Resource,
    data: usize,
    size: u64,
}

// SAFETY: a D3D12 object (free-threaded) and its mapping, with one owner.
unsafe impl Send for Memory {}

/// What submissions that have run gave back.
static SPARE: Mutex<Vec<Memory>> = Mutex::new(Vec::new());

/// At most this much is kept (of the card's memory, with [`on_card`]).
fn kept() -> u64 {
    if on_card() { 256 << 20 } else { 768 << 20 }
}

/// Whether the memory lies in the card's memory, written by the CPU through
/// the bus (`VICTORY_SEND_BAR=1`): the copies on the card's drawing queue, which
/// nothing overlaps, then cross no bus. In a captured frame they took 5.4 ms of
/// the card's 30 (342 copies), and take 0.4 this way; but filling it takes the
/// submitting thread 8-9 ms a frame instead of 3 (73-93 MB, run card-3 against
/// card-1), so it is a loss while the card waits for that thread: off by
/// default.
fn on_card() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SEND_BAR").is_some_and(|v| v == "1"))
}

fn memory(host: &Host, size: u64) -> Option<Memory> {
    {
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        let fit = spare.iter().enumerate().filter(|(_, m)| m.size >= size).min_by_key(|(_, m)| m.size).map(|(i, _)| i);
        if let Some(i) = fit {
            return Some(spare.swap_remove(i));
        }
    }
    // By doubles, so that the next of about this size finds it: what
    // submissions send differs from frame to frame.
    let size = size.max(8 << 20).next_power_of_two();
    let mapped = if on_card() { host.mapped_card_buffer(size, false) } else { None };
    let (buffer, data) = match mapped {
        Some(made) => made,
        None => host.upload_buffer(size)?,
    };
    name(&buffer, || "bytes a submission sends up".into());
    Some(Memory {
        buffer,
        data: data as usize,
        size,
    })
}

/// The submission has run.
pub(super) fn give_back(memory: Memory) {
    let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
    spare.push(memory);
    spare.sort_by_key(|m| std::cmp::Reverse(m.size));
    while spare.iter().map(|m| m.size).sum::<u64>() > kept() {
        spare.remove(0);
    }
}

/// Since the last frame-rate line: bytes copied, runs copied, words stored,
/// microseconds of taking them out of the CPU sides.
pub(super) static STATS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// One copy's sends: ranges copied (offset in the copy, in the memory,
/// bytes), and its pairs (where in the memory, how many).
struct Copy {
    runs: Vec<(usize, u64, usize)>,
    pairs: (u64, u32),
}

/// Where a submission's bytes are taken to.
pub(super) struct Own {
    memory: Memory,
    copies: Vec<Copy>,
    /// Bytes of memory used.
    used: u64,
}

/// Runs whose pairs are one piece of work.
const RUNS_AT_A_TIME: usize = 4096;

/// Takes what `dirty` sends up out of the CPU sides.
pub(super) fn take(host: &Host, dirty: &[(std::sync::Arc<Shadow>, Vec<(usize, usize)>)]) -> Option<Own> {
    let began = std::time::Instant::now();
    pass(host)?;
    // Runs of whole words of the copy: only those are pairs. (A byte beside
    // them in a word may be the card's.)
    let short = |&(a, b): &(usize, usize)| short(a, b - a);
    let mut at = 0u64;
    let mut copies = Vec::with_capacity(dirty.len());
    for (_, ranges) in dirty {
        let mut runs = Vec::new();
        for &(a, b) in ranges.iter().filter(|r| !short(r)) {
            runs.push((a, at, b - a));
            at += ((b - a) as u64).next_multiple_of(16);
        }
        let words: usize = ranges.iter().filter(|r| short(r)).map(|r| (r.1 - r.0) / 4).sum();
        copies.push(Copy { runs, pairs: (at, words as u32) });
        at += (words as u64 * 8).next_multiple_of(16);
    }
    if at == 0 {
        return None;
    }
    let memory = memory(host, at)?;
    let mut moves: Vec<(usize, usize, usize)> = Vec::new();
    let mut work: Vec<(usize, &[(usize, usize)], usize)> = Vec::new();
    for ((shadow, ranges), copy) in dirty.iter().zip(&copies) {
        let from = shadow.card_data() as usize;
        moves.extend(copy.runs.iter().map(|&(a, to, n)| (from + a, memory.data + to as usize, n)));
        // (80,000 runs of some words a frame: in pieces of work.)
        let mut to = memory.data + copy.pairs.0 as usize;
        for ranges in ranges.chunks(RUNS_AT_A_TIME) {
            let words: usize = ranges.iter().filter(|r| short(r)).map(|r| (r.1 - r.0) / 4).sum();
            if words > 0 {
                work.push((from, ranges, to));
                to += words * 8;
            }
        }
    }
    let one = |i: usize| {
        let (from, ranges, to) = work[i];
        let mut to = to as *mut [u32; 2];
        for &(a, b) in ranges.iter().filter(|r| short(r)) {
            for offset in (a..b).step_by(4) {
                // SAFETY: inside the copy's mapping (its CPU side, which
                // the thread that asked, and waits, alone writes in these
                // ranges) and inside the memory, which has room for every
                // pair, a piece of work's behind the last one's.
                unsafe {
                    let word = ((from + offset) as *const u32).read_unaligned();
                    to.write([offset as u32, word]);
                    to = to.add(1);
                }
            }
        }
    };
    crate::land::each_under(work.len(), &one);
    STATS[0].fetch_add(moves.iter().map(|m| m.2 as u64).sum(), Ordering::Relaxed);
    STATS[1].fetch_add(moves.len() as u64, Ordering::Relaxed);
    STATS[2].fetch_add(copies.iter().map(|c| u64::from(c.pairs.1)).sum(), Ordering::Relaxed);
    crate::shadow::copy_all(moves);
    STATS[3].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
    Some(Own { memory, copies, used: at })
}

impl Own {
    /// Records the sends into `list`; the copies that `looked` says are in
    /// `COPY_DEST`, the others in the common state. Those with a "before" are
    /// left readable by compute shaders.
    ///
    /// # Safety
    /// An open list of ours, whose compute state is set anew behind this.
    pub(super) unsafe fn record(
        &self,
        host: &Host,
        list: &ID3D12GraphicsCommandList,
        dirty: &[(std::sync::Arc<Shadow>, Vec<(usize, usize)>)],
        looked: impl Fn(&Shadow) -> bool,
        from: Option<&ID3D12Resource>,
    ) {
        // (With `crate::host::upq`: a copy of the memory in the card's.)
        let source = from.unwrap_or(&self.memory.buffer);
        let read = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
        let write = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let dest = D3D12_RESOURCE_STATE_COPY_DEST;
        let (mut first, mut last) = (Vec::new(), Vec::new());
        for ((shadow, _), copy) in dirty.iter().zip(&self.copies) {
            for &(a, from, n) in &copy.runs {
                // SAFETY: the caller's; live buffers, ranges inside them.
                unsafe { list.CopyBufferRegion(&shadow.buffer, a as u64, source, from, n as u64) };
            }
            let mut state = (looked(shadow) || !copy.runs.is_empty()).then_some(dest);
            if copy.pairs.1 > 0 {
                if let Some(was) = state {
                    first.push(transition(&shadow.buffer, was, write));
                }
                state = Some(write);
            }
            let hashed = shadow.card_sides().is_some_and(|(_, before, _)| before.is_some());
            if let Some(was) = state
                && hashed
                && (!copy.runs.is_empty() || copy.pairs.1 > 0)
            {
                last.push(transition(&shadow.buffer, was, read));
            }
        }
        // SAFETY: the caller's; live objects.
        unsafe {
            if !first.is_empty() {
                list.ResourceBarrier(&first);
            }
            if let Some(pass) = pass(host)
                && self.copies.iter().any(|c| c.pairs.1 > 0)
            {
                list.SetComputeRootSignature(&pass.root);
                list.SetPipelineState(&pass.pso);
                list.SetComputeRootShaderResourceView(1, source.GetGPUVirtualAddress());
                list.SetComputeRootUnorderedAccessView(3, pass.notes.GetGPUVirtualAddress());
                for ((shadow, _ranges), copy) in dirty.iter().zip(&self.copies).filter(|(_, c)| c.pairs.1 > 0) {
                    list.SetComputeRootUnorderedAccessView(2, shadow.buffer.GetGPUVirtualAddress());
                    const MOST: u32 = 64 * D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION;
                    let (at, words) = copy.pairs;
                    let bytes = bound(shadow.width());
                    #[cfg(feature = "diagnostics")]
                    if crate::crumbs::on() {
                        // (Where the last of the copy's changed words ends.)
                        let end = _ranges.iter().filter(|r| short(r.0, r.1 - r.0)).map(|r| r.1 as u64).max().unwrap_or(0);
                        let pairs = source.GetGPUVirtualAddress() + at;
                        crate::crumbs::scatter(list, pairs, u64::from(words), shadow.buffer.GetGPUVirtualAddress(), shadow.width(), end);
                    }
                    let mut done = 0;
                    while done < words {
                        let n = (words - done).min(MOST);
                        let constants = [at as u32 + done * 8, n, bytes];
                        list.SetComputeRoot32BitConstants(0, 3, constants.as_ptr().cast(), 0);
                        list.Dispatch(n.div_ceil(64), 1, 1);
                        done += n;
                    }
                }
            }
            if !last.is_empty() {
                list.ResourceBarrier(&last);
            }
        }
    }

    /// The memory and the bytes of it used.
    pub(super) fn source(&self) -> (&ID3D12Resource, u64) {
        (&self.memory.buffer, self.used)
    }

    /// The memory, to be kept until the submission has run.
    pub(super) fn memory(self) -> Memory {
        self.memory
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pass that writes changed words into a copy on the card builds and
    /// writes each pair's word at its byte (its root signature made in code:
    /// under Wine the blob has none, and the pass was not made).
    #[test]
    fn the_scatter_writes_each_pair_where_it_says() {
        let Some(host) = host() else {
            return; // no host GPU here
        };
        let pass = pass(host).expect("the scatter must build");
        let (pairs, pairs_ptr) = host.shared_buffer(256).expect("pairs");
        let (card, card_ptr) = host.shared_buffer(256).expect("card copy");
        // SAFETY: inside the mappings.
        unsafe {
            std::ptr::write_bytes(card_ptr, 0, 256);
            // Three pairs from byte 16 of the pairs: (byte, word).
            let words = pairs_ptr.cast::<u32>();
            for (k, (at, word)) in [(0u32, 0x1111_1111u32), (12, 0x2222_2222), (200, 0x3333_3333)].into_iter().enumerate() {
                words.add(4 + 2 * k).write_volatile(at);
                words.add(5 + 2 * k).write_volatile(word);
            }
        }
        let list = host.new_list().expect("a list");
        // SAFETY: an open list, live buffers in the common state (promoted).
        unsafe {
            let l = &list.list;
            l.SetComputeRootSignature(&pass.root);
            l.SetPipelineState(&pass.pso);
            l.SetComputeRoot32BitConstants(0, 3, [16u32, 3, 256].as_ptr().cast(), 0);
            l.SetComputeRootShaderResourceView(1, pairs.GetGPUVirtualAddress());
            l.SetComputeRootUnorderedAccessView(2, card.GetGPUVirtualAddress());
            l.SetComputeRootUnorderedAccessView(3, pass.notes.GetGPUVirtualAddress());
            l.Dispatch(1, 1, 1);
        }
        list.close();
        host.execute(&[&list]);
        // SAFETY: the mapping; the list has run.
        let read = |at: usize| unsafe { card_ptr.add(at).cast::<u32>().read_volatile() };
        assert_eq!([read(0), read(4), read(12), read(200), read(204)], [0x1111_1111, 0, 0x2222_2222, 0x3333_3333, 0]);
    }

    /// A pair whose word would not lie whole inside the copy's bytes is not
    /// stored, and is counted: bound by its address alone, the copy would
    /// take the store wherever it points, in mapped memory or not. (The
    /// test's buffer is wider than the bytes it says the copy has, so that a
    /// store the pass let through would show, and harm nothing.)
    #[test]
    fn the_scatter_stores_nothing_outside_the_copy() {
        let Some(host) = host() else {
            return; // no host GPU here
        };
        let pass = pass(host).expect("the scatter must build");
        let (pairs, pairs_ptr) = host.shared_buffer(256).expect("pairs");
        let (card, card_ptr) = host.shared_buffer(4096).expect("card copy");
        // The copy is said to be 128 bytes: its last word, the first past
        // it, one that begins inside it and ends past it, two far past it,
        // and its first word. (None past the test's buffer: a pass that let
        // one through must not write where nothing is mapped.)
        let all = [
            (124u32, 0x1111_1111u32),
            (128, 0x2222_2222),
            (126, 0x3333_3333),
            (2048, 0x4444_4444),
            (4092, 0x5555_5555),
            (0, 0x6666_6666),
        ];
        // SAFETY: inside the mappings.
        unsafe {
            std::ptr::write_bytes(card_ptr, 0, 4096);
            let words = pairs_ptr.cast::<u32>();
            for (k, (at, word)) in all.into_iter().enumerate() {
                words.add(2 * k).write_volatile(at);
                words.add(2 * k + 1).write_volatile(word);
            }
        }
        // (Other tests' passes run beside this one: none of theirs is
        // refused, so the count moves by this one's alone.)
        let before = refused();
        let list = host.new_list().expect("a list");
        // SAFETY: an open list, live buffers in the common state (promoted).
        unsafe {
            let l = &list.list;
            l.SetComputeRootSignature(&pass.root);
            l.SetPipelineState(&pass.pso);
            l.SetComputeRoot32BitConstants(0, 3, [0u32, all.len() as u32, 128].as_ptr().cast(), 0);
            l.SetComputeRootShaderResourceView(1, pairs.GetGPUVirtualAddress());
            l.SetComputeRootUnorderedAccessView(2, card.GetGPUVirtualAddress());
            l.SetComputeRootUnorderedAccessView(3, pass.notes.GetGPUVirtualAddress());
            l.Dispatch(1, 1, 1);
        }
        list.close();
        host.execute(&[&list]);
        // SAFETY: the mapping; the list has run.
        let held: Vec<u32> = (0..1024).map(|i| unsafe { card_ptr.cast::<u32>().add(i).read_volatile() }).collect();
        let mut wanted = vec![0u32; 1024];
        (wanted[0], wanted[31]) = (0x6666_6666, 0x1111_1111);
        assert_eq!(held, wanted, "only the pairs inside the 128 bytes are stored");
        assert_eq!(refused() - before, 4, "the four pairs outside are counted");
    }

    /// The bytes the pass is told are the buffer's, and all a pair's offset
    /// can reach of a buffer wider than that.
    #[test]
    fn the_bound_is_the_buffers_width() {
        assert_eq!(bound(0), 0);
        assert_eq!(bound(4096), 4096);
        assert_eq!(bound(u64::from(u32::MAX)), u32::MAX);
        assert_eq!(bound(1 << 32), u32::MAX);
    }
}
