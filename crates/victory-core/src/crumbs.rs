//! Breadcrumbs: what was sent to the card last, kept where a lost device
//! cannot take it.
//!
//! When the card stops with a page fault the device is lost, and often the
//! session with it: the log's last lines are not written, and nothing says
//! which of the submissions in the air named the address. A diagnostics
//! build with `VICTORY_CRUMBS=1` therefore notes, as it records and sends,
//! every submission and what its lists hold that reaches memory by an
//! address of ours (our passes with root views, the copies and slots of a
//! card-step list, the buffers a list uploads from), with every address,
//! offset and width, in a ring of fixed records in a file that is mapped
//! into the process: what is written there is the system's from that moment
//! on, whatever becomes of the process.
//!
//! This module is the ring, the records and the reader, in plain Rust: the
//! runtime maps the file and calls [`Ring::put`]; `cargo xtask crumbs` reads
//! the file afterwards ([`parse`], [`Report`]) and says which submissions
//! the card had not finished and which records name bytes outside their
//! buffer.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// The file's first word.
pub const MAGIC: u64 = u64::from_le_bytes(*b"VICRUMB1");
/// The layout's number.
pub const VERSION: u64 = 1;
/// The bytes before the first record.
pub const HEADER: usize = 4096;
/// The bytes of a record: its number (written last), its kind and time, six
/// words.
pub const RECORD: usize = 64;
/// The words a record carries.
pub const WORDS: usize = 6;

/// What a record is. (Numbers are the file's: none is given a new meaning.)
pub mod kind {
    /// Lists handed to a queue: the queue, what | lists << 8 | more << 16
    /// (the next record goes on), the submission's number, three lists.
    pub const EXEC: u8 = 1;
    /// A fence signalled behind them: the queue, the fence, the value.
    pub const SIGNAL: u8 = 2;
    /// A wait for a fence's value came back: the fence, the value.
    pub const WAITED: u8 = 3;
    /// The main fence as the watching thread found it: the queue, the
    /// fence, the value it has completed.
    pub const FENCE: u8 = 4;
    /// The device is lost: the reason.
    pub const REMOVED: u8 = 5;
    /// A root view of a pass of ours: the list, what | writes << 8, the
    /// address, the buffer's bytes from it on.
    pub const VIEW: u8 = 6;
    /// Changed words stored into a copy: the list, the pairs' address, the
    /// words, the copy's address, its bytes, the largest end of a word.
    pub const SCATTER: u8 = 7;
    /// A batched clamp: the list, the table's address, the clamped
    /// records' address, their buffer's bytes, the first place, the budget.
    pub const BATCH: u8 = 8;
    /// A clamp of one execution: the list, the clamped records' address,
    /// their buffer's bytes, the bytes written, the source, the budget.
    pub const CLAMP: u8 = 9;
    /// A card-step list: the list, its buffer between, that one's bytes,
    /// the steps, the levels, the bytes used.
    pub const STEP_LIST: u8 = 10;
    /// A copy into the buffer between: the list, the source, its bytes,
    /// the offset in it, the offset between, the length.
    pub const STEP_READ: u8 = 11;
    /// A copy out of the buffer between, likewise (the destination).
    pub const STEP_WRITE: u8 = 12;
    /// A copy that was left out because it did not fit: as the two above,
    /// reads and writes.
    pub const STEP_READ_LEFT: u8 = 13;
    pub const STEP_WRITE_LEFT: u8 = 14;
    /// A descriptor slot pointed into the buffer between: the list, the
    /// title heap, the host heap's number, the slot, the address, the bytes.
    pub const STEP_SLOT: u8 = 15;
    /// A buffer a list uploads from: the list, the address, the bytes, the
    /// card-step stage.
    pub const UPLOAD_MADE: u8 = 16;
    /// Released: the address, the value the main fence had completed.
    pub const UPLOAD_GONE: u8 = 17;
}

/// What an [`kind::EXEC`] is of.
pub mod exec {
    /// A submission sent ahead, and one that is waited for.
    pub const LATE: u64 = 1;
    pub const SYNC: u64 = 2;
    /// The list that brings copies in the card's memory up to date.
    pub const CARD_SYNC: u64 = 3;
    /// A fetch of pieces of copies in the card's memory.
    pub const FETCH: u64 = 4;
    /// Anything else of ours.
    pub const OTHER: u64 = 5;
}

/// What a [`kind::VIEW`] is of.
pub mod view {
    pub const HASH_NOW: u64 = 1;
    pub const HASH_BEFORE: u64 = 2;
    pub const HASH_DIFFERS: u64 = 3;
    pub const MEMORY_DIRECTORY: u64 = 4;
    pub const MEMORY_FAULTS: u64 = 5;
    pub const SCATTER_NOTES: u64 = 6;
    pub const BUDGET: u64 = 7;
    pub const OTHER: u64 = 8;
}

/// The ring, over memory the caller keeps (a mapped file).
pub struct Ring {
    words: &'static [AtomicU64],
    capacity: u64,
}

