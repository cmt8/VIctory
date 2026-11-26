//! Typed buffer loads that move 16-bit halves without the d16 form: a
//! format load of whole dwords whose result the program reads only one
//! half of, as a half float, moves 16-bit integers, like a d16 access
//! ([`super::super::HostShader::half_views`]).

use std::collections::HashSet;

use super::super::bitcode::{Attr, Bin, V};
use super::{
    Fields, Inst, Lower, Numeric, Operand, Reg, Role, RootType, Source, Value, Why, buffer_format,
};

impl<'a> Lower<'a> {
    /// The table descriptor `src` is, as (root parameter, dword in the
    /// table), when the code loads it from a fixed place in a table: the
    /// load's own place, not the start of the range its used-resource
    /// entry describes (a range of several descriptors starts before most
    /// of them). `None` for a descriptor picked at run time, built in the
    /// code or chosen by control flow.
    pub(super) fn table_place(&self, src: &Source) -> Option<(u8, u32)> {
        let Source::Loaded { load, first } = src else {
            return None;
        };
        let a = self.scalar.get(load)?;
        let rp = self.x.native.layout.get(a.param)?;
        (rp.root == RootType::Table && a.dynamic.is_none())
            .then(|| (rp.slot, a.offset / 4 + u32::from(*first)))
    }

    /// Whether the buffer access `inst` through `src` moves sixteen bits a
    /// channel through a view of a table: the bits go as they are, as
    /// unsigned integers, and the host is told (the place goes into
    /// `half_views`), which shows the shader a 16-bit float view there as
    /// its unsigned twin ([`super::super::HostShader::half_views`]). A
    /// format load of whole dwords whose every use reads one half of a
    /// result as a half float moves 16-bit integers too (the head
    /// program's reads of the face network's output), where every access
    /// through its view does ([`Self::places_read_as_halves`]). A d16
    /// access too only where every access does: a whole float read
    /// through the same place would see the unsigned twin.
    pub(super) fn moves_halves(&mut self, inst: &Inst, src: &Source, typed: bool, d16: bool) -> bool {
        let place = typed.then(|| self.table_place(src)).flatten().filter(|p| {
            self.halves_only.contains(p) && (d16 || self.format_load_read_as_halves(inst))
        });
        if let Some(place) = place {
            self.half_views.insert(place);
        }
        // (A tbuffer load names its format and decodes it exactly.)
        let tbuffer = matches!(inst.fields, Fields::Buffer { format: Some(_), .. });
        if super::super::half_report() && typed && !tbuffer {
            // (`take_half_report`: d16 as bits, d16 by way of floats, a
            // format load read as halves as bits, and by way of floats.)
            let as_halves = d16 || self.format_load_read_as_halves(inst);
            if as_halves {
                let k = usize::from(!d16) * 2 + usize::from(place.is_none());
                self.half_counts[k] += 1;
            }
        }
        place.is_some()
    }

    /// Hands the counts of the program's 16-bit typed buffer accesses to
    /// [`take_half_report`] (`VICTORY_HALF_REPORT=1`), as the lowering
    /// that succeeded made them.
    pub(super) fn report_halves(&mut self) {
        if super::super::half_report() {
            HALF_REPORT.set(Some(std::mem::take(&mut self.half_counts)));
        }
    }
}

thread_local! {
    /// See [`take_half_report`].
    static HALF_REPORT: std::cell::Cell<Option<[u32; 4]>> = const { std::cell::Cell::new(None) };
}

/// With `VICTORY_HALF_REPORT=1`: of the program this thread translated
/// last (since the last call), its typed buffer accesses that move 16-bit
/// halves, as (d16 accesses moved as bits, d16 accesses moved by way of
/// floats, format loads read only as halves moved as bits, such loads
/// moved by way of floats). An access by way of floats through one of the
/// title's 16-bit unsigned views does not keep its bits (the face network
/// computed zeros so). `None` without the switch, or with nothing
/// translated since.
pub fn take_half_report() -> Option<[u32; 4]> {
    HALF_REPORT.take()
}

impl<'a> Lower<'a> {

