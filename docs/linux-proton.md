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
A compute program's wave sum of 16-bit floats where some lanes have left
(the refusal "a wave sum of 16-bit floats where not every lane runs"; compute
`b22a16a178976e1b` in a Linux run, bound when the world appears) runs on a
host of 64-lane waves, bit for bit as the console adds it;
`VICTORY_HALF_SUM_PRESENT=0` refuses it as before (its dispatches are then
dropped). Part of the shader cache key.
A `DispatchMeshX` whose group counts are in GPU memory is dropped (counted
with the indirect executions dropped, and named in the drop reasons);
`VICTORY_MESH_INDIRECT=1` runs it as an indirect execution of one record,
as upstream #1749 does: that record layout is unchecked against the title,
and the first Linux run with it ended in a GPU page fault and reset.
An indirect mesh draw of one record that asks for more than 65,535 groups
(the title leaves all ones in slots it has not filled) runs empty instead
of being raised to 65,535 groups (linux/fx-next4; no switch).
A buffer a shader reads from the view heap at an index it computes reads
the host's null buffer view where the heap's stride table says the slot
holds no buffer view (a texture view read as a buffer descriptor is a wild
address); `VICTORY_BUFFER_GUARD=0` turns that off (part of the shader
cache key).
A draw or dispatch whose root descriptor (title memory's directory, the
view heap's address or stride table, a root constant buffer, a root view
outside title memory) has nothing to bind is dropped, named in the drop
reasons, instead of running with the parameter unbound.
With `VICTORY_DROP_REASONS=1` a line `indirect records per frame` also
says what the argument clamp did to the indirect records it bounded: how
many asked for work and ran empty because their list's budget (2^27 vertex
instances or threads) was spent, how many lists ran out, how many records
asked for nothing, and how many draws were cut to the bounds. Records
emptied by the budget draw nothing and are counted nowhere else.
`VICTORY_INDIRECT_BUDGET_LOG2=<20..31>` sets the budget for A/B runs; a
larger one lets garbage records (left by passes that do not run) run
longer before they are cut, which on a slow card can mean a reset.
`VICTORY_PIPELINE_WAIT_MS` sets how long a frame may wait for pipelines
being made (500 by default; 0 never waits, pipelines then appear a few
frames later).

### Memory sync and card memory (fxceless 8dd30ab4, linux/fx-next4)

- `VICTORY_VIEW_GUARD` (on; `=0` as before): a guarded 64 KiB block of a
  physical page is guarded at every title view of that page (up to 64
  views), and a write through any of them opens them all. Under Wine each
  extra view costs one `VirtualProtect` when guarded and one when opened,
  each under ntdll's process-wide `virtual_mutex` (as is every write fault
  on a guarded page), so it competes with the runtime's remapping for that
  lock. The runtime's own kept view of each section (`VICTORY_SECTION_VIEWS`)
  is not a title view and is never guarded. `=0` restores the old guard
  exactly (the views' bookkeeping is still kept, nothing acts on it).
- `VICTORY_READ_BESIDE` (on; `=0` as before): `fread` and DirectStorage
  file reads into title addresses are read into a buffer and copied in;
  a kernel write into title memory (our copy-backs, a synchronous
  `ReadFile`) that the kernel refuses because a guard was set again over it
  is opened and made again. Under Wine the kernel side of those writes does
  not fault into our handler (a read-only page fails the call). The status
  line "kernel writes into title memory refused ..." counts the second tries.
- `VICTORY_PAGE_WRITES` (under Wine `1` by default, elsewhere `0`):
  DirectStorage writes into physical pages, which go through the runtime's
  kept view, are told to the GPU side at every title address that shows
  the pages: `1` opens the guards there and makes the placed textures that
  hold the bytes due again; `0` counts only (as before); `guards`, `slots`,
  `all` (also plain-address reads) for finer A/B. The status line "writes
  behind the guards, since the start" has the counts.
- `VICTORY_DYNAMIC_WAIT_MS` (250; `0` as before): a descriptor table made
  afresh for a draw that finds no free dynamic slots waits that long for
  slots given back by lists the card has run (the helper thread is asked
  to land them) before the draw is dropped. Where the host's view heap has
  room (2^21 views under vkd3d-proton on RADV) every slot between the
  title's and the null buffer views is a dynamic one, up to 2^20 (2^18
  before). The status line "dynamic descriptor slots" says how many are in
  use, the most ever, and how many tables waited, got slots or were refused.
- `VICTORY_DSTORAGE_RETRIES` (12): page-destination writes refused for a
  moment are tried again; `0` as before.
- `VICTORY_FULL_CARD_PASS` (on; `=0` as before): on a full card, texture
  slots that need a new host copy are held back for about four seconds so a
  pass reaches the slots behind them.
