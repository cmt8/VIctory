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
//!   the forms' rectangles were seen.
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
/// ([`wave64_keyed_apart`]).
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

/// Whether `container`'s program is a compute program compiled for the
/// console's waves of 64 (the program [`super::translate`] takes).
fn console_wave64_compute(container: &[u8]) -> bool {
    use crate::native::HwStage;
    let Ok(c) = crate::dxbc::Container::parse(container) else {
        return false;
    };
    let Some(part) = c.part(crate::dxbc::FourCC::DXIL).or(c.part(crate::dxbc::FourCC::ACCC)) else {
        return false;
    };
    crate::native_programs(part.data)
        .ok()
        .and_then(|all| all.into_iter().find(|p| matches!(p.stage, HwStage::Vs | HwStage::Ps | HwStage::Cs | HwStage::Ms)))
        .is_some_and(|p| p.stage == HwStage::Cs && p.wave64)
}

/// Whether the translation of `container` is one [`wave64_as_console`]
/// changes: a console wave64 compute program, on a host and with a switch
/// where that holds. Whoever caches translations keys such a program apart
/// from the translation made of it while the switch unset meant the forms
/// for host waves of 32 (the switch's value is the same for both); every
/// other program keeps its key.
pub fn wave64_keyed_apart(container: &[u8]) -> bool {
    wave64_as_console() && console_wave64_compute(container)
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