impl Ring {
    /// A ring in `bytes` bytes at `at`, begun anew.
    ///
    /// # Safety
    /// `at` is 8-byte aligned memory of `bytes` bytes that may be read and
    /// written for as long as the process lives, and nothing else writes it.
    pub unsafe fn new(at: *mut u8, bytes: usize, started_ms: u64) -> Option<Ring> {
        if at.is_null() || !(at as usize).is_multiple_of(8) || bytes < HEADER + RECORD {
            return None;
        }
        let capacity = ((bytes - HEADER) / RECORD) as u64;
        // SAFETY: the caller's; an atomic word is laid out as a word.
        let words = unsafe { std::slice::from_raw_parts(at.cast::<AtomicU64>(), bytes / 8) };
        for w in &words[..HEADER / 8] {
            w.store(0, Ordering::Relaxed);
        }
        // (A record of memory used before is not this run's: its number is
        // cleared, so the reader skips it. Only where there is one: a file
        // just made is all zeros, and stays unwritten where nothing is put.)
        for slot in 0..capacity as usize {
            let number = &words[HEADER / 8 + slot * (RECORD / 8)];
            if number.load(Ordering::Relaxed) != 0 {
                number.store(0, Ordering::Relaxed);
            }
        }
        words[1].store(VERSION, Ordering::Relaxed);
        words[2].store(capacity, Ordering::Relaxed);
        words[4].store(started_ms, Ordering::Relaxed);
        words[5].store(RECORD as u64, Ordering::Relaxed);
        words[0].store(MAGIC, Ordering::Release);
        Some(Ring { words, capacity })
    }

    /// The records it holds at most.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Adds a record, over the oldest one once the ring is full. From any
    /// thread.
    pub fn put(&self, kind: u8, time_us: u64, words: [u64; WORDS]) {
        let number = self.words[3].fetch_add(1, Ordering::Relaxed);
        let at = HEADER / 8 + (number % self.capacity) as usize * (RECORD / 8);
        let record = &self.words[at..at + RECORD / 8];
        // Its number last, and none while it is written: a record half
        // written when the process ends is skipped.
        record[0].store(0, Ordering::Relaxed);
        record[1].store(u64::from(kind) | (time_us << 8), Ordering::Relaxed);
        for (to, word) in record[2..].iter().zip(words) {
            to.store(word, Ordering::Relaxed);
        }
        record[0].store(number + 1, Ordering::Release);
    }
}

/// A record as read back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    /// Its number, from 0 in the order the records were begun.
    pub number: u64,
    pub kind: u8,
    /// Microseconds since the ring was begun.
    pub time_us: u64,
    pub words: [u64; WORDS],
}

/// A file's records.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    /// In the order they were begun.
    pub records: Vec<Record>,
    /// The records the ring holds at most.
    pub capacity: u64,
    /// When the ring was begun (milliseconds since 1970).
    pub started_ms: u64,
    /// Places whose record was being written, or is not this run's.
    pub unwritten: u64,
    /// Numbers that should lie between the first and the last record kept
    /// and do not (their pages had not reached the file).
    pub missing: u64,
}

/// Why a file cannot be read.
#[derive(Debug, PartialEq, Eq)]
pub enum Unreadable {
    TooShort,
    NotCrumbs,
    Version(u64),
    Layout,
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unreadable::TooShort => write!(f, "shorter than a header and a record"),
            Unreadable::NotCrumbs => write!(f, "not a breadcrumb file (its first word is another)"),
            Unreadable::Version(v) => write!(f, "layout {v}; this reader knows {VERSION}"),
            Unreadable::Layout => write!(f, "its header names more records than it holds, or another record size"),
        }
    }
}

impl std::error::Error for Unreadable {}

/// The records of a breadcrumb file's bytes.
pub fn parse(bytes: &[u8]) -> Result<Parsed, Unreadable> {
    if bytes.len() < HEADER + RECORD {
        return Err(Unreadable::TooShort);
    }
    let word = |i: usize| u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
    if word(0) != MAGIC {
        return Err(Unreadable::NotCrumbs);
    }
    if word(1) != VERSION {
        return Err(Unreadable::Version(word(1)));
    }
    let capacity = word(2);
    if capacity == 0 || word(5) != RECORD as u64 || capacity > ((bytes.len() - HEADER) / RECORD) as u64 {
        return Err(Unreadable::Layout);
    }
    let mut out = Parsed {
        capacity,
        started_ms: word(4),
        ..Parsed::default()
    };
    for slot in 0..capacity {
        let at = HEADER / 8 + slot as usize * (RECORD / 8);
        let number = word(at);
        // (The header's count may be older than the records: each says its
        // own number, and lies where that number puts it.)
        if number == 0 || (number - 1) % capacity != slot {
            out.unwritten += 1;
            continue;
        }
        let mut words = [0; WORDS];
        for (k, w) in words.iter_mut().enumerate() {
            *w = word(at + 2 + k);
        }
        out.records.push(Record {
            number: number - 1,
            kind: (word(at + 1) & 0xff) as u8,
            time_us: word(at + 1) >> 8,
            words,
        });
    }
    out.records.sort_by_key(|r| r.number);
    // A ring written by one run holds one stretch of numbers; what a page
    // that did not reach the file still holds is older than that by whole
    // turns of the ring, and is dropped.
    if let Some(last) = out.records.last().map(|r| r.number) {
        let before = out.records.len();
        out.records.retain(|r| last - r.number < capacity);
        out.unwritten += (before - out.records.len()) as u64;
        if let Some(first) = out.records.first().map(|r| r.number) {
            out.missing = (last - first + 1) - out.records.len() as u64;
        }
    }
    Ok(out)
}

