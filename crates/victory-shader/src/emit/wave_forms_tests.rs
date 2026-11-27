//! The same console compute programs through the forms for a host of
//! 64-lane waves (`WaveSize(64)` for a console wave64 program) and through
//! the forms for a host of 32-lane waves that declare `WaveSize(32)`
//! ([`super::compute_wave_32`], `VICTORY_COMPUTE_WAVE`), run on the
//! hardware device and compared with what the console computes: first-lane
//! reads, lane masks read as numbers and as branch conditions, EXEC as data
//! on both sides of an if/else, and `v_mbcnt`; console wave64 programs and
//! console wave32 programs, in groups that are not whole host waves too.
//! Synthetic programs only.

use super::host::{Dispatch, Host, Root, View};
use super::valu_tests::ONE_AT_A_TIME;
use super::{HostShader, compute_wave, lower, validator};
use crate::dxbc::Container;
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ShaderError, io, ir, resource, structure};

/// Root constants: s0 (an offset, and the thread a branch looks for) and s1
/// (the bits of the thread index that pick the lanes of a mask).
const S0: u32 = 40;
const S1: u32 = 0x25;

/// Each program starts `s_mov_b32 s5, 0`, `s_load_dwordx4 s[8:11], s[4:5]`
/// (the raw UAV), `v_lshlrev_b32 v10, 4, v0` (thread t's four dwords at
/// 16 t) and stores up to four dwords there.
const FIRST64: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x3602_0083, // v_and_b32 v1, 3, v0
    0x7D8A_0280, // v_cmp_ne_u32 vcc, 0, v1
    0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
    0xBF88_0006, // s_cbranch_execz skip
    0xD543_0002, 0x0001_0700, // v_mad_u32_u24 v2, v0, 3, s0
    0x7E1C_0502, // v_readfirstlane_b32 s14, v2
    0x7E06_020E, // v_mov_b32 v3, s14
    0xE070_1000, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen
    0xBEFE_040C, // skip: s_mov_b64 exec, s[12:13]
    0x7E1E_0500, // v_readfirstlane_b32 s15, v0
    0x7E08_020F, // v_mov_b32 v4, s15
    0xE070_1004, 0x8002_040A, // buffer_store_dword v4, v10, s[8:11], 0 offen offset:4
    0xBF81_0000, // s_endpgm
];
const FIRST32: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x3602_0083, // v_and_b32 v1, 3, v0
    0x7D8A_0280, // v_cmp_ne_u32 vcc_lo, 0, v1
    0xBE8C_3C6A, // s_and_saveexec_b32 s12, vcc_lo
    0xBF88_0006, // s_cbranch_execz skip
    0xD543_0002, 0x0001_0700, // v_mad_u32_u24 v2, v0, 3, s0
    0x7E1C_0502, // v_readfirstlane_b32 s14, v2
    0x7E06_020E, // v_mov_b32 v3, s14
    0xE070_1000, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen
    0xBEFE_030C, // skip: s_mov_b32 exec_lo, s12
    0x7E1E_0500, // v_readfirstlane_b32 s15, v0
    0x7E08_020F, // v_mov_b32 v4, s15
    0xE070_1004, 0x8002_040A, // buffer_store_dword v4, v10, s[8:11], 0 offen offset:4
    0xBF81_0000, // s_endpgm
];
const BALLOT64: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x3602_0001, // v_and_b32 v1, s1, v0
    0x7D8A_0280, // v_cmp_ne_u32 vcc, 0, v1
    0x7E04_026A, // v_mov_b32 v2, vcc_lo
    0x7E06_026B, // v_mov_b32 v3, vcc_hi
    0xBE8C_106A, // s_bcnt1_i32_b64 s12, vcc
    0xBE8D_146A, // s_ff1_i32_b64 s13, vcc
    0x7E08_020C, // v_mov_b32 v4, s12
    0x7E0A_020D, // v_mov_b32 v5, s13
    0xE070_1000, 0x8002_020A, // buffer_store_dword v2, v10, s[8:11], 0 offen
    0xE070_1004, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen offset:4
    0xE070_1008, 0x8002_040A, // buffer_store_dword v4, v10, s[8:11], 0 offen offset:8
    0xE070_100C, 0x8002_050A, // buffer_store_dword v5, v10, s[8:11], 0 offen offset:12
    0xBF81_0000, // s_endpgm
];
const BALLOT32: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x3602_0001, // v_and_b32 v1, s1, v0
    0x7D8A_0280, // v_cmp_ne_u32 vcc_lo, 0, v1
    0x7E04_026A, // v_mov_b32 v2, vcc_lo
    0x7E06_0280, // v_mov_b32 v3, 0
    0xBE8C_0F6A, // s_bcnt1_i32_b32 s12, vcc_lo
    0xBE8D_136A, // s_ff1_i32_b32 s13, vcc_lo
    0x7E08_020C, // v_mov_b32 v4, s12
    0x7E0A_020D, // v_mov_b32 v5, s13
    0xE070_1000, 0x8002_020A, // buffer_store_dword v2, v10, s[8:11], 0 offen
    0xE070_1004, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen offset:4
    0xE070_1008, 0x8002_040A, // buffer_store_dword v4, v10, s[8:11], 0 offen offset:8
    0xE070_100C, 0x8002_050A, // buffer_store_dword v5, v10, s[8:11], 0 offen offset:12
    0xBF81_0000, // s_endpgm
];
const BRANCH64: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x7E04_0280, // v_mov_b32 v2, 0
    0x7D84_0000, // v_cmp_eq_u32 vcc, s0, v0
    0xBF86_0001, // s_cbranch_vccz skip
    0x7E04_0281, // v_mov_b32 v2, 1
    0xE070_1000, 0x8002_020A, // skip: buffer_store_dword v2, v10, s[8:11], 0 offen
    0xBF81_0000, // s_endpgm
];
/// (The same words: `vcc` is `vcc_lo` in wave32.)
const BRANCH32: &[u32] = BRANCH64;
const IFELSE64: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x3602_0001, // v_and_b32 v1, s1, v0
    0x7D8A_0280, // v_cmp_ne_u32 vcc, 0, v1
    0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
    0x898C_0C7E, // s_xor_b64 s[12:13], exec, s[12:13]
    0xBF88_0002, // s_cbranch_execz else
    0x7E04_027E, // v_mov_b32 v2, exec_lo
    0x7E06_027F, // v_mov_b32 v3, exec_hi
    0xBE8C_250C, // else: s_or_saveexec_b64 s[12:13], s[12:13]
    0x89FE_0C7E, // s_xor_b64 exec, exec, s[12:13]
    0xBF88_0002, // s_cbranch_execz end
    0x7E04_6E7E, // v_not_b32 v2, exec_lo
    0x7E06_6E7F, // v_not_b32 v3, exec_hi
    0x88FE_0C7E, // end: s_or_b64 exec, exec, s[12:13]
    0xE070_1000, 0x8002_020A, // buffer_store_dword v2, v10, s[8:11], 0 offen
    0xE070_1004, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen offset:4
    0xBF81_0000, // s_endpgm
];
const IFELSE32: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x3602_0001, // v_and_b32 v1, s1, v0
    0x7D8A_0280, // v_cmp_ne_u32 vcc_lo, 0, v1
    0xBE8C_3C6A, // s_and_saveexec_b32 s12, vcc_lo
    0x890C_0C7E, // s_xor_b32 s12, exec_lo, s12
    0xBF88_0002, // s_cbranch_execz else
    0x7E04_027E, // v_mov_b32 v2, exec_lo
    0x7E06_0280, // v_mov_b32 v3, 0
    0xBE8C_3D0C, // else: s_or_saveexec_b32 s12, s12
    0x897E_0C7E, // s_xor_b32 exec_lo, exec_lo, s12
    0xBF88_0002, // s_cbranch_execz end
    0x7E04_6E7E, // v_not_b32 v2, exec_lo
    0x7E06_0280, // v_mov_b32 v3, 0
    0x887E_0C7E, // end: s_or_b32 exec_lo, exec_lo, s12
    0xE070_1000, 0x8002_020A, // buffer_store_dword v2, v10, s[8:11], 0 offen
    0xE070_1004, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen offset:4
    0xBF81_0000, // s_endpgm
];
const MBCNT64: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x3602_0001, // v_and_b32 v1, s1, v0
    0x7D8A_0280, // v_cmp_ne_u32 vcc, 0, v1
    0xD765_0002, 0x0201_00C1, // v_mbcnt_lo_u32_b32 v2, -1, 0
    0xD766_0002, 0x0202_04C1, // v_mbcnt_hi_u32_b32 v2, -1, v2
    0xD765_0003, 0x0201_006A, // v_mbcnt_lo_u32_b32 v3, vcc_lo, 0
    0xD766_0003, 0x0202_066B, // v_mbcnt_hi_u32_b32 v3, vcc_hi, v3
    0xE070_1000, 0x8002_020A, // buffer_store_dword v2, v10, s[8:11], 0 offen
    0xE070_1004, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen offset:4
    0xBF81_0000, // s_endpgm
];
const MBCNT32: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x3602_0001, // v_and_b32 v1, s1, v0
    0x7D8A_0280, // v_cmp_ne_u32 vcc_lo, 0, v1
    0xD765_0002, 0x0201_00C1, // v_mbcnt_lo_u32_b32 v2, -1, 0
    0xD765_0003, 0x0201_006A, // v_mbcnt_lo_u32_b32 v3, vcc_lo, 0
    0xE070_1000, 0x8002_020A, // buffer_store_dword v2, v10, s[8:11], 0 offen
    0xE070_1004, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen offset:4
    0xBF81_0000, // s_endpgm
];

