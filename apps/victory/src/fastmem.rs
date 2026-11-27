//! `memcmp` of our own, for the whole program.
//!
//! The runtime compares memory all the time (title memory against the
//! shadow copies the card has, what shaders wrote against what was there,
//! descriptor records against the last ones), and every such comparison of
//! slices (`==`, `!=`, `cmp`) is a call of the C runtime's `memcmp`,
//! imported from `VCRUNTIME140.dll`. Under Wine (Proton) that export is
//! forwarded to Wine's `ucrtbase.dll`, whose `memcmp` is a scalar loop.
//! Defined here, the linker takes this one before the import, for every
//! crate linked into the program (not for the title's own code, whose
//! imports the loader binds).
//!
//! Same contract as C's, and the same values as Wine's and Microsoft's
//! runtimes give: 1 or -1 by the first differing byte taken as unsigned, of
//! `a` against `b`; 0 when the `n` bytes are equal. No byte at or past `n`
//! is read on either side, so memory that ends at a guarded page compares
//! as it did.
//!
//! `VICTORY_FASTMEM=0` gives the C runtime's own back (looked up once by
//! name), for comparison.
//!
//! From SlaktarnVI (`apps/victory/src/fastmem.rs`, commit 03f0f10).

// Only a Windows x86-64 program has the `memcmp` that uses the rest.
#![cfg_attr(not(all(windows, target_arch = "x86_64")), allow(dead_code))]

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{__m256i, _mm256_and_si256, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8};
#[cfg(all(windows, target_arch = "x86_64"))]
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

type Memcmp = unsafe extern "C" fn(*const core::ffi::c_void, *const core::ffi::c_void, usize) -> i32;

/// Not decided yet.
#[cfg(all(windows, target_arch = "x86_64"))]
const UNDECIDED: u8 = 0;
/// Being decided (what decides may compare too).
#[cfg(all(windows, target_arch = "x86_64"))]
const DECIDING: u8 = 1;
/// Ours, with AVX2.
const AVX2: u8 = 2;
/// Ours, eight bytes a turn (a CPU without AVX2).
const WORDS: u8 = 3;
/// The C runtime's (`VICTORY_FASTMEM=0`), at [`CRT`].
const RUNTIMES: u8 = 4;

#[cfg(all(windows, target_arch = "x86_64"))]
static MODE: AtomicU8 = AtomicU8::new(UNDECIDED);
/// The C runtime's `memcmp`, when [`MODE`] is [`RUNTIMES`] (stored before).
#[cfg(all(windows, target_arch = "x86_64"))]
static CRT: AtomicUsize = AtomicUsize::new(0);

/// # Safety
/// `a` and `b` are readable for `n` bytes.
// Never inlined: a call of `memcmp` of a few bytes known when compiled
// (`a[i..i + 8] == b[i..i + 8]`) is then still made a plain load and
// compare by the compiler, as with the C runtime's.
#[cfg(all(windows, target_arch = "x86_64"))]
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn memcmp(a: *const core::ffi::c_void, b: *const core::ffi::c_void, n: usize) -> i32 {
    let (a, b) = (a.cast::<u8>(), b.cast::<u8>());
    // SAFETY: `a` and `b` are readable for `n` bytes (the caller's
    // promise, passed on unchanged). `compare_avx2` runs only in the AVX2
    // mode, which `decide` sets only where the CPU (and the system) has
    // AVX2. The runtime's function is read only in that mode, and `decide`
    // stored a non-zero address of the C runtime's `memcmp` before it
    // published the mode (release, read here with acquire).
    unsafe {
        match MODE.load(Ordering::Acquire) {
            AVX2 => compare_avx2(a, b, n),
            RUNTIMES => std::mem::transmute::<usize, Memcmp>(CRT.load(Ordering::Relaxed))(a.cast(), b.cast(), n),
            UNDECIDED => first(a, b, n),
            _ => compare_words(a, b, n),
        }
    }
}

