//! The process's allocator: the system's, except that a block that grows
//! is moved by us.
//!
//! Rust's allocator for Windows grows a block with `HeapReAlloc`. Under
//! Wine that copies with ntdll's own `memcpy`, a byte at a time: 21 ms a
//! frame of the process's processor time in the world (16 of the relay
//! thread's 46 ms a frame, growing the queue of objects let go), beside
//! the C runtime's vectorised copy that every other copy of ours uses.
//! Here a block that grows is a new block, the bytes copied with
//! `copy_nonoverlapping` (the C runtime's `memcpy`), and the old block let
//! go: what `GlobalAlloc`'s own default does. A block that shrinks stays
//! with `HeapReAlloc`, which keeps it where it is.
//!
//! Every block still comes from the system's heap (`HeapAlloc`), as
//! before: no address range is reserved by us, so the title's fixed ranges
//! (its image, the memory the card sees) stay as free as they were.
//! `VICTORY_REALLOC_COPY=0`: `HeapReAlloc` for every block, as before
//! (read once, at the first block that grows, without allocating).
//!
//! With the `diagnostics` feature the counting allocator of
//! `crate::copystat` is the process's, and it passes on to this one.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};

/// The system's allocator with blocks that grow moved by us.
pub(crate) struct Heap;

/// 0: not read yet; 1: blocks that grow are moved by us; 2: not.
static COPY: AtomicU8 = AtomicU8::new(0);

/// Whether a block that grows is moved by us (`VICTORY_REALLOC_COPY`, on
/// unless `0`). Read with the system's call into a buffer on the stack:
/// an allocation here would come back to this allocator.
fn copy_on() -> bool {
    match COPY.load(Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let name: Vec16 = Vec16::of("VICTORY_REALLOC_COPY");
            let mut value = [0u16; 8];
            // SAFETY: a NUL-terminated name and a buffer of the size given.
            let n = unsafe {
                windows_sys::Win32::System::Environment::GetEnvironmentVariableW(
                    name.0.as_ptr(),
                    value.as_mut_ptr(),
                    value.len() as u32,
                )
            };
            let on = !(n == 1 && value[0] == u16::from(b'0'));
            COPY.store(if on { 1 } else { 2 }, Relaxed);
            on
        }
    }
}

/// A short NUL-terminated UTF-16 name, on the stack.
struct Vec16([u16; 32]);

impl Vec16 {
    fn of(name: &str) -> Self {
        let mut out = [0u16; 32];
        for (o, b) in out.iter_mut().zip(name.bytes().take(31)) {
            *o = u16::from(b);
        }
        Self(out)
    }
}

/// `at`'s block of `layout` grown to `size` bytes: a new block from the
/// system's heap with the old one's bytes, and the old one let go; null
/// (the old block left as it was) if there is no room.
///
/// # Safety
/// As `GlobalAlloc::realloc`, with `size` above `layout.size()`.
unsafe fn grow_by_copy(at: *mut u8, layout: Layout, size: usize) -> *mut u8 {
    // SAFETY: the caller's contract: `size`, rounded up to the alignment,
    // does not overflow, and the alignment is that of a valid layout.
    let grown = unsafe { Layout::from_size_align_unchecked(size, layout.align()) };
    // SAFETY: a layout of non-zero size (larger than the old one's).
    let moved = unsafe { System.alloc(grown) };
    if !moved.is_null() {
        // SAFETY: the old block holds `layout.size()` bytes, fewer than the
        // new one; two live blocks of the heap do not overlap. The old
        // block is let go with the layout it was made with.
        unsafe {
            std::ptr::copy_nonoverlapping(at, moved, layout.size());
            System.dealloc(at, layout);
        }
    }
    moved
}

// SAFETY: every block comes from and goes back to the system's allocator
// with its own layout; a block that grows is a new block of the system's
// with the old one's bytes (`GlobalAlloc`'s default `realloc`).
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(at, layout) }
    }

    unsafe fn realloc(&self, at: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if size > layout.size() && copy_on() {
            // SAFETY: the caller's contract, and `size` is larger.
            return unsafe { grow_by_copy(at, layout, size) };
        }
        // SAFETY: the caller's contract, passed on.
        unsafe { System.realloc(at, layout, size) }
    }
}

#[cfg(not(feature = "diagnostics"))]
#[global_allocator]
static HEAP: Heap = Heap;

#[cfg(test)]
mod tests {
    use super::{Heap, grow_by_copy};
    use std::alloc::{GlobalAlloc, Layout, System};

    fn fill(at: *mut u8, n: usize, seed: u8) {
        for i in 0..n {
            // SAFETY: inside a block of at least `n` bytes.
            unsafe { at.add(i).write(seed.wrapping_add((i * 7) as u8)) };
        }
    }

    fn holds(at: *const u8, n: usize, seed: u8) -> bool {
        // SAFETY: inside a block of at least `n` bytes.
        (0..n).all(|i| unsafe { at.add(i).read() } == seed.wrapping_add((i * 7) as u8))
    }

    /// A block grown keeps its bytes and its alignment, from small blocks
    /// to ones the heap gives from fresh pages, for alignments past the
    /// heap's own; one shrunk keeps the bytes it still holds. Grown by copy
    /// or by the system's `HeapReAlloc` (`VICTORY_REALLOC_COPY=0`), the
    /// blocks are the same heap's: either may grow, shrink or free them.
    #[test]
    fn a_grown_block_keeps_its_bytes_and_alignment() {
        for align in [1usize, 8, 16, 64, 4096] {
            for (from, to) in [(1usize, 2usize), (24, 100), (4000, 4097), (100_000, 1 << 20), (3 << 20, 9 << 20)] {
                let layout = Layout::from_size_align(from, align).unwrap();
                for by_copy in [true, false] {
                    // SAFETY: a valid layout of non-zero size; the blocks
                    // are let go with the layouts they have.
                    unsafe {
                        let at = Heap.alloc(layout);
                        assert!(!at.is_null());
                        fill(at, from, align as u8);
                        let grown = if by_copy { grow_by_copy(at, layout, to) } else { System.realloc(at, layout, to) };
                        assert!(!grown.is_null());
                        assert_eq!(grown as usize % align, 0, "align {align}, {from} to {to}");
                        assert!(holds(grown, from, align as u8), "align {align}, {from} to {to}, by copy {by_copy}");
                        // The new bytes are the block's to write.
                        fill(grown, to, 3);
                        let bigger = Layout::from_size_align(to, align).unwrap();
                        let shrunk = Heap.realloc(grown, bigger, from);
                        assert!(!shrunk.is_null());
                        assert_eq!(shrunk as usize % align, 0);
                        assert!(holds(shrunk, from, 3));
                        Heap.dealloc(shrunk, layout);
                    }
                }
            }
        }
    }

    /// The process's allocator (this one, without `diagnostics`): vectors
    /// and strings that grow by doubling keep what they held.
    #[test]
    fn growing_vectors_keep_what_they_held() {
        let mut v: Vec<u64> = Vec::new();
        for i in 0..300_000u64 {
            v.push(i.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        }
        assert!(v.iter().enumerate().all(|(i, &x)| x == (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)));
        let mut s = String::new();
        for i in 0..50_000 {
            s.push_str(&i.to_string());
        }
        assert!(s.starts_with("0123456789101112"));
        let mut d = std::collections::VecDeque::new();
        for i in 0..100_000u32 {
            d.push_back(i);
            if i % 3 == 0 {
                d.pop_front();
            }
        }
        assert_eq!(d.len(), 100_000 - 33_334);
        assert_eq!(d.back(), Some(&99_999));
    }
}
