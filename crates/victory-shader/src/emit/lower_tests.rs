//! The tests of `lower` (moved out of `lower.rs` whole, one indent less and
//! otherwise unchanged, so that the file stays under the asset policy's 512 KiB
//! per file: docs/asset-policy.md). A child module of `lower`: `super` is `lower`.

use super::{Lowered, Numeric, Parts, buffer_format, half_shift, lower};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

#[test]
fn packed_pixel_inputs_find_their_second_row() {
    use super::super::container::SigElement;
    use super::half_input_partners;
    use crate::native::{PsInput, Usage};
    let entry = |row, row2, pack2| PsInput {
        usage: Usage::Generic,
        row,
        mask: 0xF,
        attr: row,
        flat: false,
        fp16: true,
        pack2,
        row2,
    };
    let element = |row| SigElement {
        name: "TEXCOORD".into(),
        index: row,
        kind: 0,
        comp: 9,
        interp: 2,
        row: Some(row),
        col: 0,
        cols: 4,
        usage: 0,
        half: true,
    };
    // Rows 0 and 3 pack and leave their second row out; 5 names its
    // own (6). Unnamed 16-bit rows: 1, 4, 6, 7.
    let io = StageIo::Ps {
        inputs: vec![entry(0, 0, true), entry(3, 3, true), entry(5, 6, true)],
        input_addr: 0,
        db_shader_control: 0,
        patches: vec![],
    };
    let sig: Vec<_> = [0, 1, 3, 4, 5, 6, 7].map(element).into();
    let p = half_input_partners(&io, &sig);
    assert_eq!(p.get(&0), Some(&1));
    assert_eq!(p.get(&3), Some(&4));
    assert_eq!(p.get(&5), None);
}

/// The packed pedestrian layouts of the title: an element
/// behind another one in its register follows in the first parameter
/// with a dword left, and is not a half of its register's row.
#[test]
fn packed_elements_behind_another_take_the_first_free_dword() {
    use super::super::container::SigElement;
    use super::packed_half;
    let element = |index, row, col, cols| SigElement {
        name: "TEXCOORD".into(),
        index,
        kind: 0,
        comp: 9,
        interp: 2,
        row: Some(row),
        col,
        cols,
        usage: 0,
        half: true,
    };
    // MS_VS_PedTransformPreSkinD_Packed 005fe88ec1bdc0c7 and its pixel
    // shader 93667197d3d05e1c: TEXCOORD6 is one value at register 4
    // column 3; parameter 3 (rows 6 and 7) carries it in dword 3.
    let sig = vec![
        element(1, 2, 0, 4),
        element(2, 3, 0, 4),
        element(4, 4, 0, 3),
        element(6, 4, 3, 1),
        element(3, 5, 0, 4),
        element(5, 6, 0, 4),
        element(7, 7, 0, 2),
        element(9, 9, 0, 3),
        element(11, 10, 0, 3),
        element(15, 12, 0, 4),
    ];
    let packs = [
        (2, Some(3)),
        (4, Some(5)),
        (6, Some(7)),
        (9, Some(10)),
        (12, None),
    ];
    let at = |a, h| packed_half(&sig, &packs, a, h);
    // Parameter 2: TEXCOORD4's three values, a zero half, TEXCOORD3.
    assert_eq!(at(4, 2), Some((4, 2)));
    assert_eq!(at(4, 3), None);
    assert_eq!(at(4, 4), Some((5, 0)));
    assert_eq!(at(4, 7), Some((5, 3)));
    // Parameter 3: TEXCOORD5, TEXCOORD7, then TEXCOORD6 alone.
    assert_eq!(at(6, 3), Some((6, 3)));
    assert_eq!(at(6, 5), Some((7, 1)));
    assert_eq!(at(6, 6), Some((4, 3)));
    assert_eq!(at(6, 7), None);
    // Parameters without such an element are as before.
    assert_eq!(at(9, 3), None);
    assert_eq!(at(9, 4), Some((10, 0)));
    assert_eq!(at(12, 3), Some((12, 3)));
    assert_eq!(at(12, 4), None);
    // The common layout (29805d23a495cdf8): TEXCOORD7 is two values at
    // register 1 columns 2-3; parameter 0 is TEXCOORD0, TEXCOORD1,
    // TEXCOORD7.
    let sig = vec![
        element(0, 1, 0, 2),
        element(7, 1, 2, 2),
        element(1, 2, 0, 4),
        element(2, 3, 0, 4),
        element(4, 4, 0, 3),
    ];
    let packs = [(3, Some(4)), (1, Some(2))];
    let at = |a, h| packed_half(&sig, &packs, a, h);
    assert_eq!(at(1, 1), Some((1, 1)));
    assert_eq!(at(1, 2), Some((2, 0)));
    assert_eq!(at(1, 5), Some((2, 3)));
    assert_eq!(at(1, 6), Some((1, 2)));
    assert_eq!(at(1, 7), Some((1, 3)));
    assert_eq!(at(3, 6), Some((4, 2)));
    assert_eq!(at(3, 7), None);
    // A row no element holds gives nothing, as before.
    assert_eq!(packed_half(&sig, &[(8, None)], 8, 0), None);
}

/// Whether a synthetic compute program translates with these input registers.
fn lowers(words: &[u32], rsrc2: u32) -> Result<(), crate::ShaderError> {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 256,
        layout: vec![],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2,
            threads: [1, 1, 1],
        }),
        code,
    };
    let p = ir::build(&native.code, false, 256).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    lower(&Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: vec![],
        outputs: vec![],
        prims: vec![],
        mesh_stride: 0,
    })
    .map(|_| ())
}

