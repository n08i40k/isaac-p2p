#!/bin/sh
# Builds the proxy and the test harness, then runs each scenario under Wine.
# WINE defaults to the newest Proton found in the Steam library.
set -eu
root="$(cd "$(dirname "$0")/.." && pwd)"
"$root/build.sh"

if [ -z "${WINE:-}" ]; then
    WINE="$(ls -d "$HOME"/.local/share/Steam/steamapps/common/Proton*/files/bin/wine 2>/dev/null | sort -V | tail -n1)"
fi
[ -x "$WINE" ] || { echo "Wine not found; set WINE=/path/to/wine" >&2; exit 1; }

work="${TMPDIR:-/tmp}/isaac-p2p-tests"
rm -rf "$work"
mkdir -p "$work/prefix"
export WINEPREFIX="$work/prefix" WINEDEBUG=-all WINEDLLOVERRIDES="winmm=n,b;mscoree=;mshtml="
# No display for the tests below: the console window stays closed and nothing pops up. The overlay test needs
# one for OpenGL, but its window is never shown.
display="${DISPLAY:-}"
unset DISPLAY WAYLAND_DISPLAY

zig cc -target x86-windows-gnu -O1 -shared -o "$work/steam_api.dll" "$root/tests/mock_steam_api.c" -Wl,--out-implib,"$work/steam_api.lib"
zig cc -target x86-windows-gnu -O1 -o "$work/test_runner.exe" "$root/tests/test_runner.c"
zig cc -target x86-windows-gnu -O1 -o "$work/transport_test.exe" "$root/tests/transport_test.c" "$work/steam_api.lib"

status=0
game_dir="${GAME_DIR:-$HOME/.local/share/Steam/steamapps/common/The Binding of Isaac Rebirth}"
if [ -d "$game_dir" ]; then
    python3 "$root/tests/check_exports.py" "$root/dist/winmm.dll" "$game_dir" || status=1
else
    echo "game folder not found, skipping the export check (set GAME_DIR)"
fi
for scenario in default custom norelay disabled nosteam missing_ini; do
    case_dir="$work/$scenario"
    mkdir -p "$case_dir"
    cp "$root/dist/winmm.dll" "$work/steam_api.dll" "$work/test_runner.exe" "$case_dir/"
    run_as="$scenario"
    case "$scenario" in
        default|nosteam) cp "$root/isaac_p2p.ini" "$case_dir/" ;;
        missing_ini) run_as=default ;;
        norelay) printf "[p2p]\nforce_no_relay=1\nmonitor=0\n" > "$case_dir/isaac_p2p.ini" ;;
        disabled) printf '[p2p]\nenabled=0\n' > "$case_dir/isaac_p2p.ini" ;;
        custom) printf '[p2p]\nallow_public=0\nallow_turn=1\ndirect_penalty_ms=5\nrelay_penalty_ms=50\nlog=0\nstun_servers=stun:stun.example.org:3478\n' > "$case_dir/isaac_p2p.ini" ;;
    esac
    printf '%s -> ' "$scenario"
    "$WINE" "$case_dir/test_runner.exe" "$run_as" 2>/dev/null | tr -d '\r' || { status=1; sed 's/^/    /' "$case_dir/isaac_p2p.log" 2>/dev/null || true; }
    if [ "$scenario" = missing_ini ]; then
        # The DLL writes the shipped defaults when the file is missing.
        if [ "$(tr -d '\r' < "$case_dir/isaac_p2p.ini" 2>/dev/null)" = "$(tr -d '\r' < "$root/isaac_p2p.ini")" ]; then
            echo "missing_ini: default isaac_p2p.ini written"
        else
            echo "FAIL missing_ini: isaac_p2p.ini was not written with the defaults"
            status=1
        fi
    fi
done
# Host and client in two processes over localhost, with 20% of datagrams dropped on both sides.
for side in host client; do
    mkdir -p "$work/$side"
    cp "$root/dist/winmm.dll" "$work/steam_api.dll" "$work/transport_test.exe" "$work/$side/"
