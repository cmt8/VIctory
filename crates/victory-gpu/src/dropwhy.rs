//! `VICTORY_DROP_WHY=1`: which draws and dispatches were not run, by the
//! title pipeline's programs and by cause. Off unless set; metadata only
//! (program hashes, kinds, target formats, counts, frames).
//!
//! The frame rate line's "dropped" number says how many, not which or why,
//! and the reasons line of diagnostics builds names no program for a
//! pipeline that is still being made. A pass the title runs once, in a frame
//! in which its pipeline is not made yet, is lost for the run
//! (`crate::pipeline::make`): this says which programs that happened to,
//! when their pipeline came, and whether they ran after it.
//!
//! Lines, all beginning "drop why:":
//! - "first": the first time programs are dropped as one kind for one cause;
//! - "ran": the first time they ran after having been dropped (how long the
//!   hole was);
//! - with every frame rate line, what was dropped since the last one, per
//!   program (the structure builder's as one line), and the making times of
//!   the pipelines made meanwhile;
//! - "settled": a quarter of a minute after programs were last dropped,
//!   whether they have run since; and every minute the programs that were
//!   dropped while their pipeline was made and have not run since.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Whether the lines are written (`VICTORY_DROP_WHY=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_DROP_WHY").is_ok_and(|v| v.trim() == "1"))
}

/// What was recorded and not run.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Kind {
    Draw,
    Indexed,
    Mesh,
    Dispatch,
    /// An indirect execution, by its argument's kind (0 draw, 1 indexed
    /// draw, 2 dispatch, 10 mesh dispatch).
    Indirect(u32),
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Kind::Draw => f.write_str("draw"),
            Kind::Indexed => f.write_str("indexed draw"),
            Kind::Mesh => f.write_str("mesh dispatch"),
            Kind::Dispatch => f.write_str("dispatch"),
            Kind::Indirect(0) => f.write_str("indirect draw"),
            Kind::Indirect(1) => f.write_str("indirect indexed draw"),
            Kind::Indirect(2) => f.write_str("indirect dispatch"),
            Kind::Indirect(10) => f.write_str("indirect mesh dispatch"),
            Kind::Indirect(kind) => write!(f, "indirect execution of kind {kind}"),
        }
    }
}

/// Why it was not run.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Cause {
    /// The list had no pipeline set.
    NoPipeline,
    /// The host pipeline was not made yet.
    Making,
    /// A shader of the pipeline does not translate.
    Untranslated,
    /// The host refused the pipeline, or the driver's compiler ended on it.
    Refused,
    /// A stage of the ray-tracing structure builder, which is off
    /// (`crate::pipeline::builder_runs`).
    BuilderOff,
    /// There is a host pipeline: the draw could not be bound or prepared,
    /// or a guard dropped it.
    Other(&'static str),
}

impl std::fmt::Display for Cause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Cause::NoPipeline => f.write_str("no pipeline set"),
            Cause::Making => f.write_str("its host pipeline was being made"),
            Cause::Untranslated => f.write_str("a shader does not translate"),
            Cause::Refused => f.write_str("the host made no pipeline of it"),
            Cause::BuilderOff => f.write_str("the structure builder is off"),
            Cause::Other(why) => write!(f, "not bound: {why}"),
        }
    }
}

/// When a pipeline was asked for by a draw first, when it was made, and how
/// long the making took, in ms ([`crate::host::now_ms`]; 0: not yet).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Times {
    pub(crate) asked: u64,
    pub(crate) made: u64,
    pub(crate) took: u64,
}

struct Entry {
    /// The title pipeline's key (`Pending::key`) and what it is.
    key: String,
    what: String,
    kind: Kind,
    cause: Cause,
    total: u64,
    /// Since the last report.
    lately: u64,
    /// Frames in which it was dropped.
    frames: u64,
    first: (u64, u64),
    last: (u64, u64),
    /// The first frame it ran in after a drop, and how often it ran after
    /// its last drop.
    ran_first: Option<(u64, u64)>,
    ran_since: u64,
    settled: bool,
}