/// Lowers a compute program with the root parameters `layout` (in user
/// SGPRs from s0 on) and the used resources `used`.
fn lower_with(
    words: &[u32],
    layout: Vec<RootParam>,
    used: Vec<UsedResource>,
) -> Result<Lowered, crate::ShaderError> {
    lower_wave(words, layout, used, false)
}

/// [`lower_with`] in wave64 if `wave64`.
fn lower_wave(
    words: &[u32],
    layout: Vec<RootParam>,
    used: Vec<UsedResource>,
    wave64: bool,
) -> Result<Lowered, crate::ShaderError> {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let sgprs: u32 = layout.iter().map(|p| p.sgprs).sum();
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 256,
        layout,
        used,
        io: StageIo::Cs(ComputeIo {
            // The layout's user SGPRs (at least two), 512 bytes of LDS.
            rsrc2: 1 << 15 | sgprs.max(2) << 1,
            threads: [1, 1, 1],
        }),
        code,
    };
    let p = ir::build(&native.code, wave64, 256).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, &native).unwrap();
    lower(&Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: vec![],
        outputs: vec![],
        prims: vec![],
        mesh_stride: 0,
    })
}

/// Root parameter `slot` in user SGPRs from `user_data` on.
fn root(root: RootType, user_data: u8, slot: u8) -> RootParam {
    RootParam {
        root,
        table_offset_dwords: 0,
        sgprs: if root == RootType::Descriptor { 2 } else { 1 },
        base_register: 0,
        user_data,
        slot,
    }
}

#[test]
fn buffer_cache_invalidations_lower_as_no_ops() {
    // MUBUF opcode 0x071/0x072 with no data operand: the title asks the
    // host to invalidate its cached view of a buffer.
    assert!(lowers(&[0xE1C4_0000, 0, S_ENDPGM], 0).is_ok(), "buffer_gl0_inv");
    assert!(lowers(&[0xE1C8_0000, 0, S_ENDPGM], 0).is_ok(), "buffer_gl1_inv");
}

#[test]
fn a_scalar_carry_chain_reaches_the_select() {
    // The high dword of a 64-bit add or subtract, then a select on the
    // carry (borrow) the high half leaves in SCC, and a branch on it
    // (only code with an effect needs its operands).
    let add = [
        0xBE80_03C1, // s_mov_b32 s0, -1
        0xBE81_0380, // s_mov_b32 s1, 0
        0x8000_8100, // s_add_u32 s0, s0, 1
        0x8201_8001, // s_addc_u32 s1, s1, 0
        0x8502_0001, // s_cselect_b32 s2, s1, s0
        0xBF07_8002, // s_cmp_lg_u32 s2, 0
        0xBF85_0001, // s_cbranch_scc1 -> s_endpgm
        0xBE83_0380, // s_mov_b32 s3, 0
        S_ENDPGM,
    ];
    let sub = [
        0xBE80_0380, // s_mov_b32 s0, 0
        0xBE81_0380, // s_mov_b32 s1, 0
        0x8080_8100, // s_sub_u32 s0, s0, 1
        0x8281_8001, // s_subb_u32 s1, s1, 0
        0x8502_0001, // s_cselect_b32 s2, s1, s0
        0xBF07_8002, // s_cmp_lg_u32 s2, 0
        0xBF85_0001, // s_cbranch_scc1 -> s_endpgm
        0xBE83_0380, // s_mov_b32 s3, 0
        S_ENDPGM,
    ];

    for words in [add, sub] {
        lowers(&words, 0).unwrap();
    }
}

fn perm_features(selector: u32) -> u64 {
    // Synthetic compute shader:
    // v0 is the thread ID and can supply a dynamic selector.
    // Store the result in LDS to keep the permutation live.
    let words = [
        0x7E02_02FFu32,
        0x817F_34AB,
        0x7E04_02FF,
        0xFE01_56CD,
        0xD744_0003,
        257 | (258 << 9) | (selector << 18),
        0x3408_0082,
        0xD834_0000,
        0x0000_0304,
        S_ENDPGM,
    ];

    let code: Vec<u8> = words.iter()
        .flat_map(|w| w.to_le_bytes())
        .collect();

    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 256,
        layout: vec![],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 15,
            threads: [64, 1, 1],
        }),
        code,
    };

    let p = ir::build(&native.code, false, 256).unwrap();

    assert!(p.blocks.iter()
        .flat_map(|b| &b.insts)
        .any(|i| i.name() == "v_perm_b32"));

    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();

    let io = io::raise(&p, &native).unwrap();

    let emitted = lower(&Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: vec![],
        outputs: vec![],
        prims: vec![],
        mesh_stride: 0,
    }).unwrap();

    let container =
        crate::Container::parse(&emitted.dxil).unwrap();

    let features =
        container.part(crate::FourCC::SFI0).unwrap();

    u64::from_le_bytes(features.data.try_into().unwrap())
}

#[test]
fn perm_constant_selector_needs_no_int64() {
    assert_eq!(perm_features(128) & 0x8000, 0);
}

#[test]
fn perm_dynamic_selector_needs_no_int64() {
    assert_eq!(
        perm_features(256) & 0x8000,
        0,
        "dynamic v_perm_b32 must not require INT64"
    );
}

