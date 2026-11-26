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

use std::cell::Cell;
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

/// `VICTORY_COMPUTE_WAVE`: on (unset, or `=32`) unless `=0` or `=host`
/// (any other value: off too), which translate compute programs for the
/// host's widest wave as before.
fn asked() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_COMPUTE_WAVE") {
        Err(_) => true,
        Ok(v) => matches!(v.trim(), "" | "32"),
    })
}

thread_local! {
    /// Set while a program is translated as before ([`without`]).
    static SUSPENDED: Cell<bool> = const { Cell::new(false) };
    /// A test's choice for [`compute_wave_32`] on its thread (`None`: the
    /// switch and the host's).
    #[cfg(test)]
    pub(super) static TEST: Cell<Option<bool>> = const { Cell::new(None) };
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
    asked() && min != 0 && min <= 32 && max >= 64
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