done
printf '[p2p]\nmonitor=0\n[transport]\nconsole=0\nhost=1\nport=27931\ndrop_percent=20\npassword=test\n' > "$work/host/isaac_p2p.ini"
printf '[p2p]\nmonitor=0\n[transport]\nconsole=0\nconnect=127.0.0.1:27931\ndrop_percent=20\npassword=test\n' > "$work/client/isaac_p2p.ini"
MOCK_STEAM_ID=76561198000000001 "$WINE" "$work/host/transport_test.exe" host > "$work/host/out.txt" 2>/dev/null &
host_pid=$!
sleep 1
MOCK_STEAM_ID=76561198000000002 "$WINE" "$work/client/transport_test.exe" client > "$work/client/out.txt" 2>/dev/null || status=1
wait $host_pid || status=1
for side in host client; do
    tr -d '\r' < "$work/$side/out.txt"
    if grep -q FAIL "$work/$side/out.txt"; then
        status=1
        sed 's/^/    /' "$work/$side/isaac_p2p.log"
    fi
done
# The host goes away without a goodbye and comes back on the same port; the client must reconnect.
mkdir -p "$work/rhost" "$work/rclient"
for side in rhost rclient; do
    cp "$root/dist/winmm.dll" "$work/steam_api.dll" "$work/transport_test.exe" "$work/$side/"
done
printf '[p2p]\nmonitor=0\n[transport]\nconsole=0\nhost=1\nport=27932\npassword=test\n' > "$work/rhost/isaac_p2p.ini"
printf '[p2p]\nmonitor=0\n[transport]\nconsole=0\nconnect=127.0.0.1:27932\npassword=test\n' > "$work/rclient/isaac_p2p.ini"
MOCK_STEAM_ID=76561198000000001 "$WINE" "$work/rhost/transport_test.exe" host idle 4 > "$work/rhost/out1.txt" 2>/dev/null &
host_pid=$!
sleep 1
MOCK_STEAM_ID=76561198000000002 "$WINE" "$work/rclient/transport_test.exe" client reconnect > "$work/rclient/out.txt" 2>/dev/null &
client_pid=$!
wait $host_pid || status=1
sleep 12 # the client notices the host is gone after the link timeout
MOCK_STEAM_ID=76561198000000001 "$WINE" "$work/rhost/transport_test.exe" host idle 15 > "$work/rhost/out2.txt" 2>/dev/null &
host_pid=$!
wait $client_pid || status=1
wait $host_pid || status=1
tr -d '\r' < "$work/rclient/out.txt"
if grep -q FAIL "$work/rclient/out.txt"; then
    status=1
    sed 's/^/    /' "$work/rclient/isaac_p2p.log"
fi
# The client knows no IP and neither side has a password or host=1: the lobby owner starts hosting by itself, the
# client finds it through the lobby's offer, and the host lets it in once Steam verified its ticket and it is a
# lobby member.
mkdir -p "$work/lhost" "$work/lclient" "$work/lobby"
for side in lhost lclient; do
    cp "$root/dist/winmm.dll" "$work/steam_api.dll" "$work/transport_test.exe" "$work/$side/"
done
printf '[p2p]\nmonitor=0\n[transport]\nconsole=0\nport=27934\n' > "$work/lhost/isaac_p2p.ini"
printf '[p2p]\nmonitor=0\n[transport]\nconsole=0\nport=27935\n' > "$work/lclient/isaac_p2p.ini"
lobby_dir="Z:$(printf '%s' "$work/lobby" | tr / '\\\\')" # Wine maps / to Z:
export MOCK_LOBBY_OWNER=76561198000000001
MOCK_LOBBY_DIR="$lobby_dir" MOCK_STEAM_ID=76561198000000001 "$WINE" "$work/lhost/transport_test.exe" host \
    > "$work/lhost/out.txt" 2>/dev/null &
host_pid=$!
sleep 1
MOCK_LOBBY_DIR="$lobby_dir" MOCK_STEAM_ID=76561198000000002 "$WINE" "$work/lclient/transport_test.exe" client lobby \
    2>/dev/null | tr -d '\r' | tee "$work/lclient/out.txt"
