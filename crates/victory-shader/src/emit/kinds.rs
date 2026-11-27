//! `VICTORY_KINDS_WHY`: what kinds of instruction a translated program is
//! made of, one line `PROGRAM-KINDS <hash> ...` on stderr per program, in the
//! style of `VICTORY_WAVE_WHY`. For finding which lowering paths a program
//! the log names goes through (the face network's, say) without its code.
//!
//! Metadata only: the stage, the console wave, the thread group, the
//! used-resource entries (kind, root type, stride), and a count of each kind
//! of instruction, a kind being the mnemonic with the classes of its operands
//! and its modifiers (SDWA selects, op_sel, abs/neg, clamp, the output
//! modifier, DPP) or a buffer access's format and addressing. No operand
//! numbers, no constants' values, no order: the code cannot be put together
//! again from a line.
//!
//! * `VICTORY_KINDS_WHY=1` (or `all`): every program translated.
//! * `VICTORY_KINDS_WHY=<hash>[,<hash>...]`: the programs the log names by
//!   those hashes; `cs`, `vs`, `ps` and `ms` in the list name every program
//!   of a stage.
//!
//! A cached translation is not made again; whoever caches translations keys
//! a set made with the variable apart ([`asked`]), so a warm-up with it
//! translates every program and leaves the run's set alone.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use super::compute_wave;
use crate::gfx10::{Dpp, Fields, Operand};
use crate::ir;
use crate::native::{HwStage, NativeProgram, StageIo};

/// The variable's value (`None`: unset, nothing is written).
fn value() -> Option<&'static str> {
    static VALUE: OnceLock<Option<String>> = OnceLock::new();
    VALUE.get_or_init(|| std::env::var("VICTORY_KINDS_WHY").ok().filter(|v| !v.trim().is_empty())).as_deref()
}

/// Whether `VICTORY_KINDS_WHY` is set: part of what tells one set of cached
/// translations from another (the lines are written when a program is
/// translated).
pub fn asked() -> bool {
    value().is_some()
}

/// Whether the program of stage `stage` the log names `name` is one to
/// describe.
fn wanted(name: &str, stage: HwStage) -> bool {
    value().is_some_and(|v| listed(v, name, stage))
}

/// Whether the value `v` of the variable lists the program. Pure, for testing.
fn listed(v: &str, name: &str, stage: HwStage) -> bool {
    let stage = match stage {
        HwStage::Cs => "cs",
        HwStage::Vs => "vs",
        HwStage::Ps => "ps",
        HwStage::Ms => "ms",
        _ => "",
    };
    v.split([',', ' ', ';']).map(str::trim).any(|t| t == "1" || t == "all" || t == stage || t.eq_ignore_ascii_case(name))
}

/// The class of an operand: a VGPR, an SGPR, `null`, an inline integer or
/// float constant, a literal, an attribute.
fn class(op: &Operand) -> char {
    match *op {
        Operand::Vector { .. } => 'v',
        Operand::Scalar { code: 125, .. } => '0',
        Operand::Scalar { .. } => 's',
        Operand::Const(240..=248) => 'f',
        Operand::Const(_) => 'k',
        Operand::Literal(_) => 'l',
        Operand::Attr { .. } => 'a',
    }
}

/// An SDWA select by name.
fn select(sel: u8) -> &'static str {
    ["B0", "B1", "B2", "B3", "W0", "W1", "D", "?"][usize::from(sel.min(7))]
}

/// The kind of `inst` ([the module's notes](self)).
pub(super) fn kind(inst: &ir::Inst) -> String {
    let mut s = inst.name().to_owned();
    match &inst.fields {
        Fields::Valu(v) => {
            s.push('(');
            s.extend(v.dst.iter().map(class));
            s.push('<');
            s.extend(v.src.iter().map(class));
            s.push(')');
            if let Some(sd) = v.sdwa {
                let unused = ["pad", "sext", "keep", "3"][usize::from(sd.dst_unused & 3)];
                s += &format!(" sdwa[d={}/{unused}", select(sd.dst_sel));
                for (k, sel) in sd.src_sel.iter().enumerate().take(v.src.len().min(2)) {
                    s += &format!(" s{k}={}{}", select(*sel), if sd.sext >> k & 1 != 0 { "+sext" } else { "" });
                }
                s.push(']');
            }
            match v.dpp {
                Some(Dpp::Row { .. }) => s += " dpp16",
                Some(Dpp::Lanes { .. }) => s += " dpp8",
                None => {}
            }
            for (what, bits) in
                [("opsel", v.opsel), ("opsel_hi", v.opsel_hi), ("neg", v.neg), ("neg_hi", v.neg_hi), ("abs", v.abs)]
            {
                // (VOP3P's op_sel_hi is set for the plain case: said either way there)
                if bits != 0 || what == "opsel_hi" && inst.name().starts_with("v_pk_") {
                    s += &format!(" {what}={bits:b}");
                }
            }
            if v.clamp {
                s += " clamp";
            }
            if v.omod != 0 {
                s += &format!(" omod={}", v.omod);
            }
        }
        Fields::Buffer { soffset, offset, offen, idxen, format, lds, tfe, cache, .. } => {
            if let Some(f) = format {
                s += &format!(" fmt={f}");
            }
            for (on, what) in [
                (*idxen, " idx"),
                (*offen, " off"),
                (*offset != 0, " +k"),
                (!matches!(soffset, Operand::Scalar { code: 125, .. } | Operand::Const(128)), " +s"),
                (*lds, " lds"),
                (*tfe, " tfe"),
                (cache.glc, " glc"),
            ] {
                if on {
                    s += what;
                }
            }
        }
        Fields::Image { dmask, dim, a16, d16, .. } => {
            s += &format!(" dim={dim} dmask={dmask:x}");
            if *a16 {
                s += " a16";
            }
            if *d16 {
                s += " d16";
            }
        }
        Fields::Sop { dst, src, imm } => {
            s.push('(');
            s.extend(dst.iter().map(class));
            s.push('<');
            s.extend(src.iter().map(class));
            if imm.is_some() {
                s.push('i');
            }
            s.push(')');
        }
        Fields::Ds { gds, .. } => {
            if *gds {
                s += " gds";
            }
        }
        _ => {}
    }
    s
}

