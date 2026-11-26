//! Compute programs in host waves of 32 on a host that runs waves of 32 and
//! of 64 (`VICTORY_COMPUTE_WAVE`; AMD under vkd3d-proton and RADV).
//!
//! RADV runs a compute program in waves of 64 unless it declares a size or
//! its group has at most 32 threads (`radv_shader_choose_subgroup_size`,
//! radv_shader.c; `RADV_PERFTEST=cswave32` makes the default 32 except for
//! programs with subgroup operations whose group is a multiple of 64). A
//! declared `WaveSize(32)` becomes vkd3d-proton's `requiredSubgroupSize` 32
//! (`vkd3d_setup_shader_stage`, state.c: the declared range is narrower
//! than the device's 32..64), which RADV honours for compute on GFX10 and
//! later (`requiredSubgroupSizeStages`, radv_physical_device.c;
//! `vk_set_subgroup_size`, vk_pipeline.c).
//!
//! With the switch on, every compute program is translated as for a host
//! whose waves have at most 32 lanes (the forms NVIDIA gets, where a
//! console wave64 program is two host waves exchanging what crosses
//! halves) and declares the wave it is then written for (`pin_wave`):
//! * a console wave32 program: 32, wave operations or not (one without any
//!   whose group barriers the host leaves out is left to the host);
//! * a console wave64 program that a 32-lane form takes exactly: 32 (with
//!   a first-lane read of a value that differs between lanes counted as
//!   crossing halves, `wave64_halves::first_lane_crosses`);
//! * a console wave64 program that the 32-lane forms would refuse or take
//!   only approximately (each half-wave on its own): translated as before
//!   instead ([`super::emit_sized`]), in waves of 64 where it declares a
//!   size.
//!
//! To find which of these a fault in a run comes from:
//! * `VICTORY_COMPUTE_WAVE=pin` does only the first (console wave64 programs
//!   are translated as before), `=forms` only the other two (console wave32
//!   programs are);
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
    /// Unset, or `=32`: every compute program.
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
        None | Some("" | "32") => Asked::All,
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
}

/// Whether compute programs are translated for host waves of 32 and pinned
/// to them: the switch is on and the host runs waves of 32 and of 64
/// (`WaveLaneCountMin` at most 32, `WaveLaneCountMax` at least 64). A host
/// whose waves are at most 32 lanes (NVIDIA) already gets those forms and
/// keeps what it had; so does one whose waves are not known.
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
        if let Some(only) = TEST_ONLY.get() {
            return only == wave64;
        }
        if TEST.get().is_some() {
            return true;
        }
    }
    match asked() {
        Asked::All => true,
        Asked::Pin => !wave64,
        Asked::Forms => wave64,
        Asked::Off => false,
    }
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
        assert_eq!(parse(None), Asked::All);
        assert_eq!(parse(Some("")), Asked::All);
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