/// A whole-wave sum (`WaveActiveSum` of t + 1 over the lanes S1 picks) as the console
/// compiler expands it, into the picked lanes, and the inclusive prefix sum the same way.
const SUM64: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x3602_0001, // v_and_b32 v1, s1, v0
    0x7D8A_0280, // v_cmp_ne_u32 vcc, 0, v1
    0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
    0xBF88_0018, // s_cbranch_execz end
    0x4A0E_0081, // v_add_nc_u32 v7, 1, v0
    0xBE84_25C1, // s_or_saveexec_b64 s[4:5], -1
    0xD501_0001, 0x0012_0E80, // v_cndmask_b32_e64 v1, 0, v7, s[4:5]
    0x4A02_02E9, 0xDE54_C101, // v_add_nc_u32_dpp v1, v1, v1 dpp8:[1,0,3,2,5,4,7,6]
    0x4A02_02E9, 0x9360_1201, // v_add_nc_u32_dpp v1, v1, v1 dpp8:[2,2,0,0,6,6,4,4]
    0x4A02_02E9, 0x0009_2401, // v_add_nc_u32_dpp v1, v1, v1 dpp8:[4,4,4,4,0,0,0,0]
    0x4A02_02FA, 0xFF09_4001, // v_add_nc_u32_dpp v1, v1, v1 row_mirror row_mask:0xf bank_mask:0xf bound_ctrl:1
    0xD778_0002, 0x0201_0101, // v_permlanex16_b32 v2, v1, 0, 0
    0x4A02_0302, // v_add_nc_u32 v1, v2, v1
    0xD760_0007, 0x0201_4101, // v_readlane_b32 s7, v1, 32
    0x4A02_0207, // v_add_nc_u32 v1, s7, v1
    0xD760_0007, 0x0201_0101, // v_readlane_b32 s7, v1, 0
    0xBEFE_0404, // s_mov_b64 exec, s[4:5]
    0x7E06_0207, // v_mov_b32 v3, s7
    0xE070_1000, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen
    0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    0xBF81_0000, // s_endpgm
];
const SUM32: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x3602_0001, // v_and_b32 v1, s1, v0
    0x7D8A_0280, // v_cmp_ne_u32 vcc_lo, 0, v1
    0xBE8C_3C6A, // s_and_saveexec_b32 s12, vcc_lo
    0xBF88_0015, // s_cbranch_execz end
    0x4A0E_0081, // v_add_nc_u32 v7, 1, v0
    0xBE84_3DC1, // s_or_saveexec_b32 s4, -1
    0xD501_0001, 0x0012_0E80, // v_cndmask_b32_e64 v1, 0, v7, s4
    0x4A02_02E9, 0xDE54_C101, // v_add_nc_u32_dpp v1, v1, v1 dpp8:[1,0,3,2,5,4,7,6]
    0x4A02_02E9, 0x9360_1201, // v_add_nc_u32_dpp v1, v1, v1 dpp8:[2,2,0,0,6,6,4,4]
    0x4A02_02E9, 0x0009_2401, // v_add_nc_u32_dpp v1, v1, v1 dpp8:[4,4,4,4,0,0,0,0]
    0x4A02_02FA, 0xFF09_4001, // v_add_nc_u32_dpp v1, v1, v1 row_mirror row_mask:0xf bank_mask:0xf bound_ctrl:1
    0xD778_0002, 0x0201_0101, // v_permlanex16_b32 v2, v1, 0, 0
    0x4A02_0302, // v_add_nc_u32 v1, v2, v1
    0xD760_0007, 0x0201_0101, // v_readlane_b32 s7, v1, 0
    0xBEFE_0304, // s_mov_b32 exec_lo, s4
    0x7E06_0207, // v_mov_b32 v3, s7
    0xE070_1000, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen
    0xBEFE_030C, // s_mov_b32 exec_lo, s12
    0xBF81_0000, // s_endpgm
];
const SCAN64: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x3602_0001, // v_and_b32 v1, s1, v0
    0x7D8A_0280, // v_cmp_ne_u32 vcc, 0, v1
    0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
    0xBF88_0018, // s_cbranch_execz end
    0x4A0E_0081, // v_add_nc_u32 v7, 1, v0
    0xBE84_25C1, // s_or_saveexec_b64 s[4:5], -1
    0xD501_0001, 0x0012_0E80, // v_cndmask_b32_e64 v1, 0, v7, s[4:5]
    0x4A02_02FA, 0xFF09_1101, // v_add_nc_u32_dpp v1, v1, v1 row_shr:1 row_mask:0xf bank_mask:0xf bound_ctrl:1
    0x4A02_02FA, 0xFF09_1201, // v_add_nc_u32_dpp v1, v1, v1 row_shr:2 row_mask:0xf bank_mask:0xf bound_ctrl:1
    0x4A02_02FA, 0xFF09_1401, // v_add_nc_u32_dpp v1, v1, v1 row_shr:4 row_mask:0xf bank_mask:0xf bound_ctrl:1
    0x4A02_02FA, 0xFF09_1801, // v_add_nc_u32_dpp v1, v1, v1 row_shr:8 row_mask:0xf bank_mask:0xf bound_ctrl:1
    0xD778_0002, 0x0305_8301, // v_permlanex16_b32 v2, v1, -1, -1
    0x4A02_02FA, 0xAF00_E402, // v_add_nc_u32_dpp v1, v2, v1 quad_perm:[0,1,2,3] row_mask:0xa bank_mask:0xf
    0xD760_0007, 0x0201_3F01, // v_readlane_b32 s7, v1, 31
    0x7E04_0207, // v_mov_b32 v2, s7
    0x4A02_02FA, 0xCF00_E402, // v_add_nc_u32_dpp v1, v2, v1 quad_perm:[0,1,2,3] row_mask:0xc bank_mask:0xf
    0xBEFE_0404, // s_mov_b64 exec, s[4:5]
    0xE070_1000, 0x8002_010A, // buffer_store_dword v1, v10, s[8:11], 0 offen
    0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    0xBF81_0000, // s_endpgm
];
const SCAN32: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x3602_0001, // v_and_b32 v1, s1, v0
    0x7D8A_0280, // v_cmp_ne_u32 vcc_lo, 0, v1
    0xBE8C_3C6A, // s_and_saveexec_b32 s12, vcc_lo
    0xBF88_0013, // s_cbranch_execz end
    0x4A0E_0081, // v_add_nc_u32 v7, 1, v0
    0xBE84_3DC1, // s_or_saveexec_b32 s4, -1
    0xD501_0001, 0x0012_0E80, // v_cndmask_b32_e64 v1, 0, v7, s4
    0x4A02_02FA, 0xFF09_1101, // v_add_nc_u32_dpp v1, v1, v1 row_shr:1 row_mask:0xf bank_mask:0xf bound_ctrl:1
    0x4A02_02FA, 0xFF09_1201, // v_add_nc_u32_dpp v1, v1, v1 row_shr:2 row_mask:0xf bank_mask:0xf bound_ctrl:1
    0x4A02_02FA, 0xFF09_1401, // v_add_nc_u32_dpp v1, v1, v1 row_shr:4 row_mask:0xf bank_mask:0xf bound_ctrl:1
    0x4A02_02FA, 0xFF09_1801, // v_add_nc_u32_dpp v1, v1, v1 row_shr:8 row_mask:0xf bank_mask:0xf bound_ctrl:1
    0xD778_0002, 0x0305_8301, // v_permlanex16_b32 v2, v1, -1, -1
    0x4A02_02FA, 0xAF00_E402, // v_add_nc_u32_dpp v1, v2, v1 quad_perm:[0,1,2,3] row_mask:0xa bank_mask:0xf
    0xBEFE_0304, // s_mov_b32 exec_lo, s4
    0xE070_1000, 0x8002_010A, // buffer_store_dword v1, v10, s[8:11], 0 offen
    0xBEFE_030C, // s_mov_b32 exec_lo, s12
    0xBF81_0000, // s_endpgm
];