wait $host_pid || status=1
unset MOCK_LOBBY_OWNER
grep -q "you own lobby" "$work/lhost/isaac_p2p.log" || { echo "FAIL lobby owner did not start hosting"; status=1; }
if grep -q FAIL "$work/lclient/out.txt" "$work/lhost/out.txt"; then
    status=1
    echo "--- lobby host"; tr -d '\r' < "$work/lhost/out.txt"; sed 's/^/    /' "$work/lhost/isaac_p2p.log"
    echo "--- lobby client"; sed 's/^/    /' "$work/lclient/isaac_p2p.log"
fi
# The HUD draws over frames presented with SwapBuffers and leaves the GL state as it was.
if [ -n "$display" ]; then
    mkdir -p "$work/overlay"
    zig cc -target x86-windows-gnu -O1 -o "$work/overlay/overlay_test.exe" "$root/tests/overlay_test.c" -lopengl32 -lgdi32
    cp "$root/dist/winmm.dll" "$work/overlay/"
    printf '[p2p]\nmonitor=0\nenabled=0\n[transport]\nconsole=0\n' > "$work/overlay/isaac_p2p.ini"
    DISPLAY="$display" "$WINE" "$work/overlay/overlay_test.exe" 2>/dev/null | tr -d '\r' | tee "$work/overlay/out.txt"
    if grep -q FAIL "$work/overlay/out.txt"; then
        status=1
        sed 's/^/    /' "$work/overlay/isaac_p2p.log"
    fi
else
    echo "no display, skipping the overlay test"
fi
# A client whose Steam ticket does not check out must not get in.
mkdir -p "$work/thost" "$work/tclient"
for side in thost tclient; do
    cp "$root/dist/winmm.dll" "$work/steam_api.dll" "$work/transport_test.exe" "$work/$side/"
done
printf '[p2p]\nmonitor=0\n[transport]\nconsole=0\nhost=1\nport=27936\n' > "$work/thost/isaac_p2p.ini"
printf '[p2p]\nmonitor=0\n[transport]\nconsole=0\nconnect=127.0.0.1:27936\nport=27937\n' > "$work/tclient/isaac_p2p.ini"
MOCK_STEAM_ID=76561198000000001 "$WINE" "$work/thost/transport_test.exe" host idle 14 > /dev/null 2>&1 &
host_pid=$!
sleep 1
MOCK_BAD_TICKET=1 MOCK_STEAM_ID=76561198000000002 "$WINE" "$work/tclient/transport_test.exe" client refused 2>/dev/null \
    | tr -d '\r' | sed 's/wrong password/invalid Steam ticket/' | tee "$work/tclient/out.txt"
wait $host_pid || status=1
if grep -q FAIL "$work/tclient/out.txt" || ! grep -q "did not verify" "$work/thost/isaac_p2p.log"; then
    status=1
    echo "FAIL invalid ticket: host log:"
    sed 's/^/    /' "$work/thost/isaac_p2p.log"
fi
# A client with a different password must not get in.
mkdir -p "$work/phost" "$work/pclient"
for side in phost pclient; do
    cp "$root/dist/winmm.dll" "$work/steam_api.dll" "$work/transport_test.exe" "$work/$side/"
done
printf '[p2p]\nmonitor=0\n[transport]\nconsole=0\nhost=1\nport=27933\npassword=right\n' > "$work/phost/isaac_p2p.ini"
printf '[p2p]\nmonitor=0\n[transport]\nconsole=0\nconnect=127.0.0.1:27933\npassword=wrong\n' > "$work/pclient/isaac_p2p.ini"
MOCK_STEAM_ID=76561198000000001 "$WINE" "$work/phost/transport_test.exe" host idle 14 > /dev/null 2>&1 &
host_pid=$!
sleep 1
MOCK_STEAM_ID=76561198000000002 "$WINE" "$work/pclient/transport_test.exe" client refused 2>/dev/null | tr -d '\r' \
    | tee "$work/pclient/out.txt"
wait $host_pid || status=1
if grep -q FAIL "$work/pclient/out.txt" || ! grep -q "different password" "$work/phost/isaac_p2p.log"; then
    status=1
    echo "FAIL wrong password: host log:"
    sed 's/^/    /' "$work/phost/isaac_p2p.log"
fi
exit $status
