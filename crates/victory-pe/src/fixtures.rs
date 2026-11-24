//! Hand-built PE images for tests.
//!
//! Tests that need a real PE32+ image used to read `current_exe()`. That only
//! works when the tests are built for Windows: on any other host the test
//! binary is an ELF and the parser rightly rejects it. Building the image here
//! keeps those tests host-independent, and pins down exactly what they assert
//! instead of depending on whatever the current linker emits.
//!
//! Enabled by the `fixtures` feature (and inside this crate's own tests).

use crate::MACHINE_AMD64;
use crate::layout;

/// Data directory index of the TLS directory.
const DIR_TLS: usize = 9;

const IMAGE_BASE: u64 = 0x1_4000_0000;
const RDATA_RVA: u32 = 0x2000;
const RDATA_OFFSET: usize = 0x600;

fn put16(d: &mut [u8], at: usize, v: u16) {
    d[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(d: &mut [u8], at: usize, v: u32) {
    d[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(d: &mut [u8], at: usize, v: u64) {
    d[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// A complete PE32+ executable: two sections (`.text` at RVA 0x1000,
/// `.rdata` at 0x2000), an entry point, one KERNEL32 import with a real
/// import name table and IAT, a TLS directory with no callbacks, and a
/// one-entry exception table.
///
/// Everything the loader and the image parser look at is present, and all
/// sixteen data directories exist so that callers may address any of them.
pub fn executable() -> Vec<u8> {
    let mut d = vec![0u8; 0x400];
    d[0..2].copy_from_slice(b"MZ");
    put32(&mut d, layout::DOS_E_LFANEW, 0x40);
    d[0x40..0x44].copy_from_slice(b"PE\0\0");

    // IMAGE_FILE_HEADER. No DLL flag: this is an executable.
    put16(&mut d, 0x44, MACHINE_AMD64);
    put16(&mut d, 0x46, 2); // sections
    put16(&mut d, 0x54, 240); // optional header size
    put16(&mut d, 0x56, 0x0022); // executable image, large address aware

    let opt = 0x58;
    put16(&mut d, opt, 0x20B); // PE32+
    put32(&mut d, opt + layout::OPT_ADDRESS_OF_ENTRY_POINT, 0x1000);
    put64(&mut d, opt + 24, IMAGE_BASE);
    put32(&mut d, opt + 32, 0x1000); // section alignment
    put32(&mut d, opt + 36, 0x200); // file alignment
    put32(&mut d, opt + 56, 0x3000); // size of image
    put32(&mut d, opt + 60, 0x400); // size of headers
    put16(&mut d, opt + 68, layout::subsystem::WINDOWS_CUI);
    put32(&mut d, opt + layout::OPT_NUMBER_OF_RVA_AND_SIZES, 16);

    let dd = |i: usize| opt + layout::OPT_DATA_DIRECTORY + i * layout::DATA_DIRECTORY_SIZE;
    put32(&mut d, dd(layout::dir::IMPORT), RDATA_RVA);
    put32(&mut d, dd(layout::dir::IMPORT) + 4, 40); // descriptor + terminator
    put32(&mut d, dd(layout::dir::EXCEPTION), 0x2300);
    put32(&mut d, dd(layout::dir::EXCEPTION) + 4, 12); // one RUNTIME_FUNCTION
    put32(&mut d, dd(DIR_TLS), 0x2200);
    put32(&mut d, dd(DIR_TLS) + 4, 40);
    // The IAT directory describes the same slots as the descriptor's thunks.
    put32(&mut d, dd(layout::dir::IAT), 0x2050);
    put32(&mut d, dd(layout::dir::IAT) + 4, 24);

    let mut section = |i: usize, name: &[u8], rva: u32, size: u32, raw: u32| {
        let s = opt + 240 + i * 40;
        d[s..s + name.len()].copy_from_slice(name);
        put32(&mut d, s + 8, size); // virtual size
        put32(&mut d, s + 12, rva);
        put32(&mut d, s + 16, size); // raw size
        put32(&mut d, s + 20, raw); // raw offset
    };
    section(0, b".text", 0x1000, 0x200, 0x400);
    section(1, b".rdata", RDATA_RVA, 0x400, RDATA_OFFSET as u32);

    d.resize(0xA00, 0); // .text at 0x400, .rdata at 0x600
    let at = |rva: u32| (rva - RDATA_RVA) as usize + RDATA_OFFSET;

    // IMAGE_IMPORT_DESCRIPTOR; the next 20 bytes stay zero to terminate it.
    put32(&mut d, at(0x2000), 0x2030); // OriginalFirstThunk
    put32(&mut d, at(0x2000) + 12, 0x2080); // Name
    put32(&mut d, at(0x2000) + 16, 0x2050); // FirstThunk, i.e. the IAT
    // Import name table and IAT: the same two named imports, zero-terminated.
    for base in [0x2030u32, 0x2050] {
        put64(&mut d, at(base), 0x2100);
        put64(&mut d, at(base) + 8, 0x2110);
    }
    d[at(0x2080)..at(0x2080) + 13].copy_from_slice(b"KERNEL32.dll\0");
    d[at(0x2100) + 2..at(0x2100) + 15].copy_from_slice(b"VirtualAlloc\0");
    d[at(0x2110) + 2..at(0x2110) + 14].copy_from_slice(b"CloseHandle\0");

    // IMAGE_TLS_DIRECTORY64. AddressOfCallBacks stays 0: an image using
    // static TLS legitimately has no callbacks.
    put64(&mut d, at(0x2200), IMAGE_BASE + 0x1000);
    put64(&mut d, at(0x2200) + 8, IMAGE_BASE + 0x1010);
    put64(&mut d, at(0x2200) + 16, IMAGE_BASE + 0x1020);

    // One RUNTIME_FUNCTION, so the exception directory is non-empty.
    put32(&mut d, at(0x2300), 0x1000);
    put32(&mut d, at(0x2300) + 4, 0x1010);
    put32(&mut d, at(0x2300) + 8, 0x2310);
    d
}