/// `WaveActiveMin` the same way (identity !0), whose missing half must not count as 0.
const MIN64: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x3602_0001, // v_and_b32 v1, s1, v0
    0x7D8A_0280, // v_cmp_ne_u32 vcc, 0, v1
    0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
    0xBF88_0018, // s_cbranch_execz end
    0x4A0E_0081, // v_add_nc_u32 v7, 1, v0
    0xBE84_25C1, // s_or_saveexec_b64 s[4:5], -1
    0xD501_0001, 0x0012_0EC1, // v_cndmask_b32_e64 v1, -1, v7, s[4:5]
    0x2602_02E9, 0xDE54_C101, // v_min_u32_dpp v1, v1, v1 dpp8:[1,0,3,2,5,4,7,6]
    0x2602_02E9, 0x9360_1201, // v_min_u32_dpp v1, v1, v1 dpp8:[2,2,0,0,6,6,4,4]
    0x2602_02E9, 0x0009_2401, // v_min_u32_dpp v1, v1, v1 dpp8:[4,4,4,4,0,0,0,0]
    0x2602_02FA, 0xFF09_4001, // v_min_u32_dpp v1, v1, v1 row_mirror row_mask:0xf bank_mask:0xf bound_ctrl:1
    0xD778_0002, 0x0201_0101, // v_permlanex16_b32 v2, v1, 0, 0
    0x2602_0302, // v_min_u32 v1, v2, v1
    0xD760_0007, 0x0201_4101, // v_readlane_b32 s7, v1, 32
    0x2602_0207, // v_min_u32 v1, s7, v1
    0xD760_0007, 0x0201_0101, // v_readlane_b32 s7, v1, 0
    0xBEFE_0404, // s_mov_b64 exec, s[4:5]
    0x7E06_0207, // v_mov_b32 v3, s7
    0xE070_1000, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen
    0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    0xBF81_0000, // s_endpgm
];
const MIN32: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x3602_0001, // v_and_b32 v1, s1, v0
    0x7D8A_0280, // v_cmp_ne_u32 vcc_lo, 0, v1
    0xBE8C_3C6A, // s_and_saveexec_b32 s12, vcc_lo
    0xBF88_0015, // s_cbranch_execz end
    0x4A0E_0081, // v_add_nc_u32 v7, 1, v0
    0xBE84_3DC1, // s_or_saveexec_b32 s4, -1
    0xD501_0001, 0x0012_0EC1, // v_cndmask_b32_e64 v1, -1, v7, s4
    0x2602_02E9, 0xDE54_C101, // v_min_u32_dpp v1, v1, v1 dpp8:[1,0,3,2,5,4,7,6]
    0x2602_02E9, 0x9360_1201, // v_min_u32_dpp v1, v1, v1 dpp8:[2,2,0,0,6,6,4,4]
    0x2602_02E9, 0x0009_2401, // v_min_u32_dpp v1, v1, v1 dpp8:[4,4,4,4,0,0,0,0]
    0x2602_02FA, 0xFF09_4001, // v_min_u32_dpp v1, v1, v1 row_mirror row_mask:0xf bank_mask:0xf bound_ctrl:1
    0xD778_0002, 0x0201_0101, // v_permlanex16_b32 v2, v1, 0, 0
    0x2602_0302, // v_min_u32 v1, v2, v1
    0xD760_0007, 0x0201_0101, // v_readlane_b32 s7, v1, 0
    0xBEFE_0304, // s_mov_b32 exec_lo, s4
    0x7E06_0207, // v_mov_b32 v3, s7
    0xE070_1000, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen
    0xBEFE_030C, // s_mov_b32 exec_lo, s12
    0xBF81_0000, // s_endpgm
];

