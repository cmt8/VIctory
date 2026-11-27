//! `VICTORY_TWIN_WHY`: which programs are bound buffer views that do not
//! begin on 16 bytes of their host copy. Off unless set.
//!
//! A program that reads a table's buffer as raw bytes reads it through the
//! raw twin of the view's slot (`victory_shader::emit::raw_views`), and the
//! twin of a view that begins between two multiples of 16 was a null view
//! on every host but Windows with an AMD card (`descriptor::host::
//! twin_dwords`): zeros read, writes dropped. The frame line's count of such
//! twins does not say whose they are. With the switch, at every dispatch
//! (`=all`: every draw too) each buffer view its programs' tables hold is
//! looked at, and the log says:
//!
//! - `TWIN-WHY first`: once per program and table place, the first time the
//!   view bound there begins off 16 bytes;
//! - `TWIN-WHY per frame`: with the frame line, how many table buffer views
//!   were bound, how many of them off 16 bytes (and off a dword), by how
//!   many programs, and the programs with the most;
//! - `TWIN-WHY program`: for programs named in the switch's value
//!   (`VICTORY_TWIN_WHY=<hash>,<hash>`), per table place and frame line: the
//!   binds, where their views began past 16 (0, 4, 8, 12, off a dword) and
//!   how often that differed from the bind before.
//! - `TWIN-WHY twins`: with the frame line, the twins made of views off 16
//!   bytes, by what the view is (a shader resource or unordered access view,
//!   its stride, format and whether it is raw), where it begins past 16 in
//!   the title's memory and in its host copy, and how many of them show the
//!   view's bytes. These are all such twins, those of slots a program picks
//!   by an index it computes among them, which no table bind tells of.
//!
//! Names, places and counts only: nothing of a buffer's bytes.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// What the switch asks for: nothing, dispatches, draws too, and the
/// programs to tell of place by place.
struct Asked {
    draws: bool,
    programs: Vec<String>,
}

fn asked() -> Option<&'static Asked> {
    static ASKED: OnceLock<Option<Asked>> = OnceLock::new();
    ASKED
        .get_or_init(|| {
            let value = std::env::var("VICTORY_TWIN_WHY").ok()?;
            let value = value.trim();
            if value.is_empty() || value == "0" {
                return None;
            }
            let programs = value
                .split(',')
                .map(str::trim)
                .filter(|p| p.len() == 16 && p.bytes().all(|b| b.is_ascii_hexdigit()))
                .map(str::to_ascii_lowercase)
                .collect();
            Some(Asked {
                draws: value.split(',').any(|p| p.trim() == "all"),
                programs,
            })
        })
        .as_ref()
}

/// Whether binds are looked at: those of dispatches, or of draws
/// (`compute` false).
pub(crate) fn on(compute: bool) -> bool {
    asked().is_some_and(|a| compute || a.draws)
}

/// A program's table place: its hash, the kind of view (`S`, `U`), the root
/// parameter, the dwords into the table and the stride the program names.
type Place = (String, char, u8, u32, u32);

#[derive(Default, Clone, Copy)]
struct Seen {
    /// Binds since the last line.
    binds: u32,
    /// Of them by where the view began past 16 bytes: 0, 4, 8, 12, off a dword.
    by: [u32; 5],
    /// Binds whose view began elsewhere past 16 than the one before.
    moved: u32,
    /// Where the last one began, past 16.
    last: u8,
    /// Whether one off 16 bytes was ever told of.
    told: bool,
}

static SEEN: Mutex<Option<HashMap<Place, Seen>>> = Mutex::new(None);

/// A buffer view bound at `place`, which begins `start` bytes into its host
/// copy.
pub(crate) fn bound(place: Place, start: u64) {
    let past = (start % 16) as u8;
    let class = if past % 4 != 0 { 4 } else { usize::from(past / 4) };
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = seen.get_or_insert_with(HashMap::new);
    let first = {
        let s = seen.entry(place.clone()).or_default();
        if s.binds > 0 && s.last != past {
            s.moved += 1;
        }
        s.binds += 1;
        s.by[class] += 1;
        s.last = past;
        past != 0 && !std::mem::replace(&mut s.told, true)
    };
    if first {
        let (hash, kind, slot, dwords, stride) = place;
        tracing::info!(
            "TWIN-WHY first: program {hash} {kind}/Table+{dwords} (root parameter {slot}, stride {stride}) is bound a view that begins {past} bytes past 16 in its host copy (frame {})",
            crate::frame::current()
        );
    }
}

/// A kind of view whose twin was made off 16 bytes: unordered access or
/// not, the view's stride, whether it is raw, its format, and where it
/// begins past 16 bytes in the title's memory and in its host copy.
type Twin = (bool, u32, bool, u32, u8, u8);

/// Twins made since the last line, and those of them that show their view.
static TWINS: Mutex<Option<HashMap<Twin, [u32; 2]>>> = Mutex::new(None);

/// The twin of a view that begins `by` bytes past 16 in its host copy was
/// made: of an unordered access view or not, of `kind` (stride, raw,
/// format), which begins `title` bytes past 16 in the title's memory;
/// `shown`: whether the twin shows the view's bytes (it is a null view
/// otherwise).
pub(crate) fn twin(unordered: bool, kind: (u32, bool, u32), title: u8, by: u8, shown: bool) {
    let mut twins = TWINS.lock().unwrap_or_else(|e| e.into_inner());
    let n = twins.get_or_insert_with(HashMap::new).entry((unordered, kind.0, kind.1, kind.2, title, by)).or_default();
    n[0] += 1;
    n[1] += u32::from(shown);
}