/// The line for `p`, the program of `native` the log names `name`.
fn line(name: &str, native: &NativeProgram, p: &ir::Program) -> String {
    let mut kinds = BTreeMap::<String, usize>::new();
    let mut count = 0;
    for inst in p.blocks.iter().flat_map(|b| &b.insts) {
        *kinds.entry(kind(inst)).or_default() += 1;
        count += 1;
    }
    let io = match &native.io {
        StageIo::Cs(cs) => format!(" group={:?} lds={}", cs.threads, cs.lds_bytes()),
        StageIo::Ms(ms) => format!(" group={} lds={}", ms.threads, ms.lds_bytes),
        _ => String::new(),
    };
    let used: Vec<String> = native
        .used
        .iter()
        .map(|u| format!("{:?}/{:?}+{} stride {} dwords {}", u.kind, u.root, u.table_offset_dwords, u.stride, u.dwords))
        .collect();
    let kinds: Vec<String> = kinds.into_iter().map(|(k, n)| if n == 1 { k } else { format!("{k} x{n}") }).collect();
    format!(
        "PROGRAM-KINDS {name} {:?} console wave{}{io} blocks={} insts={count} used=[{}] :: {}",
        native.stage,
        if native.wave64 { 64 } else { 32 },
        p.blocks.len(),
        used.join("; "),
        kinds.join("; "),
    )
}

/// Writes the line for the program being translated, if it is asked for.
pub(super) fn report(native: &NativeProgram, p: &ir::Program) {
    if !asked() {
        return;
    }
    let name = compute_wave::name();
    if wanted(&name, native.stage) {
        eprintln!("{}", line(&name, native, p));
    }
}

/// Writes what the translation of the program made of 16-bit views, if it is
/// asked for: the table places whose views it moves halves through.
pub(super) fn report_result(shader: &super::HostShader) {
    if !asked() {
        return;
    }
    let name = compute_wave::name();
    if wanted(&name, shader.stage) {
        eprintln!("PROGRAM-RESULT {name} half_views={:?}", shader.half_views);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The program of `words` (GFX10 code), wave64, eight VGPRs.
    fn program(words: &[u32]) -> ir::Program {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        ir::build(&code, true, 8).expect("the code decodes")
    }

    #[test]
    fn a_kind_is_the_mnemonic_with_operand_classes_and_modifiers() {
        // v_mov_b32 v2, 11; ds_bpermute_b32 v1, v0, v2; s_endpgm
        let p = program(&[0x7E04_028B, 0xDACC_0000, 0x0100_0200, 0xBF81_0000]);
        let kinds: Vec<String> = p.blocks.iter().flat_map(|b| &b.insts).map(kind).collect();
        assert_eq!(kinds, ["v_mov_b32(v<k)", "ds_bpermute_b32", "s_endpgm"]);
    }

    #[test]
    fn programs_are_listed_by_name_or_by_stage() {
        assert!(listed("1", "aa", HwStage::Ps));
        assert!(listed("all", "aa", HwStage::Cs));
        assert!(listed("0123456789abcdef, AA", "aa", HwStage::Vs));
        assert!(!listed("0123456789abcdef", "aa", HwStage::Vs));
        assert!(listed("cs;bb", "aa", HwStage::Cs));
        assert!(!listed("cs;bb", "aa", HwStage::Ps));
        assert!(listed("ms", "aa", HwStage::Ms));
    }

    #[test]
    fn a_line_counts_kinds_and_carries_no_operands() {
        // v_mov_b32 v2, 11; v_mov_b32 v3, 12; s_endpgm
        let p = program(&[0x7E04_028B, 0x7E06_028C, 0xBF81_0000]);
        let native = NativeProgram {
            stage: crate::native::HwStage::Cs,
            wave64: true,
            vgprs: 8,
            layout: Vec::new(),
            used: Vec::new(),
            io: StageIo::None,
            code: Vec::new(),
        };
        let line = line("0123456789abcdef", &native, &p);
        assert_eq!(
            line,
            "PROGRAM-KINDS 0123456789abcdef Cs console wave64 blocks=1 insts=3 used=[] :: s_endpgm; v_mov_b32(v<k) x2"
        );
    }
}
