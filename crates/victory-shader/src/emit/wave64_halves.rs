//! A console wave64 program in host waves of 32: what the 32-lane forms
//! have to know beyond the lowering's own choice of them.

use super::super::bitcode::{Bin, V, pred};
use super::{Lower, Parts, all_lane_blocks, lane_uniform};
use crate::gfx10::{Fields, Operand};
use crate::ir::Reg;

/// Whether a `v_readfirstlane_b32` of the program reads a VGPR that may
/// differ between the lanes of a console wave (not [`lane_uniform`]; not a
/// waterfall loop's descriptor read, which is the lane's own), with
/// [`super::super::compute_wave_32`] only.
///
/// Such a read makes the program's lanes the console's (`console_lanes`:
/// threads relabelled as the halves exchange them) and crosses halves.
/// The console's first lane of a wave64 program is in its lower half, so
/// the upper half's host wave has to get the value from the lower one
/// (`exchange`): without anything else crossing halves the lowering would
/// split the wave without that, and the upper half would read its own
/// first lane (lane 32: on the RX 7900 XTX threads 32..63 of a group of 64
/// with LDS read thread 32's value). The program then takes the exchanging
/// form, or keeps 64 lanes where that form cannot be had.
/// (NVIDIA's forms are left as they were, the upper half reading its own
/// first lane: there no form in waves of 64 is left to fall back on where
/// the exchange cannot be had.)
pub(super) fn first_lane_crosses(x: &Parts) -> bool {
    if !x.native.wave64 || !super::super::compute_wave_32() {
        return false;
    }
    let reads: Vec<_> = x
        .p
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter(|i| i.name() == "v_readfirstlane_b32")
        .filter(|i| !i.writes.first().is_some_and(|w| x.res.waterfall.reads.contains(&w.1)))
        .filter_map(|i| match &i.fields {
            Fields::Valu(v) => match v.src.first() {
                Some(&Operand::Vector { reg, .. }) => Some(i.read(Reg::V(reg))),
                _ => None,
            },
            _ => Some(None),
        })
        .collect();
    if reads.is_empty() {
        return false;
    }
    let Some(all) = all_lane_blocks(x.st) else {
        return true;
    };
    let same = lane_uniform(x.p, x.st, &all);
    reads.iter().any(|r| r.is_none_or(|v| !same.contains(&v)))
}

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
        if group <= 32 || group % 64 == 0 || group % 64 > 32 {
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
