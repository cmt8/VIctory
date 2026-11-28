//! Compute programs in the console's waves on a host that runs waves of 32
//! and of 64 (`VICTORY_COMPUTE_WAVE`; AMD under vkd3d-proton and RADV).
//!
//! What such a host does with a compute program depends on whether it
//! declares a wave size (vkd3d-proton 3.2.0 as Proton Experimental ships it,
//! its dxil-spirv, Mesa 26.3.1):
//! * A declared `WaveSize(n)` is dxil-spirv's minimum wave size
//!   (`emit_execution_modes_thread_wave_properties`, dxil_converter.cpp) and
//!   becomes vkd3d-proton's `requiredSubgroupSize` n wherever the device's
//!   range is wider than the program's (`vkd3d_setup_shader_stage`, state.c),
//!   which RADV honours for compute on GFX10 and later
//!   (`requiredSubgroupSizeStages`, radv_physical_device.c;
//!   `radv_shader_stage_key`, radv_pipeline.c; `vk_set_subgroup_size`,
//!   vk_pipeline.c). That is 32 lanes or 64 for certain.
//! * With no size declared nothing is certain. vkd3d-proton then requires
//!   **32** lanes of the driver for a program whose code suggests it to
//!   dxil-spirv (`suggest_maximum_wave_size`: a lane read at a computed lane,
//!   a ballot of which only the low word is read, wave operations in a group
//!   of at most 32 threads; `promote_wave_size_heuristics`, dxil.c), and
//!   otherwise leaves the choice to RADV, which runs a compute program in
//!   waves of 64 unless its group has at most 32 threads
//!   (`radv_shader_choose_subgroup_size`, radv_shader.c;
//!   `RADV_PERFTEST=cswave32` makes that 32 too, except for programs with
//!   subgroup operations whose group is a multiple of 64).
//!
//! So with the switch on every compute program declares the wave it is
//! written for (`pin_wave`), and which that is depends on the switch's value:
//! * Unset (or `=console`): the console's own. A console wave32 program
//!   declares 32, wave operations or not (one without any whose group
//!   barriers the host leaves out is left to the host). A console wave64
//!   program is translated as for a host of 64-lane waves and declares 64,
//!   wave operations or not ([`wave64_as_console`]): a host wave is a console
//!   wave, and none of the forms below is used. The forms are for hosts that
//!   run no wave of 64 (NVIDIA, Intel), which get them whatever this switch
//!   says; on the RX 7900 XTX they gave rectangles of 128 x 16 pixels (one
//!   group of 64 threads) darker than their surroundings in lit areas, which
//!   `=pin` did not.
//! * `=32`: every compute program is translated as for a host whose waves
//!   have at most 32 lanes (the forms NVIDIA gets, where a console wave64
//!   program is two host waves exchanging what crosses halves) and declares
//!   32 where a form takes it exactly (with a first-lane read of a value that
//!   differs between lanes counted as crossing halves,
//!   `wave64_halves::first_lane_crosses`); a console wave64 program that the
//!   32-lane forms would refuse or take only approximately (each half-wave on
//!   its own) is translated as before instead ([`super::emit_sized`]), in
//!   waves of 64 where it declares a size. What the switch did unset until
//!   the forms' rectangles were seen. In a group of several console waves
//!   with a barrier of the program's own, a form's barriers stand only where
//!   every thread of the group passes once ([`super::even_barriers`],
//!   `VICTORY_WAVE_EVEN_BARRIERS`, `=0`: wherever every lane runs, as
//!   before): elsewhere they set the group's console waves out of step at
//!   the program's barriers, and such a program keeps its waves of 64.
//! * `=pin`: console wave32 programs declare 32; console wave64 programs are
//!   translated as before the switch (64 declared only by those that use the
//!   host's wave operations, `VICTORY_WAVE_PIN`). `=forms`: console wave64
//!   programs as with `=32`; console wave32 programs as before the switch.
//! * `=0`, `=host`: off, everything as before the switch.
//!
//! To find which of these a fault in a run comes from:
//! * `VICTORY_WAVE_KEEP=<hash>[,<hash>...]` translates the compute programs
//!   the log names by those hashes as before ([`named`]), the others as the
//!   switch says;
//! * `VICTORY_WAVE_WHY=1` writes a line `WAVE-FORM <hash> ...` to stderr for
//!   every compute program translated, with what it was taken as, beside the
//!   lines of the forms tried (a cached translation is not made again: a
//!   run with a new value of the switch translates them all).

