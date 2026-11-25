//! Benchmarks of the locks the list recorders take per descriptor, on the
//! host at hand: the views table's lock (`super::super::VIEWS`) shared or
//! taken in turn by several threads, the host heap's lock on the records
//! its slots were made from (`HostHeap::made`), and the copies' tables a
//! buffer view's copy is looked up in (`buffer_copy`).
//!
//! Only with `VICTORY_MEMBENCH=1` (the tests return at once otherwise), as
//! `crate::membench`; the heap ones need a host GPU. Run under Wine with
//! `VICTORY_MEMBENCH=1 cargo xtask wine-test --vkd3d-proton <dir> -- -p
//! victory-gpu --release --lib host_bench -- --nocapture --test-threads 1`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use super::super::{VIEW_TAG, View, view, words_for};
use super::{HostHeap, buffer_copy, device_removed};

pub(super) fn on() -> bool {
    std::env::var_os("VICTORY_MEMBENCH").is_some_and(|v| v == "1")
}

/// What the bench hands to its threads: the host heap holds COM objects
/// the recorders share the same way in the layer.
pub(super) struct Shared<T>(pub(super) T);
// SAFETY: as the layer's recorders share them (the host heap is reached
// through the title's heap object from every recording thread).
unsafe impl<T> Sync for Shared<T> {}
// SAFETY: as above.
unsafe impl<T> Send for Shared<T> {}

/// Title memory with `n` structured buffer views placed in it, 4 KiB apart,
/// and a title view heap (32-byte slots) holding their records, as the
/// title writes them: the slots' handles and the views.
pub(super) fn placed_buffers(n: usize) -> (Vec<u64>, Vec<View>) {
    const EACH: usize = 4096;
    let memory: &'static mut [u8] = Box::leak(vec![0u8; n * EACH + (64 << 10)].into_boxed_slice());
    let base = (memory.as_ptr() as u64).next_multiple_of(64 << 10);
    let slots: &'static mut [[u32; 8]] = Box::leak(vec![[0u32; 8]; n].into_boxed_slice());
    let mut handles = Vec::with_capacity(n);
    let mut views = Vec::with_capacity(n);
    for (i, slot) in slots.iter_mut().enumerate() {
        let address = base + (i * EACH) as u64;
        let mut resource = [0u32; 14];
        resource[0] = 1; // a buffer
        resource[4] = EACH as u32;
        let mut desc = [0u32; 16];
        // Format unknown, a buffer, the identity mapping; 256 elements of
        // 16 bytes from element 0.
        desc[..10].copy_from_slice(&[0, 1, 5768, 0, 0, 0, (EACH / 16) as u32, 16, 0, 0]);
        (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
        let v = View::Placed(false, resource, desc);
        let [i_word, generation] = words_for(v.clone());
        slot[..3].copy_from_slice(&[VIEW_TAG, i_word, generation]);
        handles.push(slot.as_ptr() as u64);
        views.push(v);
    }
    (handles, views)
}

/// Wall time of `threads` threads each running `f(thread)`, started
/// together.
fn together(threads: usize, f: impl Fn(usize) + Sync) -> f64 {
    let go = AtomicBool::new(false);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let (go, f) = (&go, &f);
                s.spawn(move || {
                    while !go.load(Ordering::Acquire) {
                        std::hint::spin_loop();
                    }
                    f(t)
                })
            })
            .collect();
        let started = Instant::now();
        go.store(true, Ordering::Release);
        for h in handles {
            h.join().expect("bench thread");
        }
        started.elapsed().as_secs_f64() * 1e3
    })
}