/// The first comparison: decides, then compares as decided.
///
/// # Safety
/// `a` and `b` are readable for `n` bytes.
#[cfg(all(windows, target_arch = "x86_64"))]
#[cold]
#[inline(never)]
unsafe fn first(a: *const u8, b: *const u8, n: usize) -> i32 {
    decide();
    // SAFETY: the caller's. After `decide` the mode is decided or being
    // decided (by this thread or another), so this is no loop.
    unsafe { memcmp(a.cast(), b.cast(), n) }
}

/// Which `memcmp` runs, decided once: ours (with AVX2 where the CPU has it),
/// or the C runtime's with `VICTORY_FASTMEM=0`. Allocates nothing and takes
/// no lock of ours, as it can run inside any comparison; a comparison made
/// meanwhile (by this or another thread) gets [`compare_words`].
#[cfg(all(windows, target_arch = "x86_64"))]
fn decide() {
    if MODE.compare_exchange(UNDECIDED, DECIDING, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetEnvironmentVariableW(name: *const u16, buffer: *mut u16, size: u32) -> u32;
        fn GetModuleHandleW(name: *const u16) -> *mut core::ffi::c_void;
        fn GetProcAddress(module: *mut core::ffi::c_void, name: *const u8) -> usize;
    }
    let wide = |s: &str, out: &mut [u16; 32]| {
        for (o, c) in out.iter_mut().zip(s.encode_utf16().chain([0])) {
            *o = c;
        }
    };
    let mut name = [0u16; 32];
    wide("VICTORY_FASTMEM", &mut name);
    let mut value = [0u16; 8];
    // SAFETY: `name` is NUL-terminated and `value` holds the 8 units we
    // say; the call writes at most that many.
    let len = unsafe { GetEnvironmentVariableW(name.as_ptr(), value.as_mut_ptr(), value.len() as u32) };
    let off = len == 1 && value[0] == u16::from(b'0');
    let mut crt = 0;
    if off {
        // The library the program imports `memcmp` from, then Wine's and
        // Microsoft's universal runtime.
        for dll in ["vcruntime140.dll", "ucrtbase.dll"] {
            wide(dll, &mut name);
            // SAFETY: plain lookups, by NUL-terminated names, of a library
            // the program already has loaded (`GetModuleHandleW` loads
            // nothing; the program imports `memcpy` and more from
            // `vcruntime140.dll`) and of its export, a `memcmp`.
            crt = unsafe {
                let module = GetModuleHandleW(name.as_ptr());
                if module.is_null() { 0 } else { GetProcAddress(module, c"memcmp".as_ptr().cast()) }
            };
            if crt != 0 {
                break;
            }
        }
    }
    let mode = pick(off, crt != 0, std::arch::is_x86_feature_detected!("avx2"));
    CRT.store(crt, Ordering::Relaxed);
    MODE.store(mode, Ordering::Release);
}

/// The mode for: switched off, the runtime's found, the CPU has AVX2. Off
/// without the runtime's found is ours all the same.
fn pick(off: bool, found: bool, avx2: bool) -> u8 {
    match (off && found, avx2) {
        (true, _) => RUNTIMES,
        (false, true) => AVX2,
        (false, false) => WORDS,
    }
}

/// The ordering of the bytes at `i`, which differ.
///
/// # Safety
/// Both readable at `i`.
#[inline(always)]
unsafe fn at(a: *const u8, b: *const u8, i: usize) -> i32 {
    // -1 or 1, as Wine's and Microsoft's runtimes give (C asks only for the
    // sign): what we hand back is what callers had before.
    // SAFETY: as the caller says.
    unsafe { if *a.add(i) > *b.add(i) { 1 } else { -1 } }
}

/// Eight bytes a turn, then bytes. Written so that it is no call of
/// `memcmp` itself.
///
/// # Safety
/// `a` and `b` are readable for `n` bytes.
#[inline(never)]
unsafe fn compare_words(a: *const u8, b: *const u8, n: usize) -> i32 {
    // SAFETY: the caller's; `0 <= n`.
    unsafe { tail(a, b, 0, n) }
}