#[derive(Default)]
struct Table {
    entries: Vec<Entry>,
    /// By the pipeline's address, kind and cause; and all of an address.
    by: HashMap<(usize, Kind, Cause), usize>,
    of: HashMap<usize, Vec<usize>>,
    /// Pipelines made since the last report: how many took under 10 ms,
    /// 100 ms, 1 s, 3 s and more; the ms of all, and the longest.
    made: [u64; 5],
    made_ms: u64,
    made_longest: u64,
    reports: u64,
    /// Frames presented since the last report.
    frames: f64,
    /// What the title had created at the last report
    /// (`crate::pipeline::created`, `crate::device::unknown_pipelines`).
    created: ([u64; 3], [u32; 2]),
    /// Drops told since the start, and at the last report what was
    /// recorded, what ran and what was told ([`account`]).
    told: u64,
    accounted: [u64; 3],
}

/// The line that says whether every draw and dispatch recorded either ran
/// or was told here as not run: `now` and `was` are what was recorded, what
/// ran and what was told as not run, since the start, now and at the last
/// report. What is neither is a draw that was left out without a word; the
/// numbers are read while the title records, so a few either way mean
/// nothing, and a number that grows with the frames does.
fn account(now: [u64; 3], was: [u64; 3], frames: f64) -> String {
    let per = |i: usize| now[i].saturating_sub(was[i]) as f64 / frames.max(1.0);
    let neither = |n: [u64; 3]| n[0] as i64 - n[1] as i64 - n[2] as i64;
    format!(
        "drop why: per frame {:.1} draws and dispatches recorded, {:.1} run, {:.1} told as not run, {:.1} neither; since the start \
         {} recorded, {} neither",
        per(0),
        per(1),
        per(2),
        (neither(now) - neither(was)) as f64 / frames.max(1.0),
        now[0],
        neither(now)
    )
}

/// How long after its last drop an entry is settled, in ms.
const SETTLE_MS: u64 = 15_000;
/// The most lines of one report.
const LINES: usize = 80;

impl Table {
    /// A drop of the pipeline at `id` (its address), with `key`; the line
    /// to write if it is the first for these programs, kind and cause.
    fn dropped(
        &mut self,
        (id, key): (usize, &str),
        what: impl FnOnce() -> String,
        (kind, cause): (Kind, Cause),
        (frame, ms): (u64, u64),
    ) -> Option<String> {
        let found = self.by.get(&(id, kind, cause)).copied().filter(|&i| self.entries[i].key == key);
        let i = match found {
            Some(i) => i,
            None => {
                // Another pipeline at an address the title used before
                // begins anew.
                if self.of.get(&id).is_some_and(|all| all.iter().any(|&i| self.entries[i].key != key)) {
                    self.of.remove(&id);
                    self.by.retain(|k, _| k.0 != id);
                }
                self.entries.push(Entry {
                    key: key.to_owned(),
                    what: what(),
                    kind,
                    cause,
                    total: 0,
                    lately: 0,
                    frames: 0,
                    first: (frame, ms),
                    last: (u64::MAX, 0),
                    ran_first: None,
                    ran_since: 0,
                    settled: false,
                });
                let i = self.entries.len() - 1;
                self.by.insert((id, kind, cause), i);
                self.of.entry(id).or_default().push(i);
                i
            }
        };
        self.told += 1;
        let e = &mut self.entries[i];
        e.total += 1;
        e.lately += 1;
        if e.last.0 != frame {
            e.frames += 1;
        }
        e.last = (frame, ms);
        e.ran_since = 0;
        e.settled = false;
        (e.total == 1).then(|| {
            format!("drop why: first: frame {frame} at {:.3} s: {kind}, {cause}: {}", ms as f64 / 1e3, e.what)
        })
    }

    /// The pipeline at `id` ran; the lines to write if that is the first
    /// time since it was dropped.
    fn ran(&mut self, (id, key): (usize, &str), times: Times, (frame, ms): (u64, u64)) -> Vec<String> {
        let Some(all) = self.of.get(&id) else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        for &i in all {
            let e = &mut self.entries[i];
            if e.key != key {
                continue;
            }
            e.ran_since += 1;
            if e.ran_first.is_none() {
                e.ran_first = Some((frame, ms));
                lines.push(format!(
                    "drop why: ran: frame {frame} at {:.3} s: {} {} after {} drops in {} frames, frames {} to {} ({:.3} s to \
                     {:.3} s); {}",
                    ms as f64 / 1e3,
                    e.kind,
                    e.what,
                    e.total,
                    e.frames,
                    e.first.0,
                    e.last.0,
                    e.first.1 as f64 / 1e3,
                    e.last.1 as f64 / 1e3,
                    told(times)
                ));
            }
        }
        lines
    }