/// The bytes of the buffer between of a card-step list.
const BETWEEN: u64 = 4 << 20;
/// The places of a batched clamp's pass, and the bytes of one.
const BATCH_JOBS: u64 = 256;
const BATCH_SLOT: u64 = 32;

/// Lists handed to a queue together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Execution {
    /// The number of its first record.
    pub record: u64,
    pub time_us: u64,
    pub queue: u64,
    /// [`exec`].
    pub what: u64,
    /// The submission's number (0: none).
    pub submission: u64,
    pub lists: Vec<u64>,
    /// The fence and the value signalled behind it on its queue, if one
    /// was.
    pub signalled: Option<(u64, u64)>,
}

/// A record that names bytes outside their buffer, or a buffer released too
/// early.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flag {
    pub record: Record,
    pub why: String,
    /// Whether a list that had not finished holds it.
    pub in_flight: bool,
}

/// What a file's records say.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub records: usize,
    /// When the last one was put (microseconds since the ring was begun).
    pub last_us: u64,
    /// The main queue and its fence, as the watching thread named them.
    pub main: Option<(u64, u64)>,
    /// The last value the main fence was seen to have completed, and when.
    pub completed: Option<(u64, u64)>,
    /// When the device was found lost, and the reason.
    pub removed: Option<(u64, u64)>,
    /// Whether the signal of the completed value is among the records (if
    /// not, everything kept counts as not finished).
    pub found_signal: bool,
    /// What the card had not finished, in the order it was sent.
    pub in_flight: Vec<Execution>,
    /// The records of each list of those, since the list last ran.
    pub of_list: Vec<(u64, Vec<Record>)>,
    pub flags: Vec<Flag>,
}

/// Whether `[offset, offset + len)` lies in `width` bytes.
fn inside(width: u64, offset: u64, len: u64) -> bool {
    offset.checked_add(len).is_some_and(|end| end <= width)
}

/// What is wrong with a record by itself, if anything.
fn wrong(r: &Record) -> Option<String> {
    let w = r.words;
    let null = |what: &str, address: u64| (address == 0).then(|| format!("{what} at address 0"));
    match r.kind {
        kind::VIEW => null("a root view", w[2]),
        kind::SCATTER => null("the pairs", w[1]).or_else(|| null("the copy", w[3])).or_else(|| {
            (w[5] > w[4]).then(|| format!("a changed word ends at {:#x} of a copy of {:#x} bytes", w[5], w[4]))
        }),
        kind::BATCH => null("the table", w[1]).or_else(|| null("the clamped records", w[2])).or_else(|| {
            let end = w[4].saturating_add(BATCH_JOBS * BATCH_SLOT);
            (end > w[3]).then(|| format!("the places end at {end:#x} of a buffer of {:#x} bytes", w[3]))
        }),
        kind::CLAMP => null("the clamped records", w[1]).or_else(|| {
            (w[3] > w[2]).then(|| format!("{:#x} bytes of records into a buffer of {:#x} bytes", w[3], w[2]))
        }),
        kind::STEP_LIST => null("the buffer between", w[1]).or_else(|| {
            (w[5] > w[2]).then(|| format!("{:#x} bytes used of a buffer between of {:#x}", w[5], w[2]))
        }),
        kind::STEP_READ | kind::STEP_WRITE | kind::STEP_READ_LEFT | kind::STEP_WRITE_LEFT => {
            let left = matches!(r.kind, kind::STEP_READ_LEFT | kind::STEP_WRITE_LEFT);
            let what = if matches!(r.kind, kind::STEP_READ | kind::STEP_READ_LEFT) { "read" } else { "written" };
            let fate = if left { " (left out by the last check)" } else { " AND RECORDED" };
            null("a card step's buffer", w[1])
                .or_else(|| {
                    (!inside(w[2], w[3], w[5]))
                        .then(|| format!("{:#x} bytes {what} at {:#x} of a buffer of {:#x} bytes{fate}", w[5], w[3], w[2]))
                })
                .or_else(|| {
                    (!inside(BETWEEN, w[4], w[5]))
                        .then(|| format!("{:#x} bytes at {:#x} of the buffer between ({BETWEEN:#x} bytes){fate}", w[5], w[4]))
                })
                .or_else(|| left.then(|| "left out by the last check though it fits what was noted".to_owned()))
        }
        kind::UPLOAD_MADE => null("an upload buffer", w[1]),
        _ => None,
    }
}

/// The list a record is of, if it is of one.
fn list_of(r: &Record) -> Option<u64> {
    matches!(
        r.kind,
        kind::VIEW
            | kind::SCATTER
            | kind::BATCH
            | kind::CLAMP
            | kind::STEP_LIST
            | kind::STEP_READ
            | kind::STEP_WRITE
            | kind::STEP_READ_LEFT
            | kind::STEP_WRITE_LEFT
            | kind::STEP_SLOT
            | kind::UPLOAD_MADE
    )
    .then_some(r.words[0])
}

