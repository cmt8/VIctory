# Wine on Linux (unofficial)

VIctory is a Windows program and Windows is the only supported host. This
page records what has been checked on Linux under Wine: building
`victory.exe` there, the commands that need no GPU or game files, and the
test suite. Running a title needs a GPU and is marked `TODO(hardware)`
below until someone runs it on one.

Related work, not merged as of this page: #994 (`docs/linux-proton.md`, a
Proton recipe and Wine fixes), #1218 (`cargo xtask wine-test`), #217
(Winsock fallbacks), #340 (tracking issue).

Checked on (2026-11-24, `main` at `ffe310e`): Ubuntu 24.04 x86-64 in a
container with no GPU, Rust 1.98.1, cargo-xwin 0.24.1, Ubuntu's clang/lld
18.1.3, Wine 11.22 (`winehq-devel` from WineHQ's Ubuntu repository).

## Building

Packages (Ubuntu 24.04 names): `clang`, `lld` and `llvm` (for `clang-cl`,
`lld-link` and `llvm-lib`). `rust-toolchain.toml` pins the compiler and the
Windows target.

```sh
rustup toolchain install 1.98.1 --profile minimal --target x86_64-pc-windows-msvc
cargo install --locked cargo-xwin
cargo xwin build --release -p victory --target x86_64-pc-windows-msvc
# target/x86_64-pc-windows-msvc/release/victory.exe
```

The first build downloads Microsoft's CRT and Windows SDK (about 630 MiB)
into `~/.cache/cargo-xwin`. cargo-xwin does not ask about their licence:
running it means accepting it
(<https://go.microsoft.com/fwlink/?LinkId=2086102>), as its README states.
`XWIN_ACCEPT_LICENSE` only affects the standalone `xwin` tool and changes
nothing here. `XWIN_ARCH=x86_64` skips the aarch64 libraries.

The diagnostics build works the same way:
`cargo xwin build --profile diag -p victory --features diagnostics --target x86_64-pc-windows-msvc`.

lld-link warns that DWARF section names (`.debug_info`, ...) are longer
than 8 characters. The warning is harmless.

## Wine

WineHQ's packages (see winehq.org for the repository set-up), in a prefix
of VIctory's own:

```sh
sudo apt install --install-recommends winehq-devel
export WINEPREFIX=~/.local/share/victory/wine
wineboot -i
```

Without a GPU, `victory.exe` runs the commands that need no title:

```sh
wine target/x86_64-pc-windows-msvc/release/victory.exe config
```

```
config file : (none)
game_dir    : (not set)
cache_dir   : C:\users\<you>\AppData\Local\victory\cache
data_dir    : C:\users\<you>\AppData\Local\victory\data
```

- `victory.toml` is read from the working directory, or from
  `VICTORY_CONFIG`. Paths in it and in `VICTORY_*_DIR` can be Wine paths
  (`Z:\home\...`) or Linux paths (`/home/...`); Wine resolves both.
- The asset policy check works under Wine: `config` refuses a cache folder
  inside the checkout, given either way.
- `--help`, `--version` and `inspect` also run.
- A bare container prints `XDG_RUNTIME_DIR is invalid` until that variable
  names a directory. A desktop session sets it.

## Tests

`cargo xwin test` runs the test executables with `wine` and
`WINEDEBUG=-all`, in `WINEPREFIX` if it is set:

```sh
cargo xwin test --target x86_64-pc-windows-msvc --workspace --no-fail-fast
```

Without a GPU, and with an X display, this gave 324 passed, 18 failed, 6
ignored:

| Test | Why it fails here |
|------|-------------------|
| 13 `emit::valu_tests::*`, 2 `emit::waterfall_tests::*` | No D3D12 SM 6.6 device, and Wine has no WARP. They need a GPU, or `VICTORY_WARP=1` on Windows. |
| `modules::net::tests::only_loopback_is_reachable` | Wine 11.22's `ws2_32.dll` has no `WSAConnectByList` or `GetAddrInfoExA`. The HLE forwards to them, so a title calling them would also panic. #217 and #994 fall back. |
| `loads_binds_and_calls_probe` | It runs `cargo` to build its fixture, which does not work from inside Wine. With the fixture built beforehand (#1218's `VICTORY_HLE_PROBE`), it passes under Wine. |
| `policy::tests::repository_passes_policy` | It runs `git`. Run `cargo xtask policy` natively instead. |

Without an X display (no `DISPLAY`), two more fail:
`lifecycle::tests::hooked_windows_still_reach_the_title_procedure` and
`the_keyboard_menu_never_reaches_the_title` cannot create a window. A
desktop session or `xvfb-run` fixes this.

Tests that need a GPU and find none return early and count as passed. That
includes `argument_clamp_builds`, `a_rejected_blob_gives_an_empty_library`
and `stub_pipeline_is_created`, which #1218 reports failing under
vkd3d-proton, and `buffer_data_reaches_the_host_texture`, which #195
reported failing. Those tests ran but checked nothing here.

Software rendering does not help. Wine's own D3D12 on lavapipe (Mesa
25.2.8) creates no feature level 12.0 device, so the same tests skip and
fail. #994 reports that lavapipe cannot run the title either.

## Running a title

`TODO(hardware)`: nothing below has been run here. Reported recipes are in
#994 (`docs/linux-proton.md`) and #879. Fill these in from a run on an
RX 7900 XTX:

- `TODO(hardware)`: vkd3d-proton version and how its `d3d12.dll` and
  `d3d12core.dll` go into the prefix. #752 and #994 report that 3.1 crashes
  RADV and 3.2 works. Wine's own D3D12 lacks SM 6.6.
- `TODO(hardware)`: whether DXVK's `dxgi.dll` is needed, and the
  `WINEDLLOVERRIDES` used. #879 reports that Wine's own `dxgi` fails and
  that `d3d12core` must be overridden as well as `d3d12`.
- `TODO(hardware)`: the test suite with vkd3d-proton: which of the GPU
  tests above pass.
- `TODO(hardware)`: the launch command, where it gets to, and its log
  lines.
- `TODO(hardware)`: memory, frame times, and how they compare with
  Windows.
