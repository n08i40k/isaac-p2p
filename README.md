# isaac-p2p

A `winmm.dll` proxy for The Binding of Isaac: Repentance+ that makes online co-op sessions try a direct
peer-to-peer connection (Steam ICE, UDP hole punching) and prefer it over Valve's relay servers (SDR), and logs the
route and ping to every player.

Isaac's online co-op uses the legacy `ISteamNetworking` P2P API (`SteamNetworking006`: `SendP2PPacket` /
`ReadP2PPacket`). Steam carries those sessions over Steam Networking Sockets inside the game process, so the
global Steam Networking Sockets config decides whether a session may connect directly. By default that follows each
player's Steam client setting, and a relayed route adds the detour through a relay server to every frame of input
delay.

The DLL waits for the game to initialise Steam, then:

- sets the global config below;
- reads it back every `monitor_interval_ms` and sets it again if Steam replaced it (Steam loads its own networking
  config the first time the game uses the network, about 20 s after start-up);
- hooks `ISteamNetworking::SendP2PPacket` to learn which players the game talks to, and
  `ISteamNetworking::AllowP2PPacketRelay` to log (and with `force_no_relay=1` override) the game's relay choice;
- logs each player's route as it changes: `RELAY`, or `DIRECT` with the remote address, plus the ping from Steam
  Networking Sockets every `ping_log_interval_s`.

| Setting | Value |
| --- | --- |
| `P2P_Transport_ICE_Enable` | public + private (from `allow_*`) |
| `P2P_Transport_ICE_Penalty` | `direct_penalty_ms` (0) |
| `P2P_Transport_SDR_Penalty` | `relay_penalty_ms` (20) |
| `P2P_STUN_ServerList` | `stun_servers`, only when set |

When a direct route can't be punched through (both players behind symmetric NAT / CGNAT), the session uses the
relay. `force_no_relay=1` forbids the relay entirely: the session then fails instead of falling back.

## Own transport (direct IP)

Instead of Steam networking, game traffic can go over the DLL's own UDP transport. A console window opens with the
game (`console=1`) and takes these commands:

| Command | |
| --- | --- |
| `host [port] [password]` | listen for players on a UDP port (default `port`, 27777) |
| `connect <ip[:port]> [password]` | connect to a host |
| `join [password]` | connect to the host of the Steam lobby you are in, no IP needed |
| `status` | peers, RTT, unacked packets, lobby |
| `disconnect` | leave the session |

With `host=auto` (the default) a player hosts while owning the Steam lobby it is in: create a lobby in the game,
invite a friend through Steam, and the friend's game connects over the transport by itself. `host=1` hosts from
start-up, `host=0` never; F11 in the game starts or stops hosting by hand. `connect=ip:port` in `[transport]`
connects at start-up.

By default there is no password: a connecting player sends a Steam session ticket, the host has Steam verify it
and lets the player in only when Steam confirms the Steam ID and that player is a member of the host's lobby.
Inviting a friend through Steam is all it takes. A host can set `password=` in `[transport]` (or `host [port]
<password>`) instead; then players connect with that password, also by IP without a lobby. The ticket check
does not stop someone who can read the traffic on the way from replaying a ticket before its owner uses it; a
password does.

How a session comes together:

1. The host runs `host` and creates a lobby in the game's online menu. Its lobby must be joinable by the other
   players: **public**, or friends-only between Steam friends.
2. A client runs `connect <host ip>` (with the password, if the host set one). Once the host answers, it sends its lobby ID, and the client's game joins that
   lobby through its own invite handler (`GameLobbyJoinRequested_t`), with no Steam invitation involved.
3. Every packet between players connected through the transport goes over its UDP socket; players who are not
   connected through it still use Steam.

Without an IP, players find each other through the Steam lobby: the host offers its addresses in the lobby
(public ones from STUN, LAN ones, and `addresses=` from the ini, such as a forwarded port), each client offers its
own, and both sides send packets to each other at the same time, which opens their NATs (hole punching). A client
with `join=1` (the default) connects by itself when it enters a lobby with such an offer (for a host with a
password, it needs `password=` too, or `join <password>`). The first address that answers is used, so a forwarded port or LAN address
wins over a slower path. A password stays out of the lobby, since anyone who can see a lobby can read its data.

Hole punching fails when the NAT gives each destination its own port (symmetric NAT, common with mobile
carriers and CGNAT); the log says so after the STUN check. Then the host needs the UDP port forwarded on the
router (offer it with `addresses=`) or a VPN such as Tailscale or ZeroTier. Clients need no open ports.

The transport is a star: the host forwards traffic between clients. Steam's unreliable send types go out as one
datagram each; the reliable ones share an ordered stream per link with acknowledgements, retransmission and
fragmentation of messages up to 1 MiB.