/// The twins line's text, of what was counted over `frames` frames; `None`
/// if nothing was.
fn twins_text(twins: &HashMap<Twin, [u32; 2]>, frames: f64) -> Option<String> {
    let mut kinds: Vec<(&Twin, &[u32; 2])> = twins.iter().collect();
    if kinds.is_empty() {
        return None;
    }
    kinds.sort_unstable_by(|a, b| b.1[0].cmp(&a.1[0]).then(a.0.cmp(b.0)));
    let (made, shown) = kinds.iter().fold((0u64, 0u64), |(m, s), (_, n)| (m + u64::from(n[0]), s + u64::from(n[1])));
    let most: Vec<String> = kinds
        .iter()
        .take(16)
        .map(|&(&(unordered, stride, raw, format, title, by), n)| {
            format!(
                "{} stride {stride} format {format}{} at {title} of the title's and {by} of the copy's: {:.1} ({:.1} shown)",
                if unordered { "U" } else { "S" },
                if raw { " raw" } else { "" },
                f64::from(n[0]) / frames,
                f64::from(n[1]) / frames
            )
        })
        .collect();
    Some(format!(
        "TWIN-WHY twins per frame: {:.1} made of views off 16 bytes of their host copy, {:.1} of them show the view's bytes, of {} kinds; most: {}",
        made as f64 / frames,
        shown as f64 / frames,
        kinds.len(),
        most.join("; ")
    ))
}

/// With the frame line, over `frames` frames.
pub(crate) fn line(frames: f64) {
    let Some(asked) = asked() else { return };
    if let Some(twins) = TWINS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        if let Some(text) = twins_text(twins, frames) {
            tracing::info!("{text}");
        }
        twins.clear();
    }
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let Some(seen) = seen.as_mut() else { return };
    let (mut binds, mut off, mut odd) = (0u64, 0u64, 0u64);
    let mut programs: HashMap<&str, u32> = HashMap::new();
    for (place, s) in seen.iter() {
        let off_here = s.by[1] + s.by[2] + s.by[3] + s.by[4];
        binds += u64::from(s.binds);
        off += u64::from(off_here);
        odd += u64::from(s.by[4]);
        if off_here > 0 {
            *programs.entry(place.0.as_str()).or_default() += off_here;
        }
    }
    if binds > 0 {
        let mut most: Vec<(&str, u32)> = programs.iter().map(|(&p, &n)| (p, n)).collect();
        most.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let most: Vec<String> = most.iter().take(12).map(|(p, n)| format!("{p} {:.1}", f64::from(*n) / frames)).collect();
        tracing::info!(
            "TWIN-WHY per frame: {:.1} buffer views bound through tables{}, {:.1} of them off 16 bytes of their host copy ({:.1} off a dword), by {} programs; most: {}",
            binds as f64 / frames,
            if asked.draws { "" } else { " by dispatches" },
            off as f64 / frames,
            odd as f64 / frames,
            programs.len(),
            most.join(", ")
        );
    }
    let mut named: Vec<(&Place, &Seen)> =
        seen.iter().filter(|(place, s)| s.binds > 0 && asked.programs.contains(&place.0)).collect();
    named.sort_unstable_by(|a, b| a.0.cmp(b.0));
    for ((hash, kind, slot, dwords, stride), s) in named {
        tracing::info!(
            "TWIN-WHY program {hash} {kind}/Table+{dwords} (root parameter {slot}, stride {stride}): {:.1} binds a frame; their views began past 16 bytes at 0: {}, 4: {}, 8: {}, 12: {}, off a dword: {}; {} began elsewhere than the bind before",
            f64::from(s.binds) / frames,
            s.by[0],
            s.by[1],
            s.by[2],
            s.by[3],
            s.by[4],
            s.moved
        );
    }
    for s in seen.values_mut() {
        *s = Seen { last: s.last, told: s.told, ..Seen::default() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Binds are counted by where their view begins past 16 bytes, and a
    /// place is told of once.
    #[test]
    fn binds_are_counted_by_where_their_view_begins() {
        let place = |n: u32| ("00000000000000aa".to_owned(), 'S', 1u8, n, 1u32);
        for start in [32u64, 36, 36, 40, 44, 6] {
            bound(place(8), start);
        }
        bound(place(16), 64);
        let seen = SEEN.lock().unwrap();
        let s = seen.as_ref().unwrap()[&place(8)];
        assert_eq!((s.binds, s.by, s.moved, s.told), (6, [1, 2, 1, 1, 1], 4, true));
        let s = seen.as_ref().unwrap()[&place(16)];
        assert_eq!((s.binds, s.by, s.told), (1, [1, 0, 0, 0, 0], false));
    }

    /// Twins are counted by the kind of their view, and the line tells the
    /// kinds with the most first.
    #[test]
    fn twins_are_counted_by_the_kind_of_their_view() {
        let mut twins = HashMap::new();
        for (key, shown) in [((false, 12, false, 0, 4, 4), true), ((false, 12, false, 0, 4, 4), true), ((true, 0, false, 57, 0, 6), false)] {
            let n: &mut [u32; 2] = twins.entry(key).or_default();
            n[0] += 1;
            n[1] += u32::from(shown);
        }
        let text = twins_text(&twins, 2.0).unwrap();
        assert!(text.starts_with("TWIN-WHY twins per frame: 1.5 made of views off 16 bytes of their host copy, 1.0 of them show the view's bytes, of 2 kinds; most: S stride 12 format 0 at 4 of the title's and 4 of the copy's: 1.0 (1.0 shown); U stride 0 format 57 at 0 of the title's and 6 of the copy's: 0.5 (0.0 shown)"), "{text}");
        assert!(twins_text(&HashMap::new(), 1.0).is_none());
    }
}