/// The executions among `records`, each with the value signalled behind it.
fn executions(records: &[Record]) -> Vec<Execution> {
    let mut all: Vec<Execution> = Vec::new();
    // By queue: the executions no signal has followed yet.
    let mut open: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut more = false;
    for r in records {
        let w = r.words;
        match r.kind {
            kind::EXEC => {
                let n = ((w[1] >> 8) & 0xff).min(3) as usize;
                // (A record that goes on belongs to the one before it,
                // unless that one fell out of the ring.)
                if more && let Some(last) = all.last_mut().filter(|e| e.queue == w[0]) {
                    last.lists.extend_from_slice(&w[3..3 + n]);
                } else {
                    open.entry(w[0]).or_default().push(all.len());
                    all.push(Execution {
                        record: r.number,
                        time_us: r.time_us,
                        queue: w[0],
                        what: w[1] & 0xff,
                        submission: w[2],
                        lists: w[3..3 + n].to_vec(),
                        signalled: None,
                    });
                }
                more = (w[1] >> 16) & 1 == 1;
            }
            kind::SIGNAL => {
                for i in open.remove(&w[0]).unwrap_or_default() {
                    all[i].signalled = Some((w[1], w[2]));
                }
            }
            _ => {}
        }
    }
    all
}

impl Report {
    /// What `parsed` says of the submissions the card had not finished.
    pub fn of(parsed: &Parsed) -> Report {
        let records = &parsed.records;
        let mut report = Report {
            records: records.len(),
            last_us: records.last().map_or(0, |r| r.time_us),
            ..Report::default()
        };
        for r in records {
            match r.kind {
                kind::FENCE => report.main = Some((r.words[0], r.words[1])),
                kind::REMOVED if report.removed.is_none() => report.removed = Some((r.time_us, r.words[0])),
                _ => {}
            }
        }
        // What the main fence had completed: as the watching thread, a
        // wait or a release last found it (all ones is a lost device's).
        let main_fence = report.main.map(|m| m.1);
        for r in records {
            let value = match r.kind {
                kind::FENCE => Some(r.words[2]),
                kind::WAITED if Some(r.words[0]) == main_fence => Some(r.words[1]),
                kind::UPLOAD_GONE => Some(r.words[1]),
                _ => None,
            };
            if let Some(v) = value.filter(|&v| v != u64::MAX)
                && report.completed.is_none_or(|(was, _)| v >= was)
            {
                report.completed = Some((v, r.time_us));
            }
        }
        let all = executions(records);
        let main_queue = report.main.map(|m| m.0);
        let done = report.completed.map_or(0, |c| c.0);
        // The main queue runs in order: what was sent up to the signal of
        // the completed value has run. And whatever its queue, an execution
        // has run if a wait for the signal behind it came back.
        let signal_at = records
            .iter()
            .rev()
            .find(|r| r.kind == kind::SIGNAL && Some(r.words[0]) == main_queue && Some(r.words[1]) == main_fence && r.words[2] == done)
            .map(|r| r.number);
        report.found_signal = signal_at.is_some();
        let waited: Vec<(u64, u64, u64)> =
            records.iter().filter(|r| r.kind == kind::WAITED).map(|r| (r.words[0], r.words[1], r.number)).collect();
        for e in &all {
            let waited_for = e.signalled.is_some_and(|(fence, value)| waited.iter().any(|&(f, v, at)| f == fence && v >= value && at > e.record));
            let in_order = Some(e.queue) == main_queue || main_queue.is_none();
            let run_in_order = in_order && signal_at.is_some_and(|at| e.record < at);
            if !(waited_for || run_in_order) {
                report.in_flight.push(e.clone());
            }
        }
        // Each list's records since it last ran before this.
        let mut ran: HashMap<u64, Vec<u64>> = HashMap::new();
        for e in &all {
            for &list in &e.lists {
                ran.entry(list).or_default().push(e.record);
            }
        }
        let mut flying_records: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for e in &report.in_flight {
            for &list in &e.lists {
                if report.of_list.iter().any(|(l, _)| *l == list) {
                    continue;
                }
                // From the list's last run that had finished to the end:
                // what it held in each run that had not.
                let runs = &ran[&list];
                let first_flying = report.in_flight.iter().filter(|f| f.lists.contains(&list)).map(|f| f.record).min().unwrap_or(e.record);
                let from = runs.iter().copied().filter(|&at| at < first_flying).max();
                let own: Vec<Record> = records
                    .iter()
                    .filter(|r| list_of(r) == Some(list) && from.is_none_or(|from| r.number > from))
                    .copied()
                    .collect();
                flying_records.extend(own.iter().map(|r| r.number));
                report.of_list.push((list, own));
            }
        }
        // What is wrong: a record by itself, a slot outside its list's
        // buffer between, an upload released before its list had run.
        let mut between: HashMap<u64, (u64, u64)> = HashMap::new();
        let mut uploads: HashMap<u64, (u64, u64)> = HashMap::new();
        for r in records {
            let w = r.words;
            let mut why = wrong(r);
            match r.kind {
                kind::STEP_LIST => {
                    between.insert(w[0], (w[1], w[2]));
                }
                kind::STEP_SLOT => {
                    why = match between.get(&w[0]) {
                        _ if w[4] == 0 => Some("a slot pointed at address 0".to_owned()),
                        Some(&(at, width)) if w[4] < at || !inside(width, w[4] - at, w[5]) => Some(format!(
                            "a slot pointed at {:#x} (+{:#x}), outside its list's buffer between at {at:#x} (+{width:#x})",
                            w[4], w[5]
                        )),
                        _ => None,
                    };
                }
                kind::UPLOAD_MADE => {
                    uploads.insert(w[1], (w[0], r.number));
                }
                kind::UPLOAD_GONE => {
                    if let Some((list, made)) = uploads.remove(&w[0]) {
                        let sent = all.iter().find(|e| e.lists.contains(&list) && e.record > made && e.record < r.number);
                        let signalled = sent.and_then(|e| e.signalled).filter(|s| Some(s.0) == main_fence).map(|s| s.1);
                        if let Some(value) = signalled.filter(|&v| w[1] != u64::MAX && v > w[1]) {
                            why = Some(format!(
                                "an upload buffer of list {list:#x} released when the fence had completed {}, before the list's \
                                 submission (signalled {value}) had run",
                                w[1]
                            ));
                        }
                    }
                }
                _ => {}
            }
            if let Some(why) = why {
                report.flags.push(Flag {
                    record: *r,
                    why,
                    in_flight: flying_records.contains(&r.number),
                });
            }
        }
        report
    }
}

