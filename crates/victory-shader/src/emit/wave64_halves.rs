//! A console wave64 program in host waves of 32: what the 32-lane forms
//! have to know beyond the lowering's own choice of them.

use super::super::bitcode::{Bin, V, pred};
use super::Lower;

impl<'a> Lower<'a> {
    /// The upper half's value `hi` that `exchange_halves` read for this
    /// thread's console wave, or `instead` (0 if none) where that wave has
    /// no upper half: the last console wave of a group whose size is not a
    /// multiple of 64 and whose last 32 threads or fewer are its lower half
    /// (a group of 96: threads 64..95). No host wave writes that half's
    /// dword, so what is read there is whatever the LDS held; on the
    /// console those lanes are outside EXEC and give nothing (a ballot's
    /// high dword 0) or the operation's identity, which the reduction asks
    /// for by passing the lower half's value (an AND, a minimum).
    /// (A group of 32 threads or fewer exchanges nothing.)
    pub(super) fn missing_upper(&mut self, at: usize, hi: V, instead: Option<V>) -> V {
        let group = self.s.threads.map_or(0, |t| t.iter().product::<u32>());
        if group <= 32 || group % 64 == 0 || group % 64 > 32 || !super::super::missing_upper_on() {
            return hi;
        }
        let instead = instead.unwrap_or_else(|| self.s.m.i32(0));
        let thread = self.console_thread();
        let mask = self.s.m.i32(!63);
        let base = self.s.m.bin(at, Bin::And, thread, mask);
        let k32 = self.s.m.i32(32);
        let upper = self.s.m.bin(at, Bin::Add, base, k32);
        let g = self.s.m.i32(group);
        let here = self.s.m.cmp(at, pred::IULT, upper, g);
        self.s.m.select(at, here, hi, instead)
    }
}
