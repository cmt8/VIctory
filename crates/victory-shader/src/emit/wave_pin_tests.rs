//! The console's wave size declared for compute programs that use the
//! host's wave operations (`lower::wave_pin`, `VICTORY_WAVE_PIN`).

use super::host::{Dispatch, Host, Root, View};
use super::valu_tests::ONE_AT_A_TIME;
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;
const THREADS: u32 = 64;

/// `v_readfirstlane_b32 s12, v0` (the thread's index), then every thread
/// stores what it read at its own dword: `out[t] = s12`. On the console a
/// wave32 program of 64 threads is two waves, and the second's first lane
/// is thread 32: threads 32..63 store 32.
fn program() -> Vec<u32> {
    let mut words = vec![
        0xBE85_0380u32, // s_mov_b32 s5, 0
        0xF408_0202,    // s_load_dwordx4 s[8:11], s[4:5], 0
        0xFA00_0000,
        0x7E18_0500, // v_readfirstlane_b32 s12, v0
        0x7E02_020C, // v_mov_b32 v1, s12
        0x3404_0082, // v_lshlrev_b32 v2, 2, v0
    ];
    // buffer_store_dword v1, v2, s[8:11], 0 offen
    words.extend_from_slice(&[0xE070_1000, 0x8002_0102]);
    words.push(S_ENDPGM);
    words
}

/// [`program`] lowered for a console wave of 32 (or 64) on a host whose
/// widest wave is `host_wave`, with the pin `pin` on or off: the shader
/// and the wave size it declares.
fn lowered(wave64: bool, host_wave: u32, pin: bool) -> (HostShader, Option<u32>) {
    let words = program();
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 8,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [THREADS, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let host_was = lower::TEST_HOST_WAVE.replace(Some(host_wave));
    let pin_was = super::WAVE_PIN_TEST.replace(Some(pin));
    let l = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    });
    lower::TEST_HOST_WAVE.set(host_was);
    super::WAVE_PIN_TEST.set(pin_was);
    let l = l.expect("lower");
    let wave = l.wave_size;
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: l.dxil,
        user_data: l.user_data,
        samplers: l.samplers,
        static_samplers: Vec::new(),
        root_cbvs: l.root_cbvs,
        root_srvs: l.root_srvs,
        root_uavs: l.root_uavs,
        root_indexed_views: res.root_indexed_views,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut dxil = shader.dxil.clone();
        v.validate(&mut dxil).expect("DXIL validation failed");
    }
    (shader, wave)
}

/// A compute program whose only wave operation is a first-lane read (no
/// other lane read, no lane mask as data) declared no wave size, and the
/// host chose one: RADV runs waves of 64, so a console wave32 program of
/// 64 threads put its two console waves in one host wave, and threads
/// 32..63 read thread 0's value. With the pin the program declares the
/// console's wave (32, or 64 for a console wave64 program on a host that
/// takes it), and on the host every thread reads its console wave's first
/// lane. Not on a host of at most 32 lanes for a console wave64 program,
/// nor with the host's widest wave unknown, nor with `VICTORY_WAVE_PIN=0`.
#[test]
fn a_program_with_wave_operations_declares_the_consoles_wave() {
    assert_eq!(lowered(false, 64, true).1, Some(32));
    assert_eq!(lowered(false, 32, true).1, Some(32));
    assert_eq!(lowered(true, 64, true).1, Some(64));
    assert_eq!(lowered(true, 32, true).1, None);
    assert_eq!(lowered(false, 0, true).1, None);
    assert_eq!(lowered(false, 64, false).1, None);

    let (pinned, _) = lowered(false, 64, true);
    let (unpinned, _) = lowered(false, 64, false);
    let want: Vec<u32> = (0..THREADS).map(|t| t & !31).collect();
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let run = |shader: &HostShader| -> Option<Vec<u32>> {
            let (root, pso) = host.compute_pipeline(shader).ok()?;
            let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
            for u in &shader.user_data {
                ud[u.index as usize] = match (u.root, u.slot) {
                    (RootType::Table, 1) => 0,
                    other => panic!("unexpected user data {other:?}"),
                };
            }
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(ud))],
                        views: &[View::RawUav(THREADS)],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
            Some(out[0][..THREADS as usize].to_vec())
        };
        let Some(got) = run(&pinned) else {
            // A device whose waves cannot be 32 lanes takes no WaveSize(32).
            eprintln!("{label}: no pipeline with WaveSize(32); skipped");
            continue;
        };
        assert_eq!(got, want, "{label}: each console wave's first lane, pinned");
        let free = run(&unpinned).expect("a pipeline without a declared wave");
        eprintln!(
            "{label}: without the pin threads 32..63 read {:?} (the console: 32)",
            &free[32..36]
        );
        ran += 1;
    }
    assert!(ran > 0, "no SM 6.6 device available");
}