/// LDS shared within one console wave without a barrier (the console's compiler leaves
/// it out for a group of one wave): thread t writes t + 1 at its dword and reads the
/// dword half a wave on.
const LDS64: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x4A04_0081, // v_add_nc_u32 v2, 1, v0
    0x3408_0082, // v_lshlrev_b32 v4, 2, v0
    0xD834_0000, 0x0000_0204, // ds_write_b32 v4, v2
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x4A0A_00A0, // v_add_nc_u32 v5, 32, v0
    0x360A_0ABF, // v_and_b32 v5, 63, v5
    0x340A_0A82, // v_lshlrev_b32 v5, 2, v5
    0xD8D8_0000, 0x0300_0005, // ds_read_b32 v3, v5
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xE070_1000, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen
    0xBF81_0000, // s_endpgm
];
const LDS32: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x4A04_0081, // v_add_nc_u32 v2, 1, v0
    0x3408_0082, // v_lshlrev_b32 v4, 2, v0
    0xD834_0000, 0x0000_0204, // ds_write_b32 v4, v2
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x4A0A_0090, // v_add_nc_u32 v5, 16, v0
    0x360A_0A9F, // v_and_b32 v5, 31, v5
    0x340A_0A82, // v_lshlrev_b32 v5, 2, v5
    0xD8D8_0000, 0x0300_0005, // ds_read_b32 v3, v5
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xE070_1000, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen
    0xBF81_0000, // s_endpgm
];
/// `ds_bpermute_b32`: thread t reads t + 1 from the lane a quarter wave on.
const BPERM64: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x4A04_0081, // v_add_nc_u32 v2, 1, v0
    0x4A08_0090, // v_add_nc_u32 v4, 16, v0
    0x3608_08BF, // v_and_b32 v4, 63, v4
    0x3408_0882, // v_lshlrev_b32 v4, 2, v4
    0xDACC_0000, 0x0300_0204, // ds_bpermute_b32 v3, v4, v2
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xE070_1000, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen
    0xBF81_0000, // s_endpgm
];
const BPERM32: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x4A04_0081, // v_add_nc_u32 v2, 1, v0
    0x4A08_0088, // v_add_nc_u32 v4, 8, v0
    0x3608_089F, // v_and_b32 v4, 31, v4
    0x3408_0882, // v_lshlrev_b32 v4, 2, v4
    0xDACC_0000, 0x0300_0204, // ds_bpermute_b32 v3, v4, v2
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xE070_1000, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen
    0xBF81_0000, // s_endpgm
];

/// Every thread stores its index: no wave operation, no lane read, no LDS (the
/// same words for both console waves).
const PLAIN: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0xE070_1000, 0x8002_000A, // buffer_store_dword v0, v10, s[8:11], 0 offen
    0xBF81_0000, // s_endpgm
];

/// The 16-bit float sum over the wave (`v_add_f16` in [`super::wave::WAVE_OR_V7_TO_S7`]'s
/// steps) of 0x3C00 + 37 t, which the console adds as a tree of pairs, each addition
/// rounded: every thread stores it.
fn half_sum() -> Vec<u32> {
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
        0x3414_0084, // v_lshlrev_b32 v10, 4, v0
        0x340C_0085, // v_lshlrev_b32 v6, 5, v0
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0x4A0E_0F06, // v_add_nc_u32 v7, v6, v7
        0x4A0E_0F00, // v_add_nc_u32 v7, v0, v7
        0x4A0E_0EFF, 0x0000_3C00, // v_add_nc_u32 v7, 0x3c00, v7
    ];
    // (opcode 50, v_add_f16, for 28, v_or_b32)
    words.extend(super::wave::WAVE_OR_V7_TO_S7.iter().map(|&w| if w >> 25 == 28 { w & 0x01FF_FFFF | 50 << 25 } else { w }));
    words.extend_from_slice(&[
        0x7E06_0207, // v_mov_b32 v3, s7
        0xE070_1000, 0x8002_030A, // buffer_store_dword v3, v10, s[8:11], 0 offen
        0xBF81_0000, // s_endpgm
    ]);
    words
}

