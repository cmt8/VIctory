//! Raw unordered buffers a program loads past its wave's cache (GLC):
//! other groups write what it reads, and the host's view of them is
//! globally coherent (after AvatarManz/VIctory-7900XTX 875a63a4).

use std::collections::HashSet;

use super::{Fields, Lower, ResourceType, Source};

impl<'a> Lower<'a> {
    /// Which table entry `src` is loaded from, at a fixed place: its root
    /// parameter and byte.
    fn coherent_entry(&self, src: &Source) -> Option<(usize, u32)> {
        let Source::Loaded { load, first } = src else {
            return None;
        };
        let a = self.scalar.get(load).filter(|a| a.dynamic.is_none())?;
        Some((a.param, a.offset + 4 * u32::from(*first)))
    }

    /// The table entries of raw unordered buffers the program loads with
    /// GLC (`buffer_load_dword*`). On the console such a load reads past
    /// the wave's cache what other groups of the dispatch wrote (a group
    /// that waits for others, the upscaler's; the faces' blend shapes); on
    /// the host only a globally coherent view promises that, and a load in
    /// a loop that only waits may otherwise be made once. Every raw access
    /// through such an entry takes the coherent view
    /// ([`Self::is_coherent`]), so its handle is one. Empty with
    /// `VICTORY_GLC_COHERENT=0` (see [`super::super::glc_coherent`]).
    pub(super) fn find_coherent(&self) -> HashSet<(usize, u32)> {
        if !super::super::glc_coherent() {
            return HashSet::new();
        }
        self.x
            .res
            .descriptors
            .iter()
            .filter(|d| {
                let i = &self.x.p.blocks[d.site.0].insts[d.site.1];
                matches!(&i.fields, Fields::Buffer { cache, .. } if cache.glc)
                    && i.name().starts_with("buffer_load_dword")
                    && self
                        .used_kind(&d.source)
                        .is_some_and(|u| u.kind == ResourceType::Uav)
            })
            .filter_map(|d| self.coherent_entry(&d.source))
            .collect()
    }

    /// Whether a raw access through `src` takes the globally coherent view
    /// ([`Self::find_coherent`]).
    pub(super) fn is_coherent(&self, src: &Source) -> bool {
        self.coherent_entry(src)
            .is_some_and(|e| self.coherent.contains(&e))
    }
}