    /// A host pipeline was made in `ms`.
    fn made(&mut self, ms: u64) {
        let slot = [10, 100, 1000, 3000].iter().position(|&most| ms < most).unwrap_or(4);
        self.made[slot] += 1;
        self.made_ms += ms;
        self.made_longest = self.made_longest.max(ms);
    }

    /// The lines of a report over `frames` frames, at `ms`.
    fn report(&mut self, frames: f64, ms: u64) -> Vec<String> {
        let mut lines = Vec::new();
        self.reports += 1;
        let per = |n: u64| n as f64 / frames.max(1.0);
        // The structure builder's as one line: it is off as a whole.
        let builder: Vec<&Entry> =
            self.entries.iter().filter(|e| e.cause == Cause::BuilderOff && e.lately > 0).collect();
        if !builder.is_empty() {
            let programs: std::collections::HashSet<&str> = builder.iter().map(|e| e.key.as_str()).collect();
            lines.push(format!(
                "drop why: {:.1}/frame: the structure builder is off (VICTORY_BVH_BUILDER=1: runs): {:.1} dispatches, {:.1} \
                 indirect, of {} pipelines",
                per(builder.iter().map(|e| e.lately).sum()),
                per(builder.iter().filter(|e| e.kind == Kind::Dispatch).map(|e| e.lately).sum()),
                per(builder.iter().filter(|e| e.kind != Kind::Dispatch).map(|e| e.lately).sum()),
                programs.len()
            ));
        }
        let mut others: Vec<&Entry> =
            self.entries.iter().filter(|e| e.cause != Cause::BuilderOff && e.lately > 0).collect();
        others.sort_by(|a, b| b.lately.cmp(&a.lately).then_with(|| a.what.cmp(&b.what)));
        for cause in [Cause::NoPipeline, Cause::Making, Cause::Untranslated, Cause::Refused] {
            let of: Vec<&&Entry> = others.iter().filter(|e| e.cause == cause).collect();
            if !of.is_empty() {
                lines.push(format!(
                    "drop why: {:.1}/frame: {cause}, of {} pipelines and kinds",
                    per(of.iter().map(|e| e.lately).sum()),
                    of.len()
                ));
            }
        }
        for e in others.iter().take(LINES) {
            lines.push(format!("drop why: {:.2}/frame: {}, {}: {}", per(e.lately), e.kind, e.cause, e.what));
        }
        if others.len() > LINES {
            lines.push(format!("drop why: and {} more", others.len() - LINES));
        }
        if self.made.iter().any(|&n| n > 0) {
            let [a, b, c, d, e] = self.made;
            lines.push(format!(
                "drop why: host pipelines made since the last line: {} ({a} under 10 ms, {b} under 100 ms, {c} under 1 s, {d} \
                 under 3 s, {e} longer; {:.1} s in all, the longest {:.1} s)",
                a + b + c + d + e,
                self.made_ms as f64 / 1e3,
                self.made_longest as f64 / 1e3
            ));
            (self.made, self.made_ms, self.made_longest) = ([0; 5], 0, 0);
        }
        for e in &mut self.entries {
            e.lately = 0;
            if !e.settled && e.cause != Cause::BuilderOff && ms.saturating_sub(e.last.1) >= SETTLE_MS {
                e.settled = true;
                lines.push(format!(
                    "drop why: settled: {} {}, {}: {} drops in {} frames, frames {} to {} ({:.3} s to {:.3} s); {}",
                    e.kind,
                    e.what,
                    e.cause,
                    e.total,
                    e.frames,
                    e.first.0,
                    e.last.0,
                    e.first.1 as f64 / 1e3,
                    e.last.1 as f64 / 1e3,
                    match e.ran_first {
                        Some((frame, at)) if e.ran_since > 0 => format!(
                            "it ran first after a drop in frame {frame} at {:.3} s, and {} times since its last drop",
                            at as f64 / 1e3,
                            e.ran_since
                        ),
                        _ => "it has NOT run since its last drop".to_owned(),
                    }
                ));
            }
        }
        // Every twelfth report (a minute): what was dropped while its
        // pipeline was made and never ran after: a pass that may be lost.
        if self.reports % 12 == 0 {
            let lost: Vec<&Entry> = self
                .entries
                .iter()
                .filter(|e| e.cause == Cause::Making && e.settled && e.ran_since == 0)
                .collect();
            let all = self.entries.iter().filter(|e| e.cause == Cause::Making).count();
            lines.push(format!(
                "drop why: so far {all} pipelines and kinds were dropped while their host pipeline was made; {} of them have not \
                 run since their last drop{}",
                lost.len(),
                if lost.is_empty() { "" } else { ":" }
            ));
            for e in lost.iter().take(LINES) {
                lines.push(format!(
                    "drop why: not run since: {} {} ({} drops, frames {} to {})",
                    e.kind, e.what, e.total, e.first.0, e.last.0
                ));
            }
        }
        lines
    }
}