use std::cell::{Cell, RefCell};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

/// The narrowest wave the host GPU runs (0: not known); see
/// [`set_host_min_wave`].
static HOST_MIN_WAVE: AtomicU32 = AtomicU32::new(0);

/// The host GPU's `WaveLaneCountMin` (with `set_host_max_wave`'s maximum it
/// says whether the host takes both 32 and 64: [`compute_wave_32`]).
pub fn set_host_min_wave(lanes: u32) {
    HOST_MIN_WAVE.store(lanes, Ordering::Relaxed);
}

/// What [`set_host_min_wave`] set (0: not known).
pub fn host_min_wave_set() -> u32 {
    HOST_MIN_WAVE.load(Ordering::Relaxed)
}

/// What `VICTORY_COMPUTE_WAVE` asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Asked {
    /// `=0`, `=host`, or any value not known: compute programs are
    /// translated for the host's widest wave as before.
    Off,
    /// Unset, or `=console`: console wave32 programs declare 32, console
    /// wave64 programs are translated for host waves of 64 and declare 64.
    Console,
    /// `=32`: every compute program in host waves of 32.
    All,
    /// `=pin`: console wave32 programs only (they declare 32); console
    /// wave64 programs are translated as before.
    Pin,
    /// `=forms`: console wave64 programs only (the 32-lane forms where they
    /// are exact); console wave32 programs are translated as before.
    Forms,
}

/// [`Asked`] of the variable's value. Pure, for testing.
fn parse(value: Option<&str>) -> Asked {
    match value.map(str::trim) {
        None | Some("" | "console") => Asked::Console,
        Some("32") => Asked::All,
        Some("pin") => Asked::Pin,
        Some("forms") => Asked::Forms,
        Some(_) => Asked::Off,
    }
}

fn asked() -> Asked {
    static ASKED: OnceLock<Asked> = OnceLock::new();
    *ASKED.get_or_init(|| parse(std::env::var("VICTORY_COMPUTE_WAVE").ok().as_deref()))
}

thread_local! {
    /// Set while a program is translated as before ([`without`]).
    static SUSPENDED: Cell<bool> = const { Cell::new(false) };
    /// Set while a console wave64 program is translated for host waves of 64
    /// that it declares ([`in_console_waves`]).
    static DECLARES_64: Cell<bool> = const { Cell::new(false) };
    /// The name of the program being translated ([`named`]; empty: none).
    static NAME: RefCell<String> = const { RefCell::new(String::new()) };
    /// A test's choice for [`compute_wave_32`] on its thread (`None`: the
    /// switch and the host's).
    #[cfg(test)]
    pub(super) static TEST: Cell<Option<bool>> = const { Cell::new(None) };
    /// A test's choice for [`takes`] on its thread: only console wave64
    /// programs (`Some(true)`, `=forms`) or only console wave32 ones
    /// (`Some(false)`, `=pin`); `None`: the switch's.
    #[cfg(test)]
    pub(super) static TEST_ONLY: Cell<Option<bool>> = const { Cell::new(None) };
    /// A test's choice on its thread, before [`TEST_ONLY`]: the switch unset
    /// (console wave64 programs in host waves of 64, [`wave64_as_console`]).
    #[cfg(test)]
    pub(super) static TEST_CONSOLE: Cell<bool> = const { Cell::new(false) };
}

/// Whether compute programs declare the wave they are written for, and are
/// translated for host waves of 32 where the switch's value says so: the
/// switch is on and the host runs waves of 32 and of 64 (`WaveLaneCountMin`
/// at most 32, `WaveLaneCountMax` at least 64). A host whose waves are at
/// most 32 lanes (NVIDIA) already gets the forms for such waves and keeps
/// what it had; so does one whose waves are not known.
pub fn compute_wave_32() -> bool {
    if SUSPENDED.get() {
        return false;
    }
    #[cfg(test)]
    if let Some(on) = TEST.get() {
        return on;
    }
    let (min, max) = (host_min_wave_set(), super::lower::host_max_wave());
    asked() != Asked::Off && min != 0 && min <= 32 && max >= 64
}

/// With [`compute_wave_32`]: whether a compute program compiled for the
/// console's waves of 64 (`wave64`) or of 32 is among those the switch asks
/// for (`VICTORY_COMPUTE_WAVE=pin` and `=forms` leave one kind as before).
pub(super) fn takes(wave64: bool) -> bool {
    #[cfg(test)]
    {
        if TEST_CONSOLE.get() {
            return !wave64;
        }
        if let Some(only) = TEST_ONLY.get() {
            return only == wave64;
        }
        if TEST.get().is_some() {
            return true;
        }
    }
    match asked() {
        Asked::All => true,
        Asked::Console | Asked::Pin => !wave64,
        Asked::Forms => wave64,
        Asked::Off => false,
    }
}

