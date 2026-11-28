//! The console's ray-tracing node test, `image_bvh_intersect_ray` and
//! `image_bvh64_intersect_ray`, as inline DXIL: the host has no instruction
//! that tests one ray against one node of the title's own structures.
//!
//! [`crate::bvh`] says what the instruction computes and is the reference:
//! the code made here does the same float operations in the same order
//! (DXIL float operations carry no fast-math flags, so the host may not
//! reorder or fuse them), and the tests in `intersect_tests.rs` run it on
//! the card against the reference.
//!
//! The title's nodes are in title memory, named by address: they are read
//! through the host's copies of title memory ([`Lower::memory`]), one look
//! in its directory per node (two for a 128-byte box node).
//!
//! Shape of the code, under the instruction's lane guard:
//!
//! ```text
//! kind = pointer & 7
//! box (4, 5):  4 child pointers; 24 box floats (kind 5) or 24 halves widened (kind 4);
//!              four slab tests; the sorting network
//! triangle (0-3): the id dword, three vertices at the bytes it names; the watertight test
//! other (6, 7): a triangle miss
//! ```
//!
//! Each lane reads only the dwords its node's kind needs: 28 for a 32-bit
//! box node, 16 for a 16-bit one, 10 for a triangle.

use super::{Attr, Bin, Cast, Fields, Inst, Lower, Operand, Reg, Term, V, Why, flags, pred};
use crate::bvh;
use crate::resource::BvhNodes;

#[cfg(test)]
thread_local! {
    /// The tests' stand-in for title memory ([`Lower::bvh_memory`]): a
    /// heap slot holding a raw buffer, and the title address of its first
    /// byte. (The card tests have no host to keep copies of title memory
    /// and their directory.)
    pub(in crate::emit) static TEST_MEMORY: std::cell::Cell<Option<(u32, u64)>> =
        const { std::cell::Cell::new(None) };
}

/// Where the instruction's node is.
#[derive(Clone, Copy)]
enum NodeAt {
    /// At this byte of a raw buffer of the host.
    Buffer { handle: V, byte: V },
    /// In title memory: the raw view of the host copy holding the node's
    /// first 64 bytes and the node's byte in it ([`Lower::bvh_memory`]),
    /// the node's address, for the second half of a 128-byte node, and
    /// whether there is a copy.
    Memory { handle: V, byte: V, low: V, high: V, mapped: V },
}

/// The ray operands, as floats.
struct Ray {
    extent: V,
    origin: [V; 3],
    direction: [V; 3],
    inverse: [V; 3],
}

/// LOCAL (from our wt5): `VICTORY_BVH_MISS=1` (off by default): the node test
/// misses every node. While the structure builder is off the structures are
/// never built, and a walk of what lies in their memory is not the console's.
/// (The switch itself is `crate::emit::bvh_miss`, which the shader cache's
/// keys read too.)
fn bvh_miss() -> bool {
    crate::emit::bvh_miss()
}