- Off by default: `VICTORY_LEVELS_ON_DEMAND=1` (host copies of placed
  textures take card memory level by level; `VICTORY_LEVELS_MIN_KB`),
  `VICTORY_CARD_ROOM_FOR_BUFFERS=1`, and the diagnostics
  `VICTORY_MERGE_RACE=1`, `VICTORY_REFRESH_CHECK=1` (no longer turned on by
  `VICTORY_WALLS_TRACE`), `VICTORY_CARD_ACCOUNTING`.
- Not switches: a copy-back keeps the title's byte where the title and a
  shader both changed it, and writes back only the shader's bytes; a
  render target in R9G9B9E5 (format 67) is made in 16-bit float RGBA when
  the host does not render to it. fxceless's `translate --warm` and its
  shader cache key are not in this tree (its own key and
  `translate --cache` are).

### Streaming (linux/fx-next5-perf)

- `VICTORY_TEXTURE_QUIET_FRAMES` (2; `0` as before): a texture picked by
  index whose memory the runtime maps, unmaps or writes again before the
  look an earlier change made due waits until its memory has been still
  for that many frames, and at most six frames from the first change; a
  texture streaming in is then uploaded once its pages are in instead of
  whole at every frame of the stream. The status line "what is mapped,
  from the runtime's word" counts the changes folded so.
- `VICTORY_VIEW_BOOKS` (on under Wine, off elsewhere; `=0` or `=1`
  says): what of title memory in the runtime's placeholders for physical
  pages is mapped is answered from the views the runtime told of, not
  asked of the kernel: the texture pass's looks at a slot, a texture
  refresh of a texture with a page not there (read around its holes),
  copies into host memory around holes, and the guards' questions where
  an unmapped run ends and whether a range is view memory. Under Wine
  each such question took ntdll's `virtual_mutex`, which the runtime's
  remapping holds across wineserver requests (`membench`
  `view_books_bench`: a half-mapped 4 MiB texture asked about in 0.8 us
  from the books against 71 us idle, 355 us beside one thread remapping
  and 45 ms beside two). A placeholder is looked at again at most every
  250 ms to see it is still there (the title releases one with the host's
  `VirtualFree`, which nothing hooks); in that window an answer about
  memory allocated where a released one was may be wrong for a moment.
  The guards also note a block of a placeholder that no view shows absent
  without the protection change Wine would refuse.
- `VICTORY_GUARD_RUNS` (on; `=0` as before): the guards change the
  protection of consecutive blocks that lie in one view the runtime told
  of with one call (guarding, opening, and the other views of the pages
  with `VICTORY_VIEW_GUARD`), not block by block; a range over several
  views is no longer first tried whole (Wine refuses it,
  `NtProtectVirtualMemory` wants one view). The status line "the guards'
  protection changes per frame" counts the calls, blocks and time by
  what they were for, and the write faults taken.
- `VICTORY_TEXTURE_STAGE_PER` (`frame`; `=pass` as before): the texture
  pass's time (`VICTORY_TEXTURE_STAGE_MS`, 250 ms, three times that with a
  backlog of 1,024 slots due, `VICTORY_TEXTURE_CATCHUP=0`: never) is the
  frame's, as its bytes (`VICTORY_TEXTURE_STAGE_MB`) are, not each pass's:
  a frame with two passes no longer takes the time twice. Lower
  `VICTORY_TEXTURE_STAGE_MS` (and `VICTORY_TEXTURE_CATCHUP=0`) for smoother
  frames while the world streams in, at the cost of textures that arrive
  later.
- `VICTORY_PAGE_WRITES_OPEN` (off; `=1` as before): a DirectStorage
  write into physical pages (`VICTORY_PAGE_WRITES`) leaves the guards over
  the title addresses that show them armed and notes the blocks written,
  instead of opening them (a `VirtualProtect` a block and one per other
  view of its pages, and as many again to guard them at the next check).
  The write went through the runtime's kept view, which no guard covers;
  the next check reports the blocks written without a call, and a write
  of the title's after it still faults.
- Not a switch: a status line "title memory copied by the refreshes per
  frame" splits the frame line's "copies" by where the bytes went (copies
  in the card's memory, of it the part compared against the copy-back's
  echo, copies in PC memory), the bytes left as unchanged, and the
  kernel's write-watch askings; what it does not name is the guards, the
  journal and the locks. Its clock reads cost about 0.14 us each a copy
  under Wine (`membench` `clock_bench`).

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
  size class for the next texture (`VICTORY_STAGING_POOL=0`); a
  submission's texture checks (read, hash, detile) run on the threads
  kept for the snapshots and not one after another on the submitting
  thread (`VICTORY_WIDE_TEXTURES=0`: one after another; `=1` turns it on
  elsewhere; fxceless/victory-fork#768).
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