#[test]
fn a_descriptor_selected_by_scc_picks_its_heap_index() {
    // Two buffer descriptors loaded from the table at s0, one picked by
    // a root constant (s1) with s_cselect_b64 per half, and loaded
    // through by index and stored to LDS so the load is needed.
    let words = [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], null
        0xF408_0302,
        0xFA00_0010, // s_load_dwordx4 s[12:15], s[4:5], 0x10
        0xBF06_8001, // s_cmp_eq_u32 s1, 0
        0x8590_0C08, // s_cselect_b64 s[16:17], s[8:9], s[12:13]
        0x8592_0E0A, // s_cselect_b64 s[18:19], s[10:11], s[14:15]
        0xE030_2000,
        0x8004_0100, // buffer_load_dword v1, v0, s[16:19], 0 idxen
        0xD834_0000,
        0x0000_0100, // ds_write_b32 v0, v1
        S_ENDPGM,
    ];
    let layout = || vec![root(RootType::Table, 0, 0), root(RootType::Constants, 1, 1)];
    // Both are structured buffers of dwords in the table's range.
    let used = || {
        vec![UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 4,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }]
    };
    lower_with(&words, layout(), used()).unwrap();
}

/// Tables of 2D textures at s0 and samplers at s1.
fn texture_tables() -> (Vec<RootParam>, Vec<UsedResource>) {
    let range = |kind, user_data, slot| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords: if kind == ResourceType::Sampler { 4 } else { 8 },
        user_data,
        slot,
    };
    (
        vec![root(RootType::Table, 0, 0), root(RootType::Table, 1, 1)],
        vec![
            range(ResourceType::Srv, 0, 0),
            range(ResourceType::Sampler, 1, 1),
        ],
    )
}

#[test]
fn waterfall_loops_over_per_lane_descriptors_sample_with_each_lanes_own() {
    use crate::resource::tests::{
        WATERFALL_FETCHED_WAVE32, WATERFALL_FETCHED_WAVE64, WATERFALL_PICKED_WAVE32,
    };
    for (words, wave64) in [
        (WATERFALL_FETCHED_WAVE32, false),
        (WATERFALL_FETCHED_WAVE64, true),
        (WATERFALL_PICKED_WAVE32, false),
    ] {
        let (layout, used) = texture_tables();
        let l = lower_wave(words, layout, used, wave64).unwrap();
        // The loop's reads of the first lane became each lane's own
        // dwords: no wave operation is left.
        assert_eq!(l.nonuniform_handles, 1);
        assert!(!l.wave_ops);
    }
}

/// [`texture_tables`] with a raw buffer UAV at dword 16 of the first.
pub(crate) fn texture_and_uav_tables() -> (Vec<RootParam>, Vec<UsedResource>) {
    let (layout, mut used) = texture_tables();
    used.push(UsedResource {
        kind: ResourceType::Uav,
        root: RootType::Table,
        table_offset_dwords: 16,
        lower_bound: 0,
        stride: 0,
        dwords: 4,
        user_data: 0,
        slot: 0,
    });
    (layout, used)
}

#[test]
fn waterfall_loops_over_descriptors_joined_per_lane_sample_with_each_lanes_own() {
    use crate::resource::tests::{WATERFALL_JOINED_MIXED_WAVE32, WATERFALL_JOINED_WAVE32};
    for words in [WATERFALL_JOINED_WAVE32, WATERFALL_JOINED_MIXED_WAVE32] {
        let (layout, used) = texture_and_uav_tables();
        let l = lower_wave(words, layout, used, false).unwrap();
        // One image handle, from the join's per-lane heap index.
        assert_eq!(l.nonuniform_handles, 1);
        assert!(!l.wave_ops);
    }
}

#[test]
fn scalar_bit_tests_lower() {
    // Each tests one bit of a constant into SCC and branches on it (only
    // code with an effect needs its operands).
    let tail = [
        0xBF85_0001, // s_cbranch_scc1 -> s_endpgm
        0xBE84_0380, // s_mov_b32 s4, 0
        S_ENDPGM,
    ];
    let bit_tests: [&[u32]; 4] = [
        &[0xBE81_0387, 0xBF0C_8301], // s_mov_b32 s1, 7; s_bitcmp0_b32 s1, 3
        &[0xBE81_0387, 0xBF0D_8301], // s_mov_b32 s1, 7; s_bitcmp1_b32 s1, 3
        &[
            0xBE80_0387, // s_mov_b32 s0, 7
            0xBE81_0380, // s_mov_b32 s1, 0
            0xBF0E_8300, // s_bitcmp0_b64 s[0:1], 3
        ],
        &[
            0xBE80_0387, // s_mov_b32 s0, 7
            0xBE81_0380, // s_mov_b32 s1, 0
            0xBF0F_8300, // s_bitcmp1_b64 s[0:1], 3
        ],
    ];
    for head in bit_tests {
        let words: Vec<u32> = head.iter().chain(&tail).copied().collect();
        lowers(&words, 0).unwrap();
    }
}

