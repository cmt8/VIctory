//! The buffer part of the pass over the heap's buffer views
//! (`State::heap_buffers` in `crate::draw`), one slot after another and
//! several at a time, on the host at hand: whether `VICTORY_WIDE_TEXTURES`'
//! way would pay for buffers too.
//!
//! Only with `VICTORY_MEMBENCH=1` and a host GPU. Under Wine:
//! `VICTORY_MEMBENCH=1 cargo xtask wine-test --vkd3d-proton <dir> -- -p
//! victory-gpu --release --lib heap_buffers_bench -- --nocapture`.

use std::time::Instant;

use super::super::view;
use super::host_bench::{Shared, on, placed_buffers};
use super::{HostHeap, buffer_copy, device_removed};

/// The buffer part of the pass over the heap (`State::heap_buffers`): for
/// each slot, `HostHeap::mirror` for a draw and `buffer_copy`. Slots whose
/// record changed since (a host view made anew: a fresh heap), and slots
/// made already (the pass's every-16th look at all of them); one after
/// another, and several at a time on the threads kept for the snapshots
/// (`shadow::wide_run`), as `VICTORY_WIDE_TEXTURES` runs texture checks.
/// The results of both ways are compared.
#[test]
fn heap_buffers_bench() {
    if !on() {
        return;
    }
    const N: usize = 4096;
    let (handles, views) = placed_buffers(N);
    let handles = std::sync::Arc::new(handles);
    // One pass, `wide` or not: per slot, the copy (as a pointer), the
    // address and the length.
    let pass =
        |heap: &std::sync::Arc<Shared<HostHeap>>, wide: bool| -> Vec<Option<(usize, u64, u64)>> {
            let one = |heap: &HostHeap, handles: &[u64], i: usize| {
                let v = heap.mirror(i as u32, handles[i], true)?;
                let (copy, address, len) = buffer_copy(&v)?;
                Some((std::sync::Arc::as_ptr(&copy) as usize, address, len))
            };
            if !wide {
                return (0..N).map(|i| one(&heap.0, &handles, i)).collect();
            }
            const PARTS: usize = 16;
            let out = std::sync::Arc::new(std::sync::Mutex::new(vec![None; N]));
            let jobs: Vec<Box<dyn FnOnce() + Send>> = (0..PARTS)
                .map(|p| {
                    let (heap, handles, out) = (heap.clone(), handles.clone(), out.clone());
                    Box::new(move || {
                        let part: Vec<_> = (p * N / PARTS..(p + 1) * N / PARTS)
                            .map(|i| (i, one(&heap.0, &handles, i)))
                            .collect();
                        let mut out = out.lock().unwrap();
                        for (i, r) in part {
                            out[i] = r;
                        }
                    }) as Box<dyn FnOnce() + Send>
                })
                .collect();
            crate::shadow::wide_run(jobs);
            std::mem::take(&mut *out.lock().unwrap())
        };
    // The copies are made once, by a first heap (as in the world, where
    // they outlive the slots that changed).
    let Some(first) = HostHeap::new(false, N as u32) else {
        return; // no host GPU
    };
    let first = std::sync::Arc::new(Shared(first));
    let reference = pass(&first, false);
    assert!(
        reference.iter().all(Option::is_some),
        "every slot has its copy"
    );
    for (i, v) in views.iter().enumerate() {
        assert_eq!(view(handles[i]).as_ref(), Some(v));
    }
    for wide in [false, true, false, true] {
        let (mut changed, mut made) = (0.0, 0.0);
        const ROUNDS: usize = 4;
        for _ in 0..ROUNDS {
            let heap =
                std::sync::Arc::new(Shared(HostHeap::new(false, N as u32).expect("made before")));
            let started = Instant::now();
            let got = pass(&heap, wide);
            changed += started.elapsed().as_secs_f64() * 1e3 / ROUNDS as f64;
            assert_eq!(got, reference, "the same copies and ranges either way");
            let started = Instant::now();
            let got = pass(&heap, wide);
            made += started.elapsed().as_secs_f64() * 1e3 / ROUNDS as f64;
            assert_eq!(got, reference, "the same copies and ranges either way");
            assert_eq!(
                *heap.0.made.lock().unwrap(),
                *first.0.made.lock().unwrap(),
                "the same records made"
            );
        }
        let how = if wide {
            "several at a time"
        } else {
            "one after another"
        };
        println!(
            "membench: heap pass, buffer part, {N} slots whose record changed, {how}: {changed:.2} ms"
        );
        println!("membench: heap pass, buffer part, {N} slots made already, {how}: {made:.2} ms");
    }
    assert_eq!(device_removed(), None);
}
