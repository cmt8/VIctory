//! The console's wave size for every compute program that uses the host's
//! wave operations ([`Lower::pin_wave`]).

use super::{HwStage, Lower, flags, host_max_wave};

impl<'a> Lower<'a> {
    /// Declares the console's wave size (`WaveSize`) for a compute program
    /// that uses the host's wave operations and has none declared yet
    /// (`VICTORY_WAVE_PIN`, on; `=0` declares nothing more, as before).
    ///
    /// Every wave operation the lowering emits stands for something of the
    /// console's wave: `v_readfirstlane_b32` is the console wave's first
    /// lane (`waveReadLaneFirst`), a branch on EXEC or VCC asks the console
    /// wave's lanes, a guarded first lane reads the console wave's ballot.
    /// Those are exact when a host wave is a console wave. The lowering
    /// declares the size only where a program reads other lanes or lane
    /// masks as data (`console_lanes`); otherwise the host picks: NVIDIA
    /// runs waves of 32, RADV waves of 64 (`cs_wave_size`, radv_physical_
    /// device.c; vkd3d-proton asks for a size only when one is declared,
    /// state.c). A console wave32 program of 64 or more threads then shares
    /// a host wave of 64 between two console waves, and the second takes
    /// the first's lane 0 for each of its first-lane reads: the face
    /// network's values went wrong on the RX 7900 XTX for every character,
    /// and `RADV_PERFTEST=cswave32` (host waves of 32 for such programs)
    /// put most of them right.
    ///
    /// The console's size, not the host's widest: that is the wave the
    /// program's code was compiled for. A console wave64 program on a host
    /// of at most 32 lanes keeps what the lowering chose (the host takes no
    /// `WaveSize(64)`).
    pub(super) fn pin_wave(&mut self) {
        if !super::super::wave_pin()
            || self.x.native.stage != HwStage::Cs
            || self.s.wave_size.is_some()
            || self.s.flags & flags::WAVE_OPS == 0
        {
            return;
        }
        let wave = if self.x.p.wave64 { 64 } else { 32 };
        // (0: the host's widest wave is not known, as in most tests: then
        // nothing is declared, as before.)
        let host = host_max_wave();
        if host >= wave {
            self.s.wave_size = Some(wave);
        }
    }
}
