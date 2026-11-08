#!/bin/sh
# Latency benchmark of the transport between this PC (host, under Wine) and a Windows PC (client, native) over SSH.
#   REMOTE=user@host [HOST_IP=ip-the-remote-reaches-us-on] [PORT=27931] [SECONDS=30] [DROP=0] ./tests/remote.sh
#   REMOTE=user@host LOBBY=1 [PORT=27931] [ADDRESSES=ip:port,...] ... ./tests/remote.sh
# The host runs here so the remote only connects outwards and its firewall needs no rule. With LOBBY=1 the client
# gets no address: both sides find each other through the mock lobby (files synced over SSH) and punch through
# their NATs, so PORT needs no forwarding.
set -eu
root="$(cd "$(dirname "$0")/.." && pwd)"
: "${REMOTE:?set REMOTE=user@host}"
HOST_IP="${HOST_IP:-$(tailscale ip -4 2>/dev/null | head -n1)}"
SECONDS_="${SECONDS:-30}"
DROP="${DROP:-0}"
LOBBY="${LOBBY:-0}"
# Through the lobby, players are verified with Steam tickets (mock); by IP, with a password.
if [ "$LOBBY" = 1 ]; then PASSWORD=""; else PASSWORD=bench; fi
PORT="${PORT:-27931}"
[ -n "$HOST_IP" ] || [ "$LOBBY" = 1 ] || { echo "set HOST_IP" >&2; exit 1; }
"$root/build.sh" >/dev/null 2>&1 || "$root/build.sh"

if [ -z "${WINE:-}" ]; then
    WINE="$(ls -d "$HOME"/.local/share/Steam/steamapps/common/Proton*/files/bin/wine 2>/dev/null | sort -V | tail -n1)"
fi
work="${TMPDIR:-/tmp}/isaac-p2p-remote"
rm -rf "$work"
mkdir -p "$work/prefix" "$work/host" "$work/client"
export WINEPREFIX="$work/prefix" WINEDEBUG=-all WINEDLLOVERRIDES="winmm=n,b;mscoree=;mshtml="
unset DISPLAY WAYLAND_DISPLAY

zig cc -target x86-windows-gnu -O1 -shared -o "$work/steam_api.dll" "$root/tests/mock_steam_api.c" -Wl,--out-implib,"$work/steam_api.lib"
zig cc -target x86-windows-gnu -O1 -o "$work/transport_test.exe" "$root/tests/transport_test.c" "$work/steam_api.lib"
for side in host client; do
    cp "$root/dist/winmm.dll" "$work/steam_api.dll" "$work/transport_test.exe" "$work/$side/"
done
printf '[p2p]\r\nmonitor=0\r\n[transport]\r\nconsole=0\r\nhost=1\r\nport=%s\r\ndrop_percent=%s\r\npassword=%s\r\naddresses=%s\r\n' "$PORT" "$DROP" "$PASSWORD" "${ADDRESSES:-}" > "$work/host/isaac_p2p.ini"
if [ "$LOBBY" = 1 ]; then
    printf '[p2p]\r\nmonitor=0\r\n[transport]\r\nconsole=0\r\ndrop_percent=%s\r\n' "$DROP" > "$work/client/isaac_p2p.ini"
else
    printf '[p2p]\r\nmonitor=0\r\n[transport]\r\nconsole=0\r\nconnect=%s:%s\r\ndrop_percent=%s\r\npassword=bench\r\n' "$HOST_IP" "$PORT" "$DROP" > "$work/client/isaac_p2p.ini"
fi

dir='isaac-p2p-bench'
ssh "$REMOTE" "(if not exist $dir mkdir $dir) & (if exist $dir\\lobby rmdir /s /q $dir\\lobby) & mkdir $dir\\lobby" >/dev/null
mkdir -p "$work/lobby"
lobby_dir="Z:$(printf '%s' "$work/lobby" | tr / '\\\\')" # Wine maps / to Z:
scp -q "$work/client/"* "$REMOTE:$dir/"

MOCK_LOBBY_DIR="$lobby_dir" MOCK_STEAM_ID=76561198000000001 "$WINE" "$work/host/transport_test.exe" host bench \
    > "$work/host/out.txt" 2>/dev/null &
host_pid=$!
sleep 2
status=0
sync_pid=
if [ "$LOBBY" = 1 ]; then
    # Stand-in for Steam's lobby: the host's offer goes to the remote, the client's member data comes back.
    (
        for _ in $(seq 40); do
            [ -f "$work/lobby/lobby.txt" ] && scp -q "$work/lobby/lobby.txt" "$REMOTE:$dir/lobby/lobby.txt" 2>/dev/null || true
            scp -q "$REMOTE:$dir/lobby/member_76561198000000002.txt" "$work/lobby/" 2>/dev/null || true
            sleep 1
        done
    ) &
    sync_pid=$!
    client_args="client lobby bench $SECONDS_"
else
    client_args="client bench $SECONDS_"
fi
ssh "$REMOTE" "cd $dir && set MOCK_LOBBY_DIR=lobby&& set MOCK_STEAM_ID=76561198000000002&& transport_test.exe $client_args" \
    | tr -d '\r' | tee "$work/client/out.txt" || status=1
[ -n "$sync_pid" ] && kill "$sync_pid" 2>/dev/null
wait $host_pid || status=1
tr -d '\r' < "$work/host/out.txt"
if grep -q FAIL "$work/client/out.txt" "$work/host/out.txt"; then
    status=1
    echo "--- host log"; sed 's/^/    /' "$work/host/isaac_p2p.log"
    echo "--- client log"; ssh "$REMOTE" "type $dir\\isaac_p2p.log" | tr -d '\r' | sed 's/^/    /'
fi
exit $status
