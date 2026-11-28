//! `cargo xtask crumbs <file>`: what a breadcrumb file says
//! (`victory_core::crumbs`).

use std::fmt::Write;

use anyhow::{Context, Result};
use victory_core::crumbs::{Report, line, parse};

/// The report of a file's `bytes`: with `records`, every record of the
/// lists the card had not finished; and the last `tail` records.
pub fn read(bytes: &[u8], records: bool, tail: usize) -> Result<String> {
    let parsed = parse(bytes).context("not a readable breadcrumb file")?;
    let mut out = String::new();
    writeln!(
        out,
        "a ring of {} records: {} kept, {} places unwritten or being written, {} numbers missing between the first and the last kept",
        parsed.capacity,
        parsed.records.len(),
        parsed.unwritten,
        parsed.missing
    )?;
    if parsed.missing > 0 {
        writeln!(out, "(missing numbers: pages of the file that the system had not written when it stopped)")?;
    }
    let report = Report::of(&parsed);
    write!(out, "{report}")?;
    if records {
        writeln!(out, "\nthe records of the lists not finished:")?;
        for (list, own) in &report.of_list {
            writeln!(out, "  list {list:#x}:")?;
            for r in own {
                writeln!(out, "    {}", line(r))?;
            }
        }
    }
    if tail > 0 {
        writeln!(out, "\nthe last {tail} records:")?;
        for r in &parsed.records[parsed.records.len().saturating_sub(tail)..] {
            writeln!(out, "  {}", line(r))?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use victory_core::crumbs::{HEADER, RECORD, Ring, exec, kind};

    /// A file as the runtime leaves it is read back: its submission that
    /// had not finished, the record outside its buffer, the records asked
    /// for.
    #[test]
    fn a_file_is_read() {
        let bytes = HEADER + 64 * RECORD;
        let at = Box::leak(vec![0u64; bytes / 8].into_boxed_slice()).as_mut_ptr();
        // SAFETY: leaked words, touched through this pointer alone.
        let ring = unsafe { Ring::new(at.cast(), bytes, 0) }.unwrap();
        ring.put(kind::FENCE, 10, [0x10, 0x20, 4, 0, 0, 0]);
        ring.put(kind::SCATTER, 20, [0xa1, 0x7000, 4, 0x9000, 0x100, 0x104]);
        ring.put(kind::EXEC, 30, [0x10, exec::LATE | (1 << 8), 5, 0xa1, 0, 0]);
        ring.put(kind::SIGNAL, 40, [0x10, 0x20, 5, 0, 0, 0]);
        // SAFETY: the leaked words; whole words are read.
        let file: Vec<u8> = (0..bytes / 8).flat_map(|i| unsafe { at.add(i).read_volatile() }.to_le_bytes()).collect();
        let text = super::read(&file, true, 2).unwrap();
        for part in [
            "a ring of 64 records: 4 kept, 60 places unwritten or being written, 0 numbers missing",
            "the main fence had completed 4 ",
            "not finished by the card: 1 executions",
            "a submission sent ahead (submission 5), signalled 5, 1 lists",
            "list 0xa1: 1 changed words stored",
            "records that name bytes outside their buffer: 1 in lists not finished, 0 in others",
            "a changed word ends at 0x104 of a copy of 0x100 bytes",
            "the records of the lists not finished:",
            "the last 2 records:",
            "#3 0.000040 s fence signalled [0x10, 0x20, 0x5, 0x0, 0x0, 0x0]",
        ] {
            assert!(text.contains(part), "{part}\n{text}");
        }
        assert!(super::read(&file[..100], false, 0).is_err());
    }
}