#[test]
#[ignore = "ours marks joins read through joins only with VICTORY_STRICT_JOINS (40 programs stop otherwise)"]
fn a_missing_value_stays_missing_through_two_live_joins() {
    let mut words = [
        0xF490_0180,
        0,           // s_memtime s[6:7]
        0xBF06_8000, // s_cmp_eq_u32 s0, 0 (s0 = group id X)
        0xBF85_0001, // skip the first overwrite
        0xBE86_0380, // s_mov_b32 s6, 0
        0xBF06_8101, // s_cmp_eq_u32 s1, 1 (s1 = group id Y)
        0xBF85_0001, // skip the second overwrite
        0xBE86_0381, // s_mov_b32 s6, 1
        0xBF07_8006, // s_cmp_lg_u32 s6, 0
        0xBF85_0001, // use the result to branch
        0xBE83_0380,
        S_ENDPGM,
    ];
    let group_ids_xy = 3 << 7;
    // Compare to a direct read so future timestamp support works in both cases.
    let refusal = |code: &[u32]| match lowers(code, group_ids_xy) {
        Ok(()) => None,
        Err(crate::ShaderError::Unsupported { what, .. }) => Some(what),
        Err(error) => panic!("{error}"),
    };
    let direct = [&words[..2], &words[8..]].concat();
    assert_eq!(refusal(&words), refusal(&direct));
    words[8] = 0xBF07_8000; // read the group id instead; the timestamp is dead
    lowers(&words, group_ids_xy).expect("dead timestamp");
}

#[test]
fn buffer_formats_by_number() {
    // The numbers the disassembler names 8_8_8_8_UNORM, 16_16_SNORM,
    // 2_10_10_10_UNORM, 16_FLOAT, 32_32_32_32_FLOAT.
    assert_eq!(buffer_format(56), Some((&[8, 8, 8, 8][..], Numeric::Unorm)));
    assert_eq!(buffer_format(24), Some((&[16, 16][..], Numeric::Snorm)));
    assert_eq!(
        buffer_format(50),
        Some((&[10, 10, 10, 2][..], Numeric::Unorm))
    );
    assert_eq!(buffer_format(13), Some((&[16][..], Numeric::Float)));
    assert_eq!(
        buffer_format(77),
        Some((&[32, 32, 32, 32][..], Numeric::Float))
    );
    // Invalid, past the table.
    assert_eq!(buffer_format(0), None);
    assert_eq!(buffer_format(78), None);
}

#[test]
fn packed_buffer_formats_by_number() {
    // LLVM's `UfmtGFX10`: 10_11_11 is 30-36, 11_11_10 37-43,
    // 10_10_10_2 44-49. Channels from the lowest bits: R11G11B10_FLOAT
    // is 10_11_11 (Mesa), 10_10_10_2 has the 2-bit channel first.
    assert_eq!(buffer_format(30), Some((&[11, 11, 10][..], Numeric::Unorm)));
    assert_eq!(buffer_format(36), Some((&[11, 11, 10][..], Numeric::Float)));
    assert_eq!(buffer_format(37), Some((&[10, 11, 11][..], Numeric::Unorm)));
    assert_eq!(buffer_format(43), Some((&[10, 11, 11][..], Numeric::Float)));
    assert_eq!(
        buffer_format(44),
        Some((&[2, 10, 10, 10][..], Numeric::Unorm))
    );
    assert_eq!(
        buffer_format(45),
        Some((&[2, 10, 10, 10][..], Numeric::Snorm))
    );
    assert_eq!(
        buffer_format(46),
        Some((&[2, 10, 10, 10][..], Numeric::Uscaled))
    );
    assert_eq!(
        buffer_format(47),
        Some((&[2, 10, 10, 10][..], Numeric::Sscaled))
    );
    assert_eq!(
        buffer_format(48),
        Some((&[2, 10, 10, 10][..], Numeric::Uint))
    );
    assert_eq!(
        buffer_format(49),
        Some((&[2, 10, 10, 10][..], Numeric::Sint))
    );
}

/// A half's value (sign, 5-bit exponent biased 15, 10-bit mantissa), as
/// `legacyF16ToF32` widens it.
fn half(h: u32) -> f32 {
    let (s, e, m) = ((h >> 15) & 1, (h >> 10) & 31, h & 1023);
    let v = match e {
        0 => m as f32 * 2f32.powi(-24),
        31 if m == 0 => f32::INFINITY,
        31 => f32::NAN,
        _ => (1024 + m) as f32 * 2f32.powi(e as i32 - 25),
    };
    if s == 1 { -v } else { v }
}

#[test]
fn small_floats_widen_as_halves() {
    // Unsigned 11- and 10-bit floats: a 5-bit exponent biased 15 over a
    // 6- or 5-bit mantissa, no sign; values from the D3D/GL spec.
    let f11 = |x: u32| half(x << half_shift(11));
    let f10 = |x: u32| half(x << half_shift(10));
    assert_eq!(f11(0), 0.0);
    assert_eq!(f11(0x001), 2f32.powi(-20)); // smallest denormal
    assert_eq!(f11(0x03F), 63.0 * 2f32.powi(-20)); // largest denormal
    assert_eq!(f11(0x040), 2f32.powi(-14)); // smallest normal
    assert_eq!(f11(0x3C0), 1.0);
    assert_eq!(f11(0x3E0), 1.5);
    assert_eq!(f11(0x7BF), 65024.0); // largest
    assert_eq!(f11(0x7C0), f32::INFINITY);
    assert!(f11(0x7C1).is_nan());
    assert_eq!(f10(0x001), 2f32.powi(-19));
    assert_eq!(f10(0x020), 2f32.powi(-14));
    assert_eq!(f10(0x1E0), 1.0);
    assert_eq!(f10(0x3DF), 64512.0);
    assert_eq!(f10(0x3E0), f32::INFINITY);
    assert!(f10(0x3FF).is_nan());
    // A 16-bit float is a half already, sign included.
    assert_eq!(half(0xBC00 << half_shift(16)), -1.0);
}