/// With [`compute_wave_32`]: whether a console wave64 compute program is
/// translated for host waves of 64 and declares them whatever it uses (the
/// switch unset or `=console`), so that the host runs it in the console's
/// waves for certain and not by the driver's default. Part of the
/// translation: whoever caches translations keys such a program apart
/// ([`keyed_apart`]).
pub fn wave64_as_console() -> bool {
    if !compute_wave_32() {
        return false;
    }
    #[cfg(test)]
    {
        if TEST_CONSOLE.get() {
            return true;
        }
        if TEST.get().is_some() {
            return false;
        }
    }
    asked() == Asked::Console
}

/// Runs `f`, the translation of a console wave64 compute program, as for a
/// host of 64-lane waves ([`without`]) with the 64 lanes declared whether or
/// not the program uses the host's wave operations ([`declares_64`]).
pub(super) fn in_console_waves<R>(f: impl FnOnce() -> R) -> R {
    let was = DECLARES_64.replace(true);
    let r = without(f);
    DECLARES_64.set(was);
    r
}

/// Whether the console wave64 program being translated declares 64 lanes
/// whatever it uses ([`in_console_waves`]).
pub(super) fn declares_64() -> bool {
    DECLARES_64.get()
}

/// `container`'s program where it is a compute program compiled for the
/// console's waves of 64 (the program [`super::translate`] takes).
fn console_wave64_compute(container: &[u8]) -> Option<crate::native::NativeProgram> {
    console_compute(container).filter(|p| p.wave64)
}

/// `container`'s program where it is a compute program (the program
/// [`super::translate`] takes).
fn console_compute(container: &[u8]) -> Option<crate::native::NativeProgram> {
    use crate::native::HwStage;
    let c = crate::dxbc::Container::parse(container).ok()?;
    let part = c.part(crate::dxbc::FourCC::DXIL).or(c.part(crate::dxbc::FourCC::ACCC))?;
    crate::native_programs(part.data)
        .ok()?
        .into_iter()
        .find(|p| matches!(p.stage, HwStage::Vs | HwStage::Ps | HwStage::Cs | HwStage::Ms))
        .filter(|p| p.stage == HwStage::Cs)
}

/// Whether some dword of `code` is a `ds_bpermute_b32` (the DS encoding,
/// opcode 0xb3) if it is an instruction's first: every program with the
/// instruction, and perhaps one whose literal looks like it.
fn has_bpermute(code: &[u8]) -> bool {
    code.chunks_exact(4).any(|w| {
        let w = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
        w >> 26 == 0x36 && w >> 18 & 0xFF == 0xB3
    })
}

/// `container`'s program, of whichever stage (the program
/// [`super::translate`] takes).
fn console_program(container: &[u8]) -> Option<crate::native::NativeProgram> {
    use crate::native::HwStage;
    let c = crate::dxbc::Container::parse(container).ok()?;
    let part = c.part(crate::dxbc::FourCC::DXIL).or(c.part(crate::dxbc::FourCC::ACCC))?;
    crate::native_programs(part.data)
        .ok()?
        .into_iter()
        .find(|p| matches!(p.stage, HwStage::Vs | HwStage::Ps | HwStage::Cs | HwStage::Ms))
}

/// Whether some dword of `code` is an `image_bvh_intersect_ray` or an
/// `image_bvh64_intersect_ray` (the MIMG encoding, opcodes 0xe6 and 0xe7:
/// seven bits at 24:18 and the eighth at bit 0) if it is an instruction's
/// first: every program with a node test, and perhaps one whose literal
/// looks like it.
fn has_bvh(code: &[u8]) -> bool {
    code.chunks_exact(4).any(|w| {
        let w = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
        w >> 26 == 0x3C && matches!((w >> 18) & 0x7F | (w & 1) << 7, 0xE6 | 0xE7)
    })
}

/// Whether `p` is a program [`super::bvh_miss`] changes: the switch is on and
/// the code has a node test. Any other program keeps its key, and its cached
/// translation, with the switch on as off.
fn bvh_marked(p: &crate::native::NativeProgram) -> bool {
    super::bvh_miss() && has_bvh(&p.code)
}