Only players Steam verified (or who know the host's password) complete the handshake. Each connection gets its own keys (X25519 exchange
mixed with the password), every packet after the handshake is encrypted and authenticated (ChaCha20-Poly1305),
and replayed packets are dropped. A player whose IP address or port changes mid-session keeps playing: the
first authentic packet from the new address moves the connection there. A client that loses the host tries
to reconnect for 60 seconds. Both players need the same isaac-p2p version.

The Steam lobby still exists, so the game's menus, player list and lobby chat work as usual; only the packet
traffic moves off Steam.

## HUD

A small panel in the top-right corner shows the connection state, every player's route (own transport, Steam
direct or Steam relay) with ping, and recent connection events, on every screen of the game. F9 cycles compact /
detailed / hidden; F10 copies the transport password to the clipboard; F11 starts or stops hosting. `hud=0` in `[overlay]` turns it off.

The game presents its OpenGL frames with `GDI32!SwapBuffers`; the DLL hooks that import and draws the panel
right before each frame goes out, restoring every OpenGL state it touched. It reads no game memory, so game
updates do not affect it.

## VPN on the host

A VPN that routes all traffic changes the address other players see (and adds its detour to every packet).
Instead of excluding processes, exclude the transport's UDP port: both host and client bind `[transport] port`
(27777) when it is free, so one rule covers them. On Linux with policy routing, e.g.:

```sh
sudo ip rule add ipproto udp sport 27777 lookup main priority 8999
```

## Install

1. Copy `winmm.dll` and `isaac_p2p.ini` next to `isaac-ng.exe`. Every player should install it: ICE needs both
   sides to allow it.
2. Linux (Proton): set the game's Steam launch options to `WINEDLLOVERRIDES="winmm=n,b" %command%`.
   Windows needs no extra step.

## Checking the route

`isaac_p2p.log` next to the game lists the applied settings and a line per player whenever the route changes:

```
peer 76561198000000001: game opened a session
peer 76561198000000001: RELAY ping 95 ms (sockets: relayed; ...)
peer 76561198000000001: DIRECT 93.184.216.34:27015 ping 42 ms (sockets: direct; ...)
```

With `log=1` Steam's own messages about route negotiation (`steam: ...`) are written there too.

## Build

```sh
rustup target add i686-pc-windows-gnu
cargo install cargo-zigbuild
./build.sh   # needs Rust, zig and cargo-zigbuild; writes dist/winmm.dll and dist/isaac_p2p.ini
```

The DLL is a Rust `cdylib` for 32-bit Windows, linked with zig's MinGW toolchain. `build.rs` generates the winmm
forwarding stubs from `src/winmm_exports.txt` and the call-tracing thunks.

GitHub Actions (`.github/workflows/build.yml`) builds the DLL on every push and pull request and attaches
`winmm.dll` with the default `isaac_p2p.ini` as the `isaac-p2p` artifact. When `isaac_p2p.ini` is missing next
to the game, the DLL writes one with the defaults at start-up.

## Tests

```sh
./tests/run.sh   # needs zig and Wine (defaults to the newest Proton in the Steam library; override with WINE=)
```

The tests load `winmm.dll` next to a mock `steam_api.dll` in a throwaway Wine prefix. The mock records the config
calls and provides an `ISteamNetworking` object that the test calls through its vtable the way the game does.
Scenarios:

- `default`: settings applied, Steam debug output logged, both hooks pass calls through, relay and direct routes
  logged with address and ping, a value replaced by Steam is set again;
- `custom`: every ini key reaches Steam, `log=0` leaves the debug output alone;
- `norelay`: `force_no_relay=1` turns the relay off at start-up and when the game turns it back on;
- `disabled`, `nosteam`: nothing is changed;
- `missing_ini`: built-in defaults.

Each scenario also checks that the winmm exports are forwarded to the system DLL.

`cargo-zigbuild test --target i686-pc-windows-gnu` runs the Rust unit tests (crypto) under Wine.

`transport_test.exe` runs as a host and a client process over localhost, both dropping 20% of their datagrams.
They act like the game: the client must receive a lobby join request for the host's lobby through its
`GameLobbyJoinRequested_t` handler, and each side sends 20 reliable 50 KB messages that the other must receive
intact and in order, alongside unreliable ones. Further pairs check that a client finds the host through the lobby's offer alone, that a client reconnects after the host
went away without a goodbye and came back, and that a client with a wrong password is turned away.

### Two PCs

```sh
REMOTE=user@windows-pc [HOST_IP=...] [PORT=27931] [SECONDS=30] [DROP=0] ./tests/remote.sh
```

Runs the transport between this PC (host, under Wine) and a Windows PC reached over SSH (client, native).
With `LOBBY=1` the client gets no address and finds the host through the mock lobby, whose files the script
copies between the PCs, so `PORT` needs no forwarding; `ADDRESSES` goes into the host's `addresses=`.
`HOST_IP` is the address the remote reaches this PC on (default: this PC's Tailscale IPv4); `DROP` drops that
percentage of datagrams on both sides. The client sends 60 Hz unreliable pings and a 4 KB reliable message every
250 ms, the host echoes them, and the client prints loss and RTT percentiles for both.
