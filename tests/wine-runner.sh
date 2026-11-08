#!/bin/sh
# cargo test runner for the i686-pc-windows-gnu target: runs the test executable under Wine.
# WINE defaults to the newest Proton in the Steam library.
if [ -z "${WINE:-}" ]; then
    WINE="$(ls -d "$HOME"/.local/share/Steam/steamapps/common/Proton*/files/bin/wine 2>/dev/null | sort -V | tail -n1)"
fi
export WINEPREFIX="${WINEPREFIX:-${TMPDIR:-/tmp}/isaac-p2p-cargo-test}" WINEDEBUG=-all
mkdir -p "$WINEPREFIX"
unset DISPLAY WAYLAND_DISPLAY
exec "$WINE" "$@"