/// Whether some dword of `code` is an `s_barrier` if it is an instruction's
/// first: every program with the instruction, and perhaps one whose literal
/// looks like it.
fn has_barrier(code: &[u8]) -> bool {
    code.chunks_exact(4).any(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]) >> 16 == 0xBF8A)
}

/// Whether console wave64 compute programs take the forms for host waves of
/// 32 where those are exact (`VICTORY_COMPUTE_WAVE=32` or `=forms` on a
/// host of waves of 32 and 64).
fn wave64_in_forms() -> bool {
    compute_wave_32() && !wave64_as_console() && takes(true)
}

/// [`keyed_apart`]'s mark of a console wave64 compute program translated
/// for host waves of 64 that it declares ([`wave64_as_console`]).
const WAVE64_AS_CONSOLE: &str = "VICTORY_COMPUTE_WAVE=console: wave64 compute programs declare 64";
/// [`keyed_apart`]'s mark of a console wave64 compute program whose
/// backward permutes read within halves of 32 lanes
/// ([`super::bpermute_halves`]).
const BPERMUTE_HALVES: &str = "VICTORY_BPERMUTE_HALVES: ds_bpermute within halves of 32 lanes";
/// [`keyed_apart`]'s mark of a console wave64 compute program whose form for
/// host waves of 32 has its barriers only where every thread of the group
/// passes once ([`super::even_barriers`]).
const EVEN_BARRIERS: &str = "VICTORY_WAVE_EVEN_BARRIERS: a form's barriers where every thread passes once";
/// [`keyed_apart`]'s mark of a compute program with an `s_barrier` whose typed
/// unordered buffer views that it only reads are shown to the host as written
/// too ([`super::typed_rw`]).
const TYPED_RW: &str = "VICTORY_TYPED_RW: typed unordered buffer views read behind a barrier are not read-only";
/// [`keyed_apart`]'s mark of a compute program with an `s_barrier` and no LDS
/// declared, which gets a dword of group-shared memory it does not need
/// ([`super::group_cu`]).
pub const GROUP_CU: &str = "VICTORY_GROUP_CU: a group with a barrier and no LDS adds to a dword of it";
/// [`keyed_apart`]'s mark of a program with a ray-tracing node test whose
/// tests all miss ([`super::bvh_miss`]).
const BVH_MISS: &str = "VICTORY_BVH_MISS=1: every ray-tracing node test a miss";
/// [`keyed_apart`]'s mark of every program with a ray-tracing node test:
/// its instance and procedural nodes give all ones (no child), as the
/// console's instruction does, where they gave a triangle miss before.
pub(super) const BVH_OTHER_NONE: &str = "node test: instance and procedural nodes return no child";
/// [`keyed_apart`]'s mark of a program with a ray-tracing node test that
/// counts what its walks do ([`super::bvh_count`]). (The translator's version
/// does not key translations: a change of what is counted changes the mark,
/// or the cache serves the counting translations made before it.)
const BVH_COUNT: &str = "VICTORY_BVH_COUNT=1: ray-tracing node tests counted into u252, with the cycle check";

/// [`keyed_apart`]'s mark of a compute program with a ray-tracing node test
/// whose loops are left at [`super::bvh_loop_limit`]'s count, with the count.
fn bvh_loop_mark() -> Option<&'static str> {
    static MARK: OnceLock<Option<&'static str>> = OnceLock::new();
    #[cfg(test)]
    if let Some(n) = super::BVH_LOOP_LIMIT_TEST.get() {
        return Some(Box::leak(format!("VICTORY_BVH_LOOP_LIMIT={n}: loops of programs with a node test").into_boxed_str()));
    }
    *MARK.get_or_init(|| {
        super::bvh_loop_limit()
            .map(|n| &*Box::leak(format!("VICTORY_BVH_LOOP_LIMIT={n}: loops of programs with a node test").into_boxed_str()))
    })
}

/// Whether `code` (a compute program's) has a node test: its loops take
/// [`super::bvh_loop_limit`].
pub(super) fn walks(code: &[u8]) -> bool {
    has_bvh(code)
}

/// Diagnostics: the run of `SLOT_COUNTS` the program of `container` counts
/// into ([`super::bvh_count`]), for a compute program with a node test.
pub fn bvh_count_slot(container: &[u8]) -> Option<u32> {
    let p = console_program(container)?;
    (p.stage == crate::native::HwStage::Cs && has_bvh(&p.code)).then(|| super::bvh_count_slot_of(&p.code))
}