/// `times` as text.
fn told(times: Times) -> String {
    if times.made == 0 {
        return "its pipeline's times are not known".to_owned();
    }
    format!(
        "its pipeline was asked for by a draw at {:.3} s and made at {:.3} s, in {} ms of making",
        times.asked as f64 / 1e3,
        times.made as f64 / 1e3,
        times.took
    )
}

static TABLE: Mutex<Option<Table>> = Mutex::new(None);

fn table() -> std::sync::MutexGuard<'static, Option<Table>> {
    TABLE.lock().unwrap_or_else(|e| e.into_inner())
}

thread_local! {
    /// Whether a drop was noted on this thread since [`take_noted`].
    static NOTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a drop was noted on this thread since the last call: an
/// indirect execution that did not run and had none noted gets one.
pub(crate) fn take_noted() -> bool {
    NOTED.replace(false)
}

/// A draw or dispatch of the title's pipeline `this` was not run, for
/// `cause`.
pub(crate) fn dropped(this: crate::object::This, kind: Kind, cause: Cause) {
    if !on() {
        return;
    }
    NOTED.set(true);
    let now = (crate::frame::current(), crate::host::now_ms());
    let key = crate::pipeline::drop_key(this);
    let line = table().get_or_insert_default().dropped(
        (this as usize, key),
        || crate::pipeline::drop_what(this),
        (kind, cause),
        now,
    );
    if let Some(line) = line {
        tracing::info!("{line}");
    }
}

/// A draw or dispatch of `this` was not run: for want of a host pipeline
/// (why it has none is looked up), else for `why`.
pub(crate) fn not_run(this: crate::object::This, kind: Kind, why: Option<&'static str>) {
    if !on() {
        return;
    }
    let cause = crate::pipeline::lack(this).unwrap_or(Cause::Other(why.unwrap_or("it could not be prepared")));
    dropped(this, kind, cause);
}

/// A draw or dispatch of the title's pipeline `this` ran.
pub(crate) fn ran(this: crate::object::This) {
    if !on() {
        return;
    }
    let mut table = table();
    let Some(table) = table.as_mut().filter(|t| t.of.contains_key(&(this as usize))) else {
        return;
    };
    let lines = table.ran(
        (this as usize, crate::pipeline::drop_key(this)),
        crate::pipeline::drop_times(this),
        (crate::frame::current(), crate::host::now_ms()),
    );
    for line in lines {
        tracing::info!("{line}");
    }
}

/// A host pipeline was made in `ms`.
pub(crate) fn made(ms: u64) {
    if on() {
        table().get_or_insert_default().made(ms);
    }
}

/// `frames` more frames were presented; with the frame rate line (`now`),
/// the report over the frames since the last.
pub(crate) fn report(frames: f64, now: bool) {
    if !on() {
        return;
    }
    let lines = {
        let mut table = table();
        let table = table.get_or_insert_default();
        table.frames += frames;
        if !now {
            return;
        }
        let frames = std::mem::take(&mut table.frames);
        let mut lines = table.report(frames, crate::host::now_ms());
        // What the title created, when it has created more.
        let created = (crate::pipeline::created(), crate::device::unknown_pipelines());
        if created != table.created {
            table.created = created;
            let ([graphics, compute, builder], [a, b]) = created;
            lines.push(format!(
                "drop why: pipelines the title has created: {graphics} graphics, {compute} compute ({builder} of them the \
                 structure builder's); {a} and {b} more were asked for through device methods 0x1c8 and 0x218, which are not \
                 implemented: the title got none of those"
            ));
        }
        // Whether everything recorded is accounted for.
        let [draws, dispatches, _] = crate::list::totals();
        let now = [draws + dispatches, crate::draw::drawn() + crate::draw::dispatched(), table.told];
        if now[0] != table.accounted[0] {
            lines.push(account(now, table.accounted, frames));
            table.accounted = now;
        }
        lines
    };
    for line in lines {
        tracing::info!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn what() -> String {
        "[compute 0123456789abcdef]".to_owned()
    }

    /// The first drop of programs as a kind for a cause is told at once,
    /// with the frame; later ones are counted only.
    #[test]
    fn the_first_drop_is_told_once() {
        let mut t = Table::default();
        let line = t.dropped((0x10, "k"), what, (Kind::Dispatch, Cause::Making), (7, 1500));
        assert_eq!(
            line.as_deref(),
            Some(
                "drop why: first: frame 7 at 1.500 s: dispatch, its host pipeline was being made: [compute \
                 0123456789abcdef]"
            )
        );
        assert_eq!(t.dropped((0x10, "k"), what, (Kind::Dispatch, Cause::Making), (7, 1501)), None);
        assert_eq!(t.dropped((0x10, "k"), what, (Kind::Dispatch, Cause::Making), (8, 1600)), None);
        let e = &t.entries[0];
        assert_eq!((e.total, e.frames, e.first, e.last), (3, 2, (7, 1500), (8, 1600)));
        // Another cause, or another kind, is another entry.
        assert!(t.dropped((0x10, "k"), what, (Kind::Dispatch, Cause::BuilderOff), (8, 1601)).is_some());
        assert!(t.dropped((0x10, "k"), what, (Kind::Indirect(2), Cause::Making), (8, 1602)).is_some());
        assert_eq!(t.entries.len(), 3);
    }

    /// A run after drops is told the first time, with how long the hole
    /// was and the pipeline's times; runs since the last drop are counted,
    /// and a drop begins the count anew.
    #[test]
    fn a_run_after_drops_ends_the_hole() {
        let mut t = Table::default();
        t.dropped((0x10, "k"), what, (Kind::Dispatch, Cause::Making), (7, 1500));
        t.dropped((0x10, "k"), what, (Kind::Dispatch, Cause::Making), (9, 1700));
        // A pipeline that was never dropped tells nothing.
        assert!(t.ran((0x20, "other"), Times::default(), (10, 1800)).is_empty());
        let times = Times { asked: 1500, made: 1750, took: 240 };
        let lines = t.ran((0x10, "k"), times, (10, 1800));
        assert_eq!(
            lines,
            vec![
                "drop why: ran: frame 10 at 1.800 s: dispatch [compute 0123456789abcdef] after 2 drops in 2 frames, frames 7 to 9 \
                 (1.500 s to 1.700 s); its pipeline was asked for by a draw at 1.500 s and made at 1.750 s, in 240 ms of making"
                    .to_owned()
            ]
        );
        assert!(t.ran((0x10, "k"), times, (11, 1900)).is_empty());
        assert_eq!(t.entries[0].ran_since, 2);
        t.dropped((0x10, "k"), what, (Kind::Dispatch, Cause::Making), (12, 2000));
        assert_eq!(t.entries[0].ran_since, 0);
    }

    /// Another pipeline at an address the title used before is not taken
    /// for the one that was dropped there.
    #[test]
    fn an_address_used_again_is_another_pipeline() {
        let mut t = Table::default();
        t.dropped((0x10, "old"), what, (Kind::Draw, Cause::Making), (1, 100));
        assert!(t.ran((0x10, "new"), Times::default(), (2, 200)).is_empty());
        assert_eq!(t.entries[0].ran_since, 0);
        assert!(t.dropped((0x10, "new"), what, (Kind::Draw, Cause::Making), (3, 300)).is_some());
        assert_eq!(t.entries.len(), 2);
        assert_eq!(t.ran((0x10, "new"), Times::default(), (4, 400)).len(), 1);
        assert_eq!((t.entries[0].ran_since, t.entries[1].ran_since), (0, 1));
    }

    /// A report has the structure builder's drops as one line, the others
    /// per program with the most first, and the making times; counts begin
    /// anew after it.
    #[test]
    fn a_report_tells_what_was_dropped_since_the_last() {
        let mut t = Table::default();
        for frame in 0..10 {
            t.dropped((0x10, "a"), || "[compute a]".into(), (Kind::Dispatch, Cause::BuilderOff), (frame, 100));
            t.dropped((0x20, "b"), || "[compute b]".into(), (Kind::Indirect(2), Cause::BuilderOff), (frame, 100));
            t.dropped((0x20, "b"), || "[compute b]".into(), (Kind::Indirect(2), Cause::BuilderOff), (frame, 100));
        }
        t.dropped((0x30, "c"), || "[vertex c, pixel d]".into(), (Kind::Draw, Cause::Making), (3, 100));
        for _ in 0..4 {
            t.dropped((0x40, "e"), || "[compute e]".into(), (Kind::Dispatch, Cause::Other("a root at 0")), (3, 100));
        }
        t.made(5);
        t.made(1500);
        let lines = t.report(10.0, 200);
        assert_eq!(
            lines,
            vec![
                "drop why: 3.0/frame: the structure builder is off (VICTORY_BVH_BUILDER=1: runs): 1.0 dispatches, 2.0 indirect, of \
                 2 pipelines"
                    .to_owned(),
                "drop why: 0.1/frame: its host pipeline was being made, of 1 pipelines and kinds".to_owned(),
                "drop why: 0.40/frame: dispatch, not bound: a root at 0: [compute e]".to_owned(),
                "drop why: 0.10/frame: draw, its host pipeline was being made: [vertex c, pixel d]".to_owned(),
                "drop why: host pipelines made since the last line: 2 (1 under 10 ms, 0 under 100 ms, 0 under 1 s, 1 under 3 s, 0 \
                 longer; 1.5 s in all, the longest 1.5 s)"
                    .to_owned(),
            ]
        );
        assert!(t.report(10.0, 300).is_empty());
    }

    /// The accounting line says what was recorded, run and told as not run
    /// since the last report, and what is neither.
    #[test]
    fn the_account_names_what_is_neither_run_nor_told() {
        let line = account([1000, 900, 60], [500, 450, 40], 10.0);
        assert_eq!(
            line,
            "drop why: per frame 50.0 draws and dispatches recorded, 45.0 run, 2.0 told as not run, 3.0 neither; since the \
             start 1000 recorded, 40 neither"
        );
    }

    /// A quarter of a minute after the last drop an entry is settled: the
    /// line says whether the programs ran since; the minute's line names
    /// those dropped while being made that did not.
    #[test]
    fn what_never_ran_after_its_pipeline_came_is_named() {
        let mut t = Table::default();
        t.dropped((0x10, "a"), || "[compute a]".into(), (Kind::Dispatch, Cause::Making), (5, 1000));
        t.dropped((0x20, "b"), || "[compute b]".into(), (Kind::Dispatch, Cause::Making), (5, 1000));
        t.ran((0x20, "b"), Times::default(), (9, 2000));
        assert!(t.report(1.0, 1000 + SETTLE_MS - 1).iter().all(|l| !l.contains("settled")));
        let lines = t.report(1.0, 1000 + SETTLE_MS);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].ends_with("1 drops in 1 frames, frames 5 to 5 (1.000 s to 1.000 s); it has NOT run since its last drop"));
        assert!(lines[1].ends_with("it ran first after a drop in frame 9 at 2.000 s, and 1 times since its last drop"));
        // Settled once.
        for _ in 3..12 {
            assert!(t.report(1.0, 60_000).is_empty());
        }
        let lines = t.report(1.0, 60_000);
        assert_eq!(
            lines,
            vec![
                "drop why: so far 2 pipelines and kinds were dropped while their host pipeline was made; 1 of them have not run \
                 since their last drop:"
                    .to_owned(),
                "drop why: not run since: dispatch [compute a] (1 drops, frames 5 to 5)".to_owned(),
            ]
        );
    }
}
