//! What a copy-up costs besides the copying, on the host at hand: a
//! submission's refreshes of host copies (`Shadow::update`, as
//! `Batch::flush` in `crate::host` runs them, one write-watch asking ahead
//! for all of them) taking the same bytes in one piece a copy, in many
//! pieces, in one small piece, and taking none; beside the kernel's copies
//! of the same pieces alone and a plain `memcpy` of them.
//!
//! Only with `VICTORY_MEMBENCH=1` (the test returns at once otherwise) and a
//! host GPU (the copies are mapped host buffers). Run under Wine with
//! `VICTORY_MEMBENCH=1 cargo xtask wine-test --vkd3d-proton <dir> -- -p
//! victory-gpu --release --lib copy_up_pieces_bench -- --nocapture`.

use std::sync::Arc;
use std::time::Instant;

use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_WRITE, MEM_COMMIT, MEM_RESERVE, MapViewOfFile, PAGE_READWRITE,
    VirtualAlloc,
};

use super::{PAGE, Shadow, host};

const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// Bytes of each copy.
const LEN: usize = 2 << 20;
/// Pages (64 KiB) of each copy.
const PAGES: usize = LEN / PAGE;

/// Title memory for `n` copies: write-watched allocations, or views of a
/// section (what the guards take: the runtime's physical pages).
fn title_memory(n: usize, views: bool) -> Vec<usize> {
    (0..n)
        .map(|_| {
            // SAFETY: fresh memory of the bench's, never freed.
            let at = unsafe {
                if views {
                    let section = CreateFileMappingW(
                        -1isize as _,
                        std::ptr::null(),
                        PAGE_READWRITE,
                        0,
                        LEN as u32,
                        std::ptr::null(),
                    );
                    MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, LEN).Value as usize
                } else {
                    VirtualAlloc(
                        std::ptr::null(),
                        LEN,
                        MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                        PAGE_READWRITE,
                    ) as usize
                }
            };
            assert_ne!(at, 0);
            at
        })
        .collect()
}

/// The 64 KiB pages of each copy a pattern writes.
fn written(pattern: usize) -> Vec<usize> {
    match pattern {
        // Every page: one piece of 2 MiB a copy.
        0 => (0..PAGES).collect(),
        // Every other page: 16 pieces of 64 KiB a copy (1 MiB).
        1 => (0..PAGES).step_by(2).collect(),
        // Every other page from the second on, a fourth of each written:
        // the same pieces as above, other pages.
        2 => (1..PAGES).step_by(2).collect(),
        // One page: one piece of 64 KiB.
        3 => vec![PAGES / 2],
        // Nothing: what a refresh costs that copies nothing.
        _ => Vec::new(),
    }
}

const NAMES: [&str; 5] = [
    "every page written (one 2 MiB piece a copy)",
    "every other page written (16 pieces of 64 KiB a copy)",
    "every other page written, the others (16 pieces of 64 KiB a copy)",
    "one page written (one 64 KiB piece a copy)",
    "nothing written (no piece)",
];