/// Whether `p` is a compute program with an `s_barrier` that declares no LDS:
/// the programs [`super::group_cu`] reaches, as far as the program's header
/// and code tell without lowering it. (Lowering also asks for a store to a
/// UAV, which every program of the title with a barrier has, and for no LDS
/// of a form's.)
fn barrier_without_lds(p: &crate::native::NativeProgram) -> bool {
    matches!(&p.io, crate::native::StageIo::Cs(cs) if cs.lds_bytes() == 0) && has_barrier(&p.code)
}

/// What of the translation of `container` whoever caches translations keys
/// apart, beside what it keys every program by: a mark for each change of
/// the translator's that reaches this program alone and has no value of a
/// switch to tell it by. The other programs keep their keys, and their
/// cached translations.
/// * A console wave64 compute program where [`wave64_as_console`] holds: the
///   switch unset used to mean a form for host waves of 32 for it. Not one
///   `VICTORY_WAVE_KEEP` lists (`kept`), which is translated as before the
///   switch in both cases.
/// * A console wave64 compute program with a `ds_bpermute_b32` where
///   [`super::bpermute_halves`] holds: the permute used to read within the
///   whole wave (as it does with `VICTORY_BPERMUTE_HALVES=0`, whose
///   translations so keep the key they had).
/// * A compute program with an `s_barrier` where [`super::typed_rw`] holds
///   (`VICTORY_TYPED_RW=1`): its typed unordered buffer views used to be
///   read-only to the host where it only read through them.
/// * A compute program with an `s_barrier` that declares no LDS, where
///   [`super::group_cu`] holds: it used to have no LDS on the host either
///   (as with `VICTORY_GROUP_CU=0`, whose translations so keep the key they
///   had).
/// * A console wave64 compute program with an `s_barrier` in a group over
///   64 threads, where such programs take the forms for host waves of 32
///   (`VICTORY_COMPUTE_WAVE=32` or `=forms`) and [`super::even_barriers`]
///   holds: the form's barriers used to stand wherever every lane runs (as
///   they do with `VICTORY_WAVE_EVEN_BARRIERS=0`). Not one
///   `VICTORY_WAVE_KEEP` lists.
pub fn keyed_apart(container: &[u8], kept: bool) -> Vec<&'static str> {
    let console = !kept && wave64_as_console();
    let halves = super::bpermute_halves();
    let even = !kept && super::even_barriers() && wave64_in_forms();
    let mut marks = Vec::new();
    // (Every compute program with a barrier, of either console wave: more than the ones
    // with a typed read the switch reaches, all the code tells without lowering it.)
    if super::typed_rw() && console_compute(container).is_some_and(|p| has_barrier(&p.code)) {
        marks.push(TYPED_RW);
    }
    // (Only those of them that declare no LDS: the others are translated as without the
    // switch, and keep the key and the cached translation they had.)
    if super::group_cu() && console_compute(container).is_some_and(|p| barrier_without_lds(&p)) {
        marks.push(GROUP_CU);
    }
    // (A program of any stage, `VICTORY_WAVE_KEEP` listed or not: the node test is
    // lowered the same in all.)
    if console_program(container).is_some_and(|p| bvh_marked(&p)) {
        marks.push(BVH_MISS);
    }
    if console_program(container).is_some_and(|p| has_bvh(&p.code)) {
        marks.push(BVH_OTHER_NONE);
    }
    if super::bvh_count() && bvh_count_slot(container).is_some() {
        marks.push(BVH_COUNT);
    }
    if let Some(mark) = bvh_loop_mark()
        && bvh_count_slot(container).is_some()
    {
        marks.push(mark);
    }
    if !console && !halves && !even {
        return marks;
    }
    let Some(p) = console_wave64_compute(container) else {
        return marks;
    };
    if console {
        marks.push(WAVE64_AS_CONSOLE);
    }
    if halves && has_bpermute(&p.code) {
        marks.push(BPERMUTE_HALVES);
    }
    let group = match &p.io {
        crate::native::StageIo::Cs(cs) => cs.threads.iter().product::<u32>(),
        _ => 0,
    };
    if even && group > 64 && has_barrier(&p.code) {
        marks.push(EVEN_BARRIERS);
    }
    marks
}

