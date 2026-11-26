//! A fast hasher for maps keyed by our own numbers (slot indices, title
//! addresses, pointers of ours): a multiply and an add a word, as
//! rustc-hash's FxHasher 2 (rustc's own), finished with a rotation so that
//! the bits a hash table indexes by depend on every bit of the key.
//!
//! The standard maps hash with SipHash-1-3, seeded per map against keys
//! chosen to collide: some 14.7 ms a frame of the recording threads in the
//! perf recordings of the world, in maps of slots and title addresses
//! asked thousands of times a frame. No key of these maps is chosen by
//! anyone who would want them to collide (the title's slot numbers and
//! addresses are its own layout). Iteration order: the standard maps'
//! is random per map and per run, so no caller can have relied on it.

use std::hash::{BuildHasherDefault, Hasher};

/// The multiplier of rustc-hash 2 (`FxHasher`): odd, with its bits spread.
const K: u64 = 0xf135_7aea_2e62_a9c5;

#[derive(Default, Clone, Copy)]
pub(crate) struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = self.hash.wrapping_add(word).wrapping_mul(K);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut last = [0u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            // The length in the top byte: "ab" and "ab\0" differ.
            last[7] = rest.len() as u8;
            self.add(u64::from_le_bytes(last));
        }
    }

    #[inline]
    fn write_u8(&mut self, n: u8) {
        self.add(u64::from(n));
    }

    #[inline]
    fn write_u16(&mut self, n: u16) {
        self.add(u64::from(n));
    }

    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.add(u64::from(n));
    }

    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.add(n);
    }

    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        // The product's high bits depend on all of the key's; a table takes
        // its bucket from the low bits (and hashbrown its tag from the top
        // seven), so they are rotated down. Without it, keys that are
        // multiples of a power of two (pointers, 64 KiB pages) would share
        // their low bits and buckets.
        self.hash.rotate_left(26)
    }
}

pub(crate) type FxBuild = BuildHasherDefault<FxHasher>;
pub(crate) type FxHashMap<K, V> = std::collections::HashMap<K, V, FxBuild>;
pub(crate) type FxHashSet<K> = std::collections::HashSet<K, FxBuild>;

#[cfg(test)]
mod tests {
    use super::{FxBuild, FxHashMap, FxHashSet};
    use std::hash::BuildHasher;

    /// Keys that differ only in high bits, or are multiples of large powers
    /// of two (pointers, 64 KiB pages, title addresses), spread over the
    /// low bits a table indexes by.
    #[test]
    fn aligned_keys_spread_over_the_low_bits() {
        for shift in [0u32, 4, 12, 16, 32] {
            let mut buckets = [0u32; 256];
            for i in 0..4096u64 {
                let h = FxBuild::default().hash_one((i << shift) as usize);
                buckets[(h & 255) as usize] += 1;
            }
            // 16 a bucket on average; no bucket far past it.
            let most = *buckets.iter().max().unwrap();
            assert!(most <= 48, "keys << {shift}: {most} in one bucket of 256");
        }
    }

    /// The maps work as maps: what goes in comes out, for the key kinds the
    /// layer uses (numbers, tuples, arrays).
    #[test]
    fn maps_find_what_they_hold() {
        let mut m: FxHashMap<u32, u64> = FxHashMap::default();
        for i in 0..100_000u32 {
            m.insert(i.wrapping_mul(2_654_435_761), u64::from(i));
        }
        assert!((0..100_000u32).all(|i| m.get(&i.wrapping_mul(2_654_435_761)) == Some(&u64::from(i))));
        let mut t: FxHashMap<(u64, u32, u64), u32> = FxHashMap::default();
        t.insert((1, 2, 3), 4);
        t.insert((3, 2, 1), 5);
        assert_eq!(t.get(&(1, 2, 3)), Some(&4));
        assert_eq!(t.get(&(3, 2, 1)), Some(&5));
        let mut a: FxHashMap<[u32; 14], u8> = FxHashMap::default();
        let mut k = [0u32; 14];
        a.insert(k, 1);
        k[13] = 1;
        a.insert(k, 2);
        assert_eq!(a.len(), 2);
        let s: FxHashSet<usize> = (0..1000usize).map(|i| i << 16).collect();
        assert!((0..1000usize).all(|i| s.contains(&(i << 16))));
        assert!(!s.contains(&1));
    }

    /// Byte strings of different lengths with the same leading bytes hash
    /// apart.
    #[test]
    fn byte_tails_count() {
        let b = FxBuild::default();
        assert_ne!(b.hash_one(&b"ab"[..]), b.hash_one(&b"ab\0"[..]));
        assert_ne!(b.hash_one(&b"abcdefgh"[..]), b.hash_one(&b"abcdefgh\0"[..]));
    }
}