    /// Channels of buffer format `format`, as [`Self::format_load`] gives
    /// them, as d16 data: 16 bits each, two per dword, low half first (an
    /// odd last high half 0). Integer formats keep their low 16 bits; the
    /// others become f16.
    pub(super) fn d16_channels(&mut self, format: u8, channels: Vec<V>) -> Result<Vec<V>, Why> {
        let (_, numeric) = buffer_format(format).ok_or("a buffer format")?;
        let c = self.cur;
        let mut halves = Vec::new();
        for x in channels {
            halves.push(if matches!(numeric, Numeric::Uint | Numeric::Sint) {
                let m = self.s.m.i32(0xFFFF);
                self.s.m.bin(c, Bin::And, x, m)
            } else {
                let (f, i32t) = (self.as_float(x), self.s.t.i32);
                self.s
                    .op(c, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f])
            });
        }
        let mut out = Vec::new();
        for pair in halves.chunks(2) {
            out.push(match *pair {
                [lo, hi] => {
                    let k16 = self.s.m.i32(16);
                    let hi = self.s.m.bin(c, Bin::Shl, hi, k16);
                    self.s.m.bin(c, Bin::Or, lo, hi)
                }
                [lo] => lo,
                _ => unreachable!("chunks of one or two"),
            });
        }
        Ok(out)
    }

    /// Whether every use of `value`, a typed buffer load's result, reads
    /// one 16-bit half of it as a half float: a `v_fma_mix*` source with
    /// its `op_sel_hi` bit set, or `v_cvt_f32_f16`. A program that takes a
    /// format load apart like that reads a 16-bit integer view on the
    /// console (the load gives the zero-extended integer, whose bits are a
    /// half; a float view's value it would not take apart). A use is a
    /// read as an operand ([`crate::ir::operand_reads`]): a later write of
    /// the register reads the value only to keep it in the lanes EXEC
    /// leaves alone, and a phi carries it on, its own uses counting. False
    /// without a use.
    pub(super) fn read_only_as_halves(&self, value: Value) -> bool {
        let p = self.x.p;
        let mut used = false;
        let mut seen = HashSet::from([value]);
        let mut todo = vec![value];
        while let Some(value) = todo.pop() {
            for b in &p.blocks {
                for phi in b.phis.iter().filter(|phi| phi.args.contains(&value)) {
                    if seen.insert(phi.value) {
                        todo.push(phi.value);
                    }
                }
                for i in &b.insts {
                    if !i.reads.iter().any(|r| r.1 == value) {
                        continue;
                    }
                    let operands = crate::ir::operand_reads(p, i);
                    let regs: Vec<Reg> = i
                        .reads
                        .iter()
                        .filter(|r| r.1 == value && operands.contains(&r.0))
                        .map(|r| r.0)
                        .collect();
                    if regs.is_empty() {
                        continue;
                    }
                    used = true;
                    let Fields::Valu(v) = &i.fields else {
                        return false;
                    };
                    match i.name() {
                        "v_cvt_f32_f16" => {}
                        "v_fma_mix_f32" | "v_fma_mixlo_f16" | "v_fma_mixhi_f16" => {
                            let whole = v.src.iter().enumerate().any(|(k, op)| {
                                matches!(*op, Operand::Vector { reg, .. } if regs.contains(&Reg::V(reg)))
                                    && v.opsel_hi >> k & 1 == 0
                            });
                            if whole {
                                return false;
                            }
                        }
                        _ => return false,
                    }
                }
            }
        }
        used
    }

    /// Whether `inst` is a format load of whole dwords (not d16) whose
    /// every channel is read only as halves ([`Self::read_only_as_halves`]).
    pub(super) fn format_load_read_as_halves(&self, inst: &Inst) -> bool {
        let Fields::Buffer {
            data: Some(Operand::Vector { reg, .. }),
            ..
        } = inst.fields
        else {
            return false;
        };
        let name = inst.name();
        let count = match name
            .strip_prefix("buffer_load_format_")
            .or(name.strip_prefix("tbuffer_load_format_"))
        {
            Some("x") => 1,
            Some("xy") => 2,
            Some("xyz") => 3,
            Some("xyzw") => 4,
            _ => return false,
        };
        (0..count).all(|k| {
            inst.write(Reg::V(reg + k))
                .is_some_and(|v| self.read_only_as_halves(v))
        })
    }

    /// The table places ([`Self::table_place`]) every access through which
    /// moves halves: a d16 access, or a format load read only as halves
    /// ([`Self::format_load_read_as_halves`]). A place the program reaches
    /// otherwise too is left out (a format load of numbers beside one of
    /// halves, a raw or scalar load, a descriptor chosen by control flow),
    /// and so is every place of a table the program also picks from at run
    /// time: what the program makes of such a view is not halves alone.
    pub(super) fn places_read_as_halves(&self) -> std::collections::BTreeSet<(u8, u32)> {
        let mut halves = std::collections::BTreeSet::new();
        // (None with the switch off: every access keeps the float path.)
        if !super::super::half_bits() {
            return halves;
        }
        let mut other = std::collections::BTreeSet::new();
        let mut picked = HashSet::new();
        for d in &self.x.res.descriptors {
            if d.role == Role::Sampler {
                continue;
            }
            let inst = self
                .x
                .p
                .blocks
                .get(d.site.0)
                .and_then(|b| b.insts.get(d.site.1));
            let half = matches!(d.source, Source::Loaded { .. })
                && inst.is_some_and(|i| {
                    (i.name().starts_with("buffer_") && i.name().contains("_format_d16_"))
                        || self.format_load_read_as_halves(i)
                });
            for leaf in d.source.leaves() {
                let Source::Loaded { load, .. } = leaf else {
                    continue;
                };
                let Some(a) = self.scalar.get(load) else {
                    continue;
                };
                if a.dynamic.is_some() {
                    if let Some(rp) = self.x.native.layout.get(a.param)
                        && rp.root == RootType::Table
                    {
                        picked.insert(rp.slot);
                    }
                } else if let Some(place) = self.table_place(leaf) {
                    if half {
                        halves.insert(place);
                    } else {
                        other.insert(place);
                    }
                }
            }
        }
        halves.retain(|p| !other.contains(p) && !picked.contains(&p.0));
        halves
    }
}