#[test]
fn copy_up_pieces_bench() {
    if std::env::var_os("VICTORY_MEMBENCH").is_none_or(|v| v != "1") {
        return;
    }
    const COPIES: usize = 64;
    const ROUNDS: usize = 8;
    for views in [false, true] {
        let titles = title_memory(COPIES, views);
        let mut copies = Vec::new();
        for &title in &titles {
            let Some((buffer, data)) = host().and_then(|h| h.shared_buffer(LEN as u64)) else {
                return; // no host GPU here
            };
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            copies.push((
                buffer.clone(),
                Arc::new(Shadow::new(buffer, data, gpu, title as u64, LEN as u64)),
                title,
            ));
        }
        let kind = if views {
            "guarded section views"
        } else {
            "write-watched memory"
        };
        // What a submission's preparing does: one asking ahead for all the
        // ranges, then each refresh.
        let refresh_all = || -> usize {
            let ranges: Vec<(usize, usize)> = copies.iter().map(|c| (c.2, c.2 + LEN)).collect();
            crate::watch::ask_ahead(&ranges);
            let bytes = copies
                .iter()
                .map(|(_, shadow, title)| shadow.update(*title, LEN))
                .sum();
            crate::watch::ask_ahead_done();
            bytes
        };
        // The first refresh takes everything.
        refresh_all();
        let mut round = 0u8;
        let mut write = |pages: &[usize]| {
            round = round.wrapping_add(1);
            for &title in &titles {
                for &page in pages {
                    // A fourth of the page's 4 KiB pages, as a title's
                    // scattered stores would.
                    for k in (0..PAGE / 4096).step_by(4) {
                        // SAFETY: the bench's own title memory.
                        unsafe {
                            ((title + page * PAGE + k * 4096) as *mut u8).write_volatile(round)
                        };
                    }
                }
            }
        };
        for pattern in 0..NAMES.len() {
            let pages = written(pattern);
            let pieces = pieces(&pages);
            let (mut ms, mut bytes) = (0.0, 0);
            for _ in 0..ROUNDS {
                write(&pages);
                let started = Instant::now();
                bytes += refresh_all();
                ms += started.elapsed().as_secs_f64() * 1e3 / ROUNDS as f64;
            }
            let bytes = bytes / ROUNDS;
            assert_eq!(
                bytes,
                COPIES * pages.len() * PAGE,
                "the written pages, and nothing else, copied"
            );
            // The same pieces through the kernel's copy alone, and by memcpy.
            let mut scratch = vec![0u8; LEN];
            let started = Instant::now();
            for _ in 0..ROUNDS {
                for &title in &titles {
                    for &(first, n) in &pieces {
                        let at = title + first * PAGE;
                        assert!(crate::host::read_into(
                            at as *const u8,
                            &mut scratch[first * PAGE..(first + n) * PAGE]
                        ));
                    }
                }
            }
            let kernel = started.elapsed().as_secs_f64() * 1e3 / ROUNDS as f64;
            let started = Instant::now();
            for _ in 0..ROUNDS {
                for &title in &titles {
                    for &(first, n) in &pieces {
                        // SAFETY: the bench's own title memory, readable.
                        let from = unsafe {
                            std::slice::from_raw_parts(
                                (title + first * PAGE) as *const u8,
                                n * PAGE,
                            )
                        };
                        scratch[first * PAGE..(first + n) * PAGE].copy_from_slice(from);
                    }
                }
            }
            std::hint::black_box(&scratch);
            let plain = started.elapsed().as_secs_f64() * 1e3 / ROUNDS as f64;
            let n = COPIES * pieces.len();
            println!(
                "membench: copy-up of {COPIES} copies of 2 MiB ({kind}), {}: {ms:.2} ms for {:.1} MB in {n} pieces; \
                 the same pieces by the kernel's copy alone {kernel:.2} ms, by memcpy {plain:.2} ms; \
                 besides the kernel's copy {:.1} us a copy, {:.2} us a piece",
                NAMES[pattern],
                bytes as f64 / 1e6,
                (ms - kernel) * 1e3 / COPIES as f64,
                if n > 0 {
                    (ms - kernel) * 1e3 / n as f64
                } else {
                    0.0
                },
            );
            // Again with the memory calls timed (`VICTORY_VM_STATS`), apart
            // from the times above: which calls the difference is.
            crate::vmstat::force(true);
            drop(crate::vmstat::lines_now(1.0));
            for _ in 0..ROUNDS {
                write(&pages);
                refresh_all();
            }
            for line in crate::vmstat::lines_now(ROUNDS as f64) {
                if line.contains("site ") || line.contains("all our calls") {
                    println!("membench:   {line}");
                }
            }
            crate::vmstat::force(false);
        }
    }
}

/// Runs of consecutive pages, as (first, count).
fn pieces(pages: &[usize]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &p in pages {
        match out.last_mut() {
            Some((first, n)) if *first + *n == p => *n += 1,
            _ => out.push((p, 1)),
        }
    }
    out
}