/// From `i` on, eight bytes a turn, then bytes.
///
/// # Safety
/// `a` and `b` are readable for `n` bytes, `i <= n`.
#[inline(always)]
unsafe fn tail(a: *const u8, b: *const u8, mut i: usize, n: usize) -> i32 {
    // SAFETY (the whole function): every read is of bytes `i..i + 8` with
    // `i + 8 <= n`, or of byte `i` with `i < n`: inside both. The words
    // are read unaligned.
    unsafe {
        while i + 8 <= n {
            let (x, y) = (a.add(i).cast::<u64>().read_unaligned(), b.add(i).cast::<u64>().read_unaligned());
            if x != y {
                // Little endian: the first differing byte is the lowest.
                return at(a, b, i + ((x ^ y).trailing_zeros() / 8) as usize);
            }
            i += 8;
        }
        while i < n {
            if *a.add(i) != *b.add(i) {
                return at(a, b, i);
            }
            i += 1;
        }
        0
    }
}

/// # Safety
/// `a` and `b` are readable for `n` bytes; the CPU has AVX2.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn compare_avx2(a: *const u8, b: *const u8, n: usize) -> i32 {
    // SAFETY (the whole function): every load is of 32 bytes at `i` with
    // `i + 32 <= n` (unaligned loads), and `tail` reads only below `n`;
    // the AVX2 instructions are there (the caller's promise).
    unsafe {
        let load = |p: *const u8, i: usize| _mm256_loadu_si256(p.add(i).cast::<__m256i>());
        let equal = |i: usize| _mm256_cmpeq_epi8(load(a, i), load(b, i));
        let first = |mask: u32, i: usize| at(a, b, i + (!mask).trailing_zeros() as usize);
        let mut i = 0;
        // 128 bytes a turn: four compares joined, looked into only where one
        // differs.
        while i + 128 <= n {
            let (e0, e1, e2, e3) = (equal(i), equal(i + 32), equal(i + 64), equal(i + 96));
            let all = _mm256_and_si256(_mm256_and_si256(e0, e1), _mm256_and_si256(e2, e3));
            if _mm256_movemask_epi8(all) as u32 != u32::MAX {
                for (k, e) in [e0, e1, e2, e3].into_iter().enumerate() {
                    let mask = _mm256_movemask_epi8(e) as u32;
                    if mask != u32::MAX {
                        return first(mask, i + 32 * k);
                    }
                }
            }
            i += 128;
        }
        while i + 32 <= n {
            let mask = _mm256_movemask_epi8(equal(i)) as u32;
            if mask != u32::MAX {
                return first(mask, i);
            }
            i += 32;
        }
        tail(a, b, i, n)
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    fn avx2() -> bool {
        std::arch::is_x86_feature_detected!("avx2")
    }

    /// Both of ours (the AVX2 one where the CPU has it), named for messages.
    fn both() -> Vec<(&'static str, unsafe fn(*const u8, *const u8, usize) -> i32)> {
        let mut out: Vec<(&'static str, unsafe fn(*const u8, *const u8, usize) -> i32)> = vec![("words", compare_words)];
        if avx2() {
            /// # Safety
            /// [`compare_avx2`]'s but the CPU's, which `both` checked.
            unsafe fn avx2_checked(a: *const u8, b: *const u8, n: usize) -> i32 {
                // SAFETY: the caller's; the CPU has AVX2.
                unsafe { compare_avx2(a, b, n) }
            }
            out.push(("avx2", avx2_checked));
        }
        out
    }

    /// The reference: byte by byte.
    fn reference(a: &[u8], b: &[u8]) -> i32 {
        for (x, y) in a.iter().zip(b) {
            if x != y {
                return if x > y { 1 } else { -1 };
            }
        }
        0
    }

    fn bytes(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect()
    }

    /// Every length 0..=300, equal and with one byte different (lower and
    /// higher) at every place, with `a` at every start 0..=31 in a page and
    /// `b` at the same start, at 31 less it and at another: ours agree with
    /// the reference in value. Over all 32 x 32 starts, the first, middle
    /// and last places.
    #[test]
    fn agrees_with_the_reference_everywhere() {
        let page = 4096;
        // Page-aligned room for 32 + 300 bytes on each side.
        let mut room_a = vec![0u8; 2 * page];
        let mut room_b = vec![0u8; 2 * page];
        let pa = room_a.as_ptr().align_offset(page);
        let pb = room_b.as_ptr().align_offset(page);
        let fill = bytes(400, 7);
        let fs = both();
        let mut checked = 0u64;
        for oa in 0..32usize {
            for ob in 0..32usize {
                let all_places = ob == oa || ob == 31 - oa || ob == (oa * 5 + 3) % 32;
                let a = &mut room_a[pa + oa..pa + oa + 301];
                a.copy_from_slice(&fill[oa..oa + 301]);
                let a: &[u8] = a;
                let b = &mut room_b[pb + ob..pb + ob + 301];
                b.copy_from_slice(a);
                for n in 0..=300usize {
                    let (a, b) = (&a[..n], &mut b[..n]);
                    let places: Vec<usize> =
                        if all_places { (0..n).collect() } else { [0, n / 2, n.wrapping_sub(1)].into_iter().filter(|&p| p < n).collect() };
                    for &(name, f) in &fs {
                        // SAFETY: both hold `n` bytes.
                        assert_eq!(unsafe { f(a.as_ptr(), b.as_ptr(), n) }, 0, "{name} equal n={n} starts {oa} {ob}");
                    }
                    for &place in &places {
                        for change in [1u8, 255] {
                            b[place] = a[place].wrapping_add(change);
                            let want = reference(a, b);
                            assert_ne!(want, 0);
                            for &(name, f) in &fs {
                                // SAFETY: as above.
                                let got = unsafe { f(a.as_ptr(), b.as_ptr(), n) };
                                assert_eq!(got, want, "{name} n={n} at {place} starts {oa} {ob}");
                                checked += 1;
                            }
                            b[place] = a[place];
                        }
                    }
                }
            }
        }
        assert!(checked > 1_000_000, "{checked}");
    }

    /// Unsigned: 0x80 and more are above 0x7f, at every place of a block.
    #[test]
    fn bytes_are_taken_as_unsigned() {
        let fs = both();
        for n in [1usize, 7, 8, 9, 31, 32, 33, 127, 128, 129, 200] {
            for place in 0..n {
                let a = vec![0x7fu8; n];
                let mut b = a.clone();
                b[place] = 0x80;
                for &(name, f) in &fs {
                    // SAFETY: both hold `n` bytes.
                    unsafe {
                        assert_eq!(f(a.as_ptr(), b.as_ptr(), n), -1, "{name} n={n} at {place}");
                        assert_eq!(f(b.as_ptr(), a.as_ptr(), n), 1, "{name} n={n} at {place}");
                    }
                }
            }
        }
    }

    /// The mode chosen for the switch, the runtime's being found, AVX2.
    #[test]
    fn the_switch_gives_the_runtimes_own_only_where_found() {
        assert_eq!(pick(false, false, true), AVX2);
        assert_eq!(pick(false, true, true), AVX2);
        assert_eq!(pick(false, true, false), WORDS);
        assert_eq!(pick(true, true, true), RUNTIMES);
        assert_eq!(pick(true, true, false), RUNTIMES);
        assert_eq!(pick(true, false, true), AVX2);
        assert_eq!(pick(true, false, false), WORDS);
    }

    /// Slices compared by Rust go through `memcmp` (this one, in the test
    /// program too): their ordering is unchanged.
    #[cfg(windows)]
    #[test]
    fn slice_ordering_goes_through_it() {
        let a = bytes(4096, 3);
        let mut b = a.clone();
        assert!(a == b);
        b[4000] = b[4000].wrapping_add(1);
        assert_eq!(a.cmp(&b), reference(&a, &b).cmp(&0));
        assert!(a != b);
        // SAFETY: both hold the bytes.
        unsafe {
            assert_eq!(memcmp(a.as_ptr().cast(), b.as_ptr().cast(), 4096), reference(&a, &b));
            assert_eq!(memcmp(a.as_ptr().cast(), b.as_ptr().cast(), 4000), 0);
        }
    }

    #[cfg(windows)]
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryA(name: *const u8) -> *mut core::ffi::c_void;
        fn GetProcAddress(lib: *mut core::ffi::c_void, name: *const u8) -> usize;
        fn VirtualAlloc(at: *mut core::ffi::c_void, size: usize, kind: u32, protect: u32) -> *mut core::ffi::c_void;
        fn VirtualProtect(at: *mut core::ffi::c_void, size: usize, protect: u32, old: *mut u32) -> i32;
        fn VirtualFree(at: *mut core::ffi::c_void, size: usize, kind: u32) -> i32;
    }

    /// The C runtime's own `memcmp` (Wine's `ucrtbase.dll` under Proton).
    #[cfg(windows)]
    fn runtimes() -> Option<Memcmp> {
        // SAFETY: plain lookups of a system library's export, by
        // NUL-terminated names; the address is that of a `memcmp`.
        unsafe {
            let lib = LoadLibraryA(c"ucrtbase.dll".as_ptr().cast());
            let f = if lib.is_null() { 0 } else { GetProcAddress(lib, c"memcmp".as_ptr().cast()) };
            (f != 0).then(|| std::mem::transmute::<usize, Memcmp>(f))
        }
    }

    /// One readable and writable page followed by one that cannot be
    /// touched: a read past the first page's end faults.
    #[cfg(windows)]
    struct Guarded(*mut u8);

    #[cfg(windows)]
    impl Guarded {
        const PAGE: usize = 4096;

        fn new() -> Guarded {
            const MEM_COMMIT: u32 = 0x1000;
            const MEM_RESERVE: u32 = 0x2000;
            const PAGE_READWRITE: u32 = 0x04;
            const PAGE_NOACCESS: u32 = 0x01;
            // SAFETY: a fresh allocation of two pages, the second of which
            // is made inaccessible; nothing else holds them.
            unsafe {
                let at = VirtualAlloc(std::ptr::null_mut(), 2 * Self::PAGE, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE).cast::<u8>();
                assert!(!at.is_null());
                let mut old = 0;
                assert_ne!(VirtualProtect(at.add(Self::PAGE).cast(), Self::PAGE, PAGE_NOACCESS, &mut old), 0);
                Guarded(at)
            }
        }

        /// The last `n` bytes of the readable page.
        fn last(&mut self, n: usize) -> &mut [u8] {
            assert!(n <= Self::PAGE);
            // SAFETY: inside the readable page, borrowed from `self`.
            unsafe { std::slice::from_raw_parts_mut(self.0.add(Self::PAGE - n), n) }
        }
    }

    #[cfg(windows)]
    impl Drop for Guarded {
        fn drop(&mut self) {
            const MEM_RELEASE: u32 = 0x8000;
            // SAFETY: the allocation `new` made, released whole.
            unsafe { VirtualFree(self.0.cast(), 0, MEM_RELEASE) };
        }
    }

    /// Bytes that end where a page ends, followed by one that faults when
    /// read: every length 0..=300 and some to a whole page, equal and with
    /// one byte different at every place (or every 61st past 300), both
    /// ending there and one of them ending there. Nothing past the end is
    /// read (or the test program would fault), and the values are the
    /// reference's.
    #[cfg(windows)]
    #[test]
    fn nothing_past_the_end_is_read() {
        let (mut ga, mut gb) = (Guarded::new(), Guarded::new());
        let mut free = vec![0u8; 5000];
        let fill = bytes(Guarded::PAGE, 13);
        let fs = both();
        let mut checked = 0u64;
        for n in (0..=300usize).chain([301, 511, 512, 513, 1000, 4095, 4096]) {
            let a = ga.last(n);
            a.copy_from_slice(&fill[..n]);
            let a: &[u8] = a;
            let b = gb.last(n);
            b.copy_from_slice(a);
            // `b` ending at the page, `a` elsewhere at an odd start.
            let c = &mut free[3..3 + n];
            c.copy_from_slice(a);
            let step = if n <= 300 { 1 } else { 61 };
            for &(name, f) in &fs {
                // SAFETY: each holds `n` bytes.
                unsafe {
                    assert_eq!(f(a.as_ptr(), b.as_ptr(), n), 0, "{name} n={n}");
                    assert_eq!(f(c.as_ptr(), b.as_ptr(), n), 0, "{name} n={n}");
                    assert_eq!(memcmp(a.as_ptr().cast(), b.as_ptr().cast(), n), 0, "memcmp n={n}");
                }
            }
            for place in (0..n).step_by(step).chain(n.checked_sub(1)) {
                for change in [1u8, 255] {
                    b[place] = a[place].wrapping_add(change);
                    let want = reference(a, b);
                    for &(name, f) in &fs {
                        // SAFETY: as above.
                        unsafe {
                            assert_eq!(f(a.as_ptr(), b.as_ptr(), n), want, "{name} n={n} at {place}");
                            assert_eq!(f(b.as_ptr(), a.as_ptr(), n), -want, "{name} n={n} at {place}");
                            assert_eq!(f(c.as_ptr(), b.as_ptr(), n), want, "{name} n={n} at {place}");
                        }
                        checked += 1;
                    }
                    // SAFETY: as above.
                    unsafe { assert_eq!(memcmp(a.as_ptr().cast(), b.as_ptr().cast(), n), want, "memcmp n={n} at {place}") };
                    b[place] = a[place];
                }
            }
        }
        assert!(checked > 40_000, "{checked}");
    }

    /// The very values Wine's (or Microsoft's) runtime gives, for equal and
    /// differing bytes of many lengths and places.
    #[cfg(windows)]
    #[test]
    fn same_values_as_the_runtimes_own() {
        let theirs = runtimes().expect("ucrtbase.dll's memcmp");
        let fs = both();
        let a = bytes(5000, 21);
        let mut checked = 0;
        for n in (0..5000).step_by(37) {
            let mut b = a[..n].to_vec();
            for place in (0..n).step_by(53).chain(n.checked_sub(1)) {
                for change in [1u8, 128, 255] {
                    b[place] = a[place].wrapping_add(change);
                    // SAFETY: both hold `n` bytes.
                    let wine = unsafe { theirs(a.as_ptr().cast(), b.as_ptr().cast(), n) };
                    for &(name, f) in &fs {
                        // SAFETY: as above.
                        assert_eq!(unsafe { f(a.as_ptr(), b.as_ptr(), n) }, wine, "{name} n={n} at {place}");
                    }
                    b[place] = a[place];
                    checked += 1;
                }
            }
            // SAFETY: as above.
            let wine = unsafe { theirs(a.as_ptr().cast(), b.as_ptr().cast(), n) };
            for &(name, f) in &fs {
                // SAFETY: as above.
                assert_eq!(unsafe { f(a.as_ptr(), b.as_ptr(), n) }, wine, "{name} n={n} equal");
            }
        }
        assert!(checked > 1000);
    }

    /// Against the C runtime's own `memcmp`, equal bytes of four sizes.
    /// Printed, not asserted (and in a test build without optimisation
    /// ours is slower than it is in the program: time it in a release
    /// test build).
    #[cfg(windows)]
    #[test]
    fn timed_against_the_runtimes_own() {
        let theirs = runtimes();
        let fs = both();
        for (size, turns) in [(64usize, 2_000_000u32), (4 << 10, 200_000), (64 << 10, 20_000), (64 << 20, 10)] {
            let a = bytes(size, 11);
            let b = a.clone();
            let time = |f: &dyn Fn() -> i32| {
                let start = std::time::Instant::now();
                for _ in 0..turns {
                    assert_eq!(std::hint::black_box(f()), 0);
                }
                start.elapsed().as_secs_f64() * 1e6 / f64::from(turns)
            };
            let (pa, pb) = (std::hint::black_box(a.as_ptr()), std::hint::black_box(b.as_ptr()));
            for &(name, f) in &fs {
                // SAFETY: both hold the bytes.
                let mine = time(&|| unsafe { f(std::hint::black_box(pa), pb, size) });
                match theirs {
                    Some(theirs) => {
                        // SAFETY: as above.
                        let wine = time(&|| unsafe { theirs(std::hint::black_box(pa).cast(), pb.cast(), size) });
                        eprintln!("memcmp of {size} bytes: ours ({name}) {mine:.4} us, ucrtbase's {wine:.4} us ({:.2}x)", wine / mine);
                    }
                    None => eprintln!("memcmp of {size} bytes: ours ({name}) {mine:.4} us (no ucrtbase)"),
                }
            }
        }
    }
}