/// The marks of [`keyed_apart`] some program has with the switches and the
/// host as they are: part of what tells one set of translations from
/// another.
pub fn keyed_apart_any() -> Vec<&'static str> {
    let mut marks = Vec::new();
    if wave64_as_console() {
        marks.push(WAVE64_AS_CONSOLE);
    }
    if super::bpermute_halves() {
        marks.push(BPERMUTE_HALVES);
    }
    if super::even_barriers() && wave64_in_forms() {
        marks.push(EVEN_BARRIERS);
    }
    if super::typed_rw() {
        marks.push(TYPED_RW);
    }
    if super::group_cu() {
        marks.push(GROUP_CU);
    }
    if super::bvh_miss() {
        marks.push(BVH_MISS);
    }
    marks.push(BVH_OTHER_NONE);
    if super::bvh_count() {
        marks.push(BVH_COUNT);
    }
    if let Some(mark) = bvh_loop_mark() {
        marks.push(mark);
    }
    marks
}

/// The widest host wave a compute program's lane forms are chosen for: 32
/// with [`compute_wave_32`], else the host's widest (0: not known).
pub(super) fn lanes_wave() -> u32 {
    if compute_wave_32() { 32 } else { super::lower::host_max_wave() }
}

/// Runs `f` with [`compute_wave_32`] off on this thread: a program
/// translated as before.
pub(super) fn without<R>(f: impl FnOnce() -> R) -> R {
    let was = SUSPENDED.replace(true);
    let r = f();
    SUSPENDED.set(was);
    r
}

/// `VICTORY_WAVE_KEEP=<hash>[,<hash>...]`: whether the program the log names
/// `name` (the `hash=` of its "shader translated" and "first bind" lines) is
/// one to translate as before, whatever `VICTORY_COMPUTE_WAVE` says. Whoever
/// caches translations keys such a program apart: [`named`] translates it
/// differently from a run without the variable.
pub fn wave_kept(name: &str) -> bool {
    static KEPT: OnceLock<Vec<String>> = OnceLock::new();
    let kept = KEPT.get_or_init(|| kept_names(std::env::var("VICTORY_WAVE_KEEP").ok().as_deref()));
    kept.iter().any(|k| k.eq_ignore_ascii_case(name))
}

