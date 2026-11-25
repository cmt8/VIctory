#!/usr/bin/env bash
# Runs victory.exe (or another Windows exe) under Proton's Wine on Linux,
# with vkd3d-proton (D3D12) and DXVK (DXGI) from the Proton build.
# See docs/linux-proton.md.
#
#   scripts/proton-run.sh config
#   scripts/proton-run.sh run /path/to/game/<the title's exe> --execute
#
# As cargo's runner (its first argument is the test exe):
#   CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER=$PWD/scripts/proton-run.sh \
#       cargo xwin test --workspace --target x86_64-pc-windows-msvc
#
# Environment:
#   PROTON       a Proton folder (default: Proton - Experimental from Steam)
#   PREFIX       the Proton prefix to use (default: ~/.local/share/victory/proton)
#   VICTORY_EXE  the exe to run (default: this checkout's release build)
#   VICTORY_WINE another Wine to run it with, e.g. /usr/bin/wine: in a prefix
#                of its own (default: ~/.local/share/victory/wine), with
#                vkd3d-proton and DXVK's dxgi from the Proton build, and the
#                data folder of the Proton prefix
#
# The window's frame: under GNOME, Proton's Wine draws the window's title
# bar and borders itself (Windows' classic look) instead of GNOME's. Its
# X11 driver asks the window manager for no decorations when that is Mutter
# (Valve's wine, experimental_11.0, dlls/winex11.drv/window.c:
# get_mwm_decorations_for_style returns 0 for "Mutter", which
# net_supporting_wm_check_init makes of "GNOME Shell"), and then draws the
# non-client area itself (X11DRV_GetWindowStyleMasks takes no frame off the
# window). Its Wayland driver has no server-side decorations at all (and
# GNOME offers none). No setting turns this off; a Wine without the Proton
# change gives GNOME's title bar: VICTORY_WINE=/usr/bin/wine. With
# VICTORY_FULLSCREEN=1 victory shows the title's window borderless over the
# whole screen instead (no frame with either Wine).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if [[ -z "${PROTON:-}" ]]; then
    for d in "$HOME/.steam/steam/steamapps/common/Proton - Experimental" \
             "$HOME/.local/share/Steam/steamapps/common/Proton - Experimental" \
             "$HOME/.var/app/com.valvesoftware.Steam/data/Steam/steamapps/common/Proton - Experimental"; do
        if [[ -x "$d/files/bin/wine" ]]; then PROTON="$d"; break; fi
    done
fi
if [[ -z "${PROTON:-}" || ! -x "$PROTON/files/bin/wine" ]]; then
    echo "Proton not found: install Proton Experimental in Steam or set PROTON" >&2
    exit 1
fi
PROTON_PREFIX="${XDG_DATA_HOME:-$HOME/.local/share}/victory/proton"
if [[ -n "${VICTORY_WINE:-}" ]]; then
    PREFIX="${PREFIX:-${XDG_DATA_HOME:-$HOME/.local/share}/victory/wine}"
else
    PREFIX="${PREFIX:-$PROTON_PREFIX}"
fi

if [[ -n "${VICTORY_WINE:-}" ]]; then
    # A system Wine: its own prefix (Proton's would be upgraded and back),
    # with Proton's vkd3d-proton and DXVK's dxgi in it, and the saves and
    # settings of the Proton prefix.
    export WINEPREFIX="$PREFIX/pfx"
    if [[ ! -f "$WINEPREFIX/system.reg" ]]; then
        mkdir -p "$WINEPREFIX"
        WINEDEBUG=-all "$VICTORY_WINE" wineboot -i >/dev/null 2>&1 || true
    fi
    system32="$WINEPREFIX/drive_c/windows/system32"
    for dll in vkd3d-proton/x86_64-windows/d3d12.dll vkd3d-proton/x86_64-windows/d3d12core.dll \
               dxvk/x86_64-windows/dxgi.dll; do
        from="$PROTON/files/lib/wine/$dll"
        if [[ -f "$from" ]] && ! cmp -s "$from" "$system32/${dll##*/}"; then
            cp -f "$from" "$system32/"
        fi
    done
    data="$PROTON_PREFIX/pfx/drive_c/users/steamuser/AppData/Local/victory/data"
    if [[ -z "${VICTORY_DATA_DIR:-}" && -d "$data" ]]; then
        export VICTORY_DATA_DIR="$data"
    fi
# First run: let Proton build the prefix (it installs vkd3d-proton and DXVK).
elif [[ ! -f "$PREFIX/pfx/system.reg" ]]; then
    mkdir -p "$PREFIX"
    STEAM_COMPAT_DATA_PATH="$PREFIX" \
    STEAM_COMPAT_CLIENT_INSTALL_PATH="${STEAM_COMPAT_CLIENT_INSTALL_PATH:-$HOME/.steam/steam}" \
    SteamAppId=0 SteamGameId=0 \
        python3 "$PROTON/proton" run cmd /c exit >/dev/null 2>&1 || true
fi

export WINEPREFIX="$PREFIX/pfx"
export WINEDLLOVERRIDES="d3d12,d3d12core,dxgi,d3d11,d3d10core=n,b;${WINEDLLOVERRIDES:-}"
export WINEDEBUG="${WINEDEBUG:--all}"
export WINEFSYNC=1
# vkd3d-proton's pipeline cache, out of the working directory.
export VKD3D_SHADER_CACHE_PATH="${VKD3D_SHADER_CACHE_PATH:-$PREFIX/vkd3d-cache}"
mkdir -p "$VKD3D_SHADER_CACHE_PATH"

WINE="${VICTORY_WINE:-$PROTON/files/bin/wine}"
if [[ -n "${VICTORY_EXE:-}" ]]; then
    exec "$WINE" "$VICTORY_EXE" "$@"
elif [[ "${1:-}" == *.exe && -f "${1:-}" ]]; then
    exec "$WINE" "$@"
else
    exec "$WINE" "$ROOT/target/x86_64-pc-windows-msvc/release/victory.exe" "$@"
fi