impl<'a> Lower<'a> {
    /// `image_bvh_intersect_ray` and `image_bvh64_intersect_ray`.
    pub(super) fn bvh(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Image {
            data,
            addr,
            rsrc,
            dmask,
            a16,
            d16,
            tfe,
            lwe,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        if *a16 || *d16 || *tfe || *lwe || *dmask != 0xf {
            return Err("a ray-tracing node test with 16-bit operands or a partial result");
        }
        let wide = inst.name() == "image_bvh64_intersect_ray";
        // Operand VGPRs in order: the node pointer (two dwords in the
        // 64-bit form), the ray's extent, origin, direction and inverse
        // direction.
        let mut regs = Vec::new();
        for a in addr {
            if let Operand::Vector { reg, dwords } = *a {
                regs.extend((0..dwords).map(|k| reg + k));
            }
        }
        if regs.len() != if wide { 12 } else { 11 } {
            return Err("a ray-tracing node test's operands");
        }
        let Operand::Vector { reg: dst, .. } = *data else {
            return Err("a ray-tracing node test's result");
        };
        let site = (b, i);
        let nodes = self
            .x
            .res
            .bvh
            .iter()
            .find(|n| n.0 == site)
            .map(|n| n.1)
            .ok_or("an untracked ray-tracing structure")?;
        let (sort, barycentrics) = match nodes {
            BvhNodes::Resource => (true, true),
            BvhNodes::Memory { word1, word3 } => (
                word1 & bvh::BOX_SORT != 0,
                word3 & bvh::RETURN_BARYCENTRICS != 0,
            ),
        };
        if !barycentrics {
            // Mode 0 returns a triangle id and a hit status in the last two
            // dwords; no program of the title asks for it.
            return Err("a ray-tracing structure that returns triangle ids");
        }
        let guard = self.guard(b, i)?;
        let mut x = Vec::with_capacity(regs.len());
        for &r in &regs {
            let v = inst.read(Reg::V(r)).ok_or("an unread operand")?;
            x.push(self.i32_of(v)?);
        }
        let pointer = x[0];
        let pointer_high = wide.then(|| x[1]);
        let f: Vec<V> = x[1 + usize::from(wide)..]
            .iter()
            .map(|&v| self.as_float(v))
            .collect();
        let ray = Ray {
            extent: f[0],
            origin: [f[1], f[2], f[3]],
            direction: [f[4], f[5], f[6]],
            inverse: [f[7], f[8], f[9]],
        };
        // A lane whose walk is cut makes no test and finds nothing: no child
        // of a box node, no triangle hit ([`Self::bvh_none`]).
        let none = self.bvh_none(b, guard, pointer, pointer_high, &x[1 + usize::from(wide)..]);
        let c = self.cur;
        let test = match none {
            Some(none) => {
                let (off, on) = (self.s.m.bool(false), guard.unwrap_or_else(|| self.s.m.bool(true)));
                Some(self.s.m.select(c, none, off, on))
            }
            None => guard,
        };
        let nothing: [V; 4] = match none {
            Some(_) => {
                let (k4, k6) = (self.s.m.i32(4), self.s.m.i32(6));
                let pair = self.s.m.bin(c, Bin::And, pointer, k6);
                let is_box = self.s.m.cmp(c, pred::EQ, pair, k4);
                std::array::from_fn(|k| {
                    let (no_child, miss) = (self.s.m.i32(bvh::NONE), self.s.m.i32(bvh::MISS[k].to_bits()));
                    self.s.m.select(c, is_box, no_child, miss)
                })
            }
            None => [self.s.m.i32(0); 4],
        };
        let mut descriptor = None;
        let at = match nodes {
            BvhNodes::Resource => {
                let src = self
                    .descriptors
                    .get(&(site, false))
                    .copied()
                    .ok_or("an untracked ray-tracing structure")?;
                let handle = self.handle(site, src, false, (11, 0))?;
                // (pointer >> 3) << 6 from the buffer's start.
                let (kind_bits, k3) = (self.s.m.i32(!7), self.s.m.i32(3));
                let byte = self.s.m.bin(c, Bin::And, pointer, kind_bits);
                let byte = self.s.m.bin(c, Bin::Shl, byte, k3);
                NodeAt::Buffer { handle, byte }
            }
            BvhNodes::Memory { .. } => {
                let Operand::Scalar { code, .. } = *rsrc else {
                    return Err("a ray-tracing structure descriptor not in SGPRs");
                };
                let mut words = [pointer; 2];
                for (k, w) in words.iter_mut().enumerate() {
                    let v = inst
                        .read(Reg::S(code + k as u8))
                        .ok_or("an unread operand")?;
                    *w = self.i32_of(v)?;
                }
                descriptor = Some((words[0], words[1]));
                let (low, high) = self.bvh_address(words[0], words[1], pointer, pointer_high);
                // One look for the node's copy, before the branches: a
                // node is 64 bytes on 64, so its first half lies in one
                // 64 KiB page whatever its kind.
                let (handle, byte, mapped) = self.bvh_memory(low, high, test);
                NodeAt::Memory {
                    handle,
                    byte,
                    low,
                    high,
                    mapped,
                }
            }
        };
        let gate = self.begin_guarded(test);
        let head = self.cur;
        let (k4, k6, k7) = (self.s.m.i32(4), self.s.m.i32(6), self.s.m.i32(7));
        let kind = self.s.m.bin(head, Bin::And, pointer, k7);
        let pair = self.s.m.bin(head, Bin::And, pointer, k6);
        let is_box = self.s.m.cmp(head, pred::EQ, pair, k4);
        let is_triangle = self.s.m.cmp(head, pred::IULT, kind, k4);
        let (box_block, leaf_block) = (self.s.m.new_block(), self.s.m.new_block());
        let (triangle_block, join) = (self.s.m.new_block(), self.s.m.new_block());
        self.s
            .m
            .terminate(head, Term::CondBr(is_box, box_block, leaf_block));
        self.s
            .m
            .terminate(leaf_block, Term::CondBr(is_triangle, triangle_block, join));
        self.cur = box_block;
        // LOCAL (from our wt5): with `VICTORY_BVH_MISS=1` every node is missed
        // without being read: a box returns no child and a triangle the
        // triangle miss, so a walk ends at its first node.
        let boxes = if bvh_miss() {
            [self.s.m.i32(bvh::NONE); 4]
        } else {
            self.bvh_boxes(at, kind, &ray, sort)
        };
        let box_end = self.cur;
        self.s.m.terminate(box_end, Term::Br(join));
        self.cur = triangle_block;
        let triangle = if bvh_miss() {
            bvh::MISS.map(|f| self.s.m.i32(f.to_bits()))
        } else {
            self.bvh_triangle(at, kind, &ray)
        };
        let triangle_end = self.cur;
        self.s.m.terminate(triangle_end, Term::Br(join));
        self.cur = join;
        // Kinds 6 and 7 (an instance, a procedural primitive): a triangle
        // miss ([`bvh::intersect`]).
        let other = bvh::MISS.map(|f| self.s.m.i32(f.to_bits()));
        let i32t = self.s.t.i32;
        let mut results = Vec::with_capacity(4);
        for k in 0..4 {
            let phi = self.s.m.phi(join, i32t);
            self.s.m.set_phi(
                phi,
                vec![
                    (boxes[k], box_end),
                    (triangle[k], triangle_end),
                    (other[k], leaf_block),
                ],
            );
            results.push((phi, nothing[k]));
        }
        if super::super::bvh_count() && self.s.threads.is_some() {
            let found: [V; 4] = std::array::from_fn(|k| results[k].0);
            self.bvh_count_test(at, kind, &found, descriptor, (pointer, pointer_high), &x[1 + usize::from(wide)..]);
        }
        let results = self.end_guarded(gate, &results);
        for (k, &r) in results.iter().enumerate() {
            self.write(inst, Reg::V(dst + k as u8), r, guard);
        }
        Ok(())
    }

    /// Title memory at `low`/`high` for a node's bytes: the raw view of the
    /// host copy holding the address and the byte in that view
    /// ([`Lower::memory`]: a page the host has no copy of reads zeros and
    /// is left for the host to fetch, for lanes that run).
    fn bvh_memory(&mut self, low: V, high: V, guard: Option<V>) -> (V, V, V) {
        #[cfg(test)]
        if let Some((slot, base)) = TEST_MEMORY.get() {
            // The buffer at heap slot `slot` holds the bytes from title
            // address `base` on; any other address reads past its end,
            // which is zeros.
            let c = self.cur;
            let slot = self.s.m.i32(slot);
            let handle = self.s.heap_handle(c, slot, false, false, (11, 0));
            let (base_low, base_high) =
                (self.s.m.i32(base as u32), self.s.m.i32((base >> 32) as u32));
            let at = self.s.m.bin(c, Bin::Sub, low, base_low);
            let in_low = self.s.m.cmp(c, pred::IUGE, low, base_low);
            let in_high = self.s.m.cmp(c, pred::EQ, high, base_high);
            let inside = self.s.m.bin(c, Bin::And, in_low, in_high);
            let outside = self.s.m.i32(0xffff_ff00);
            let at = self.s.m.select(c, inside, at, outside);
            return (handle, at, inside);
        }
        let (handle, byte) = self.memory(low, high, guard, false);
        let mapped = self.page_there.expect("memory says");
        (handle, byte, mapped)
    }

    /// The second 64 bytes of a 128-byte node: in a page, and in a copy, of
    /// their own maybe. Goes on in another block.
    fn bvh_second_half(&mut self, at: NodeAt) -> NodeAt {
        match at {
            NodeAt::Buffer { handle, byte } => {
                let size = self.s.m.i32(64);
                let byte = self.s.m.bin(self.cur, Bin::Add, byte, size);
                NodeAt::Buffer { handle, byte }
            }
            NodeAt::Memory { low, high, .. } => {
                let (low, high) = self.address_plus(low, high, 64);
                // Every lane in this block runs the instruction.
                let (handle, byte, mapped) = self.bvh_memory(low, high, None);
                NodeAt::Memory {
                    handle,
                    byte,
                    low,
                    high,
                    mapped,
                }
            }
        }
    }

    /// `count` dwords of 64 bytes of a node from their byte `base` + `offset`
    /// (`base` computed, `offset` fixed), read four at a time.
    fn bvh_node_words(&mut self, at: NodeAt, base: Option<V>, offset: u32, count: u32) -> Vec<V> {
        let (undef, align) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
        let (handle, start_byte) = match at {
            NodeAt::Buffer { handle, byte } | NodeAt::Memory { handle, byte, .. } => (handle, byte),
        };
        let mut out = Vec::with_capacity(count as usize);
        for start in (0..count).step_by(4) {
            let n = (count - start).min(4);
            let node_byte = offset + 4 * start;
            let mut byte = start_byte;
            let c = self.cur;
            if let Some(b) = base {
                byte = self.s.m.bin(c, Bin::Add, byte, b);
            }
            let k = self.s.m.i32(node_byte);
            let byte = self.s.m.bin(c, Bin::Add, byte, k);
            let mask = self.s.m.i8((1u8 << n) - 1);
            let r = self.s.op(
                c,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                self.s.t.res_ret,
                139,
                &[handle, byte, undef, mask, align],
            );
            for k in 0..n {
                let f = self.s.m.extract(c, r, k);
                out.push(self.as_int(f));
            }
        }
        out
    }

    /// The node's title address, [`bvh::node_address`]: the descriptor's
    /// base (dword 0 and the low byte of dword 1, in 256-byte units) plus
    /// `(pointer >> 3) << 6`.
    fn bvh_address(&mut self, word0: V, word1: V, pointer: V, pointer_high: Option<V>) -> (V, V) {
        let c = self.cur;
        let i64t = self.s.m.int(64);
        let (zero, byte) = (self.s.m.i32(0), self.s.m.i32(0xff));
        let base_high = self.s.m.bin(c, Bin::And, word1, byte);
        let base = self.join64(word0, base_high);
        let k8 = self.s.m.int_const(i64t, 8);
        let base = self.s.m.bin(c, Bin::Shl, base, k8);
        let p = self.join64(pointer, pointer_high.unwrap_or(zero));
        let (k3, k6) = (self.s.m.int_const(i64t, 3), self.s.m.int_const(i64t, 6));
        let p = self.s.m.bin(c, Bin::LShr, p, k3);
        let p = self.s.m.bin(c, Bin::Shl, p, k6);
        let a = self.s.m.bin(c, Bin::Add, base, p);
        self.s.flags |= flags::INT64;
        self.split64(a)
    }

    /// The box test ([`bvh::boxes`]): four result dwords. Starts in the
    /// current block and leaves `self.cur` at the block it ends in.
    fn bvh_boxes(&mut self, at: NodeAt, kind: V, ray: &Ray, sort: bool) -> [V; 4] {
        let head = self.cur;
        let children = self.bvh_node_words(at, None, 0, 4);
        let k5 = self.s.m.i32(5);
        let wide = self.s.m.cmp(head, pred::EQ, kind, k5);
        let (wide_block, half_block) = (self.s.m.new_block(), self.s.m.new_block());
        let join = self.s.m.new_block();
        self.s
            .m
            .terminate(head, Term::CondBr(wide, wide_block, half_block));
        // 32-bit floats: 24 dwords from byte 16.
        self.cur = wide_block;
        let k16 = self.s.m.i32(16);
        let mut words = self.bvh_node_words(at, None, 16, 12);
        let second = self.bvh_second_half(at);
        words.extend(self.bvh_node_words(second, None, 0, 12));
        let floats: Vec<V> = words.iter().map(|&w| self.as_float(w)).collect();
        // (The look for the second half goes on in another block.)
        let wide_block = self.cur;
        self.s.m.terminate(wide_block, Term::Br(join));
        // 16-bit floats: 24 halves in 12 dwords from byte 16, the low half
        // of a dword first.
        self.cur = half_block;
        let words = self.bvh_node_words(at, None, 16, 12);
        let f32t = self.s.t.f32;
        let mut halves = Vec::with_capacity(24);
        for n in 0..24 {
            let mut h = words[n / 2];
            if n % 2 == 1 {
                h = self.s.m.bin(half_block, Bin::LShr, h, k16);
            }
            // The conversion reads the low 16 bits.
            halves.push(self.s.op(
                half_block,
                "legacyF16ToF32",
                Attr::ReadNone,
                f32t,
                131,
                &[h],
            ));
        }
        self.s.m.terminate(half_block, Term::Br(join));
        self.cur = join;
        // A page that is not there reads zeros, which are a child and a box:
        // no child of it is hit. (The last two boxes of a 128-byte node are in
        // its second half.)
        let there = match (at, second) {
            (NodeAt::Memory { mapped: first, .. }, NodeAt::Memory { mapped: last, .. }) => {
                let i1t = self.s.m.type_of(first);
                let mapped = self.s.m.phi(join, i1t);
                self.s
                    .m
                    .set_phi(mapped, vec![(last, wide_block), (first, half_block)]);
                let both = self.s.m.bin(join, Bin::And, first, mapped);
                Some([first, first, both, both])
            }
            _ => None,
        };
        let mut coords = Vec::with_capacity(24);
        for n in 0..24 {
            let phi = self.s.m.phi(join, f32t);
            self.s
                .m
                .set_phi(phi, vec![(floats[n], wide_block), (halves[n], half_block)]);
            coords.push(phi);
        }
        // The inverse direction held finite ([`bvh::boxes`]).
        let (most, least) = (
            self.s.m.f32(f32::MAX.to_bits()),
            self.s.m.f32((-f32::MAX).to_bits()),
        );
        let inverse = ray.inverse.map(|x| {
            let x = self.binary(35, x, least);
            self.binary(36, x, most)
        });
        let c = join;
        let (none, infinity, fzero) = (
            self.s.m.i32(bvh::NONE),
            self.s.m.f32(f32::INFINITY.to_bits()),
            self.s.m.f32(0),
        );
        let mut ids = [none; 4];
        let mut distances = [infinity; 4];
        for n in 0..4 {
            let bx = &coords[6 * n..6 * n + 6];
            let (mut near, mut far) = ([fzero; 3], [fzero; 3]);
            for a in 0..3 {
                let low = self.s.m.bin(c, Bin::Sub, bx[a], ray.origin[a]);
                let low = self.s.m.bin(c, Bin::Mul, low, inverse[a]);
                let high = self.s.m.bin(c, Bin::Sub, bx[3 + a], ray.origin[a]);
                let high = self.s.m.bin(c, Bin::Mul, high, inverse[a]);
                near[a] = self.binary(36, low, high);
                far[a] = self.binary(35, low, high);
            }
            let enter = self.binary(35, near[0], near[1]);
            let enter = self.binary(35, enter, near[2]);
            let leave = self.binary(36, far[0], far[1]);
            let leave = self.binary(36, leave, far[2]);
            let from = self.binary(35, enter, fzero);
            // Ordered compares: a NaN anywhere is a miss.
            let through = self.s.m.cmp(c, pred::OGE, leave, from);
            let before = self.s.m.cmp(c, pred::OLT, enter, ray.extent);
            let off = self.is_nan_f32(bx[0]);
            let on = self.s.m.bool(true);
            let on = self.s.m.bin(c, Bin::Xor, off, on);
            let hit = self.s.m.bin(c, Bin::And, through, before);
            let hit = self.s.m.bin(c, Bin::And, hit, on);
            let child = self.s.m.cmp(c, pred::NE, children[n], none);
            let hit = self.s.m.bin(c, Bin::And, hit, child);
            let hit = match there {
                Some(there) => self.s.m.bin(c, Bin::And, hit, there[n]),
                None => hit,
            };
            ids[n] = self.s.m.select(c, hit, children[n], none);
            distances[n] = self.s.m.select(c, hit, enter, infinity);
        }
        if sort {
            for (a, b) in [(0, 1), (2, 3), (0, 2), (1, 3), (1, 2)] {
                let swap = self.s.m.cmp(c, pred::OLT, distances[b], distances[a]);
                let (da, db) = (distances[a], distances[b]);
                distances[a] = self.s.m.select(c, swap, db, da);
                distances[b] = self.s.m.select(c, swap, da, db);
                let (ia, ib) = (ids[a], ids[b]);
                ids[a] = self.s.m.select(c, swap, ib, ia);
                ids[b] = self.s.m.select(c, swap, ia, ib);
            }
        }
        ids
    }

    /// The triangle test ([`bvh::triangle`] of [`bvh::triangle_vertices`]):
    /// four result dwords, made in the current block.
    fn bvh_triangle(&mut self, at: NodeAt, kind: V, ray: &Ray) -> [V; 4] {
        let c = self.cur;
        let k = |l: &mut Self, v: u32| l.s.m.i32(v);
        // The triangle's byte of the dword at byte 60: which of its
        // vertices barycentric I and J weigh.
        let id = self.bvh_node_words(at, None, 60, 1)[0];
        let (k2, k3, k12, k15) = (k(self, 2), k(self, 3), k(self, 12), k(self, 15));
        let shift = self.s.m.bin(c, Bin::Shl, kind, k3);
        let id = self.s.m.bin(c, Bin::LShr, id, shift);
        let i = self.s.m.bin(c, Bin::And, id, k3);
        let j = self.s.m.bin(c, Bin::LShr, id, k2);
        let j = self.s.m.bin(c, Bin::And, j, k3);
        let first = self.s.m.bin(c, Bin::Sub, k3, i);
        let first = self.s.m.bin(c, Bin::Sub, first, j);
        let first = self.s.m.bin(c, Bin::And, first, k3);
        // The node's vertices the triangle is made of, four bits each.
        let (k0, k1) = (k(self, 0), k(self, 1));
        let maps: [V; 4] = std::array::from_fn(|n| k(self, bvh::vertex_map(n as u32)));
        let (is0, is1, is2) = (
            self.s.m.cmp(c, pred::EQ, kind, k0),
            self.s.m.cmp(c, pred::EQ, kind, k1),
            self.s.m.cmp(c, pred::EQ, kind, k2),
        );
        let map = self.s.m.select(c, is2, maps[2], maps[3]);
        let map = self.s.m.select(c, is1, maps[1], map);
        let map = self.s.m.select(c, is0, maps[0], map);
        let mut p = Vec::with_capacity(3);
        for n in [first, i, j] {
            let nibble = self.s.m.bin(c, Bin::Shl, n, k2);
            let vertex = self.s.m.bin(c, Bin::LShr, map, nibble);
            let vertex = self.s.m.bin(c, Bin::And, vertex, k15);
            let byte = self.s.m.bin(c, Bin::Mul, vertex, k12);
            let words = self.bvh_node_words(at, Some(byte), 0, 3);
            // Relative to the ray's origin.
            let r: Vec<V> = (0..3)
                .map(|a| {
                    let f = self.as_float(words[a]);
                    self.s.m.bin(c, Bin::Sub, f, ray.origin[a])
                })
                .collect();
            p.push([r[0], r[1], r[2]]);
        }
        // The ray's largest axis z, and x and y after it in an order that
        // keeps the winding.
        let fzero = self.s.m.f32(0);
        let size = ray.direction.map(|d| self.unary(6, d));
        let xy = self.s.m.cmp(c, pred::OGE, size[0], size[1]);
        let xz = self.s.m.cmp(c, pred::OGE, size[0], size[2]);
        let yz = self.s.m.cmp(c, pred::OGE, size[1], size[2]);
        let z0 = self.s.m.bin(c, Bin::And, xy, xz);
        let on = self.s.m.bool(true);
        let not_xy = self.s.m.bin(c, Bin::Xor, xy, on);
        let z1 = self.s.m.bin(c, Bin::And, not_xy, yz);
        // Component (kz + n) % 3 of `q`.
        let pick = |l: &mut Self, q: [V; 3], n: usize| {
            let rest = l.s.m.select(c, z1, q[(1 + n) % 3], q[(2 + n) % 3]);
            l.s.m.select(c, z0, q[n % 3], rest)
        };
        let dz = pick(self, ray.direction, 0);
        let backwards = self.s.m.cmp(c, pred::OLT, dz, fzero);
        // (x, y, z) of `q`: the two after the largest axis, swapped for a
        // ray that runs backwards along it.
        let axes = |l: &mut Self, q: [V; 3]| {
            let (x, y) = (pick(l, q, 1), pick(l, q, 2));
            [
                l.s.m.select(c, backwards, y, x),
                l.s.m.select(c, backwards, x, y),
                pick(l, q, 0),
            ]
        };
        let d = axes(self, ray.direction);
        let sz = pick(self, ray.inverse, 0);
        let sx = self.s.m.bin(c, Bin::Mul, d[0], sz);
        let sy = self.s.m.bin(c, Bin::Mul, d[1], sz);
        let mut sheared = Vec::with_capacity(3);
        for r in p {
            let r = axes(self, r);
            let x = self.s.m.bin(c, Bin::Mul, sx, r[2]);
            let x = self.s.m.bin(c, Bin::Sub, r[0], x);
            let y = self.s.m.bin(c, Bin::Mul, sy, r[2]);
            let y = self.s.m.bin(c, Bin::Sub, r[1], y);
            let z = self.s.m.bin(c, Bin::Mul, sz, r[2]);
            sheared.push((x, y, z));
        }
        let [(ax, ay, az), (bx, by, bz), (cx, cy, cz)] = sheared[..] else {
            unreachable!()
        };
        // p * q - r * s.
        let cross = |l: &mut Self, p: V, q: V, r: V, s: V| {
            let a = l.s.m.bin(c, Bin::Mul, p, q);
            let b = l.s.m.bin(c, Bin::Mul, r, s);
            l.s.m.bin(c, Bin::Sub, a, b)
        };
        let u = cross(self, cx, by, cy, bx);
        let v = cross(self, ax, cy, ay, cx);
        let w = cross(self, bx, ay, by, ax);
        let any = |l: &mut Self, p: u8| {
            let a = l.s.m.cmp(c, p, u, fzero);
            let b = l.s.m.cmp(c, p, v, fzero);
            let d = l.s.m.cmp(c, p, w, fzero);
            let a = l.s.m.bin(c, Bin::Or, a, b);
            l.s.m.bin(c, Bin::Or, a, d)
        };
        let behind = any(self, pred::OLT);
        let before = any(self, pred::OGT);
        let outside = self.s.m.bin(c, Bin::And, behind, before);
        let inside = self.s.m.bin(c, Bin::Xor, outside, on);
        let det = self.s.m.bin(c, Bin::Add, v, w);
        let det = self.s.m.bin(c, Bin::Add, u, det);
        let t = self.s.m.bin(c, Bin::Mul, u, az);
        let t2 = self.s.m.bin(c, Bin::Mul, v, bz);
        let t = self.s.m.bin(c, Bin::Add, t, t2);
        let t2 = self.s.m.bin(c, Bin::Mul, w, cz);
        let t = self.s.m.bin(c, Bin::Add, t, t2);
        // The distance is not negative: the numerator has the
        // denominator's sign (or is 0). Ordered compares: a denominator
        // of 0 or a NaN is a miss.
        let front = self.s.m.cmp(c, pred::OGT, det, fzero);
        let front_ahead = self.s.m.cmp(c, pred::OGE, t, fzero);
        let front = self.s.m.bin(c, Bin::And, front, front_ahead);
        let back = self.s.m.cmp(c, pred::OLT, det, fzero);
        let back_ahead = self.s.m.cmp(c, pred::OLE, t, fzero);
        let back = self.s.m.bin(c, Bin::And, back, back_ahead);
        let ahead = self.s.m.bin(c, Bin::Or, front, back);
        let hit = self.s.m.bin(c, Bin::And, inside, ahead);
        let miss = bvh::MISS.map(|f| self.s.m.f32(f.to_bits()));
        let out = [t, det, v, w];
        std::array::from_fn(|n| {
            let f = self.s.m.select(c, hit, out[n], miss[n]);
            self.as_int(f)
        })
    }
}

#[cfg(test)]
thread_local! {
    /// The tests' stand-in for the root UAV of the counts
    /// ([`super::super::shader::SLOT_COUNTS`]): a heap slot holding a raw UAV,
    /// and the byte in it where the runs of counts start.
    pub(in crate::emit) static TEST_COUNTS: std::cell::Cell<Option<(u32, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// Turns of its walk's loops after which a lane whose walk was cut
/// (`Lower::bvh_cut`) leaves them (`Lower::bvh_leaving`): the title's walk
/// did not end of itself by then.
pub(in crate::emit) const SPIN_GRACE: u32 = 64;

/// Which of a lane's counts ([`super::super::shader::COUNT_NAMES`]).
pub(in crate::emit) mod count {
    pub const TESTS: usize = 0;
    pub const WIDE: usize = 1;
    pub const BOX16: usize = 2;
    pub const BOX32: usize = 3;
    pub const TRIANGLE: usize = 4;
    pub const OTHER: usize = 5;
    pub const NO_COPY: usize = 6;
    pub const WILD: usize = 7;
    pub const BOX_NONE: usize = 8;
    pub const TRI_HIT: usize = 9;
    pub const ZERO_BOX: usize = 10;
    pub const BAD_HEADER: usize = 11;
    pub const PAST_SIZE: usize = 12;
    pub const BASE_0: usize = 13;
    pub const LANES: usize = 14;
    pub const LOOP_LIMIT: usize = 15;
    pub const THREADS: usize = 16;
    pub const MOST: usize = 17;
    pub const CHILDREN: usize = 18;
    pub const REPEATS: usize = 19;
    pub const REPEAT_LANES: usize = 20;
    pub const LIMIT_REPEAT_LANES: usize = 21;
    pub const LIMIT_REPEATS: usize = 22;
    pub const REPEAT_BOX: usize = 23;
    pub const REPEAT_OTHER: usize = 24;
    pub const REPEAT_NO_COPY: usize = 25;
    pub const LIMIT_TESTS: usize = 26;
    pub const RUN_FREE: usize = 27;
    pub const RUN_LIMIT: usize = 28;
    pub const CUT_TESTS: usize = 29;
    pub const SPIN_LANES: usize = 30;
    pub const OVER_LANES: usize = 31;
    /// Past the counts: whether the lane left at the loop limit, and the
    /// node pointer of its last test numbered a power of two (two dwords).
    pub const ABORTED: usize = super::super::super::shader::COUNT_NAMES.len();
    pub const CHECK_LOW: usize = ABORTED + 1;
    pub const CHECK_HIGH: usize = ABORTED + 2;
    /// The lane's run of tests of one node with one ray: its last pointer
    /// (two dwords), ray digest, run, and longest run.
    pub const RUN_AT: [usize; 4] = [ABORTED + 3, ABORTED + 4, ABORTED + 5, ABORTED + 6];
    pub const RUN_MOST: usize = ABORTED + 7;
}

impl Lower<'_> {
    /// [`super::super::bvh_spin`]: the lane's private last pointer (low,
    /// high), ray digest, run, whether its walk is cut and the turns of its
    /// walk's loops since, made the first time. The high dword starts at a value no
    /// pointer has, so the first test is no repeat.
    fn bvh_spin_vars(&mut self) -> [V; 6] {
        if let Some(v) = self.bvh_spin {
            return v;
        }
        let (zero, none) = (self.s.m.i32(0), self.s.m.i32(u32::MAX));
        let inits = [zero, none, zero, zero, zero, zero];
        let v: [V; 6] = std::array::from_fn(|k| self.s.m.private_global(&format!("victory.bvh.spin.{k}"), inits[k]));
        self.bvh_spin = Some(v);
        v
    }

    /// At a back edge to loop header `to` of a compute program, where the
    /// spin stop is on and the loop is a walk's or inside one (`walks`):
    /// whether the lane leaves the loop. A lane whose walk was cut
    /// ([`Self::bvh_cut`]) and that still turns its walk's loops
    /// [`SPIN_GRACE`] turns later (the title's walk does not end of itself,
    /// tests or none) leaves them. The turn is counted here, in the current
    /// block.
    pub(super) fn bvh_leaving(&mut self, to: usize) -> Option<V> {
        let at = *self.rpo.get(to)?;
        let inside = self.walks.values().any(|&(lo, hi)| lo <= at && at <= hi);
        if !inside || self.s.threads.is_none() || super::super::bvh_spin().is_none() {
            return None;
        }
        let [.., cut_at, since_at] = self.bvh_spin_vars();
        let c = self.cur;
        let (cut, since) = (self.s.m.load(c, cut_at), self.s.m.load(c, since_at));
        let since = self.s.m.bin(c, Bin::Add, since, cut);
        self.s.m.store(c, since_at, since);
        let grace = self.s.m.i32(SPIN_GRACE);
        Some(self.s.m.cmp(c, pred::IUGT, since, grace))
    }

    /// [`super::super::bvh_spin`], for one node test of a lane that runs it
    /// (in the current block): whether its walk is cut, so that the test is
    /// not made and finds nothing. The same node pointer with the same ray
    /// (its extent, origin and direction, as a digest) as the lane's last
    /// test adds to a run, anything else starts it again; a run of
    /// `bvh_spin` tests cuts the walk, and it stays cut while the ray stays
    /// (the title's walk empties its stack and ends with what it found); a
    /// test with another ray is the next walk's. A run [`SPIN_GRACE`] tests
    /// turns of its loops later leaves them ([`Self::bvh_leaving`]). A walk of a tree
    /// tests no node twice in a row for one ray, and a lane whose rays all
    /// miss the root tests it with another ray each time.
    fn bvh_cut(&mut self, pointer: V, pointer_high: Option<V>, ray: &[V]) -> V {
        let n = super::super::bvh_spin().expect("on");
        let [low_at, high_at, ray_at, run_at, cut_at, since_at] = self.bvh_spin_vars();
        let (run, same_ray) = self.bvh_run_step([low_at, high_at, ray_at, run_at], pointer, pointer_high, ray);
        let c = self.cur;
        let limit = self.s.m.i32(n);
        let spinning = self.s.m.cmp(c, pred::IUGE, run, limit);
        let was = self.s.m.load(c, cut_at);
        let zero = self.s.m.i32(0);
        let was = self.s.m.cmp(c, pred::NE, was, zero);
        let kept = self.s.m.bin(c, Bin::And, was, same_ray);
        let cut = self.s.m.bin(c, Bin::Or, kept, spinning);
        // The turns since the cut: kept while it stays, from 0 at a new one.
        let since = self.s.m.load(c, since_at);
        let since = self.s.m.select(c, kept, since, zero);
        let i32t = self.s.t.i32;
        let cut_word = self.s.m.cast(c, Cast::ZExt, cut, i32t);
        self.s.m.store(c, cut_at, cut_word);
        self.s.m.store(c, since_at, since);
        if super::super::bvh_count() {
            self.bvh_count_or(count::SPIN_LANES, spinning);
        }
        cut
    }

    /// For a node test in native block `b` with `guard`: whether the lane's
    /// walk is cut, by the spin stop ([`Self::bvh_cut`]) or because it went
    /// on past its limit ([`Lower::walk_over`]); `None` where neither can
    /// cut it (not a compute program, not in a walk with the spin stop
    /// off). False in lanes outside the guard. Goes on in another block.
    fn bvh_none(&mut self, b: usize, guard: Option<V>, pointer: V, pointer_high: Option<V>, ray: &[V]) -> Option<V> {
        let spin = super::super::bvh_spin().is_some() && self.s.threads.is_some();
        let over = self.walk_over(b);
        if !spin && over.is_none() {
            return None;
        }
        let at = self.begin_guarded(guard);
        let mut none = over;
        if spin {
            let cut = self.bvh_cut(pointer, pointer_high, ray);
            none = Some(match none {
                Some(o) => self.s.m.bin(self.cur, Bin::Or, o, cut),
                None => cut,
            });
        }
        let none = none.expect("one of them");
        if super::super::bvh_count() {
            self.bvh_count_if(count::CUT_TESTS, none);
            if let Some(o) = over {
                self.bvh_count_or(count::OVER_LANES, o);
            }
        }
        let off = self.s.m.bool(false);
        Some(self.end_guarded(at, &[(none, off)])[0])
    }

    /// One test in a lane's run of tests of the same node pointer with the
    /// same ray (`vars`: its last pointer's two dwords, the digest of its
    /// extent, origin and direction, the run): the run after this test, 1 for
    /// a new pointer or ray, and whether the ray is the last test's. In the
    /// current block.
    fn bvh_run_step(&mut self, vars: [V; 4], pointer: V, pointer_high: Option<V>, ray: &[V]) -> (V, V) {
        let [low_at, high_at, ray_at, run_at] = vars;
        let c = self.cur;
        let zero = self.s.m.i32(0);
        let high = pointer_high.unwrap_or(zero);
        // Extent, origin, direction: rotated and xored.
        let mut digest = zero;
        for (k, &w) in ray.iter().take(7).enumerate() {
            let r = (5 * k as u32 + 1) % 32;
            let (left, right) = (self.s.m.i32(r), self.s.m.i32(32 - r));
            let a = self.s.m.bin(c, Bin::Shl, w, left);
            let b = self.s.m.bin(c, Bin::LShr, w, right);
            let rotated = self.s.m.bin(c, Bin::Or, a, b);
            digest = self.s.m.bin(c, Bin::Xor, digest, rotated);
        }
        let (last_low, last_high, last_ray) =
            (self.s.m.load(c, low_at), self.s.m.load(c, high_at), self.s.m.load(c, ray_at));
        let same_low = self.s.m.cmp(c, pred::EQ, pointer, last_low);
        let same_high = self.s.m.cmp(c, pred::EQ, high, last_high);
        let same_ray = self.s.m.cmp(c, pred::EQ, digest, last_ray);
        let same = self.s.m.bin(c, Bin::And, same_low, same_high);
        let same = self.s.m.bin(c, Bin::And, same, same_ray);
        let run = self.s.m.load(c, run_at);
        let one = self.s.m.i32(1);
        let more = self.s.m.bin(c, Bin::Add, run, one);
        let run = self.s.m.select(c, same, more, one);
        self.s.m.store(c, low_at, pointer);
        self.s.m.store(c, high_at, high);
        self.s.m.store(c, ray_at, digest);
        self.s.m.store(c, run_at, run);
        (run, same_ray)
    }

    /// Diagnostics (`VICTORY_BVH_COUNT=1`): this lane's private count `k`
    /// ([`count`]; one past them: whether it left at the loop limit), made
    /// the first time.
    fn bvh_counter(&mut self, k: usize) -> V {
        if self.bvh_counts.is_none() {
            let zero = self.s.m.i32(0);
            let names = super::super::shader::COUNT_NAMES;
            let globals = (0..names.len() + 8)
                .map(|n| self.s.m.private_global(&format!("victory.bvh.count.{n}"), zero))
                .collect();
            self.bvh_counts = Some(globals);
        }
        self.bvh_counts.as_ref().expect("made")[k]
    }

    /// Adds `by` to this lane's count `k`, in the current block.
    fn bvh_count_add(&mut self, k: usize, by: V) {
        let g = self.bvh_counter(k);
        let c = self.cur;
        let was = self.s.m.load(c, g);
        let now = self.s.m.bin(c, Bin::Add, was, by);
        self.s.m.store(c, g, now);
    }

    /// Sets count `k` to 1 where `yes` holds (a lane's flag), in the current
    /// block.
    fn bvh_count_or(&mut self, k: usize, yes: V) {
        let g = self.bvh_counter(k);
        let c = self.cur;
        let i32t = self.s.t.i32;
        let one = self.s.m.cast(c, Cast::ZExt, yes, i32t);
        let was = self.s.m.load(c, g);
        let now = self.s.m.bin(c, Bin::Or, was, one);
        self.s.m.store(c, g, now);
    }

    /// Adds 1 to count `k` where `yes` holds.
    fn bvh_count_if(&mut self, k: usize, yes: V) {
        let i32t = self.s.t.i32;
        let one = self.s.m.cast(self.cur, Cast::ZExt, yes, i32t);
        self.bvh_count_add(k, one);
    }

    /// Diagnostics: counts one node test of a lane that runs it, from its
    /// node, kind and results (`descriptor`: the descriptor's first two
    /// dwords, over title memory). May go on in another block.
    fn bvh_count_test(
        &mut self,
        at: NodeAt,
        kind: V,
        results: &[V; 4],
        descriptor: Option<(V, V)>,
        pointers: (V, Option<V>),
        ray: &[V],
    ) {
        use count::*;
        let (pointer, pointer_high) = pointers;
        // The longest run of tests of one node with one ray.
        let vars = RUN_AT.map(|k| self.bvh_counter(k));
        let (run, _) = self.bvh_run_step(vars, pointer, pointer_high, ray);
        let most_at = self.bvh_counter(RUN_MOST);
        let c = self.cur;
        let most = self.s.m.load(c, most_at);
        let longer = self.s.m.cmp(c, pred::IUGT, run, most);
        let most = self.s.m.select(c, longer, run, most);
        self.s.m.store(c, most_at, most);
        let wide = pointer_high.is_some();
        let one = self.s.m.i32(1);
        self.bvh_count_add(TESTS, one);
        if wide {
            self.bvh_count_add(WIDE, one);
        }
        let c = self.cur;
        let k = |l: &mut Self, v: u32| l.s.m.i32(v);
        let (k4, k5, k6, none, zero) = (k(self, 4), k(self, 5), k(self, 6), k(self, bvh::NONE), k(self, 0));
        let yes = self.s.m.bool(true);
        let box16 = self.s.m.cmp(c, pred::EQ, kind, k4);
        let box32 = self.s.m.cmp(c, pred::EQ, kind, k5);
        let triangle = self.s.m.cmp(c, pred::IULT, kind, k4);
        let other = self.s.m.cmp(c, pred::IUGE, kind, k6);
        let is_box = self.s.m.bin(c, Bin::Or, box16, box32);
        self.bvh_count_if(BOX16, box16);
        self.bvh_count_if(BOX32, box32);
        self.bvh_count_if(TRIANGLE, triangle);
        self.bvh_count_if(OTHER, other);
        // Children returned, and whether none was.
        let c = self.cur;
        let i32t = self.s.t.i32;
        let mut hits = zero;
        for &r in results {
            let hit = self.s.m.cmp(c, pred::NE, r, none);
            let hit = self.s.m.bin(c, Bin::And, hit, is_box);
            let n = self.s.m.cast(c, Cast::ZExt, hit, i32t);
            hits = self.s.m.bin(c, Bin::Add, hits, n);
        }
        self.bvh_count_add(CHILDREN, hits);
        let c = self.cur;
        let no_child = self.s.m.cmp(c, pred::EQ, hits, zero);
        let miss = k(self, bvh::MISS[0].to_bits());
        let tri_hit = self.s.m.cmp(c, pred::NE, results[0], miss);
        let tri_hit = self.s.m.bin(c, Bin::And, tri_hit, triangle);
        self.bvh_count_if(TRI_HIT, tri_hit);
        let there = match at {
            NodeAt::Memory { mapped, high, .. } => {
                let c = self.cur;
                let missing = self.s.m.bin(c, Bin::Xor, mapped, yes);
                self.bvh_count_if(NO_COPY, missing);
                // Below 4 GiB or past 128 GiB (bits 32 to 39 of the address
                // 0, or 0x20 and up): not the title's memory.
                let c = self.cur;
                let (kff, k20) = (k(self, 0xff), k(self, 0x20));
                let top = self.s.m.bin(c, Bin::And, high, kff);
                let below = self.s.m.cmp(c, pred::EQ, top, zero);
                let past = self.s.m.cmp(c, pred::IUGE, top, k20);
                let wild = self.s.m.bin(c, Bin::Or, below, past);
                self.bvh_count_if(WILD, wild);
                Some(mapped)
            }
            NodeAt::Buffer { .. } => None,
        };
        let c = self.cur;
        let box_there = match there {
            Some(m) => self.s.m.bin(c, Bin::And, is_box, m),
            None => is_box,
        };
        let box_none = self.s.m.bin(c, Bin::And, box_there, no_child);
        self.bvh_count_if(BOX_NONE, box_none);
        // A box node whose four child pointers read as 0.
        let children = self.bvh_node_words(at, None, 0, 4);
        let c = self.cur;
        let mut any = zero;
        for w in children {
            any = self.s.m.bin(c, Bin::Or, any, w);
        }
        let blank = self.s.m.cmp(c, pred::EQ, any, zero);
        let zero_box = self.s.m.bin(c, Bin::And, blank, box_there);
        self.bvh_count_if(ZERO_BOX, zero_box);
        // A walk round a cycle (Brent's): the node is the one the lane tested
        // at its last test numbered a power of two. A tree is walked without
        // coming back to a node within one ray.
        let c = self.cur;
        let (low_at, high_at) = (self.bvh_counter(CHECK_LOW), self.bvh_counter(CHECK_HIGH));
        let tests_at = self.bvh_counter(TESTS);
        let (check_low, check_high) = (self.s.m.load(c, low_at), self.s.m.load(c, high_at));
        let high = pointer_high.unwrap_or(zero);
        let same_low = self.s.m.cmp(c, pred::EQ, pointer, check_low);
        let same_high = self.s.m.cmp(c, pred::EQ, high, check_high);
        let again = self.s.m.bin(c, Bin::And, same_low, same_high);
        let n = self.s.m.load(c, tests_at);
        let k2 = k(self, 2);
        let past_first = self.s.m.cmp(c, pred::IUGE, n, k2);
        let again = self.s.m.bin(c, Bin::And, again, past_first);
        self.bvh_count_if(REPEATS, again);
        let c = self.cur;
        let again_box = self.s.m.bin(c, Bin::And, again, is_box);
        self.bvh_count_if(REPEAT_BOX, again_box);
        let c = self.cur;
        let again_other = self.s.m.bin(c, Bin::And, again, other);
        self.bvh_count_if(REPEAT_OTHER, again_other);
        if let Some(m) = there {
            let c = self.cur;
            let missing = self.s.m.bin(c, Bin::Xor, m, yes);
            let again_missing = self.s.m.bin(c, Bin::And, again, missing);
            self.bvh_count_if(REPEAT_NO_COPY, again_missing);
        }
        let c = self.cur;
        let one = k(self, 1);
        let before = self.s.m.bin(c, Bin::Sub, n, one);
        let bits = self.s.m.bin(c, Bin::And, n, before);
        let power = self.s.m.cmp(c, pred::EQ, bits, zero);
        let new_low = self.s.m.select(c, power, pointer, check_low);
        let new_high = self.s.m.select(c, power, high, check_high);
        self.s.m.store(c, low_at, new_low);
        self.s.m.store(c, high_at, new_high);
        // The 32-bit form over title memory: the structure's header at the
        // descriptor's base, and the node's place against the size it gives.
        if let (Some((word0, word1)), false) = (descriptor, wide) {
            let c = self.cur;
            let (k8, k24, kff) = (k(self, 8), k(self, 24), k(self, 0xff));
            let base_low = self.s.m.bin(c, Bin::Shl, word0, k8);
            let top = self.s.m.bin(c, Bin::LShr, word0, k24);
            let upper = self.s.m.bin(c, Bin::And, word1, kff);
            let upper = self.s.m.bin(c, Bin::Shl, upper, k8);
            let base_high = self.s.m.bin(c, Bin::Or, top, upper);
            let either = self.s.m.bin(c, Bin::Or, base_low, base_high);
            let base0 = self.s.m.cmp(c, pred::EQ, either, zero);
            self.bvh_count_if(BASE_0, base0);
            let (handle, byte, header_there) = self.bvh_memory(base_low, base_high, None);
            let words = self.bvh_node_words(NodeAt::Buffer { handle, byte }, None, 0, 2);
            let c = self.cur;
            let kffff = k(self, 0xffff);
            let version = self.s.m.bin(c, Bin::And, words[0], kffff);
            let six = self.s.m.cmp(c, pred::EQ, version, k6);
            let bad = self.s.m.bin(c, Bin::Xor, six, yes);
            let bad = self.s.m.bin(c, Bin::And, bad, header_there);
            self.bvh_count_if(BAD_HEADER, bad);
            let c = self.cur;
            let k3 = k(self, 3);
            let index = self.s.m.bin(c, Bin::LShr, pointer, k3);
            let past = self.s.m.cmp(c, pred::IUGE, index, words[1]);
            let good = self.s.m.bin(c, Bin::And, six, header_there);
            let past = self.s.m.bin(c, Bin::And, past, good);
            self.bvh_count_if(PAST_SIZE, past);
        }
    }

    /// Diagnostics: at the end of a compute program that counted its node
    /// tests, each lane adds its counts to its program's run of the root UAV
    /// `SLOT_COUNTS` (atomically, and only counts that are not 0), and itself
    /// to the histogram of lanes by the nodes they tested. A lane that leaves
    /// at the loop limit notes it on its way out.
    pub(super) fn bvh_count_flush(&mut self) {
        use super::super::shader::{COUNT_DWORDS, COUNT_HISTOGRAM, COUNT_NAMES, SLOT_COUNTS};
        use count::*;
        let (Some(_), Some(ret)) = (&self.bvh_counts, self.ret) else {
            return;
        };
        let i32t = self.s.t.i32;
        let (zero, one) = (self.s.m.i32(0), self.s.m.i32(1));
        if let Some(abort) = self.abort {
            let g = self.bvh_counter(ABORTED);
            self.s.m.store(abort, g, one);
        }
        let slot = super::super::bvh_count_slot_of(&self.x.native.code);
        #[allow(unused_mut)]
        let mut first = slot * COUNT_DWORDS * 4;
        #[cfg(test)]
        let test = TEST_COUNTS.get();
        #[cfg(not(test))]
        let test: Option<(u32, u32)> = None;
        let handle = match test {
            Some((heap, byte)) => {
                first += byte;
                let e = self.s.entry;
                let heap = self.s.m.i32(heap);
                self.s.heap_handle(e, heap, false, false, (11 | 0x1000, 0))
            }
            None => self.s.root_uav_handle(SLOT_COUNTS),
        };
        self.cur = ret;
        let iu = self.s.m.undef(i32t);
        let atomic = |l: &mut Self, op: u32, dword: V, value: V| {
            let yes = l.s.m.cmp(l.cur, pred::NE, value, zero);
            let gate = l.begin_guarded(Some(yes));
            let c = l.cur;
            let (k4, start, code) = (l.s.m.i32(4), l.s.m.i32(first), l.s.m.i32(op));
            let byte = l.s.m.bin(c, Bin::Mul, dword, k4);
            let byte = l.s.m.bin(c, Bin::Add, byte, start);
            l.s.op(c, "atomicBinOp.i32", Attr::NoUnwind, i32t, 78, &[handle, code, byte, iu, iu, value]);
            l.end_guarded(gate, &[]);
        };
        let loaded: Vec<V> = (0..=COUNT_NAMES.len())
            .map(|k| {
                let g = self.bvh_counter(k);
                self.s.m.load(ret, g)
            })
            .collect();
        let tests = loaded[TESTS];
        let run_most = self.s.m.load(self.cur, self.bvh_counts.as_ref().expect("made")[RUN_MOST]);
        let aborted = loaded[ABORTED];
        let c = self.cur;
        let (limited, repeated) = (self.s.m.cmp(c, pred::NE, aborted, zero), self.s.m.cmp(c, pred::NE, loaded[REPEATS], zero));
        let both = self.s.m.bin(c, Bin::And, limited, repeated);
        for (k, &v) in loaded.iter().enumerate().take(COUNT_NAMES.len()) {
            let c = self.cur;
            // Atomic add (0), or unsigned maximum (7).
            let (op, v) = match k {
                LANES => {
                    let ran = self.s.m.cmp(c, pred::NE, tests, zero);
                    (0, self.s.m.cast(c, Cast::ZExt, ran, i32t))
                }
                LOOP_LIMIT => (0, aborted),
                REPEAT_LANES => (0, self.s.m.cast(c, Cast::ZExt, repeated, i32t)),
                LIMIT_REPEAT_LANES => (0, self.s.m.cast(c, Cast::ZExt, both, i32t)),
                LIMIT_REPEATS => (0, self.s.m.select(c, limited, loaded[REPEATS], zero)),
                LIMIT_TESTS => (0, self.s.m.select(c, limited, tests, zero)),
                RUN_FREE => (7, self.s.m.select(c, limited, zero, run_most)),
                RUN_LIMIT => (7, self.s.m.select(c, limited, run_most, zero)),
                THREADS => (0, one),
                MOST => (7, tests),
                _ => (0, v),
            };
            let dword = self.s.m.i32(k as u32);
            atomic(self, op, dword, v);
        }
        // The histogram: a lane of n tests at bucket floor(log2 n) + 1.
        let c = self.cur;
        let mut bucket = zero;
        for b in 0..31 {
            let at_least = self.s.m.i32(1 << b);
            let over = self.s.m.cmp(c, pred::IUGE, tests, at_least);
            let n = self.s.m.cast(c, Cast::ZExt, over, i32t);
            bucket = self.s.m.bin(c, Bin::Add, bucket, n);
        }
        let hist = self.s.m.i32(COUNT_HISTOGRAM);
        let dword = self.s.m.bin(c, Bin::Add, bucket, hist);
        let ran = self.s.m.cmp(c, pred::NE, tests, zero);
        let mark = self.s.m.cast(c, Cast::ZExt, ran, i32t);
        atomic(self, 0, dword, mark);
        let end = self.cur;
        self.s.m.terminate(end, Term::Ret);
    }
}
