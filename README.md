# VIctory on Linux under Proton (a fork)

This is a fork of [Marvisak/VIctory](https://github.com/Marvisak/VIctory)
for running the title on Linux under Proton (Wine) with vkd3d-proton, on an
AMD card with RADV. Upstream targets Windows hosts and closed the
Linux/Proton PR [#994](https://github.com/Marvisak/VIctory/pull/994) as out
of scope ("VIctory targets Windows hosts; ports, launchers and tooling for
other platforms are not being taken"), so the Linux work lives here. The
target is 30 fps, the rate the title is locked to on the console.

**The work is not on `main`.** `main` is upstream's code as of `ffe310e`
plus this section, kept that way so the fork stays a fork of
Marvisak/VIctory. The line to build and run is
[`linux/belo-next9`](https://github.com/cmt8/VIctory/tree/linux/belo-next9),
and it is not based on upstream `main`: it is the engine of
avatarmanz/VIctory-7900XTX (`main` at `014d52ded`) as Belobug published it
in Belobug/VIctory-Belobug-AMD (`a-publier` at `4eb6cfdf9`), both merged,
with this fork's changes on top. Upstream's README follows this section,
unchanged; it describes the Windows build.

Both of those repositories, and their owners' GitHub accounts, stopped being
reachable on 2026-11-27 (HTTP 404). The tracker
[PortWatch VI](https://portwatch-vi.xyz/) reports that GitHub removed
Belobug's repository and account that day for a Terms of Service violation;
it does not say which term, no notice from GitHub is known here, and nothing
is known about avatarmanz's. Their history, with its authorship, is kept in
this fork's branches; it holds source code only, no files of the title
(`cargo xtask policy` passes on it).

## Status (2026-11-27)

**Playable, not yet at 30 fps.** Hand-played by the fork's owner with
`linux/belo-next9` on one machine: Arch Linux, kernel 7.3, AMD RX 7900 XTX
(RADV, Mesa 26.3), Ryzen 7 7800X3D, 32 GB RAM, Proton Experimental with its
vkd3d-proton 3.2; output at 4K, the scene drawn at 2/3 of that (the title's
own scale). Other GPUs, drivers and Proton versions have not been tried.
It boots, the opening cutscene plays and the open world is playable; the
latest runs ended without a crash. About 23 fps in the opening cutscene, 20
idle on a boat, about 18 driving it.

Fixed in this fork, on top of the engine (a named switch gives the old way):

- Dark bars across the picture: the console's wave64 compute programs ran in
  forms made for 32-lane host waves; they now declare a 64-lane wave on hosts
  that have one (`f69acb689` and the commits after it; `VICTORY_COMPUTE_WAVE`).
- A segmentation fault the kernel sent under Wine: the write guard made a
  title thread's stack read-only because it lay in a range taken as view
  memory. The guards now ask what memory is before protecting it (`e8a5602b9`).
- Whole groups of trees vanishing for a few frames: a copied descriptor
  record waited 4 frames before it was mirrored; now 0 (`52cb402ac`;
  `VICTORY_HEAP_SYNC_GRACE`).
- The interface drawn at the wrong size when the output is not 4K: the
  display mode the title is told now follows `VICTORY_OUTPUT` (`0bb48a3a3`).
- A cost every frame from asking Wine about holes in the title's memory: now
  the runtime's own records answer (`99a6e73b2`; `VICTORY_HOLE_RECORDS=0`).
- Raw "twin" views of buffer views that do not begin on 16 bytes were null
  under vkd3d-proton; they are now made at the view's own start
  (`c6a1b4fec`; `VICTORY_TWIN_DWORDS=0`).
- The early depth-stencil declaration is on by default (`16f7a094f`).
  Renderer patch 1 (`VICTORY_PORT_CONSTANTS`) first checks that the card
  reads the title's memory in place; this host does not, so here it does
  nothing (`b83d21fad`).

Known wrong or open, all under investigation:

- Characters' lower faces (lips, teeth, jaw) are displaced and jitter in
  cutscenes. The current lead is one compute dispatch of the face network
  whose output differs from run to run on RADV and becomes stable when the
  driver's shader compiler waits after every memory instruction
  (`ACO_DEBUG=force-waitcnt`). The cause is not yet confirmed in play.
- Speed: the frame is bound by copying the card's writes back into the
  title's memory. In the owner's measurements about 34 ms of a roughly 50 ms
  frame is spent waiting on the card for those fetches.
- 100-220 draws a frame are dropped for want of a host pipeline, as the
  runtime reports them; the causes are being sorted.
- The engine's renderer patches are off by default. `VICTORY_PORT_GUARD=1`
  and `VICTORY_PORT_JOURNAL=1` were tried here and made no measurable
  difference; patch 1 cannot work on this host; the others are left off.

## Branches and tags

- `linux/belo-next9`: the line to build and run.
- `linux/belo-faces`, `linux/belo-back`, `linux/belo-drops`: work in progress
  on the faces, the copy-back cost and the dropped draws.
- `pr/guard-views-only`: the fix for the segmentation fault, rebuilt on
  Belobug's unmodified `a-publier` (`4eb6cfdf9`). It was offered there as a
  draft pull request before that repository went.
- `up/*`: small changes kept as separate branches on fxceless's tree
  (`0b0074e0b`); not submitted anywhere.
- `archive/*` tags: tips of older lines. `archive/fx-next22-picks` is the
  last tip of this fork's earlier line on fxceless's tree,
  `archive/hardware-test-*` the first test merges on upstream `main`.
  Superseded branches were removed on 2026-11-27, their tips kept as tags.

## What is ours and what is not

The engine on `linux/belo-next9` is avatarmanz's and Belobug's, built on
fxceless's tree; that branch's own README credits whom they took from. On
top of the two bases the branch has 73 commits that are not merges: 51
written here, 22 carried from other people's pull requests and forks under
their own names (`git log --no-merges 4eb6cfdf9..linux/belo-next9 --not
014d52ded` lists them).

Written here, by the fork's owner, with AI assistance (Claude; the commits
carry `Co-Authored-By` trailers), and tested by the owner's hand-played runs
and the Wine test suite: the fixes above and the Linux/Wine side (internal
shaders Wine's HLSL compiler builds, pipeline workers' threads and stacks,
the guards under Wine's memory calls). The earlier line's list, with its
sources, is in this README's previous version (`git show dd44f622b:README.md`).

## Running it

You need your own legally obtained copy of the title's files; none are
hosted or linked here (see [docs/asset-policy.md](docs/asset-policy.md)).
Copy `victory.example.toml` to `victory.toml` and set its paths (outside the
checkout). `linux/belo-next9` has no Linux launcher of its own; the Proton
wrapper of the earlier line starts `victory.exe` under Proton's Wine (it is
the script the owner's runs use), and this line keeps the same `config` and
`run ... --execute` commands. With Proton Experimental installed from Steam:

```sh
git clone -b linux/belo-next9 https://github.com/cmt8/VIctory.git && cd VIctory
rustup target add x86_64-pc-windows-msvc && cargo install --locked cargo-xwin
# cargo-xwin downloads the MSVC CRT and Windows SDK; this accepts their licence.
export XWIN_ACCEPT_LICENSE=1
cargo xwin build --release -p victory --target x86_64-pc-windows-msvc
git show archive/fx-next22-picks:scripts/proton-run.sh > scripts/proton-run.sh
chmod +x scripts/proton-run.sh && scripts/proton-run.sh config
scripts/proton-run.sh run /path/to/game/<the title's exe> --execute
```

The wrapper's notes (also a system Wine) are in
[docs/linux-proton.md](https://github.com/cmt8/VIctory/blob/archive/fx-next22-picks/docs/linux-proton.md)
at that tag. Tests run under Wine with `cargo xtask wine-test` (an `xwin
splat` in `~/.xwin`; `--vkd3d-proton <dir>` for vkd3d-proton's D3D12).

The owner's runs also set `VICTORY_NO_WATCHDOG=1 VICTORY_SHOW_AFTER=1
VICTORY_UNDER_WIDE=0 VICTORY_LAND_ECHO=0`,
`VKD3D_CONFIG=avoid_image_buffer_aliasing` and
`WINEDLLOVERRIDES=d3dcompiler_47=n` (and `VICTORY_CARD_STEPS=1` in the
latest); a run without them has not been compared. Some runs also used a
wineserver patched to keep a process's mapped views in a tree; that patch is
not in this repository.

Switches worth knowing on this line (each is described where it is read):

- `VICTORY_OUTPUT=<w>x<h>`: the output size, one of the title's own modes.
- `VICTORY_RENDER_SCALE=<0.5-1.0>`: the title's upscaler scale (2/3 unset).
- `VICTORY_CARD_STEPS=1|2`: steps that wait for the card run on it (off).
- `VICTORY_EARLY_DEPTH=0`: no pixel program declares early depth-stencil.
- `VICTORY_HEAP_SYNC_GRACE=<frames>`: how long a copied descriptor record
  waits before it is mirrored (0 unset; 4 was the old wait).
- `VICTORY_COMPUTE_WAVE`: unset, compute programs declare the console's wave
  size; `=32` and `=0` are the earlier ways, for comparison.
- `VICTORY_TWIN_DWORDS=0`: raw twins of views off 16 bytes are null again.
- `VICTORY_FRAME_WATCH=1`: a log line per frame (draws, drops, heap sync).

## Credits and upstreaming

VIctory is Marvisak's project, with its contributors. The engine of
`linux/belo-next9` is the work of avatarmanz and Belobug and of the people
whose work they carried; this fork's earlier line was based on fxceless's
tree ([fxceless/victory-fork](https://github.com/fxceless/victory-fork)).
Carried pull requests keep their authors in the git history.

Marvisak/VIctory `main` has had no new commits since 2026-11-25, and it has
more than 1,600 open pull requests. The fork's owner has not submitted the
`up/*` branches.

---

# Upstream README

The rest of this file is upstream's README as of `ffe310e`, unchanged.

# VIctory

Runs Grand Theft Auto VI (Xbox Series X|S) natively on Windows, from files
the user supplies.

The title's x86-64 code runs unchanged inside our process; VIctory supplies
the platform underneath it and converts the title's data, ahead of time where
it can and on first use where it must (shaders). See
[docs/architecture.md](docs/architecture.md).

## Status (2026-11-23)

**Not playable yet.** The title boots, runs its menus at its own 30 fps and
loads the world, where rendering fails.

| Stage | State |
|-------|-------|
| Boot, first-boot screens (accessibility, language, settings, legal) | works, 30 fps |
| Intro video with sound | works |
| Start screen, profile and saves | works |
| Loading into the world (about 50 s) | works |
| The world (first mission's intro) | reached and runs with indirect draws; its title card renders, the scene after it is still dark; 0.5-1.8 s per frame |

Tested on one machine: Windows 11, AMD RX 6700 XT.

> **Warning.** Entering the world on a real GPU can currently hang it, and
> Windows then resets the display driver (every monitor goes black for a
> moment). To look further, use a diagnostics build with
> `VICTORY_D3D12_WARP=1`, which renders on the software adapter.

What each part does today and what it still needs:

| Subsystem | Today | Needs |
|-----------|-------|-------|
| Loader | Runs the title's executable and its DLLs in our process; crash reports name the faulting code and are kept in `crashes.log` | – |
| Runtime services | One signed-in offline user, licences, packages, saves that survive a kill at any step, text entry dialog | Speech is silent; the dialog and relaunch are untested live |
| Network | Offline by design: the title cannot reach the network | – |
| Files and streaming | Archives and streamed data read, zlib and BCPack decoded in parallel | Loading is limited by writes into title memory |
| Input | XInput pads, keyboard, keyboard stand-in pad | Trigger rumble |
| Audio | Menu, video and loading sound; XMA voices and Opus streams decode | Reverb is silent; Opus results unverified against the title |
| GPU: menus | Draws, compute and presentation on host D3D12; only changed title memory is copied | Frame pacing drifts on one screen in release builds |
| GPU: world | Mesh pipelines, shader-written buffers, descriptor lifetimes, indirect execution (arguments clamped), pipelines made by worker threads and kept on disk | Most of the scene is still missing (untranslated shaders drop draws); frames take seconds; real queries, depth bounds, HDR and scaling are missing |
| Shaders | Console GPU code recompiled to DXIL at first use, cached on disk. Menus: all. World: mesh 8,191 of 8,506, vertex/pixel 10,617 of 12,346, compute 1,643 of 2,539 | 64-bit image atomics, merged geometry stages, the ray-tracing builder's shaders; some compute programs need a GPU with 64-lane waves |
| Textures | Console tiled layouts and detiling for the modes the title uses | Render-target metadata planes |
| Series S mode | Runs through the menus to the loading screen | The world, as above |

## Getting started

Requirements: Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with
shader model 6.6 (and mesh shaders for the world), Rust (pinned in
`rust-toolchain.toml`).

The title also needs several GiB of available Windows commit at startup.
Check **Committed** under **Task Manager → Performance → Memory**. If
`crashes.log` reports Windows error 1455, close other applications or increase
the Windows paging file size, then retry.

```powershell
cargo xtask install-hooks                # asset-policy check on every commit
copy victory.example.toml victory.toml   # then set your paths (outside the repo)
cargo test --workspace                   # needs no game data
cargo run -p victory -- config           # check your paths
```

Run the title:

```powershell
cargo build --release -p victory
target\release\victory.exe run <game dir>\<the title's exe> --execute
```

The window caption shows the framerate. Saves, settings and `crashes.log`
are kept under the data folder (default `%LOCALAPPDATA%\victory\data`), derived
data under the cache folder.

Input: XInput pads. With none connected the keyboard is a pad while the
title's window has focus:

| Keys | Pad |
|------|-----|
| W A S D / I J K L | left / right stick |
| arrows | D-pad |
| Enter, Backspace, X, Y | A, B, X, Y |
| Q / E, Z / C | shoulders, triggers |
| F / H | stick clicks |
| Esc, Tab | Menu, View |

## Commands

```
victory inspect <exe|dll> [--imports] [--exports]
victory census [paths...] [--missing] [--strict]
victory lib-exports <import.lib> [--list]
victory shader-info [paths...] [--verbose] [--disasm]
victory translate [paths...] [--validate] [--pipelines] [--list] [--jobs N]
victory run <exe> [--execute] [--unimplemented trap|zero] [--strict]
            [--console series-x|series-s] [-- <title arguments>]
victory config
```

Logging: `VICTORY_LOG=victory_loader=trace,info` (a release build logs
info and above).

## Debugging

A release build has no debugging aids. Build them in with

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug and trace output |
| `VICTORY_STACKS=20,60` | every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | logs a title u32 when it changes |
| `VICTORY_KEYS=30:Enter,35:X` | presses keys at those seconds, focus or not |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_DUMP_TARGETS=1` | with the two above, saves every render target of those frames |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | omit draws and dispatches, including indirect ones, whose pipeline contains a listed 16-digit shader hash |
| `VICTORY_GPU_TIMING=1` | with the above, each list of a submission run and timed on its own; slow lists logged with their slowest commands |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane instead of 2^20 (own shader cache entries); if a lower `n` makes world frames much faster, loops hitting the limit are what makes them slow |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first 32 XMA voices' context and input |

Every build appends crashes and host GPU failures to `crashes.log` in the
title's data folder.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
