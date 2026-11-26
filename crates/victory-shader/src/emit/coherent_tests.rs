//! A raw unordered buffer loaded past the wave's cache (GLC) is globally
//! coherent on the host (`lower::coherent`), lowered to DXIL, validated by
//! `dxil.dll` when present, and run on the host GPU and WARP. After
//! AvatarManz/VIctory-7900XTX 875a63a4 (its `size_tests` helpers).

use super::host::View;
use super::waterfall_tests::run;
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

/// Lowers a wave32 compute program of one thread with the table of views at
/// s0 and the view heap at s1: two SRVs, then a raw UAV at dword 16.
fn build(words: &[u32]) -> HostShader {
    let param = |root, user_data| RootParam {
        root,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data,
        slot: 0,
    };
    let range = |kind, table_offset_dwords, dwords| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride: 0,
        dwords,
        user_data: 0,
        slot: 0,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 32,
        layout: vec![param(RootType::Table, 0), param(RootType::ResourceHeap, 1)],
        used: vec![
            range(ResourceType::Srv, 0, 8),
            range(ResourceType::Uav, 16, 4),
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 2 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resources");
    let io = io::raise(&p, &native).expect("io");
    let lowered = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    })
    .expect("translates");
    let shader = HostShader {
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
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .expect("DXIL validation failed");
    } else {
        eprintln!("no dxil.dll; validation skipped");
    }
    shader
}

/// A load past the wave's cache (GLC) of a buffer other groups write: its
/// view is globally coherent on the host, as is every access to it, and
/// the program reads what it stored either way.
#[test]
fn a_buffer_loaded_past_the_cache_is_coherent() {
    let program = |load: u32| {
        let words = [
            0xBE82_0300u32, // s_mov_b32 s2, s0
            0xBE83_0380,    // s_mov_b32 s3, 0
            0xF408_0501,    // s_load_dwordx4 s[20:23], s[2:3], 64
            0xFA00_0040,
            0x7E04_028B, // v_mov_b32 v2, 11
            0xE070_0008, // buffer_store_dword v2, off, s[20:23], 0 offset:8
            0x8005_0200,
            load, // buffer_load_dword v3, off, s[20:23], 0 offset:8
            0x8005_0300,
            0x7E08_0503, // v_readfirstlane_b32 s4, v3
            0xBE85_0380, // s_mov_b32 s5, 0
            0x7E08_0204, // v_mov_b32 v4, s4
            0x7E0A_0205, // v_mov_b32 v5, s5
            0xE070_0000, // buffer_store_dword v4, off, s[20:23], 0
            0x8005_0400,
            0xE070_0004, // buffer_store_dword v5, off, s[20:23], 0 offset:4
            0x8005_0500,
            0xBF81_0000, // s_endpgm
        ];
        build(&words)
    };
    let (plain, coherent) = (program(0xE030_0008), program(0xE030_C008));
    // The annotations of the store's and the load's handles differ.
    assert_ne!(plain.dxil, coherent.dxil);
    let texel = [[0.0; 4]; 1];
    let views = [
        View::Texture(1, 1, &texel),
        View::Texture(1, 1, &texel),
        View::RawUav(3),
    ];
    for shader in [&plain, &coherent] {
        for (label, got) in run(shader, &views) {
            assert_eq!(got[..3], [11, 0, 11], "{label}");
        }
    }
}