/// The console's tree of 16-bit additions over `terms` (normal numbers or +0), each
/// rounded to nearest, ties to even.
fn half_tree(terms: &[u16]) -> u16 {
    let wide = |h: u16| {
        if h == 0 { 0.0 } else { f32::from_bits((u32::from(h >> 10) + 112) << 23 | u32::from(h & 0x3FF) << 13) }
    };
    let narrow = |f: f32| {
        if f == 0.0 {
            return 0;
        }
        let b = f.to_bits();
        let h = ((b >> 23) - 112) << 10 | (b >> 13 & 0x3FF);
        let rest = b & 0x1FFF;
        (h + u32::from(rest > 0x1000 || rest == 0x1000 && h & 1 == 1)) as u16
    };
    let mut tree = terms.to_vec();
    while tree.len() > 1 {
        tree = tree.chunks(2).map(|c| narrow(wide(c[0]) + wide(c[1]))).collect();
    }
    tree[0]
}

/// What the console stores: four dwords per thread (0 where nothing is
/// stored), for the program `name` in waves of `wave` lanes and a group of
/// `threads`.
fn console(name: &str, wave: u32, threads: u32) -> Vec<u32> {
    let mut out = vec![0u32; 4 * threads as usize];
    // the lanes of thread t's wave whose thread is in the group and picked
    // by S1, as a mask
    let picked = |base: u32| -> u64 {
        (0..wave)
            .filter(|&l| base + l < threads && (base + l) & S1 != 0)
            .fold(0u64, |m, l| m | 1 << l)
    };
    let here = |base: u32| -> u64 { (0..wave).filter(|&l| base + l < threads).fold(0u64, |m, l| m | 1 << l) };
    for t in 0..threads {
        let (base, lane) = (t / wave * wave, t % wave);
        let o = &mut out[4 * t as usize..][..4];
        match name {
            "first" => {
                if t % 4 != 0 {
                    o[0] = 3 * (base + 1) + S0;
                }
                o[1] = base;
            }
            "ballot" => {
                let m = picked(base);
                o[0] = m as u32;
                o[1] = (m >> 32) as u32;
                o[2] = m.count_ones();
                o[3] = if m == 0 { u32::MAX } else { m.trailing_zeros() };
            }
            "branch" => o[0] = u32::from((base..base + wave).contains(&S0) && S0 < threads),
            "ifelse" => {
                let then = picked(base);
                let other = here(base) & !then;
                let m = if t & S1 != 0 { then } else { !other };
                o[0] = m as u32;
                o[1] = if wave == 64 { (m >> 32) as u32 } else { 0 };
            }
            "sum" => {
                if t & S1 != 0 {
                    o[0] = (0..wave).filter(|&l| picked(base) >> l & 1 != 0).map(|l| base + l + 1).sum();
                }
            }
            "min" => {
                if t & S1 != 0 {
                    o[0] = (0..wave).filter(|&l| picked(base) >> l & 1 != 0).map(|l| base + l + 1).min().unwrap_or(0);
                }
            }
            "scan" => {
                if t & S1 != 0 {
                    o[0] = (0..=lane).filter(|&l| picked(base) >> l & 1 != 0).map(|l| base + l + 1).sum();
                }
            }
            "lds" => o[0] = base + (lane + wave / 2) % wave + 1,
            "bperm" => o[0] = base + (lane + wave / 4) % wave + 1,
            "hsum" => {
                let terms: Vec<u16> =
                    (base..base + wave).map(|u| if u < threads { (0x3C00 + 37 * u) as u16 } else { 0 }).collect();
                o[0] = u32::from(half_tree(&terms));
            }
            "mbcnt" => {
                o[0] = lane;
                o[1] = (picked(base) & ((1u64 << lane) - 1)).count_ones();
            }
            "plain" => o[0] = t,
            _ => unreachable!(),
        }
    }
    out
}

/// How a program is translated.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Form {
    /// For a host of 64-lane waves, as before (`VICTORY_COMPUTE_WAVE=0` on RADV).
    Host64,
    /// For host waves of 32 on a host that also runs 64 (`VICTORY_COMPUTE_WAVE=32`).
    Waves32,
    /// On such a host with `VICTORY_COMPUTE_WAVE` unset: a console wave64 program for
    /// host waves of 64, which it declares; a console wave32 program as `Waves32`.
    Console,
    /// For a host of at most 32 lanes (`VICTORY_HOST_WAVE=32`, NVIDIA's forms), which
    /// on RADV runs what declares no size in the driver's waves.
    Host32,
}

/// Groups of 8 x (threads / 8) where `flat` (thread ids in v0 and v1, the flat index made
/// as `v0 = 8 v1 + v0` before the program), else threads x 1; 4 bytes of LDS per thread.
fn native(words: &[u32], wave64: bool, threads: u32, flat: bool) -> NativeProgram {
    let mut code = words.to_vec();
    if flat {
        // after the descriptor load: v_lshl_add_u32 v0, v1, 3, v0
        code.splice(3..3, [0xD746_0000, 0x0401_0701]);
    }
    let lds = (4 * threads).div_ceil(512);
    NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 16,
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
            // 5 user SGPRs: s0..s4, thread ids in x (and y), LDS
            rsrc2: 5 << 1 | u32::from(flat) << 11 | lds << 15,
            threads: if flat { [8, threads / 8, 1] } else { [threads, 1, 1] },
        }),
        code: code.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `words` translated by the whole path a title's program takes