/// A buffer read through a descriptor the program takes from the view
/// heap at an index it computes looks its slot up in the stride table
/// and reads the host's null buffer view where that says no buffer
/// view is there; one taken from a table is read as before, and so is
/// everything while no null slot is set.
#[test]
fn heap_buffers_read_at_computed_indices_are_guarded() {
    // The descriptor of slot 3 of the view heap (s0, its base address
    // as the program sees it) into s[8:11], a scalar buffer load
    // through it, and the result into LDS.
    let words = [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0060, // s_load_dwordx4 s[8:11], s[4:5], 0x60
        0xF420_0304,
        0xFA00_0000, // s_buffer_load_dword s12, s[8:11], 0x0
        0x7E02_020C, // v_mov_b32 v1, s12
        0xD834_0000,
        0x0000_0100, // ds_write_b32 v0, v1
        S_ENDPGM,
    ];
    let heap = || vec![root(RootType::ResourceHeap, 0, 0)];
    let table = || vec![root(RootType::Table, 0, 0)];
    let table_used = || {
        vec![UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }]
    };
    let nulls = |n: Option<(u32, u32)>| super::GUARD_NULLS.with(|c| c.set(n));
    nulls(Some((999_999, 999_998)));
    let guarded = lower_with(&words, heap(), vec![]).expect("a heap read lowers");
    let tabled = lower_with(&words, table(), table_used()).expect("a table read lowers");
    nulls(None);
    let unset = (super::super::view_heap_limit() == 0)
        .then(|| lower_with(&words, heap(), vec![]).expect("a heap read lowers"));
    assert_eq!(guarded.buffer_guards, 1, "the heap read is guarded");
    // Where the validator is found, the guarded program passes it.
    use super::super::validator::Validator;
    if let Ok(mut v) = Validator::load(Validator::default_path().as_deref()) {
        v.validate(&mut guarded.dxil.clone()).expect("the guarded heap read validates");
        eprintln!("the guarded heap read validates");
    }
    assert_eq!(tabled.buffer_guards, 0, "a table read is not");
    if let Some(unset) = unset {
        assert_eq!(unset.buffer_guards, 0, "nor anything without a null slot");
    }
}

/// A scalar load through a descriptor the program takes from the view heap
/// at an index it computes reads the slot as a constant buffer only where
/// the stride table marks a constant buffer view
/// (`SLOT_STRIDE_CONSTANT`), with the marks on; with them off, wherever the
/// table says no buffer view (a texture's slot too). A table's descriptor
/// is read by its used-resource entry either way. What the loads give is
/// `heap_slot_tests` (on the host GPU).
#[test]
fn heap_constant_reads_wait_for_the_mark() {
    // The descriptor of slot 3 of the view heap into s[8:11], a scalar
    // buffer load through it, and the result into LDS.
    let words = [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0060, // s_load_dwordx4 s[8:11], s[4:5], 0x60
        0xF420_0304,
        0xFA00_0000, // s_buffer_load_dword s12, s[8:11], 0x0
        0x7E02_020C, // v_mov_b32 v1, s12
        0xD834_0000,
        0x0000_0100, // ds_write_b32 v0, v1
        S_ENDPGM,
    ];
    let heap = || vec![root(RootType::ResourceHeap, 0, 0)];
    let table = || vec![root(RootType::Table, 0, 0)];
    let table_used = || {
        vec![UsedResource {
            kind: ResourceType::Cbv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }]
    };
    let marks = |on: Option<bool>| super::super::CONSTANT_MARKS_TEST.with(|c| c.set(on));
    marks(Some(true));
    let marked = lower_with(&words, heap(), vec![]).expect("a heap read lowers");
    let tabled = lower_with(&words, table(), table_used()).expect("a table read lowers");
    marks(Some(false));
    let unmarked = lower_with(&words, heap(), vec![]).expect("a heap read lowers");
    marks(None);
    assert_eq!(marked.marked_constant_reads, 1, "read as a constant buffer where marked");
    assert_eq!(unmarked.marked_constant_reads, 0, "the marks off: as before");
    assert_eq!(tabled.marked_constant_reads, 0, "a table's constant buffer as before");
    assert_ne!(marked.dxil, unmarked.dxil);
    use super::super::validator::Validator;
    if let Ok(mut v) = Validator::load(Validator::default_path().as_deref()) {
        v.validate(&mut marked.dxil.clone()).expect("the marked heap read validates");
        eprintln!("the marked heap read validates");
    }
}

/// Lowers a wave32 compute program of 32 threads that takes a descriptor
/// from the view heap itself (s0) and a raw UAV from entry 0 of a table at
/// s1, with the constant-buffer marks on or off, for the host GPU.
#[cfg(windows)]
fn heap_slot_shader(words: &[u32], marks: bool) -> super::super::HostShader {
    let param = |root, user_data| RootParam {
        root,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data,
        slot: user_data,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 32,
        layout: vec![param(RootType::ResourceHeap, 0), param(RootType::Table, 1)],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 1,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 2 << 1,
            threads: [32, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, false, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io::raise");
    super::super::CONSTANT_MARKS_TEST.with(|c| c.set(Some(marks)));
    // Heap slot 2 stands in for the host's null buffer views.
    super::GUARD_NULLS.with(|c| c.set(Some((2, 2))));
    let lowered = lower(&Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: vec![],
        outputs: vec![],
        prims: vec![],
        mesh_stride: 0,
    });
    super::GUARD_NULLS.with(|c| c.set(None));
    super::super::CONSTANT_MARKS_TEST.with(|c| c.set(None));
    let lowered = lowered.expect("lower");
    let shader = super::super::HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: Vec::new(),
        half_views: lowered.half_views,
    };
    use super::super::validator::Validator;
    if let Ok(mut v) = Validator::load(Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone()).expect("DXIL validation failed");
    }
    shader
}