/// The views table's lock: every recorder looks up the view of every slot a
/// draw reads (`view_at`: an entry and its generation, the view cloned).
/// Read through a mutex the recorders take turns; through a read-write lock
/// they share it. Here the same table and lookups under each, by one thread
/// and by five (the title's recording threads), and the table itself
/// (`descriptor::view`, which is a read-write lock) by five.
#[test]
fn views_lock_bench() {
    if !on() {
        return;
    }
    const ENTRIES: usize = 4096;
    const LOOKS: usize = 400_000;
    let (handles, views) = placed_buffers(ENTRIES);
    let table: Vec<(View, u32)> = views.iter().cloned().map(|v| (v, 1)).collect();
    let mutex = std::sync::Mutex::new(table.clone());
    let rw = std::sync::RwLock::new(table);
    let look = |list: &Vec<(View, u32)>, i: usize| -> Option<View> {
        let (v, current) = list.get(i)?;
        (*current == 1).then(|| v.clone())
    };
    for round in 0..2 {
        for threads in [1usize, 5] {
            let ms = together(threads, |t| {
                for k in 0..LOOKS {
                    let i = (k * 7 + t * 977) % ENTRIES;
                    let list = mutex.lock().unwrap();
                    std::hint::black_box(look(&list, i));
                }
            });
            println!(
                "membench: views table under a mutex, {threads} thread(s) x {LOOKS} lookups: {ms:.1} ms ({:.0} ns a lookup a thread)",
                ms * 1e6 / LOOKS as f64
            );
            let ms = together(threads, |t| {
                for k in 0..LOOKS {
                    let i = (k * 7 + t * 977) % ENTRIES;
                    let list = rw.read().unwrap();
                    std::hint::black_box(look(&list, i));
                }
            });
            println!(
                "membench: views table under a read-write lock, {threads} thread(s) x {LOOKS} lookups: {ms:.1} ms ({:.0} ns a lookup a thread)",
                ms * 1e6 / LOOKS as f64
            );
            let ms = together(threads, |t| {
                for k in 0..LOOKS {
                    let i = (k * 7 + t * 977) % ENTRIES;
                    std::hint::black_box(view(handles[i]));
                }
            });
            println!(
                "membench: descriptor::view (the layer's table, a read-write lock), {threads} thread(s) x {LOOKS} lookups: {ms:.1} ms ({:.0} ns a lookup a thread){}",
                ms * 1e6 / LOOKS as f64,
                if round == 0 { " [warm-up round]" } else { "" }
            );
        }
    }
}

/// The host heap's lock on the records its slots were made from
/// (`HostHeap::made`), taken for every slot a draw reads through a table
/// (`HostHeap::mirror` for a draw): one thread, and five at once, over slots
/// made already (the usual case: the record did not change).
#[test]
fn made_lock_bench() {
    if !on() {
        return;
    }
    const N: usize = 4096;
    const ROUNDS: usize = 40;
    let (handles, views) = placed_buffers(N);
    let Some(heap) = HostHeap::new(false, N as u32) else {
        return; // no host GPU
    };
    for (i, &h) in handles.iter().enumerate() {
        assert_eq!(heap.mirror(i as u32, h, true).as_ref(), Some(&views[i]));
    }
    let heap = Shared(heap);
    for threads in [1usize, 5, 1, 5] {
        let ms = together(threads, |t| {
            for r in 0..ROUNDS {
                for k in 0..N {
                    let i = (k + t * 811 + r) % N;
                    std::hint::black_box(heap.0.mirror(i as u32, handles[i], true));
                }
            }
        });
        println!(
            "membench: HostHeap::mirror of a slot made already, {threads} thread(s) x {} slots: {ms:.1} ms ({:.0} ns a slot a thread)",
            N * ROUNDS,
            ms * 1e6 / (N * ROUNDS) as f64
        );
    }
    // What a draw does next with a buffer view it reads (`readable` in
    // `crate::draw`): its host copy looked up (`buffer_copy`, under the
    // copies' table locks in `crate::shadow`, here the windows outside
    // buffers').
    for threads in [1usize, 5, 1, 5] {
        let ms = together(threads, |t| {
            for r in 0..ROUNDS / 4 {
                for k in 0..N {
                    let i = (k + t * 811 + r) % N;
                    std::hint::black_box(buffer_copy(&views[i]));
                }
            }
        });
        println!(
            "membench: buffer_copy of a slot's view (its copy made already), {threads} thread(s) x {} slots: {ms:.1} ms ({:.0} ns a slot a thread)",
            N * ROUNDS / 4,
            ms * 1e6 / (N * ROUNDS / 4) as f64
        );
    }
    assert_eq!(device_removed(), None);
}