/// ([`super::emit_sized`], its retries included) in `form`: the shader and
/// the wave size it declares.
fn translated(
    words: &[u32],
    wave64: bool,
    threads: u32,
    flat: bool,
    form: Form,
) -> Result<(HostShader, Option<u32>), ShaderError> {
    let native = native(words, wave64, threads, flat);
    let p = ir::build(&native.code, native.wave64, native.vgprs)?;
    let st = structure::structure(&p)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, &native)?;
    let c = Container {
        digest: [0; 16],
        version: (1, 0),
        parts: Vec::new(),
    };
    let (host, mode, console) = match form {
        Form::Host64 => (64, false, false),
        Form::Waves32 => (64, true, false),
        Form::Console => (64, true, true),
        Form::Host32 => (32, false, false),
    };
    let host_was = lower::TEST_HOST_WAVE.replace(Some(host));
    let mode_was = compute_wave::TEST.replace(Some(mode));
    let console_was = compute_wave::TEST_CONSOLE.replace(console);
    let r = super::emit_sized(&c, &native, &p, &st, &io, &res);
    lower::TEST_HOST_WAVE.set(host_was);
    compute_wave::TEST.set(mode_was);
    compute_wave::TEST_CONSOLE.set(console_was);
    let r = r?;
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut dxil = r.0.dxil.clone();
        v.validate(&mut dxil).expect("DXIL validation failed");
    }
    Ok(r)
}

/// Runs `shader` once on `host` (one group) and gives the UAV's first
/// `dwords` dwords; None where the device takes no such pipeline.
fn run(host: &Host, shader: &HostShader, dwords: u32) -> Option<Vec<u32>> {
    run_groups(host, shader, dwords, 1)
}

/// [`run`] in a dispatch of `groups` groups.
fn run_groups(host: &Host, shader: &HostShader, dwords: u32, groups: u32) -> Option<Vec<u32>> {
    let (root, pso) = host.compute_pipeline(shader).ok()?;
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        ud[u.index as usize] = match (u.root, u.slot) {
            (RootType::Constants, 0) => [S0, S1, 0, 0][u.dword as usize],
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
                views: &[View::RawUav(dwords)],
                samplers: &[],
                groups: [groups, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][..dwords as usize].to_vec())
}

/// `words` with every `buffer_store_dword` (MUBUF opcode 0x1C) made a
/// `buffer_atomic_add` (0x32) of the same dword at the same place: what the
/// groups of a dispatch leave is then the sum of what each would store,
/// whichever comes last.
fn adding(words: &[u32]) -> Vec<u32> {
    words
        .iter()
        .map(|&w| if w >> 26 == 0x38 && w >> 18 & 0x7F == 0x1C { w & !(0x7F << 18) | 0x32 << 18 } else { w })
        .collect()
}

/// The programs, their console wave, the groups each runs in, and whether those are
/// 8 threads wide (2D thread ids).
fn cases() -> Vec<(&'static str, &'static [u32], bool, &'static [u32], bool)> {
    static HSUM: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    let hsum: &'static [u32] = HSUM.get_or_init(half_sum);
    const G64: &[u32] = &[64, 96, 128];
    const G32: &[u32] = &[48, 64, 96];
    vec![
        ("first", FIRST64, true, G64, false),
        ("ballot", BALLOT64, true, G64, false),
        ("branch", BRANCH64, true, G64, false),
        ("ifelse", IFELSE64, true, G64, false),
        ("mbcnt", MBCNT64, true, G64, false),
        ("sum", SUM64, true, G64, false),
        ("min", MIN64, true, G64, false),
        ("scan", SCAN64, true, G64, false),
        ("lds", LDS64, true, &[64], false),
        ("bperm", BPERM64, true, &[64, 128], false),
        // (not in a group of 96: a 16-bit sum over a wave that is not full is refused)
        ("hsum", hsum, true, &[64, 128], false),
        ("ballot", BALLOT64, true, &[64, 96], true),
        ("mbcnt", MBCNT64, true, &[64, 96], true),
        ("sum", SUM64, true, &[64, 96], true),
        // (a group of half a console wave too)
        ("plain", PLAIN, true, &[32, 64, 96], false),
        ("first", FIRST32, false, G32, false),
        ("ballot", BALLOT32, false, G32, false),
        ("branch", BRANCH32, false, G32, false),
        ("ifelse", IFELSE32, false, G32, false),
        ("mbcnt", MBCNT32, false, G32, false),
        ("sum", SUM32, false, G32, false),
        ("min", MIN32, false, G32, false),
        ("scan", SCAN32, false, G32, false),
        ("lds", LDS32, false, &[32], false),
        ("bperm", BPERM32, false, &[32, 64], false),
        ("ballot", BALLOT32, false, &[48, 64], true),
        ("mbcnt", MBCNT32, false, &[48, 64], true),
        ("sum", SUM32, false, &[48, 64], true),
        ("plain", PLAIN, false, G32, false),
    ]
}

/// Every case through every form on the hardware device: one line each,
/// with the declared wave and where the result differs from the console's.
/// Asserted: the forms for a host of 64-lane waves and those of
/// [`super::compute_wave_32`] (`=32`, and unset) give the console's results in every case but
/// `branch`, which is lowered per lane in every form (`structure`: a branch
/// on VCC is taken to test a value the same in every lane, which is what
/// the console's compiler branches on that way); its lines show what a
/// branch on a VCC that differs between lanes gives. NVIDIA's forms
/// (`Host32`) are shown, not asserted: they include approximate ones, and
/// what declares no wave size runs in the waves the driver picks.
#[test]
fn the_wave_forms_on_the_card_give_the_consoles_results() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let mut wrong = Vec::new();
    let mut ran = 0;
    for (name, words, wave64, groups, flat) in cases() {
        let wave = if wave64 { 64 } else { 32 };
        for &threads in groups {
            let want = console(name, wave, threads);
            for form in [Form::Host64, Form::Waves32, Form::Console, Form::Host32] {
                let shape = if flat { format!("8x{}", threads / 8) } else { format!("x{threads}") };
                let label = format!("wave{wave} {name} {shape} {form:?}");
                let asserted = form != Form::Host32 && name != "branch";
                let (shader, declared) = match translated(words, wave64, threads, flat, form) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("{label}: not translated: {e}");
                        if asserted {
                            wrong.push(format!("{label}: not translated: {e}"));
                        }
                        continue;
                    }
                };
                let Some(got) = run(&host, &shader, 4 * threads) else {
                    eprintln!("{label}: declared {declared:?}: the device takes no such pipeline");
                    continue;
                };
                ran += 1;
                let diffs: Vec<String> = (0..got.len())
                    .filter(|&i| got[i] != want[i])
                    .map(|i| format!("t{}[{}]={:#x} (console {:#x})", i / 4, i % 4, got[i], want[i]))
                    .collect();
                if diffs.is_empty() {
                    eprintln!("{label}: declared {declared:?}: the console's");
                } else {
                    let shown: Vec<&String> = diffs.iter().take(6).collect();
                    eprintln!("{label}: declared {declared:?}: {} dwords differ: {shown:?}", diffs.len());
                    if asserted {
                        wrong.push(label);
                    }
                }
            }
        }
    }
    eprintln!("{ran} dispatches on the hardware device");
    assert!(wrong.is_empty(), "forms that differ from the console: {wrong:?}");
}

