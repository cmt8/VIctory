# Linux (Proton)

VIctory is a Windows program, and it also runs on Linux under Proton's Wine
with vkd3d-proton for Direct3D 12. Under Proton it gets as far as on Windows:
boot, first-boot screens, intro video, start screen, saves, loading, and the
opening of the first mission (credits over the scene; the scene itself is
still dark, as on Windows).

Tested on one machine (2026-11-24): CachyOS, Linux 7.3, AMD RX 9070 XT
(RADV, Mesa 26.3.1), Proton Experimental `experimental-11.0-20261112`.

## Requirements

- Steam's **Proton Experimental** (Steam → Library → Tools). Older Protons
  may not work: GE-Proton 11-11's vkd3d-proton turns some translated
  shaders into invalid SPIR-V, on which RADV crashes (`vkCreateGraphicsPipelines`,
  exit code 3).
- A Vulkan driver with mesh shaders (RADV on RDNA 2 or later has them).
- `rustup` and [`cargo-xwin`](https://github.com/rust-cross/cargo-xwin)
  to build the Windows target on Linux.

## Building

```sh
rustup target add x86_64-pc-windows-msvc
cargo install --locked cargo-xwin
# cargo-xwin downloads the MSVC CRT and Windows SDK; this accepts their licence.
export XWIN_ACCEPT_LICENSE=1
cargo xwin build --release -p victory --target x86_64-pc-windows-msvc
```

The diagnostics build likewise:
`cargo xwin build --profile diag -p victory --features diagnostics --target x86_64-pc-windows-msvc`.

## Running

`scripts/proton-run.sh` runs `victory.exe` with Proton's Wine, in a prefix of
its own (`~/.local/share/victory/proton`, made on first use), with
vkd3d-proton and DXVK. Linux paths work as arguments.

```sh
scripts/proton-run.sh config
scripts/proton-run.sh run /path/to/game/<the title's exe> --execute
```

`PROTON` selects another Proton folder, `PREFIX` another prefix and
`VICTORY_EXE` another exe (the diagnostics build, say). Saves, settings and
`crashes.log` live in the prefix's `%LOCALAPPDATA%\victory\data` unless
`victory.toml` or `VICTORY_DATA_DIR` says otherwise.

With an Xbox controller connected, the keyboard is not a pad (as on
Windows), so `VICTORY_KEYS` presses do not reach the title either.

### The window's frame

Under GNOME, Proton's Wine draws the window's title bar and borders itself,
in Windows' classic look, instead of GNOME's. Valve's Wine (experimental_11.0)
asks the window manager for no decorations when it is Mutter
(`dlls/winex11.drv/window.c`: `get_mwm_decorations_for_style` returns none
for "Mutter", which `net_supporting_wm_check_init` makes of GNOME's
"GNOME Shell"), and then paints the non-client area itself. Its Wayland
driver (`winewayland.drv`) has no server-side decorations either, and GNOME
offers none to Wayland clients. Upstream Wine has no such rule, so:

- `VICTORY_WINE=/usr/bin/wine scripts/proton-run.sh ...` runs a system Wine
  (11.x) instead, which leaves the frame to GNOME. The script gives it a
  prefix of its own (`~/.local/share/victory/wine`) with Proton's
  vkd3d-proton and DXVK's `dxgi.dll`, and the Proton prefix's data folder
  (saves, settings) unless `VICTORY_DATA_DIR` says otherwise.
- `VICTORY_FULLSCREEN=1` makes the title's window borderless over the
  whole primary screen (with either Wine; Wine shows it as a fullscreen
  window).

### Without Proton (system Wine)

A system Wine works too (reported with Wine 11.22 on RDNA2, RDNA3 and
RDNA 3.5). What Proton brings has to be added:

- **vkd3d-proton 3.2 or newer**: `d3d12.dll` and `d3d12core.dll` (Proton
  Experimental's `files/lib/wine/vkd3d-proton/x86_64-windows`). 3.1 turns
  some translated shaders into SPIR-V RADV cannot take.
- **DXVK's `dxgi.dll`**, and `WINEDLLOVERRIDES="d3d12,d3d12core,dxgi=n"`.
- No Microsoft `d3dcompiler_47`: Wine's built-in one compiles the argument
  clamp since it no longer needs `RWByteAddressBuffer` methods.

### Diagnostics switches

In the diagnostics build:

- `VICTORY_NO_WATCHDOG=1` (from #454, if merged) keeps the title's stall
  watchdog from ending slow runs.
- `VICTORY_SHOW_TARGET=<w>x<h>-f<format>[-<n>]` presents an intermediate
  render target instead of the final frame, the `n`th of that size and
  format the frame writes (names as in `VICTORY_DUMP_TARGETS` files). The
  world's G-buffer shows its geometry while the final image is dark:
  `VICTORY_SHOW_TARGET=2560x1440-f28-3` on a Series X profile.
- `VICTORY_DEBUG_PS=1` draws depth-tested geometry with a debug pixel
  shader (surfaces coloured by their slope, from depth derivatives) instead
  of the title's: the geometry of every draw whose vertex or mesh stage
  translates, whether its pixel shader does or not. With the two switches
  together you can walk the world in a flat-colour view.

In every build, a texture in view memory that changes with no write the
guards see (the boot logo's video planes; a write through another view of
the same pages is one the guards cannot see) is read at every refresh for
about four seconds after each such change is found;
`VICTORY_BLIND_TEXTURES=0` reads it one frame in eight, as before (the
logo then stutters).

In every build, a draw of depth-tested geometry whose pixel shader does
not translate (or that the host refuses) is drawn with that debug shader
instead of dropped; `VICTORY_PS_FALLBACK=0` turns that off.
Under Wine, a texture picked by index that was uploaded whole is looked
at again seven times (4 to about 3,600 frames after), and each look also
reads the first 512 bytes of every 64 KiB page of it: a texture the title
fills page by page after its first upload is uploaded again when any page
changes. `VICTORY_HEAP_RECHECK_PAGES=0` looks five times at three 16 KiB
windows, as on Windows (`=1` turns it on there).

`VICTORY_DROP_REASONS=1` (any build; always on in the diagnostics build)
follows the frame line every five seconds with up to 40 lines
`dropped N/frame without a host pipeline: <why>`: the shaders of a pipeline
that do not translate (stage and hash), a pipeline the host refused, "no
pipeline set", the ray-tracing structure builder's dispatches (off unless
`VICTORY_BVH_BUILDER=1`), or what else dropped a draw whose pipeline is
there (an index buffer outside title buffers, a table without free slots,
...). It writes to the log only.
`VICTORY_PIPELINE_WAIT_MS` sets how long a frame may wait for pipelines
being made (500 by default; 0 never waits, pipelines then appear a few
frames later).

## Tests

```sh
CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER=$PWD/scripts/proton-run.sh \
    cargo xwin test --workspace --target x86_64-pc-windows-msvc
```

Two tests start Linux programs from inside Wine and fail there for that
reason only: `loads_binds_and_calls_probe` (builds its fixture with `cargo`)
and `policy::tests::repository_passes_policy` (runs `git`). Run
`cargo xtask policy` natively instead.

## Known differences

- **Speed.** Wine emulates write watching page by page, so the copies of
  title memory take longer than on Windows. Under Wine the kernel is asked
  about each title allocation once per submission (`watch::new_submission`),
  views of physical pages go straight to their guards, and allocations are
  looked up from a cache rather than with `VirtualQuery`, which takes the
  same lock as the title's write faults.
  Also only under Wine, each with a switch that restores the earlier
  behaviour: a texture with a page not there is read as a partly mapped
  one instead of waiting for the runtime to finish remapping
  (`VICTORY_TEXTURE_PARTLY_WAIT=1` waits); the thread that copies back
  what shaders wrote fetches from the card on a copy queue of its own
  (`VICTORY_RELAY_FETCH_QUEUE=0`: on the host's queue); the pieces hashed
  again after sends to card copies are one pass per copy
  (`VICTORY_CARD_PIECE_LISTS=0`); texture staging buffers are kept by
  size class for the next texture (`VICTORY_STAGING_POOL=0`).
  Pages of a buffer copy in the card's memory that the copy-back wrote are
  reported written by the write watch and the guards like the title's own
  writes; their next refresh still reads them whole from title memory, but
  compares them with what the card holds and sends up only the bytes that
  differ, instead of the whole pages (some 63 MB a frame in heavy scenes).
  `VICTORY_CARD_ECHO_COMPARE=0` sends them whole as before (`=1` turns the
  compare on outside Wine too). The line "host copies in the card's
  memory" says how much of those pages went up.
- **Software rendering does not work.** On lavapipe, Wine cannot handle the
  write faults of VIctory's guarded title memory when lavapipe's own
  threads take them.
- **vkd3d-proton differs from Windows drivers in a few places VIctory now
  handles:** its HLSL compiler (Wine's `D3DCompile`) lacks
  `RWByteAddressBuffer` methods and does not embed root signatures; it
  refuses an empty pipeline library given a non-null blob pointer; it does
  not check DXIL container hashes; and its descriptor layout makes a texture
  descriptor read as a constant buffer (a stale slot) a read at address 0.
