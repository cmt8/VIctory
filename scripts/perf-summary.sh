#!/bin/sh
# Summarises a run log (see docs/linux-proton.md, "Taking a profile").
# Python 3, standard library only; kept as a shell script because the
# asset policy lists the file types this repository may hold.
exec python3 -c "$(sed -e '1,/^#PYTHON$/d' "$0")" "$@"
#PYTHON
#!/usr/bin/env python3
"""Summarise a VIctory run log: frame times and per-frame costs, split into
the first minutes after world entry and the rest.

    perf-summary.py ~/victory-logs/run-HHMMSS.log [--first-min 5]
                    [--entry HH:MM:SS | --entry-draws 1000] [--all-sites]

Reads the status lines the release build writes (the frame-rate line every
5 s, the per-second lines), the frame log (VICTORY_FRAME_LOG=1) and the
memory-call lines (VICTORY_VM_STATS=1). A line that is absent leaves its row
as "-". Each status line already holds per-frame averages over its own
second or seconds; a row's number is the median of those lines in the
window. Frame times come from the frame log's every-frame list when it is
there, else from the frame-rate lines (each standing for fps frames).
Timestamps in the log are UTC; so are the times printed here.
Standard library only.
"""

import argparse
import datetime as dt
import re
import statistics
import sys

ANSI = re.compile(r"\x1b\[[0-9;]*m")
STAMP = re.compile(r"^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?)Z")
NUM = r"(-?[0-9]+(?:\.[0-9]+)?)"

