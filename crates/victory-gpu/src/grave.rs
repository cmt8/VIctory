//! The order in which host objects let go are dropped (`crate::host::retire`
//! and `crate::host::bury`).
//!
//! An object let go while a submission may still use it waits for the
//! submission count read when it was let go: it is dropped once the
//! submissions up to that count, plus `VICTORY_BURY_LAG` more, have
//! finished. The waiting objects were one `Vec` that every bury split in
//! two whole new ones (84,000 to 196,000 entries, some 4.7 MB, 12 to 13
//! times a frame at the relay's landings alone, with the lock held all the
//! while, so every thread letting go of an object waited): some 35 of the
//! relay thread's 46 ms a frame, on the path the submitting thread waits
//! on. Here they are a queue kept in the order of the count they wait for,
//! and a bury takes only the front that is due.

use std::collections::VecDeque;

/// Objects that wait for a submission count, front first in the order of
/// that count (equal counts in the order they came).
pub(crate) struct Queue<T> {
    entries: VecDeque<(u64, T)>,
}

impl<T> Queue<T> {
    pub(crate) const fn new() -> Self {
        Self { entries: VecDeque::new() }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// `object` waits for submission count `at`. The count is read before
    /// the queue's lock is had, so a thread can come in with an older count
    /// than the one before it: such an object goes in after every object
    /// that waits for no later count (where the entries' order has it),
    /// not at the back, so the front stays the next due.
    pub(crate) fn push(&mut self, at: u64, object: T) {
        match self.entries.back() {
            Some(&(last, _)) if last > at => {
                let i = self.entries.partition_point(|&(a, _)| a <= at);
                self.entries.insert(i, (at, object));
            }
            _ => self.entries.push_back((at, object)),
        }
    }

    /// Moves every object due once submissions up to `finished` have
    /// finished with `lag` more after them (each waits for a count at most
    /// `finished - lag`) onto `out`, front first. The same test as the
    /// whole-queue split it replaces: `at <= finished.saturating_sub(lag)`.
    /// Nothing is dropped here: the caller drops `out` once the queue's lock
    /// is let go.
    pub(crate) fn take_due(&mut self, finished: u64, lag: u64, out: &mut Vec<T>) {
        let due = finished.saturating_sub(lag);
        while self.entries.front().is_some_and(|&(at, _)| at <= due) {
            if let Some((_, object)) = self.entries.pop_front() {
                out.push(object);
            }
        }
    }
}

/// Whether an object let go when `started` submissions had started and
/// `finished` had finished is dropped at once: with no lag only, and only
/// when nothing runs.
pub(crate) fn at_once(lag: u64, started: u64, finished: u64) -> bool {
    lag == 0 && started <= finished
}

#[cfg(test)]
mod tests {
    use super::{Queue, at_once};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// An object that notes its own drop.
    struct Noted(u32, Rc<RefCell<Vec<u32>>>);
    impl Drop for Noted {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0);
        }
    }

    fn counts<T>(q: &Queue<T>) -> Vec<u64> {
        q.entries.iter().map(|e| e.0).collect()
    }

    /// The front is taken while it is due, and nothing else; an object
    /// taken is dropped only by whoever holds it after.
    #[test]
    fn a_bury_takes_the_due_front_only() {
        let dropped = Rc::new(RefCell::new(Vec::new()));
        let mut q = Queue::new();
        for (id, at) in [(0, 1), (1, 1), (2, 2), (3, 3), (4, 70)] {
            q.push(at, Noted(id, dropped.clone()));
        }
        let mut out = Vec::new();
        // 66 finished, 64 more held: counts up to 2 are due.
        q.take_due(66, 64, &mut out);
        assert_eq!(out.iter().map(|n| n.0).collect::<Vec<_>>(), [0, 1, 2]);
        assert!(dropped.borrow().is_empty(), "taken objects are not dropped by the queue");
        drop(out);
        assert_eq!(*dropped.borrow(), [0, 1, 2]);
        assert_eq!(counts(&q), [3, 70]);
        let mut out = Vec::new();
        q.take_due(66, 64, &mut out);
        assert!(out.is_empty(), "the same bury again takes nothing");
        // Fewer finished than the lag: only what waits for count 0.
        q.take_due(10, 64, &mut out);
        assert!(out.is_empty());
        q.take_due(134, 64, &mut out);
        assert_eq!(out.iter().map(|n| n.0).collect::<Vec<_>>(), [3, 4]);
        assert_eq!(q.len(), 0);
    }

    /// An object that comes in with an older count than the one before it
    /// goes in by its count: it is dropped when its own count is due, and
    /// the newer ones behind it wait for theirs.
    #[test]
    fn out_of_order_counts_wait_for_their_own() {
        let mut q = Queue::new();
        for (id, at) in [(0u32, 5u64), (1, 7), (2, 6), (3, 5), (4, 3), (5, 7), (6, 6)] {
            q.push(at, id);
        }
        assert_eq!(counts(&q), [3, 5, 5, 6, 6, 7, 7]);
        let mut out = Vec::new();
        q.take_due(5, 0, &mut out);
        assert_eq!(out, [4, 0, 3], "equal counts in the order they came");
        assert_eq!(counts(&q), [6, 6, 7, 7]);
        out.clear();
        q.take_due(6, 0, &mut out);
        assert_eq!(out, [2, 6]);
        out.clear();
        q.take_due(7, 0, &mut out);
        assert_eq!(out, [1, 5]);
    }

    /// A bury with an older count than the last takes nothing more: no
    /// object goes before its count is due, whatever order the buries
    /// come in.
    #[test]
    fn an_older_bury_takes_nothing_more() {
        let mut q = Queue::new();
        for at in 1..=20u64 {
            q.push(at, at);
        }
        let mut out = Vec::new();
        q.take_due(10, 0, &mut out);
        assert_eq!(out.len(), 10);
        out.clear();
        q.take_due(8, 0, &mut out);
        assert!(out.is_empty());
        q.take_due(12, 0, &mut out);
        assert_eq!(out, [11, 12]);
    }

    /// `VICTORY_BURY_LAG=0`: an object let go while nothing runs is dropped
    /// at once, one let go while a submission runs waits for it to finish
    /// (and not longer); with a lag nothing is dropped at once.
    #[test]
    fn no_lag_drops_at_once_only_while_nothing_runs() {
        assert!(at_once(0, 4, 4));
        assert!(!at_once(0, 5, 4), "submission 5 runs");
        assert!(!at_once(64, 4, 4), "with a lag every object waits");
        let mut q = Queue::new();
        q.push(5, 'a');
        let mut out = Vec::new();
        q.take_due(4, 0, &mut out);
        assert!(out.is_empty(), "submission 5 has not finished");
        q.take_due(5, 0, &mut out);
        assert_eq!(out, ['a']);
    }

    /// Against the whole-queue split it replaces, over random lets-go
    /// (with counts that come in up to a few late) and buries (now and then
    /// older than the last): every bury drops the same objects, and no
    /// object is ever dropped before its count plus the lag has finished.
    #[test]
    fn as_the_split_did() {
        for lag in [0u64, 1, 64] {
            let mut seed = 0x2545_f491_4f6c_dd1du64 ^ lag;
            let mut next = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };
            let mut q = Queue::new();
            let mut split: Vec<(u64, u32)> = Vec::new();
            let mut started = 0u64;
            let mut finished = 0u64;
            for id in 0..20_000u32 {
                match next() % 8 {
                    0 => started += 1,
                    1 if finished < started => {
                        finished += 1 + next() % (started - finished);
                        // Now and then a bury of an older count.
                        let bury = if next() % 5 == 0 { finished.saturating_sub(next() % 4) } else { finished };
                        let mut out = Vec::new();
                        q.take_due(bury, lag, &mut out);
                        let (mut due, keep): (Vec<_>, Vec<_>) =
                            std::mem::take(&mut split).into_iter().partition(|(at, _)| *at <= bury.saturating_sub(lag));
                        split = keep;
                        let mut taken = out.clone();
                        taken.sort_unstable();
                        due.sort_unstable_by_key(|e| e.1);
                        assert_eq!(taken, due.iter().map(|e| e.1).collect::<Vec<_>>(), "lag {lag}, bury {bury}");
                        for (at, _) in &due {
                            assert!(at + lag <= bury.max(lag), "dropped before its count {at} plus the lag {lag} finished");
                        }
                    }
                    _ => {
                        // Read a moment before the lock: up to three late.
                        let at = started.saturating_sub(next() % 4);
                        if !at_once(lag, at, finished) {
                            q.push(at, id);
                            split.push((at, id));
                        }
                    }
                }
                assert_eq!(q.len(), split.len());
            }
            let c = counts(&q);
            assert!(c.windows(2).all(|w| w[0] <= w[1]), "the queue stays in count order");
        }
    }
}