/// Runs `shader` on the hardware GPU with `probe` at view heap slot 1 (the
/// UAV at 0, zeros at 2) and `entry` as slot 1's dword of the stride table;
/// the UAV's 32 dwords, `None` without a device.
#[cfg(windows)]
fn heap_slot_run(
    shader: &super::super::HostShader,
    probe: super::super::host::View,
    entry: u32,
) -> Option<Vec<u32>> {
    use super::super::host::{Dispatch, Host, Root, Stride, View};
    let _one = super::super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; skipped");
        return None;
    };
    let (root, pso) = host.compute_pipeline(shader).expect("compute_pipeline");
    let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let mut roots = vec![(0, Root::Constants(ud))];
    // Root views follow the user data, by slot and then kind.
    let mut views: Vec<(u8, u8)> = shader.root_cbvs.iter().map(|&r| (r, 0)).collect();
    views.extend(shader.root_srvs.iter().map(|&r| (r, 1)));
    views.extend(shader.root_uavs.iter().map(|&r| (r, 2)));
    views.sort_unstable();
    views.dedup();
    for (n, (slot, kind)) in views.into_iter().enumerate() {
        assert_eq!((slot, kind), (super::super::SLOT_STRIDES, 1), "root view");
        let mut table = vec![0u32; 64];
        table[1] = entry;
        roots.push((1 + n as u32, Root::View { srv: true, data: table }));
    }
    let zeros = [0u32; 64];
    let heap = [
        View::RawUav(32),
        probe,
        View::Buffer {
            data: &zeros,
            stride: Stride::Raw,
        },
    ];
    let words = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots,
                views: &heap,
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .expect("dispatch");
    Some(words[0].clone())
}

/// What a scalar load reads through heap slot 1, picked by the index the
/// program computes, for each kind of view there and each stride table
/// dword: a constant buffer only where the table marks it
/// (`SLOT_STRIDE_CONSTANT`), zeros where the table says 0 (a texture's slot
/// read as a constant buffer was its image descriptor read as a buffer's:
/// GPU page faults at address 0 plus a small offset under vkd3d-proton on
/// RADV), the buffer where it holds a stride. With the marks off, as before:
/// a constant buffer wherever the table says 0.
#[cfg(windows)]
#[test]
fn heap_slots_read_as_constant_buffers_only_where_marked() {
    use super::super::host::{Stride, View};
    use super::super::shader::SLOT_STRIDE_CONSTANT as MARK;
    let words = [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xBE86_0301, // s_mov_b32 s6, s1
        0xBE87_0380, // s_mov_b32 s7, 0
        0xF408_0202,
        0xFA00_0020, // s_load_dwordx4 s[8:11], s[4:5], 0x20 (heap slot 1)
        0xF408_0A03,
        0xFA00_0000, // s_load_dwordx4 s[40:43], s[6:7], 0x0 (the UAV)
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xF420_0304,
        0xFA00_0004, // s_buffer_load_dword s12, s[8:11], 0x4
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0x7E02_020C, // v_mov_b32 v1, s12
        0x3420_0082, // v_lshlrev_b32 v16, 2, v0
        0xE070_1000,
        0x800A_0110, // buffer_store_dword v1, v16, s[40:43], 0 offen
        S_ENDPGM,
    ];
    let marked = heap_slot_shader(&words, true);
    let unmarked = heap_slot_shader(&words, false);
    let constants: Vec<u32> = (0..64).map(|i| 0x1000 + i).collect();
    let texels = [[1.0f32, 2.0, 3.0, 4.0]];
    let buffer: Vec<u32> = (0..64).map(|i| 0x2000 + i).collect();
    let raw = || View::Buffer {
        data: &buffer,
        stride: Stride::Raw,
    };
    let cases = [
        ("marked constant buffer", &marked, View::Cbv(&constants), MARK, 0x1001),
        ("unmarked constant buffer", &marked, View::Cbv(&constants), 0, 0),
        ("texture", &marked, View::Texture(1, 1, &texels), 0, 0),
        ("buffer", &marked, raw(), 1, 0x2001),
        ("constant buffer, marks off", &unmarked, View::Cbv(&constants), 0, 0x1001),
        ("buffer, marks off", &unmarked, raw(), 1, 0x2001),
    ];
    for (name, shader, probe, entry, want) in cases {
        let Some(got) = heap_slot_run(shader, probe, entry) else {
            return;
        };
        assert_eq!(got, vec![want; 32], "{name}");
    }
}