# (row name, the line's marker, a pattern with one number group), printed
# in this order.
ROWS = [
    # The frame-rate line (every 5 s, the last second's frames).
    ("frame: ms a frame (fps line)", "fps, ", rf" {NUM} ms/frame \("),
    ("frame: draws", "fps, ", rf"\), {NUM} draws/frame"),
    ("split: preparing submissions", "fps, ", rf"ms/frame \({NUM} preparing"),
    ("split: copies up (ms)", "fps, ", rf"preparing: copies {NUM} ms"),
    ("split: copies up (MB)", "fps, ", rf"preparing: copies [0-9.]+ ms {NUM} MB"),
    ("split: textures (ms)", "fps, ", rf"textures {NUM} ms [0-9.]+ MB; "),
    ("split: waiting on the GPU", "fps, ", rf"; {NUM} on the GPU"),
    ("split: copy-back (ms)", "fps, ", rf"; {NUM} copying back"),
    ("split: copy-back (MB)", "fps, ", rf"copying back {NUM} MB"),
    ("split: pipeline waits", "fps, ", rf"waited for {NUM} ms a frame"),
    ("split: heap buffer passes (ms)", "fps, ", rf"heap buffers [0-9.]+ passes {NUM} ms"),
    # Per-second lines.
    ("split: descriptor sync (ms)", "descriptor records the title copied", rf", {NUM} ms$"),
    ("split: heap pass, texture part (ms)", "the pass over the heap per frame", rf": {NUM} ms in its"),
    ("card: idle (ms)", "the card's clock per frame", rf"nothing of ours queued {NUM} ms"),
    ("card: busy (%)", "the card's clock per frame", rf"the card about {NUM}% busy"),
    ("card: submitter stood waiting (ms)", "the card's clock per frame", rf"waiting for the card {NUM} ms"),
    ("textures: refresh, all threads (ms)", "texture refreshes per frame, all threads", rf"all threads: {NUM} ms"),
    ("textures: asking watch+guards (ms)", "texture refreshes per frame, all threads", rf"{NUM} ms asking the write watch"),
    ("textures: waiting for remaps (ms)", "texture refreshes per frame, all threads", rf"{NUM} ms waiting for title memory to"),
    ("lock: guard protects, all (ms)", "the guards' protection changes per frame", rf"; {NUM} ms in all"),
    ("lock: write faults taken", "the guards' protection changes per frame", rf"; {NUM} write faults taken"),
    ("lock: write-watch askings", "title memory copied by the refreshes per frame", rf"; {NUM} write-watch askings"),
    ("lock: write-watch askings (ms)", "title memory copied by the refreshes per frame", rf"write-watch askings of the kernel in {NUM} ms"),
    ("copies: into card copies (MB)", "title memory copied by the refreshes per frame", rf"into copies in the card's memory {NUM} MB"),
    ("copies: into card copies (ms)", "title memory copied by the refreshes per frame", rf"card's memory [0-9.]+ MB in {NUM} ms"),
    # The frame log's usual frame (VICTORY_FRAME_LOG=1).
    ("framelog: render thread ran", "usual frame (the mean of", rf"render thread: ran {NUM} ms"),
    ("framelog: in the present call", "usual frame (the mean of", rf"in the present call {NUM}"),
    ("framelog: in submit calls", "usual frame (the mean of", rf"in submit calls {NUM}"),
    ("framelog: in list calls", "usual frame (the mean of", rf"in list calls {NUM}"),
    ("framelog: submit thread at work", "usual frame (the mean of", rf"submit thread: at work {NUM}"),
    ("framelog: preparing", "usual frame (the mean of", rf"submissions, preparing {NUM}"),
    ("framelog: waiting for the card", "usual frame (the mean of", rf"waiting for the card {NUM}; copies"),
    ("framelog: copies up (ms)", "usual frame (the mean of", rf"; copies {NUM} ms"),
    ("framelog: copy-back (ms)", "usual frame (the mean of", rf"copy-back {NUM} ms"),
    ("framelog: remapping calls", "usual frame (the mean of", rf"remapping: {NUM} calls"),
    ("framelog: remapping (ms)", "usual frame (the mean of", rf"remapping: [0-9.]+ calls {NUM} ms"),
    ("framelog: page faults", "usual frame (the mean of", rf"faults: {NUM} page"),
    ("framelog: guard faults", "usual frame (the mean of", rf"page, {NUM} guard"),
    ("framelog: process kernel ms", "usual frame (the mean of", rf"process: [0-9.]+ user {NUM} kernel"),
    ("framelog: process user ms", "usual frame (the mean of", rf"process: {NUM} user"),
    # Memory calls (VICTORY_VM_STATS=1).
    ("vm: all our calls (count)", "all our calls per frame: ", rf"all our calls per frame: {NUM} calls"),
    ("vm: all our calls (ms)", "all our calls per frame: ", rf"all our calls per frame: [0-9.]+ calls {NUM} ms"),
    ("vm: of it in slow calls (ms)", "all our calls per frame: ", rf"over the slow mark taking {NUM} ms"),
    ("vm: on submitting threads (ms)", "all our calls per frame: ", rf"by thread kind: submit {NUM} ms"),
    ("vm: on the presenting thread (ms)", "all our calls per frame: ", rf"by thread kind: submit [0-9.]+ ms, present {NUM} ms"),
    ("vm: on recording threads (ms)", "all our calls per frame: ", rf"present [0-9.]+ ms, record {NUM} ms"),
    ("vm: on other threads (ms)", "all our calls per frame: ", rf"record [0-9.]+ ms, other {NUM} ms$"),
]

VM_SITE = re.compile(
    rf"vm stats: site (\S+) per frame: {NUM} calls {NUM} ms \([^)]*\), {NUM} slow taking {NUM} ms, longest {NUM} ms"
)
VM_THREAD = re.compile(
    rf"vm stats: thread (\d+) (.*?) \(([^)]*)\) per frame: ran {NUM} ms \(user {NUM}, kernel {NUM}\); in our calls {NUM} ms in {NUM} calls, {NUM} ms of it"
)
FPS = re.compile(rf"\b{NUM} fps, {NUM} ms/frame .*?\), {NUM} draws/frame")
FRAME_TIMES = re.compile(r"frame times: (\d+) frames in .*?each: ([0-9 ]+)$")
LONG = re.compile(r"frame times: .*?; (\d+) long \(over")


