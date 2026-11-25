# VIctory on Linux under Proton (a fork)

This is a fork of [Marvisak/VIctory](https://github.com/Marvisak/VIctory)
for running the title on Linux under Proton (Wine) with vkd3d-proton, tested
on AMD (RADV). Upstream targets Windows hosts and closed the Linux/Proton
PR [#994](https://github.com/Marvisak/VIctory/pull/994) as out of scope
("VIctory targets Windows hosts; ports, launchers and tooling for other
platforms are not being taken"), so the Linux work lives here.

The default branch, `main`, is upstream's code as of `ffe310e` plus this
section. The work is on the branches below. Upstream's README follows,
unchanged; it describes the Windows build.

## Status (2026-11-25)

**Not playable: about 6 fps in the world.** One person's observations on one
machine, with `linux/fx-next8`; other GPUs, drivers and Proton versions have
not been tried.

Machine: Arch Linux, kernel 7.3, AMD RX 7900 XTX (RADV, Mesa 26.3), Ryzen 7
7800X3D, 32 GB RAM, Proton Experimental (based on wine-11.0) with its own
vkd3d-proton.

| Stage | Seen |
|-------|------|
| Boot screens and menus | 30 fps |
| Boot logo video | plays cleanly |
| Entering the world | about four and a half minutes from launch, cutscenes skipped |
| First playable section (the boat) | renders close to right: sky, water, shoreline, characters and the map; the boat floats and the mission goes on. Known defects: grain on eyes, glass and some skin; 70-90 draws a frame dropped for pipelines not handled yet |
| Speed there | about 175 ms a frame (5.7 fps) at 3,900 draws; the title's own rate is 30 fps |
| Stability | Three 8-10 minute unattended runs of this branch and its parent without a crash. Before the fix carried from fxceless #1033, one run in five ended in a GPU page fault and an amdgpu ring reset; one earlier run ended in a title crash that has not been seen again or explained |

Where the frame goes now: about 55 ms preparing on the CPU (copying title
memory the card needs), 80-90 ms waiting for the card, 30 ms copying results
back. The card time is mostly work this layer adds, not the title's
rendering, and is what is being looked at next.

## Branches

| Branch | What it is |
|--------|------------|
| [`linux/fx-next8`](https://github.com/cmt8/VIctory/tree/linux/fx-next8) | Current best. `linux/fx-next` brought up to fxceless `main` of 2026-11-25, plus picks from other forks and open pull requests (texture copy-back from vx-layer, deferred views from fxceless #1033, plain copies and lazy alias guards for Wine, shader fixes) and a few fixes of our own. Every pick keeps its author and its `-x` line; `git log linux/fx-next..linux/fx-next8` lists them |
| [`linux/fx-next`](https://github.com/cmt8/VIctory/tree/linux/fx-next) | The branch the list below describes. [fxceless/victory-fork](https://github.com/fxceless/victory-fork)'s `main` (itself based on upstream `ffe310e`) plus the commits below |
| [`linux/hardware-test-v4-1279-port`](https://github.com/cmt8/VIctory/tree/linux/hardware-test-v4-1279-port) | Upstream `main` at `940d32f` plus upstream's Wine-related PRs (#151, #454, part of #994, #1152, #1218, #1849, #1279). Renders much less of the world; kept for comparison |
| other `linux/*` | Earlier test merges and drafts, kept as history |

## What is ours and what is not

`linux/fx-next` is fxceless/victory-fork `main` (`310c6eb`) plus 39 commits,
all listed here. Commits taken from elsewhere were cherry-picked with `-x`
and keep their authors.

From upstream pull requests (13 commits):

| Source | Author | Commits | What |
|--------|--------|---------|------|
| [#994](https://github.com/Marvisak/VIctory/pull/994) (closed) | slaktarn | `4adcfefd` `9cd164ba` `742e9c2c` `7de9675d` `a610e20a` `ed9855d3` | Wine fixes (empty pipeline library, missing ws2_32 exports, a DXIL hash test, one returning block per shader program); `docs/linux-proton.md`, `scripts/proton-run.sh` |
| [#151](https://github.com/Marvisak/VIctory/pull/151) | itsyusei99 | `b6973347` | 16 MiB default thread stack |
| [#454](https://github.com/Marvisak/VIctory/pull/454) | ruvenesh | `bb1f21e2` `c30a45ea` | opt-in `VICTORY_NO_WATCHDOG` |
| [#1218](https://github.com/Marvisak/VIctory/pull/1218) | neonbyte1 | `2df48987` `220ee1a4` `6ab4ee38` | `cargo xtask wine-test` |
| [#122](https://github.com/Marvisak/VIctory/pull/122) (upstream `d01b1707`) | Duff (Dufftwt) | `c27c8abd` | XAsync ends the call that finished |

From other forks (8 commits):

| Source | Author | Commits | What |
|--------|--------|---------|------|
| [fxceless #257](https://github.com/fxceless/victory-fork/pull/257) | Ortomanic | `fac4ca4a` `ff186e3a` | one latch per loop in emitted DXIL (dxil-spirv asserted on two back edges) |
| [fxceless #670](https://github.com/fxceless/victory-fork/pull/670) | Jonirulah | `ee2a5734` | no kernel write-watch asking for known guarded view memory |
| [fxceless #544](https://github.com/fxceless/victory-fork/pull/544) | justrealyme | `d5cf5fad` | shader cache keyed by the translator, `translate --cache` warm-up (includes upstream `c47b529` by Marvis) |
| [fxceless #108](https://github.com/fxceless/victory-fork/pull/108) | Kamiloj9 | `67a7e9a0` | adjacent physical-page scatter unmaps batched |
| [RecompLive/victory-fork](https://github.com/RecompLive/victory-fork) | RecompLive | `a425f208` `4e39b500` | its versions of upstream `c0228d9` (tjthebigbearcat-hub: 9-bit mesh primitive indices) and `ed0dbfd` (Kevin Pratap Sidhu: copy the title's writes under ranges shaders write) |
| [triippywhale/victory-fork](https://github.com/triippywhale/victory-fork), via RecompLive | Vax | `6a57a8dc` | a helper lane's `v_readfirstlane` reads its own value |

Written here (18 commits), by the fork's owner, with AI
assistance (Claude; the commits carry `Co-Authored-By` trailers):

- Internal shaders Wine's HLSL compiler builds (it has no Load on writable
  byte-address buffers, Interlocked only as functions, no embedded root
  signatures): the indirect argument clamp, the card compare, the depth
  plane pass (`e0416558` `f06bf40d` `0c33fd76`). Modelled on upstream
  [#1279](https://github.com/Marvisak/VIctory/pull/1279) (Mumogg).
- Memory that begins in free space is not watched, a world-load deadlock
  under Wine (`22afe22c`); the idea of upstream
  [#1849](https://github.com/Marvisak/VIctory/pull/1849) (crashbashash).
- Shader writes are copied back into title memory through the kernel read
  path, not `NtWriteVirtualMemory`/`WriteProcessMemory`, which are
  wineserver round trips under Wine (`38fe5065`; port of upstream `b58d198d`).
- Under Wine only, as Wine holds one process-wide memory-map lock across
  wineserver requests: texture reads without the remap lock (`18531a01`),
  page probes by reading (`8ac6ab8b`), one kept view per section (`a4cbcd01`).
- Pipeline workers get 16 MiB stacks (`2318b674`); under Wine a pipeline
  whose compile overflows its stack is left out instead of being waited for
  every frame, which was most of each ~600 ms frame in the run measured
  (`43686cca`); the frame line shows that wait (`93d72d5a`).
- Fewer write-watch and `VirtualQuery` calls for view memory the guards
  know, on top of fxceless #670 (`003af25a`).
- Textures changed behind the write guards (the boot logo video) are read
  every frame for about four seconds after a change (`ae88e449`).
- Shader-cache key: two more switches and the entry encoding (`c12f974e`).
- `VICTORY_MEMBENCH=1`, a benchmark of the memory calls title-memory
  tracking makes (`b4ceb62a`); a race fix in the watch tests (`afe67caa`).
- `VICTORY_FULLSCREEN=1`, a borderless window (`69fd9d72`, not yet tried
  with the title); `VICTORY_WINE`, a system Wine instead of Proton's
  (`341d2850`). `docs/linux-proton.md` and `scripts/proton-run.sh`, from
  #994, were extended here (`ae88e449` `341d2850`).
- The hand port of #1279 onto upstream's rewritten clamp shader, on
  `linux/hardware-test-v4-1279-port` (`e75c3ba4` `f2dba003`).

## Running it

Details, including running with a system Wine, are in
[docs/linux-proton.md](https://github.com/cmt8/VIctory/blob/linux/fx-next/docs/linux-proton.md)
on `linux/fx-next`. You need your own legally obtained copy of the title's
files; none are hosted or linked here (see
[docs/asset-policy.md](docs/asset-policy.md)). With Proton Experimental
installed from Steam:

```sh
git clone -b linux/fx-next https://github.com/cmt8/VIctory.git && cd VIctory
rustup target add x86_64-pc-windows-msvc && cargo install --locked cargo-xwin
# cargo-xwin downloads the MSVC CRT and Windows SDK; this accepts their licence.
export XWIN_ACCEPT_LICENSE=1
cargo xwin build --release -p victory --target x86_64-pc-windows-msvc
scripts/proton-run.sh config
scripts/proton-run.sh run /path/to/game/<the title's exe> --execute
```

## Credits and upstreaming

VIctory is Marvisak's project, with its contributors. `linux/fx-next`
builds on fxceless/victory-fork and carries the work of the PR and fork
authors named above. Fixes found here that belong elsewhere will be offered
there. For example, a double unlock of vkd3d-proton's budget lock in
`vkd3d_try_allocate_device_memory` was found while debugging; it has not
been reported yet.

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