/// A buffer read through heap slot 1 reads the null buffer view (slot 2,
/// zeros here) where the stride table marks a constant buffer view, as
/// where it says 0: the mark is no stride.
#[cfg(windows)]
#[test]
fn a_marked_slot_is_no_buffer_to_the_guard() {
    use super::super::host::{Stride, View};
    use super::super::shader::SLOT_STRIDE_CONSTANT as MARK;
    let words = [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xBE86_0301, // s_mov_b32 s6, s1
        0xBE87_0380, // s_mov_b32 s7, 0
        0xF408_0202,
        0xFA00_0020, // s_load_dwordx4 s[8:11], s[4:5], 0x20 (heap slot 1)
        0xF408_0A03,
        0xFA00_0000, // s_load_dwordx4 s[40:43], s[6:7], 0x0 (the UAV)
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0x3420_0082, // v_lshlrev_b32 v16, 2, v0
        0xE030_1000,
        0x8002_0C10, // buffer_load_dword v12, v16, s[8:11], 0 offen
        0xBF8C_3F70, // s_waitcnt vmcnt(0)
        0xE070_1000,
        0x800A_0C10, // buffer_store_dword v12, v16, s[40:43], 0 offen
        S_ENDPGM,
    ];
    let shader = heap_slot_shader(&words, true);
    let buffer: Vec<u32> = (0..64).map(|i| 0x2000 + i).collect();
    let raw = || View::Buffer {
        data: &buffer,
        stride: Stride::Raw,
    };
    let lanes: Vec<u32> = (0..32).map(|i| 0x2000 + i).collect();
    for (entry, want) in [(1, lanes), (MARK, vec![0; 32]), (0, vec![0; 32])] {
        let Some(got) = heap_slot_run(&shader, raw(), entry) else {
            return;
        };
        assert_eq!(got, want, "table dword {entry:#x}");
    }
}

#[test]
fn all_lane_mesh_gs_alloc_req_preserves_lds_barrier() {
    let lower_mesh = |words: &[u32]| -> Lowered {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let native = NativeProgram {
            stage: HwStage::Ms,
            wave64: false,
            vgprs: 256,
            layout: vec![],
            used: vec![],
            io: StageIo::Ms(crate::native::MeshIo {
                outputs: vec![],
                clip_cntl: 0,
                threads: 32,
                max_vertices: 3,
                max_primitives: 1,
                topology: 2,
                lds_bytes: 256,
            }),
            code,
        };
        let p = ir::build(&native.code, false, 256).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        lower(&Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: vec![],
            outputs: vec![],
            prims: vec![],
            mesh_stride: 0,
        })
        .unwrap()
    };

    // All-lane GS_ALLOC_REQ in entry block 0: no block is added to no_barrier.
    let all_lane = lower_mesh(&[
        0xBEFC_0380, // s_mov_b32 m0, 0
        0xBF90_0009, // s_sendmsg GS_ALLOC_REQ (id = 9)
        0xBF8A_0000, // s_barrier
        S_ENDPGM,
    ]);
    assert_eq!(
        all_lane.no_barrier_blocks, 0,
        "all-lane GS_ALLOC_REQ must not strip mesh LDS barriers"
    );

    // Divergent (single-lane) GS_ALLOC_REQ inside a per-lane branch: its
    // block (and anything it dominates) MUST still be in no_barrier.
    let divergent = lower_mesh(&[
        0xD4C2_0002,
        0x0002_0080, // v_cmp_eq_u32 s2, 0, v0
        0xBE83_3C02, // s_and_saveexec_b32 s3, s2
        0xBF88_0003, // s_cbranch_execz +3 (-> s_endpgm)
        0xBEFC_0380, // s_mov_b32 m0, 0
        0xBF90_0009, // s_sendmsg GS_ALLOC_REQ
        0xBF8A_0000, // s_barrier (single-lane, dominated by divergent alloc)
        S_ENDPGM,
    ]);
    assert_eq!(
        divergent.no_barrier_blocks, 1,
        "divergent single-lane GS_ALLOC_REQ must still suppress dominated barriers"
    );
}

#[test]
fn smem_uav_uses_raw_buffer_load_not_cbuffer() {
    // Root UAV at s[0:1] built into buffer descriptor s[4:7], read by
    // s_buffer_load_dword (sbase = 4/2 = 2) and stored to LDS.
    let root_words = [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0301, // s_mov_b32 s5, s1
        0xBE86_03C1, // s_mov_b32 s6, -1
        0xBE87_0380, // s_mov_b32 s7, 0
        0xF420_0202,
        0xFA00_0000, // s_buffer_load_dword s8, s[4:7], 0x0
        0x7E02_0208, // v_mov_b32 v1, s8
        0xD834_0000,
        0x0000_0100, // ds_write_b32 v0, v1
        S_ENDPGM,
    ];
    let root_layout = vec![root(RootType::Descriptor, 0, 0)];
    let root_used = vec![UsedResource {
        kind: ResourceType::Uav,
        root: RootType::Descriptor,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords: 2,
        user_data: 0,
        slot: 0,
    }];
    let root_out = lower_with(&root_words, root_layout, root_used)
        .expect("root UAV read via s_buffer_load_dword must lower as raw UAV load");
    assert_eq!(root_out.root_uavs, [0]);
    assert!(root_out.root_cbvs.is_empty());
    assert_eq!(root_out.smem_raw_uavs, 1);

    // Table UAV at s0 loaded via s_load_dwordx4 into s[8:11], read via
    // s_buffer_load_dword (sbase = 8/2 = 4), and also queried for its
    // size dword (s10, exercising `descriptor_data` for a UAV read only by SMEM).
    let table_words = [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
        0xF420_0304,
        0xFA00_0000, // s_buffer_load_dword s12, s[8:11], 0x0
        0x800D_0A0C, // s_add_u32 s13, s12, s10 (reads UAV descriptor dword 2 as data)
        0x7E02_020D, // v_mov_b32 v1, s13
        0xD834_0000,
        0x0000_0100, // ds_write_b32 v0, v1
        S_ENDPGM,
    ];
    let table_layout = vec![root(RootType::Table, 0, 0)];
    let table_used = vec![UsedResource {
        kind: ResourceType::Uav,
        root: RootType::Table,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords: 4,
        user_data: 0,
        slot: 0,
    }];
    let table_out = lower_with(&table_words, table_layout, table_used)
        .expect("table UAV read via s_buffer_load_dword must lower as raw UAV load");
    assert_eq!(table_out.smem_raw_uavs, 1);
}