def stamp(line):
    m = STAMP.match(line)
    if not m:
        return None
    text = m.group(1)
    if "." in text:
        head, frac = text.split(".")
        text = head + "." + (frac + "000000")[:6]
        return dt.datetime.strptime(text, "%Y-%m-%dT%H:%M:%S.%f")
    return dt.datetime.strptime(text, "%Y-%m-%dT%H:%M:%S")


def pct(values, p):
    if not values:
        return None
    v = sorted(values)
    k = min(len(v) - 1, max(0, int(round(p / 100 * (len(v) - 1)))))
    return v[k]


def fmt(x, width=9):
    if x is None:
        return "-".rjust(width)
    if abs(x) >= 100:
        return f"{x:{width}.0f}"
    if abs(x) >= 10:
        return f"{x:{width}.1f}"
    return f"{x:{width}.2f}"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("log")
    ap.add_argument("--first-min", type=float, default=5.0, help="minutes after world entry in the first window (5)")
    ap.add_argument("--entry", help="world entry, HH:MM:SS UTC as in the log (default: the first frame-rate line with --entry-draws draws)")
    ap.add_argument("--entry-draws", type=float, default=1000.0, help="draws a frame that mark world entry (1000)")
    ap.add_argument("--min-draws", type=float, default=100.0,
                    help="frame-rate lines with fewer draws a frame (menus, loading screens) are left out of the frame times (100)")
    ap.add_argument("--all-sites", action="store_true", help="every vm-stats site and thread, not the top ones")
    a = ap.parse_args()

    # (time, row, value) of every match; frame times; fps lines.
    values = []
    frames_each = []  # (time, ms) from the frame log
    long_frames = []  # (time, count)
    fps_lines = []  # (time, fps, ms, draws)
    sites = {}  # word -> list of (time, calls, ms, slow calls, slow ms, longest)
    threads = {}  # label -> list of (time, ran, in calls ms, calls, slow ms)
    first = last = None
    markers = {}
    for name, marker, pattern in ROWS:
        markers.setdefault(marker, []).append((name, re.compile(pattern)))
    with open(a.log, errors="replace") as f:
        for raw in f:
            line = ANSI.sub("", raw.rstrip("\n"))
            t = stamp(line)
            if t is None:
                continue
            first = first or t
            last = t
            for marker, rows in markers.items():
                if marker in line:
                    for name, rx in rows:
                        m = rx.search(line)
                        if m:
                            values.append((t, name, float(m.group(1))))
            if "fps, " in line:
                m = FPS.search(line)
                if m:
                    fps_lines.append((t, float(m.group(1)), float(m.group(2)), float(m.group(3))))
            if "frame times: " in line:
                m = FRAME_TIMES.search(line)
                if m:
                    frames_each += [(t, float(x)) for x in m.group(2).split()]
                m = LONG.search(line)
                if m:
                    long_frames.append((t, int(m.group(1))))
            if "vm stats: site " in line:
                m = VM_SITE.search(line)
                if m:
                    sites.setdefault(m.group(1), []).append((t, *map(float, m.groups()[1:])))
            if "vm stats: thread " in line:
                m = VM_THREAD.search(line)
                if m:
                    label = f"{m.group(1)} {m.group(2)} ({m.group(3)})"
                    g = list(map(float, m.groups()[3:]))
                    threads.setdefault(label, []).append((t, g[0], g[3], g[4], g[5]))
    if first is None:
        sys.exit(f"{a.log}: no timestamped lines")

    # World entry.
    entry = None
    if a.entry:
        h, mi, s = (int(x) for x in a.entry.split(":"))
        entry = first.replace(hour=h, minute=mi, second=s, microsecond=0)
        if entry < first:
            entry += dt.timedelta(days=1)
    else:
        entry = next((t for t, _, _, d in fps_lines if d >= a.entry_draws), None)
    if entry is None:
        print(f"no world entry found (no frame-rate line with {a.entry_draws:.0f} draws or more); all of the run is 'rest'")
        entry = first
        cut = first
    else:
        cut = entry + dt.timedelta(minutes=a.first_min)
    windows = [
        (f"first {a.first_min:g} min", lambda t: entry <= t < cut),
        ("rest", lambda t: t >= cut),
    ]

    print(f"log {a.log}")
    how = "as given" if a.entry else f"the first frame-rate line with >= {a.entry_draws:.0f} draws"
    print(f"  {first:%H:%M:%S} to {last:%H:%M:%S} UTC ({(last - first).total_seconds() / 60:.1f} min); "
          + (f"world entry {entry:%H:%M:%S} UTC ({how})" if cut != first else "world entry not found"))
    head = f"{'':40}" + "".join(f"{w[0]:>14}" for w in windows)
    print()
    print(head)

    # Frames and frame times.
    def frame_rows():
        out = []
        for label, inside in windows:
            if frames_each:
                ms = [x for t, x in frames_each if inside(t)]
                n = len(ms)
                source = "frame log"
            else:
                # Each frame-rate line stands for its second's frames; the
                # frames between two lines are fps times the gap.
                ms, n, prev = [], 0.0, None
                for t, fps, per, draws in fps_lines:
                    if inside(t) and draws >= a.min_draws:
                        ms += [per] * max(1, int(round(fps)))
                        if prev is not None:
                            n += fps * (t - prev).total_seconds()
                    prev = t
                source = f"fps lines, >= {a.min_draws:g} draws"
            out.append((n, statistics.median(ms) if ms else None, pct(ms, 95), source,
                        sum(c for t, c in long_frames if inside(t)) if long_frames else None))
        return out

    fr = frame_rows()
    src = fr[0][3]
    est = " (estimated)" if src != "frame log" else ""
    print(f"{'frames' + est:40}" + "".join(fmt(r[0], 14) if r[0] else f"{'-':>14}" for r in fr))
    print(f"{'frame time median (ms)':40}" + "".join(fmt(r[1], 14) for r in fr))
    print(f"{'frame time p95 (ms)':40}" + "".join(fmt(r[2], 14) for r in fr))
    if long_frames:
        print(f"{'long frames (frame log)':40}" + "".join(fmt(r[4], 14) for r in fr))
    print(f"  (frame times from the {src})")

    # The rows, per-frame medians of the lines in each window.
    print()
    print("per-frame costs, median of the status lines in each window (ms unless said; a line is the mean of its second's frames):")
    for name, _, _ in ROWS:
        cols = []
        for _, inside in windows:
            v = [x for t, n, x in values if n == name and inside(t)]
            cols.append(statistics.median(v) if v else None)
        if all(c is None for c in cols):
            continue
        print(f"  {name:38}" + "".join(fmt(c, 14) for c in cols))

    # Memory calls by site and by thread.
    if sites:
        print()
        print("vm stats by caller, per frame: calls, ms, ms in slow calls, longest single call (median of lines):")
        rank = sorted(sites, key=lambda w: -statistics.median(x[2] for x in sites[w]))
        for word in rank if a.all_sites else rank[:12]:
            for label, inside in windows:
                v = [x for x in sites[word] if inside(x[0])]
                if not v:
                    continue
                med = lambda i: statistics.median(x[i] for x in v)
                print(f"  {word:22} {label:>12}: {med(1):8.1f} calls {med(2):8.2f} ms, slow {med(4):7.2f} ms, longest {max(x[5] for x in v):7.2f} ms")
    if threads:
        print()
        print("vm stats by thread, per frame: ran (processor time), in our calls, of it slow (median of lines):")
        rank = sorted(threads, key=lambda k: -statistics.median(x[2] for x in threads[k]))
        for label in rank if a.all_sites else rank[:10]:
            for w, inside in windows:
                v = [x for x in threads[label] if inside(x[0])]
                if not v:
                    continue
                med = lambda i: statistics.median(x[i] for x in v)
                print(f"  {label[:46]:46} {w:>12}: ran {med(1):7.1f} ms, in our calls {med(2):7.2f} ms ({med(3):6.0f} calls), slow {med(4):7.2f} ms")
    if not values and not sites:
        print("(no status lines found)")


if __name__ == "__main__":
    main()