/// The names a value of `VICTORY_WAVE_KEEP` lists. Pure, for testing.
fn kept_names(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split([',', ' ', ';'])
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether `VICTORY_WAVE_WHY` asks for the lines on stderr.
pub(super) fn why() -> bool {
    std::env::var_os("VICTORY_WAVE_WHY").is_some()
}

/// Runs `f`, the translation of the program the log names `name`: the
/// `VICTORY_WAVE_WHY` lines of this thread carry the name meanwhile, and a
/// program [`wave_kept`] lists is translated as before.
pub fn named<R>(name: &str, f: impl FnOnce() -> R) -> R {
    let was = NAME.replace(name.to_owned());
    let r = if wave_kept(name) {
        if why() {
            eprintln!("WAVE-FORM {name}: as before (VICTORY_WAVE_KEEP)");
        }
        without(f)
    } else {
        f()
    };
    NAME.set(was);
    r
}

/// The name of the program being translated ([`named`]), `-` without one.
pub(super) fn name() -> String {
    NAME.with_borrow(|n| if n.is_empty() { "-".to_owned() } else { n.clone() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_names_all_programs_one_kind_or_none() {
        assert_eq!(parse(None), Asked::Console);
        assert_eq!(parse(Some("")), Asked::Console);
        assert_eq!(parse(Some("console")), Asked::Console);
        assert_eq!(parse(Some(" 32 ")), Asked::All);
        assert_eq!(parse(Some("pin")), Asked::Pin);
        assert_eq!(parse(Some("forms")), Asked::Forms);
        assert_eq!(parse(Some("0")), Asked::Off);
        assert_eq!(parse(Some("host")), Asked::Off);
        assert_eq!(parse(Some("64")), Asked::Off);
    }

    /// `VICTORY_TYPED_RW` is off unless it is `1`; on, it is one of the marks
    /// some program is keyed apart by, and no mark of what is no compute
    /// program with a barrier. Off, no key changes.
    #[test]
    fn typed_views_made_read_write_key_their_programs_apart_only_when_asked() {
        assert!(!crate::emit::typed_rw_said(None), "off with the switch not set");
        assert!(!crate::emit::typed_rw_said(Some("")));
        assert!(!crate::emit::typed_rw_said(Some("0")));
        assert!(crate::emit::typed_rw_said(Some("1")));
        assert!(crate::emit::typed_rw_said(Some(" 1 ")));
        let was = crate::emit::TYPED_RW_TEST.replace(Some(false));
        let off = keyed_apart_any();
        assert!(!off.contains(&TYPED_RW));
        crate::emit::TYPED_RW_TEST.set(Some(true));
        let on = keyed_apart_any();
        let no_program = keyed_apart(&[], false);
        crate::emit::TYPED_RW_TEST.set(was);
        assert!(on.contains(&TYPED_RW));
        assert_eq!(on.iter().filter(|m| **m != TYPED_RW).collect::<Vec<_>>(), off.iter().collect::<Vec<_>>());
        assert!(!no_program.contains(&TYPED_RW));
    }

    /// `VICTORY_GROUP_CU` is on unless it is `0`; on, it is one of the marks
    /// some program is keyed apart by, and no mark of what is no compute
    /// program with a barrier. Off, no key changes.
    #[test]
    fn a_group_in_one_unit_keys_its_programs_apart_unless_turned_off() {
        assert!(crate::emit::group_cu_said(None), "on with the switch not set");
        assert!(crate::emit::group_cu_said(Some("")));
        assert!(!crate::emit::group_cu_said(Some("0")));
        assert!(!crate::emit::group_cu_said(Some(" 0 ")));
        assert!(crate::emit::group_cu_said(Some("1")));
        assert!(crate::emit::group_cu_said(Some(" 1 ")));
        let was = crate::emit::GROUP_CU_TEST.replace(Some(false));
        let off = keyed_apart_any();
        assert!(!off.contains(&GROUP_CU));
        crate::emit::GROUP_CU_TEST.set(Some(true));
        let on = keyed_apart_any();
        let no_program = keyed_apart(&[], false);
        crate::emit::GROUP_CU_TEST.set(was);
        assert!(on.contains(&GROUP_CU));
        assert_eq!(on.iter().filter(|m| **m != GROUP_CU).collect::<Vec<_>>(), off.iter().collect::<Vec<_>>());
        assert!(!no_program.contains(&GROUP_CU));
    }

    /// What `VICTORY_GROUP_CU` keys apart: a compute program with a barrier
    /// that declares no LDS. One with LDS, one without a barrier and one of
    /// another stage keep their keys, and so their cached translations.
    #[test]
    fn only_a_group_with_a_barrier_and_no_lds_is_keyed_apart_for_its_dword() {
        use crate::native::{ComputeIo, HwStage, NativeProgram, StageIo};
        let bytes = |words: &[u32]| words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
        let program = |stage: HwStage, lds_granules: u32, barrier: bool| NativeProgram {
            stage,
            wave64: true,
            vgprs: 256,
            layout: vec![],
            used: vec![],
            io: StageIo::Cs(ComputeIo {
                rsrc2: lds_granules << 15,
                threads: [1024, 1, 1],
            }),
            // s_waitcnt, s_barrier or s_nop, s_endpgm
            code: bytes(&[0xBF8C_C07F, if barrier { 0xBF8A_0000 } else { 0xBF80_0000 }, 0xBF81_0000]),
        };
        assert!(barrier_without_lds(&program(HwStage::Cs, 0, true)));
        assert!(!barrier_without_lds(&program(HwStage::Cs, 1, true)), "LDS of its own");
        assert!(!barrier_without_lds(&program(HwStage::Cs, 0, false)), "no barrier");
        let mut vertex = program(HwStage::Vs, 0, true);
        vertex.io = StageIo::None;
        assert!(!barrier_without_lds(&vertex), "no compute program");
    }

    /// `VICTORY_BVH_MISS=1` keys apart only a program with a ray-tracing
    /// node test, of any stage: one without keeps its key with the switch
    /// on. Off, no program is keyed apart and no key changes.
    #[test]
    fn a_node_test_that_misses_keys_only_its_programs_apart() {
        use crate::native::{ComputeIo, HwStage, NativeProgram, StageIo};
        let bytes = |words: &[u32]| words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
        let program = |stage: HwStage, code: &[u32]| NativeProgram {
            stage,
            wave64: false,
            vgprs: 22,
            layout: vec![],
            used: vec![],
            io: StageIo::Cs(ComputeIo { rsrc2: 0, threads: [32, 1, 1] }),
            code: bytes(code),
        };
        // image_bvh64_intersect_ray v[13:16], [v1 .. v12], s[8:11]; s_endpgm
        let wide = [0xF19C_9F07, 0x0002_0D01, 0x0504_0302, 0x0908_0706, 0x000C_0B0A, 0xBF81_0000];
        // image_bvh_intersect_ray v[13:16], [v1, v3 .. v12], s[8:11]; s_endpgm
        let narrow = [0xF198_9F07, 0x0002_0D01, 0x0605_0403, 0x0A09_0807, 0x0000_0C0B, 0xBF81_0000];
        // image_sample v[0:3], v[4:5], s[8:15], s[16:19] dmask:0xf dim:2D; s_endpgm
        let sample = [0xF080_0F08, 0x0082_0004, 0xBF81_0000];
        // s_waitcnt, s_barrier, s_endpgm
        let plain = [0xBF8C_C07F, 0xBF8A_0000, 0xBF81_0000];
        assert!(has_bvh(&bytes(&wide)) && has_bvh(&bytes(&narrow)));
        assert!(!has_bvh(&bytes(&sample)) && !has_bvh(&bytes(&plain)) && !has_bvh(&[]));

        let was = crate::emit::BVH_MISS_TEST.replace(Some(false));
        let off_any = keyed_apart_any();
        let off = [&wide[..], &narrow, &sample, &plain].map(|c| bvh_marked(&program(HwStage::Cs, c)));
        crate::emit::BVH_MISS_TEST.set(Some(true));
        let on_any = keyed_apart_any();
        let on = [&wide[..], &narrow, &sample, &plain].map(|c| bvh_marked(&program(HwStage::Cs, c)));
        let pixel = bvh_marked(&program(HwStage::Ps, &wide));
        let no_program = keyed_apart(&[], false);
        crate::emit::BVH_MISS_TEST.set(was);

        assert_eq!(off, [false; 4], "off: no program is keyed apart");
        assert_eq!(on, [true, true, false, false], "on: only the programs with a node test");
        assert!(pixel, "of any stage");
        assert!(!no_program.contains(&BVH_MISS));
        assert!(!off_any.contains(&BVH_MISS));
        assert!(on_any.contains(&BVH_MISS));
        assert_eq!(on_any.iter().filter(|m| **m != BVH_MISS).collect::<Vec<_>>(), off_any.iter().collect::<Vec<_>>());
    }

    /// The node kinds' change keys apart every set of translations, and no
    /// container that holds no program.
    #[test]
    fn the_node_kinds_change_is_keyed_apart() {
        assert!(keyed_apart_any().contains(&BVH_OTHER_NONE));
        assert!(!keyed_apart(&[], false).contains(&BVH_OTHER_NONE));
    }

    #[test]
    fn a_group_barrier_is_found_in_a_programs_code() {
        let bytes = |words: &[u32]| words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
        // s_waitcnt, s_barrier, s_endpgm
        assert!(has_barrier(&bytes(&[0xBF8C_C07F, 0xBF8A_0000, 0xBF81_0000])));
        // s_waitcnt, s_cbranch_execz, s_endpgm
        assert!(!has_barrier(&bytes(&[0xBF8C_C07F, 0xBF88_0001, 0xBF81_0000])));
        assert!(!has_barrier(&[]));
    }

    #[test]
    fn a_backward_permute_is_found_in_a_programs_code() {
        let code = |words: &[u32]| -> Vec<u8> { words.iter().flat_map(|w| w.to_le_bytes()).collect() };
        // v_mov_b32 v2, 11; ds_bpermute_b32 v1, v0, v2; s_endpgm
        assert!(has_bpermute(&code(&[0x7E04_028B, 0xDACC_0000, 0x0100_0200, 0xBF81_0000])));
        // ... with an offset
        assert!(has_bpermute(&code(&[0xDACC_000C, 0x0300_0201])));
        // ds_write_b32, ds_read_b32, ds_swizzle_b32
        assert!(!has_bpermute(&code(&[0xD834_0000, 0x0000_0204, 0xD8D8_0000, 0x0300_0005, 0xD8D4_8000, 0x0100_0000])));
        assert!(!has_bpermute(&[]));
    }

    #[test]
    fn the_kept_programs_are_listed_by_name() {
        assert!(kept_names(None).is_empty());
        assert!(kept_names(Some(" ")).is_empty());
        assert_eq!(kept_names(Some("2cc4c197ee29e291, 0123456789abcdef;aa")), [
            "2cc4c197ee29e291",
            "0123456789abcdef",
            "aa"
        ]);
    }

    #[test]
    fn a_translation_carries_its_programs_name() {
        assert_eq!(name(), "-");
        let seen = named("2cc4c197ee29e291", || (name(), named("aa", name), name()));
        assert_eq!(seen, ("2cc4c197ee29e291".to_owned(), "aa".to_owned(), "2cc4c197ee29e291".to_owned()));
        assert_eq!(name(), "-");
    }
}