/// The same cases in a dispatch of many groups at once, each group adding
/// what it would store ([`adding`]): every group of the dispatch gives the
/// console's result in the forms for a host of 64-lane waves and in those
/// of [`super::compute_wave_32`], whose two host waves of a console wave
/// meet through their group's shared memory and barriers while thousands of
/// other groups do the same (the title's tile passes dispatch one group a
/// tile).
#[test]
fn the_wave_forms_give_the_consoles_results_in_every_group_of_a_dispatch() {
    const GROUPS: u32 = 4096;
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let mut wrong = Vec::new();
    let mut ran = 0;
    for (name, words, wave64, groups, flat) in cases() {
        if name == "branch" {
            // (per lane in every form; see the test above)
            continue;
        }
        let wave = if wave64 { 64 } else { 32 };
        let words = adding(words);
        for &threads in groups {
            let want: Vec<u32> = console(name, wave, threads).iter().map(|v| v.wrapping_mul(GROUPS)).collect();
            for form in [Form::Host64, Form::Waves32, Form::Console] {
                let shape = if flat { format!("8x{}", threads / 8) } else { format!("x{threads}") };
                let label = format!("wave{wave} {name} {shape} {form:?} in {GROUPS} groups");
                let (shader, declared) = match translated(&words, wave64, threads, flat, form) {
                    Ok(t) => t,
                    Err(e) => {
                        wrong.push(format!("{label}: not translated: {e}"));
                        continue;
                    }
                };
                let Some(got) = run_groups(&host, &shader, 4 * threads, GROUPS) else {
                    eprintln!("{label}: declared {declared:?}: the device takes no such pipeline");
                    continue;
                };
                ran += 1;
                let diffs: Vec<String> = (0..got.len())
                    .filter(|&i| got[i] != want[i])
                    .map(|i| format!("t{}[{}]={:#x} (console {:#x})", i / 4, i % 4, got[i], want[i]))
                    .collect();
                if !diffs.is_empty() {
                    let shown: Vec<&String> = diffs.iter().take(6).collect();
                    eprintln!("{label}: declared {declared:?}: {} dwords differ: {shown:?}", diffs.len());
                    wrong.push(label);
                }
            }
        }
    }
    eprintln!("{ran} dispatches of {GROUPS} groups on the hardware device");
    assert!(wrong.is_empty(), "forms that differ from the console: {wrong:?}");
}

/// A barrier only some threads reach (a lane branch around it), no wave operation: the
/// host leaves the barrier out (`fence_only`).
const BARRIER32: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x7D88_0000, // v_cmp_gt_u32 vcc_lo, s0, v0
    0xBE8C_3C6A, // s_and_saveexec_b32 s12, vcc_lo
    0xBF88_0001, // s_cbranch_execz end
    0xBF8A_0000, // s_barrier
    0xBEFE_030C, // end: s_mov_b32 exec_lo, s12
    0xE070_1000, 0x8002_000A, // buffer_store_dword v0, v10, s[8:11], 0 offen
    0xBF81_0000, // s_endpgm
];

/// Which wave each form declares (no device needed). With `VICTORY_COMPUTE_WAVE`
/// (`Waves32`) a console wave32 program declares 32 whether or not it uses wave
/// operations, but for one without any whose barriers the host leaves out; a console
/// wave64 program declares 32 where a 32-lane form takes it exactly (lane masks as data,
/// a first-lane read of a value that differs between lanes, a wave sum) and keeps what it
/// had before otherwise (none here without wave operations). The forms for a host of 64
/// lanes and NVIDIA's are those of before.
#[test]
fn each_form_declares_its_wave() {
    let declared = |words: &[u32], wave64: bool, threads: u32, form: Form| {
        translated(words, wave64, threads, false, form).unwrap_or_else(|e| panic!("{form:?}: {e}")).1
    };
    // a console wave32 program without wave operations
    assert_eq!(declared(BRANCH32, false, 64, Form::Host64), None);
    assert_eq!(declared(BRANCH32, false, 64, Form::Waves32), Some(32));
    assert_eq!(declared(BRANCH32, false, 64, Form::Host32), None);
    // ... whose barrier the host leaves out
    assert_eq!(declared(BARRIER32, false, 64, Form::Host64), None);
    assert_eq!(declared(BARRIER32, false, 64, Form::Waves32), None);
    // with wave operations: the console's wave in every form
    for form in [Form::Host64, Form::Waves32, Form::Host32] {
        assert_eq!(declared(FIRST32, false, 64, form), Some(32), "{form:?}");
    }
    // console wave64 programs
    assert_eq!(declared(BALLOT64, true, 128, Form::Host64), Some(64));
    assert_eq!(declared(BALLOT64, true, 128, Form::Waves32), Some(32));
    assert_eq!(declared(FIRST64, true, 128, Form::Host64), Some(64));
    assert_eq!(declared(FIRST64, true, 128, Form::Waves32), Some(32));
    assert_eq!(declared(FIRST64, true, 128, Form::Host32), None);
    assert_eq!(declared(SUM64, true, 128, Form::Waves32), Some(32));
    assert_eq!(declared(BRANCH64, true, 128, Form::Waves32), None);
}