/// A record's kind by name.
pub fn kind_name(kind: u8) -> &'static str {
    match kind {
        kind::EXEC => "lists sent",
        kind::SIGNAL => "fence signalled",
        kind::WAITED => "wait came back",
        kind::FENCE => "main fence completed",
        kind::REMOVED => "DEVICE LOST",
        kind::VIEW => "root view",
        kind::SCATTER => "changed words stored",
        kind::BATCH => "batched clamp",
        kind::CLAMP => "clamp",
        kind::STEP_LIST => "card-step list",
        kind::STEP_READ => "card step reads",
        kind::STEP_WRITE => "card step writes",
        kind::STEP_READ_LEFT => "card step read left out",
        kind::STEP_WRITE_LEFT => "card step write left out",
        kind::STEP_SLOT => "slot pointed between",
        kind::UPLOAD_MADE => "upload buffer made",
        kind::UPLOAD_GONE => "upload buffer released",
        _ => "?",
    }
}

fn exec_name(what: u64) -> &'static str {
    match what {
        exec::LATE => "a submission sent ahead",
        exec::SYNC => "a submission waited for",
        exec::CARD_SYNC => "copies in the card's memory brought up to date",
        exec::FETCH => "a fetch of card copies' pieces",
        _ => "other lists of ours",
    }
}