#[test]
fn same_target_conditional_branch_keeps_both_edge_blocks() {
    // Block 0 branches on s0 to block 2 (offset 20) or falls through to
    // block 1. Block 1 sets s2 = 1 and executes a conditional branch whose
    // taken and not-taken targets are BOTH block 2 (offset 20). Block 2
    // joins s2 with a PHI and writes it to LDS. Both same-target edges
    // from block 1 to block 2 must retain distinct entries in `edges`.
    let words = [
        0xBE82_0380, // 0:  s_mov_b32 s2, 0
        0xBF06_8000, // 4:  s_cmp_eq_u32 s0, 0
        0xBF85_0003, // 8:  s_cbranch_scc1 +3 (-> 24)
        0xBE82_0381, // 12: s_mov_b32 s2, 1
        0xBF06_8001, // 16: s_cmp_eq_u32 s1, 0
        0xBF85_0000, // 20: s_cbranch_scc1 +0 (taken == not_taken == 24)
        0x7E02_0202, // 24: v_mov_b32 v1, s2
        0xD834_0000,
        0x0000_0100, // 28: ds_write_b32 v0, v1
        S_ENDPGM,    // 36: s_endpgm
    ];
    let layout = vec![
        root(RootType::Constants, 0, 0),
        root(RootType::Constants, 1, 1),
    ];
    let out = lower_with(&words, layout, vec![]).unwrap();
    // Block 0 -> block 2 (1 edge) + block 1 -> block 2 (2 distinct edges) = 3 edge blocks.
    // Previously the second edge from block 1 overwrote the first (leaving only 2).
    assert_eq!(out.edge_blocks, 3);
}

#[test]
fn scalar_dynamic_offset_wraps_vgpr_input_and_phi_in_wave_read_lane_first() {
    // 1. Source::Built (root SRV descriptor) with dynamic byte offset taken
    //    from input VGPR v0 via v_readfirstlane_b32: built_offset must emit
    //    waveReadLaneFirst.i32 and set WAVE_OPS.
    let built_words = [
        0x7E08_0500, // v_readfirstlane_b32 s4, v0 (program input VGPR!)
        0x8004_0400, // s_add_u32 s4, s0, s4
        0x8205_8001, // s_addc_u32 s5, s1, 0
        0xBE86_03C1, // s_mov_b32 s6, -1
        0xBE87_0380, // s_mov_b32 s7, 0
        0xE030_0000,
        0x8001_0100, // buffer_load_dword v1, off, s[4:7], 0
        0xD834_0000,
        0x0000_0100, // ds_write_b32 v0, v1
        S_ENDPGM,
    ];
    let layout = vec![root(RootType::Descriptor, 0, 0)];
    let used = vec![UsedResource {
        kind: ResourceType::Srv,
        root: RootType::Descriptor,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords: 2,
        user_data: 0,
        slot: 0,
    }];
    let built_out = lower_with(&built_words, layout, used).unwrap();
    assert!(built_out.wave_ops);
    assert_eq!(
        built_out.scalar_wave_first, 1,
        "built_offset must emit waveReadLaneFirst.i32 for input-VGPR dynamic offset"
    );

    // 2. code_smem (s_load_dword from PC-relative code table) with dynamic
    //    offset from a VGPR phi (v2 joined across a scalar branch) via
    //    v_readfirstlane_b32.
    let code_words = [
        0x7E04_0280, // 0:  v_mov_b32 v2, 0
        0xBF06_8000, // 4:  s_cmp_eq_u32 s0, 0
        0xBF85_0001, // 8:  s_cbranch_scc1 +1 (-> 16)
        0x7E04_0300, // 12: v_mov_b32 v2, v0
        0xBE84_1F00, // 16: s_getpc_b64 s[4:5] (PC = 20)
        0x7E0C_0502, // 20: v_readfirstlane_b32 s6, v2 (VGPR phi!)
        0x8004_0604, // 24: s_add_u32 s4, s4, s6
        0x8205_8005, // 28: s_addc_u32 s5, s5, 0
        0xF400_0202,
        0xFA00_0020, // 32: s_load_dword s8, s[4:5], 0x20 (code table at offset 52)
        0x7E02_0208, // 40: v_mov_b32 v1, s8
        0xD834_0000,
        0x0000_0100, // 44: ds_write_b32 v0, v1
        S_ENDPGM,    // 52: s_endpgm
        0x1234_5678, // 56: constant table entry
    ];
    let code_out =
        lower_with(&code_words, vec![root(RootType::Constants, 0, 0)], vec![]).unwrap();
    assert!(code_out.wave_ops);
    assert_eq!(
        code_out.scalar_wave_first, 1,
        "code_smem must emit waveReadLaneFirst.i32 for VGPR-phi dynamic offset"
    );
}