/// `VICTORY_COMPUTE_WAVE=pin` and `=forms` (no device needed): with `pin` the console
/// wave32 programs declare their wave as with the whole switch and the console wave64
/// programs are what they were before it; with `forms` the other way round.
#[test]
fn the_switch_can_leave_one_kind_of_program_as_before() {
    let declared = |words: &[u32], wave64: bool, threads: u32, only: Option<bool>| {
        let was = compute_wave::TEST_ONLY.replace(only);
        let r = translated(words, wave64, threads, false, Form::Waves32);
        compute_wave::TEST_ONLY.set(was);
        r.unwrap_or_else(|e| panic!("{only:?}: {e}")).1
    };
    let before = |words: &[u32], wave64: bool, threads: u32| {
        translated(words, wave64, threads, false, Form::Host64).unwrap_or_else(|e| panic!("{e}")).1
    };
    let (pin, forms) = (Some(false), Some(true));
    // console wave32 programs: without wave operations, and with
    assert_eq!(before(BRANCH32, false, 64), None);
    assert_eq!(declared(BRANCH32, false, 64, None), Some(32));
    assert_eq!(declared(BRANCH32, false, 64, pin), Some(32));
    assert_eq!(declared(BRANCH32, false, 64, forms), None);
    for only in [None, pin, forms] {
        assert_eq!(declared(FIRST32, false, 64, only), Some(32), "{only:?}");
    }
    // console wave64 programs
    for words in [BALLOT64, FIRST64, SUM64] {
        assert_eq!(before(words, true, 128), Some(64));
        assert_eq!(declared(words, true, 128, None), Some(32));
        assert_eq!(declared(words, true, 128, pin), Some(64));
        assert_eq!(declared(words, true, 128, forms), Some(32));
    }
    // ... and the code is the one of before, not only the wave
    let code = |form: Form, only: Option<bool>| {
        let was = compute_wave::TEST_ONLY.replace(only);
        let r = translated(BALLOT64, true, 128, false, form);
        compute_wave::TEST_ONLY.set(was);
        r.expect("translated").0.dxil
    };
    assert!(code(Form::Waves32, pin) == code(Form::Host64, None));
    assert!(code(Form::Waves32, forms) == code(Form::Waves32, None));
    assert!(code(Form::Waves32, forms) != code(Form::Host64, None));
}

/// A wave64 barrier only some threads reach, as [`BARRIER32`].
const BARRIER64: &[u32] = &[
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0202, 0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
    0x3414_0084, // v_lshlrev_b32 v10, 4, v0
    0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
    0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
    0xBF88_0001, // s_cbranch_execz end
    0xBF8A_0000, // s_barrier
    0xBEFE_040C, // end: s_mov_b64 exec, s[12:13]
    0xE070_1000, 0x8002_000A, // buffer_store_dword v0, v10, s[8:11], 0 offen
    0xBF81_0000, // s_endpgm
];

/// `VICTORY_COMPUTE_WAVE` unset on a host of waves of 32 and 64 (no device needed): a
/// console wave32 program declares 32 as with `=32` and `=pin`; a console wave64 program
/// declares 64 whether or not it uses wave operations, whatever its group and whether or
/// not the host leaves out a barrier of its, and its code is the one for a host of
/// 64-lane waves: none of the forms for host waves of 32. The wave is the whole
/// difference from `=pin`, which declares 64 only where the program did before.
#[test]
fn unset_the_switch_declares_the_consoles_wave_and_uses_no_form_for_waves_of_32() {
    let made = |words: &[u32], wave64: bool, threads: u32, form: Form, only: Option<bool>| {
        let was = compute_wave::TEST_ONLY.replace(only);
        let r = translated(words, wave64, threads, false, form);
        compute_wave::TEST_ONLY.set(was);
        r.unwrap_or_else(|e| panic!("{form:?}: {e}"))
    };
    let declared = |words: &[u32], wave64: bool, threads: u32, form: Form| made(words, wave64, threads, form, None).1;
    let pin = Some(false);
    // console wave32 programs
    assert_eq!(declared(BRANCH32, false, 64, Form::Console), Some(32));
    assert_eq!(declared(PLAIN, false, 64, Form::Console), Some(32));
    assert_eq!(declared(FIRST32, false, 64, Form::Console), Some(32));
    assert_eq!(declared(BARRIER32, false, 64, Form::Console), None);
    for words in [BRANCH32, PLAIN, FIRST32, BARRIER32, BALLOT32] {
        assert!(
            made(words, false, 64, Form::Console, None).0.dxil == made(words, false, 64, Form::Waves32, None).0.dxil,
            "a console wave32 program is what it is with =32"
        );
    }
    // console wave64 programs with wave operations: 64, and the code of a host of 64-lane waves
    for words in [BALLOT64, FIRST64, SUM64, MBCNT64, LDS64] {
        let threads = if words == LDS64 { 64 } else { 128 };
        let (console, wave) = made(words, true, threads, Form::Console, None);
        assert_eq!(wave, Some(64));
        assert_eq!(declared(words, true, threads, Form::Host64), Some(64));
        assert!(console.dxil == made(words, true, threads, Form::Host64, None).0.dxil, "the code for a host of 64-lane waves");
        assert!(console.dxil == made(words, true, threads, Form::Waves32, pin).0.dxil, "and of =pin");
        assert!(console.dxil != made(words, true, threads, Form::Waves32, None).0.dxil, "not a form for waves of 32");
    }
    // ... without any: 64 too, where `=pin` and a host of 64-lane waves leave the size to
    // the driver (in a group of several console waves; these programs have LDS, and with
    // LDS a group of one console wave declared it before)
    for (words, threads) in [(BRANCH64, 128), (PLAIN, 128), (PLAIN, 96), (BARRIER64, 128)] {
        assert_eq!(declared(words, true, threads, Form::Console), Some(64), "{threads}");
        assert_eq!(declared(words, true, threads, Form::Host64), None, "{threads}");
        assert_eq!(made(words, true, threads, Form::Waves32, pin).1, None, "{threads}");
    }
    for threads in [64, 32, 8] {
        assert_eq!(declared(PLAIN, true, threads, Form::Console), Some(64), "{threads}");
    }
    // `=forms` is what it was (a test's choice of the switch unset is made with the form)
    assert_eq!(made(BALLOT64, true, 128, Form::Waves32, Some(true)).1, Some(32));
}