/// A record as a line.
pub fn line(r: &Record) -> String {
    let words: Vec<String> = r.words.iter().map(|w| format!("{w:#x}")).collect();
    format!("#{} {:.6} s {} [{}]", r.number, r.time_us as f64 / 1e6, kind_name(r.kind), words.join(", "))
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} records, the last at {:.3} s", self.records, self.last_us as f64 / 1e6)?;
        match self.main {
            Some((queue, fence)) => writeln!(f, "main queue {queue:#x}, its fence {fence:#x}")?,
            None => writeln!(f, "the main queue is not named among them (no record of the watching thread): every queue counts as it")?,
        }
        match self.completed {
            Some((value, at)) => writeln!(f, "the main fence had completed {value} at {:.6} s", at as f64 / 1e6)?,
            None => writeln!(f, "no value of the main fence is among them")?,
        }
        match self.removed {
            Some((at, reason)) => writeln!(f, "DEVICE LOST, found at {:.6} s (reason {reason:#x})", at as f64 / 1e6)?,
            None => writeln!(f, "no record of a lost device (the process may have ended before the watching thread saw it)")?,
        }
        if !self.found_signal {
            writeln!(f, "the signal of that value is not among the records: everything kept counts as not finished")?;
        }
        writeln!(f, "\nnot finished by the card: {} executions", self.in_flight.len())?;
        for e in &self.in_flight {
            let signalled = e.signalled.map_or_else(|| "no signal behind it yet".to_owned(), |(_, v)| format!("signalled {v}"));
            writeln!(
                f,
                "  #{} {:.6} s queue {:#x}: {} (submission {}), {signalled}, {} lists",
                e.record,
                e.time_us as f64 / 1e6,
                e.queue,
                exec_name(e.what),
                e.submission,
                e.lists.len()
            )?;
            for list in &e.lists {
                let own = self.of_list.iter().find(|(l, _)| l == list).map_or(&[][..], |(_, r)| &r[..]);
                let mut counts: Vec<(u8, usize)> = Vec::new();
                for r in own {
                    match counts.iter_mut().find(|c| c.0 == r.kind) {
                        Some(c) => c.1 += 1,
                        None => counts.push((r.kind, 1)),
                    }
                }
                let said: Vec<String> = counts.iter().map(|&(k, n)| format!("{n} {}", kind_name(k))).collect();
                writeln!(f, "    list {list:#x}: {}", if said.is_empty() { "nothing noted".to_owned() } else { said.join(", ") })?;
            }
        }
        let (flying, other): (Vec<&Flag>, Vec<&Flag>) = self.flags.iter().partition(|flag| flag.in_flight);
        writeln!(f, "\nrecords that name bytes outside their buffer: {} in lists not finished, {} in others", flying.len(), other.len())?;
        for (title, flags) in [("in lists not finished", flying), ("in others", other)] {
            if flags.is_empty() {
                continue;
            }
            writeln!(f, "  {title}:")?;
            for flag in flags.iter().take(200) {
                writeln!(f, "    {}\n      {}", flag.why, line(&flag.record))?;
            }
            if flags.len() > 200 {
                writeln!(f, "    ... and {} more", flags.len() - 200)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ring in memory of the test's (never freed: the ring may be used
    /// for as long as the process lives), and the bytes of it as a file
    /// would hold them.
    struct Memory {
        at: *mut u64,
        words: usize,
    }

    impl Memory {
        fn of(records: usize) -> (Memory, Ring) {
            let words = (HEADER + records * RECORD) / 8;
            let at = Box::leak(vec![0xdead_beef_dead_beef_u64; words].into_boxed_slice()).as_mut_ptr();
            // SAFETY: leaked words, touched through this pointer alone.
            let ring = unsafe { Ring::new(at.cast(), words * 8, 1234) }.unwrap();
            (Memory { at, words }, ring)
        }

        fn bytes(&self) -> Vec<u8> {
            // SAFETY: the leaked words; whole words are read.
            (0..self.words).flat_map(|i| unsafe { self.at.add(i).read_volatile() }.to_le_bytes()).collect()
        }
    }

    /// Sets word `i` of a file's bytes.
    fn set(bytes: &mut [u8], i: usize, word: u64) {
        bytes[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
    }

    #[test]
    fn a_ring_gives_back_what_was_put() {
        let (memory, ring) = Memory::of(8);
        assert_eq!(ring.capacity(), 8);
        // Before anything is put: no record, though the memory held others'.
        let empty = parse(&memory.bytes()).unwrap();
        assert_eq!((empty.records.len(), empty.unwritten, empty.capacity, empty.started_ms), (0, 8, 8, 1234));
        for i in 0..5u64 {
            ring.put(kind::FENCE, 100 + i, [i, 2, 3, 4, 5, u64::MAX]);
        }
        let parsed = parse(&memory.bytes()).unwrap();
        assert_eq!(parsed.records.len(), 5);
        assert_eq!(parsed.missing, 0);
        assert_eq!(
            parsed.records[4],
            Record {
                number: 4,
                kind: kind::FENCE,
                time_us: 104,
                words: [4, 2, 3, 4, 5, u64::MAX]
            }
        );
    }

    #[test]
    fn a_full_ring_keeps_the_newest() {
        let (memory, ring) = Memory::of(8);
        for i in 0..21u64 {
            ring.put(kind::VIEW, i, [i; WORDS]);
        }
        let parsed = parse(&memory.bytes()).unwrap();
        let numbers: Vec<u64> = parsed.records.iter().map(|r| r.number).collect();
        assert_eq!(numbers, (13..21).collect::<Vec<_>>());
        assert_eq!((parsed.unwritten, parsed.missing), (0, 0));
    }

    #[test]
    fn a_record_half_written_is_skipped() {
        let (memory, ring) = Memory::of(8);
        for i in 0..6u64 {
            ring.put(kind::VIEW, i, [i; WORDS]);
        }
        // The process ended inside the fourth record: its number is not
        // there yet.
        let mut bytes = memory.bytes();
        set(&mut bytes, HEADER / 8 + 3 * (RECORD / 8), 0);
        let parsed = parse(&bytes).unwrap();
        let numbers: Vec<u64> = parsed.records.iter().map(|r| r.number).collect();
        assert_eq!(numbers, [0, 1, 2, 4, 5]);
        assert_eq!((parsed.unwritten, parsed.missing), (3, 1));
        for i in 6..30u64 {
            ring.put(kind::VIEW, i, [i; WORDS]);
        }
        // And a page that never reached the file holds a record of a turn
        // of the ring long past: place 2 holds number 26; as if it still
        // held number 2.
        let mut bytes = memory.bytes();
        set(&mut bytes, HEADER / 8 + 2 * (RECORD / 8), 3);
        let parsed = parse(&bytes).unwrap();
        let numbers: Vec<u64> = parsed.records.iter().map(|r| r.number).collect();
        assert_eq!(numbers, [22, 23, 24, 25, 27, 28, 29]);
        assert_eq!(parsed.missing, 1);
    }

    #[test]
    fn many_threads_put_whole_records() {
        let (memory, ring) = Memory::of(4096);
        std::thread::scope(|s| {
            for t in 0..8u64 {
                let ring = &ring;
                s.spawn(move || {
                    for i in 0..400u64 {
                        ring.put(kind::VIEW, i, [t, i, t ^ i, t + i, t * i, !t]);
                    }
                });
            }
        });
        let parsed = parse(&memory.bytes()).unwrap();
        assert_eq!(parsed.records.len(), 3200);
        for r in &parsed.records {
            let [t, i, ..] = r.words;
            assert_eq!(r.words, [t, i, t ^ i, t + i, t * i, !t]);
        }
    }

    #[test]
    fn files_that_are_not_crumbs_are_refused() {
        assert_eq!(parse(&[0; 64]), Err(Unreadable::TooShort));
        assert_eq!(parse(&[0; HEADER + RECORD]), Err(Unreadable::NotCrumbs));
        let (memory, _ring) = Memory::of(4);
        let mut bytes = memory.bytes();
        set(&mut bytes, 1, 9);
        assert_eq!(parse(&bytes), Err(Unreadable::Version(9)));
        set(&mut bytes, 1, VERSION);
        set(&mut bytes, 2, 5);
        assert_eq!(parse(&bytes), Err(Unreadable::Layout));
        // SAFETY: nothing is touched: no memory, or too little of it.
        unsafe {
            assert!(Ring::new(std::ptr::null_mut(), 1 << 20, 0).is_none());
            assert!(Ring::new(memory.at.cast(), HEADER, 0).is_none());
        }
    }

    const QUEUE: u64 = 0x1000;
    const FENCE: u64 = 0x2000;
    const OTHER_QUEUE: u64 = 0x3000;
    const OTHER_FENCE: u64 = 0x4000;
    const BETWEEN_AT: u64 = 0x8_0000_0000;

    fn exec(ring: &Ring, queue: u64, what: u64, submission: u64, lists: &[u64]) {
        let chunks: Vec<&[u64]> = lists.chunks(3).collect();
        for (k, chunk) in chunks.iter().enumerate() {
            let mut words = [queue, what | ((chunk.len() as u64) << 8) | (u64::from(k + 1 < chunks.len()) << 16), submission, 0, 0, 0];
            words[3..3 + chunk.len()].copy_from_slice(chunk);
            ring.put(kind::EXEC, 0, words);
        }
    }

    /// Three submissions, of which the card finished the first: the other
    /// two are told, with what their lists hold, and what lies outside its
    /// buffer is flagged.
    #[test]
    fn the_report_names_what_had_not_finished_and_what_lies_outside() {
        let (memory, ring) = Memory::of(256);
        let put = |kind: u8, words: [u64; WORDS]| ring.put(kind, 0, words);
        put(kind::FENCE, [QUEUE, FENCE, 0, 0, 0, 0]);
        // Submission 1: list 0xa1, a scatter inside its copy.
        put(kind::SCATTER, [0xa1, 0x7000_0000, 16, 0x9000_0000, 0x1000, 0x1000]);
        exec(&ring, QUEUE, exec::LATE, 1, &[0xa1]);
        put(kind::SIGNAL, [QUEUE, FENCE, 1, 0, 0, 0]);
        put(kind::FENCE, [QUEUE, FENCE, 1, 0, 0, 0]);
        // Submission 2: a card-step list 0xb0 and four lists; list 0xa1
        // again, with a scatter past its copy's end.
        put(kind::STEP_LIST, [0xb0, BETWEEN_AT, BETWEEN, 3, 1, 0x300]);
        put(kind::STEP_READ, [0xb0, 0x9100_0000, 0x1000, 0xff0, 0, 0x10]);
        put(kind::STEP_WRITE, [0xb0, 0x9200_0000, 0x1000, 0xff8, 0, 0x10]);
        put(kind::STEP_WRITE_LEFT, [0xb0, 0x9200_0000, 0x1000, 0x2000, 0x100, 0x10]);
        put(kind::STEP_SLOT, [0xb0, 0x55, 7, 12, BETWEEN_AT + 0x200, 0x100]);
        put(kind::STEP_SLOT, [0xb0, 0x55, 7, 13, BETWEEN_AT + BETWEEN - 0x80, 0x100]);
        put(kind::STEP_SLOT, [0xb0, 0x55, 7, 14, 0x51_0000, 0x100]);
        put(kind::SCATTER, [0xa1, 0x7000_0000, 16, 0x9000_0000, 0x1000, 0x1004]);
        put(kind::UPLOAD_MADE, [0xa2, 0xa000_0000, 0x4000, 2, 0, 0]);
        exec(&ring, QUEUE, exec::LATE, 2, &[0xb0, 0xa1, 0xa2, 0xa3, 0xa4]);
        put(kind::SIGNAL, [QUEUE, FENCE, 2, 0, 0, 0]);
        // The upload of list 0xa2 goes while the fence stands at 1.
        put(kind::UPLOAD_GONE, [0xa000_0000, 1, 0, 0, 0, 0]);
        // Submission 3: list 0xa5, a batched clamp whose places end past
        // its buffer, and a clamp that fits.
        put(kind::BATCH, [0xa5, 0x7100_0000, 0x9300_0000, 0x1000, 0, 0x9400_0000]);
        put(kind::CLAMP, [0xa5, 0x9500_0000, 0x1_0000, 0x40, 0x9600_0000, 0x9400_0000]);
        exec(&ring, QUEUE, exec::LATE, 3, &[0xa5]);
        put(kind::SIGNAL, [QUEUE, FENCE, 3, 0, 0, 0]);
        // Another queue: one execution waited for, one not.
        exec(&ring, OTHER_QUEUE, exec::OTHER, 0, &[0xc1]);
        put(kind::SIGNAL, [OTHER_QUEUE, OTHER_FENCE, 9, 0, 0, 0]);
        put(kind::WAITED, [OTHER_FENCE, 9, 0, 0, 0, 0]);
        exec(&ring, OTHER_QUEUE, exec::OTHER, 0, &[0xc2]);
        put(kind::SIGNAL, [OTHER_QUEUE, OTHER_FENCE, 10, 0, 0, 0]);
        // A fetch on the main queue behind the submissions, waited for
        // with a fence of its own (whose value is one the main fence has
        // not reached): it has run, though it was sent after them.
        exec(&ring, QUEUE, exec::FETCH, 0, &[0xc3]);
        put(kind::SIGNAL, [QUEUE, 0x5000, 3, 0, 0, 0]);
        put(kind::WAITED, [0x5000, 3, 0, 0, 0, 0]);
        put(kind::FENCE, [QUEUE, FENCE, u64::MAX, 0, 0, 0]);
        put(kind::REMOVED, [0x887a_0006, 0, 0, 0, 0, 0]);

        let report = Report::of(&parse(&memory.bytes()).unwrap());
        assert_eq!(report.main, Some((QUEUE, FENCE)));
        assert_eq!(report.completed.map(|c| c.0), Some(1));
        assert_eq!(report.removed.map(|r| r.1), Some(0x887a_0006));
        assert!(report.found_signal);
        let flying: Vec<(u64, Vec<u64>, Option<u64>)> =
            report.in_flight.iter().map(|e| (e.submission, e.lists.clone(), e.signalled.map(|s| s.1))).collect();
        assert_eq!(
            flying,
            [
                (2, vec![0xb0, 0xa1, 0xa2, 0xa3, 0xa4], Some(2)),
                (3, vec![0xa5], Some(3)),
                (0, vec![0xc2], Some(10)),
            ]
        );
        // List 0xa1's records: only those of the run that had not finished.
        let of = |list: u64| report.of_list.iter().find(|(l, _)| *l == list).map(|(_, r)| r.len());
        assert_eq!([of(0xb0), of(0xa1), of(0xa2), of(0xa3), of(0xa5), of(0xc1)], [Some(7), Some(1), Some(1), Some(0), Some(2), None]);
        assert_eq!(of(0xc3), None);
        let flags: Vec<(u8, bool)> = report.flags.iter().map(|f| (f.record.kind, f.in_flight)).collect();
        assert_eq!(
            flags,
            [
                (kind::STEP_WRITE, true),
                (kind::STEP_WRITE_LEFT, true),
                (kind::STEP_SLOT, true),
                (kind::STEP_SLOT, true),
                (kind::SCATTER, true),
                (kind::UPLOAD_GONE, false),
                (kind::BATCH, true),
            ]
        );
        let text = report.to_string();
        for part in [
            "DEVICE LOST",
            "the main fence had completed 1 ",
            "not finished by the card: 3 executions",
            "list 0xb0: 1 card-step list, 1 card step reads, 1 card step writes, 1 card step write left out, 3 slot pointed between",
            "0x10 bytes written at 0xff8 of a buffer of 0x1000 bytes AND RECORDED",
            "a slot pointed at 0x510000 (+0x100), outside its list's buffer between",
            "a changed word ends at 0x1004 of a copy of 0x1000 bytes",
            "an upload buffer of list 0xa2 released when the fence had completed 1, before the list's submission (signalled 2) had run",
            "the places end at 0x2000 of a buffer of 0x1000 bytes",
        ] {
            assert!(text.contains(part), "{part}\n{text}");
        }
    }

    /// Without the completed value's signal among the records (the ring
    /// turned since), nothing is taken to have finished.
    #[test]
    fn without_the_signal_everything_counts_as_not_finished() {
        let (memory, ring) = Memory::of(64);
        exec(&ring, QUEUE, exec::SYNC, 7, &[0xa1]);
        ring.put(kind::SIGNAL, 0, [QUEUE, FENCE, 7, 0, 0, 0]);
        ring.put(kind::FENCE, 0, [QUEUE, FENCE, 5, 0, 0, 0]);
        let report = Report::of(&parse(&memory.bytes()).unwrap());
        assert!(!report.found_signal);
        assert_eq!(report.in_flight.len(), 1);
        assert!(report.flags.is_empty());
        // And with nothing at all the report is still one.
        let (memory, _ring) = Memory::of(4);
        let report = Report::of(&parse(&memory.bytes()).unwrap());
        assert_eq!((report.records, report.in_flight.len()), (0, 0));
        assert!(report.to_string().contains("0 records"));
    }
}
