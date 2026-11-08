//! Steam networking config enforcement and the route monitor: logs relay / direct route and ping of every peer
//! the game sends to.

use core::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::steam::*;
use crate::{hooks, log, settings, trace, win};

// ---------------------------------------------------------------------------------------------------------------
// Peer tracking: the SendP2PPacket hook records every Steam ID the game sends to.

const MAX_PEERS: usize = 16;

static PEER_IDS: [AtomicU64; MAX_PEERS] = [const { AtomicU64::new(0) }; MAX_PEERS];
static PEER_COUNT: AtomicUsize = AtomicUsize::new(0);
static PEER_LOCK: Mutex<()> = Mutex::new(());

fn known(steam_id: u64, count: usize) -> bool {
    PEER_IDS[..count].iter().any(|id| id.load(Ordering::Acquire) == steam_id)
}

pub fn remember_peer(steam_id: u64) {
    if known(steam_id, PEER_COUNT.load(Ordering::Acquire)) {
        return;
    }
    let guard = PEER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let count = PEER_COUNT.load(Ordering::Acquire);
    if known(steam_id, count) {
        return;
    }
    if count < MAX_PEERS {
        PEER_IDS[count].store(steam_id, Ordering::Release);
        PEER_COUNT.store(count + 1, Ordering::Release);
    }
    drop(guard);
    log!("peer {}: game opened a session", steam_id);
}

/// Latest Steam route of a peer, for the overlay.
#[derive(Clone)]
pub struct SteamRoute {
    pub steam_id: u64,
    pub route: &'static str,
    pub ping_ms: Option<i32>,
}

static ROUTES: Mutex<Vec<SteamRoute>> = Mutex::new(Vec::new());

pub fn steam_routes() -> Vec<SteamRoute> {
    ROUTES.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn publish_route(steam_id: u64, route: &'static str, ping_ms: Option<i32>) {
    let mut routes = ROUTES.lock().unwrap_or_else(|e| e.into_inner());
    match routes.iter_mut().find(|r| r.steam_id == steam_id) {
        Some(r) => {
            r.route = route;
            r.ping_ms = ping_ms;
        }
        None => routes.push(SteamRoute { steam_id, route, ping_ms }),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Config.

struct Config {
    utils: usize,
    set_int32: SetInt32Fn,
    get: Option<GetConfigValueFn>,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

fn enforced() -> [(i32, &'static str, i32); 3] {
    let s = settings::get();
    [
        (CONFIG_P2P_TRANSPORT_ICE_ENABLE, "P2P_Transport_ICE_Enable", s.ice_flags),
        (CONFIG_P2P_TRANSPORT_ICE_PENALTY, "P2P_Transport_ICE_Penalty", s.ice_penalty),
        (CONFIG_P2P_TRANSPORT_SDR_PENALTY, "P2P_Transport_SDR_Penalty", s.sdr_penalty),
    ]
}

fn set_int(config: &Config, key: i32, value: i32, name: &str) {
    let ok = unsafe { (config.set_int32)(config.utils as *mut c_void, key, value) };
    log!("{} = {}: {}", name, value, if ok { "ok" } else { "FAILED" });
}

unsafe extern "C" fn steam_debug_output(_kind: i32, message: *const c_char) {
    log!("steam: {}", unsafe { CStr::from_ptr(message) }.to_string_lossy());
}

/// Steam loads its own networking config (user preferences included) when the game first uses the network, which
/// can replace values set earlier. Reads the enforced values back and sets them again where they differ.
fn enforce_config() {
    let Some(config) = CONFIG.get() else { return };
    let Some(get) = config.get else { return };
    if !settings::enabled() {
        return;
    }
    for (key, name, value) in enforced() {
        let mut kind = CONFIG_TYPE_INT32;
        let mut current = 0i32;
        let mut size = 4usize;
        let result = unsafe {
            get(config.utils as *mut c_void, key, CONFIG_SCOPE_GLOBAL, 0, &mut kind, (&mut current as *mut i32).cast(), &mut size)
        };
        if (result == GET_CONFIG_OK || result == GET_CONFIG_OK_INHERITED) && current == value {
            continue;
        }
        log!("{} is {} (result {}), expected {}; setting again", name, current, result, value);
        set_int(config, key, value, name);
    }
}

pub fn apply_config(steam: win::HMODULE, utils: *mut c_void) -> bool {
    unsafe {
        let set_int32 = proc::<SetInt32Fn>(steam, "SteamAPI_ISteamNetworkingUtils_SetGlobalConfigValueInt32\0");
        let get = proc::<GetConfigValueFn>(steam, "SteamAPI_ISteamNetworkingUtils_GetConfigValue\0");
        let set_string = proc::<SetStringFn>(steam, "SteamAPI_ISteamNetworkingUtils_SetGlobalConfigValueString\0");
        let set_debug_output =
            proc::<SetDebugOutputFn>(steam, "SteamAPI_ISteamNetworkingUtils_SetDebugOutputFunction\0");
        let (Some(set_int32), Some(set_string)) = (set_int32, set_string) else {
            log!("steam_api.dll lacks SetGlobalConfigValue*");
            return false;
        };
        let config = CONFIG.get_or_init(|| Config { utils: utils as usize, set_int32, get });
        let s = settings::get();
        if s.log_steam {
            if let Some(set_debug_output) = set_debug_output {
                set_debug_output(utils, DEBUG_OUTPUT_MSG, steam_debug_output);
                set_int(config, CONFIG_LOG_LEVEL_P2P_RENDEZVOUS, DEBUG_OUTPUT_MSG, "LogLevel_P2PRendezvous");
            }
        }
        if !s.stun_servers.is_empty() {
            let value = std::ffi::CString::new(s.stun_servers.as_str()).unwrap_or_default();
            let ok = set_string(utils, CONFIG_P2P_STUN_SERVER_LIST, value.as_ptr());
            log!("P2P_STUN_ServerList = {}: {}", s.stun_servers, if ok { "ok" } else { "FAILED" });
        }
        for (key, name, value) in enforced() {
            set_int(config, key, value, name);
        }
    }
    true
}

// ---------------------------------------------------------------------------------------------------------------
// Route monitor.

#[derive(Default)]
struct Peer {
    steam_id: u64,
    last_state: P2PSessionState,
    last_relayed: i32,
    has_state: bool,
    last_ping_log: Option<Instant>,
}

struct Api {
    networking: *mut c_void,
    get_state: GetP2PSessionStateFn,
    messages: *mut c_void,
    get_info: Option<GetSessionConnectionInfoFn>,
}

fn format_ip(ip: u32, port: u16) -> String {
    format!("{}.{}.{}.{}:{}", ip >> 24, (ip >> 16) & 255, (ip >> 8) & 255, ip & 255, port)
}

fn check_peer(peer: &mut Peer, api: &Api) {
    let mut state = P2PSessionState::default();
    if !unsafe { (api.get_state)(api.networking, peer.steam_id, &mut state) } {
        state = P2PSessionState::default();
    }

    let mut info: ConnectionInfo = unsafe { core::mem::zeroed() };
    let mut status: RealTimeStatus = unsafe { core::mem::zeroed() };
    let mut info_state = 0;
    if let (false, Some(get_info)) = (api.messages.is_null(), api.get_info) {
        let mut identity: NetworkingIdentity = unsafe { core::mem::zeroed() };
        identity.kind = IDENTITY_TYPE_STEAM_ID;
        identity.size = 8;
        identity.data.0[..8].copy_from_slice(&peer.steam_id.to_le_bytes());
        info_state = unsafe { get_info(api.messages, &identity, &mut info, &mut status) };
    }
    let have_info = info_state == CONNECTION_STATE_CONNECTED;
    let relayed = if have_info { (info.flags & CONNECTION_FLAG_RELAYED != 0) as i32 } else { -1 };
    publish_route(
        peer.steam_id,
        match (state.connection_active != 0, state.using_relay != 0, state.connecting != 0) {
            (true, true, _) => "Steam relay",
            (true, false, _) => "Steam direct",
            (false, _, true) => "connecting",
            _ => "no session",
        },
        have_info.then_some(status.ping),
    );

    let last = &peer.last_state;
    let changed = !peer.has_state
        || state.connection_active != last.connection_active
        || state.connecting != last.connecting
        || state.using_relay != last.using_relay
        || state.remote_ip != last.remote_ip
        || state.remote_port != last.remote_port
        || relayed != peer.last_relayed;
    let interval = settings::get().ping_log_interval_s;
    let ping_due = have_info
        && interval > 0
        && peer.last_ping_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(interval as u64));
    if !changed && !ping_due {
        return;
    }

    let route = if state.connection_active != 0 {
        if state.using_relay != 0 { "RELAY" } else { "DIRECT" }
    } else if state.connecting != 0 {
        "connecting"
    } else {
        "no session"
    };
    let address = if state.connection_active != 0 && state.using_relay == 0 && state.remote_ip != 0 {
        format!(" {}", format_ip(state.remote_ip, state.remote_port))
    } else {
        String::new()
    };

    if have_info {
        let description = unsafe { CStr::from_ptr(info.description.as_ptr()) }.to_string_lossy();
        log!(
            "peer {}: {}{} ping {} ms (sockets: {}; {})",
            peer.steam_id,
            route,
            address,
            status.ping,
            if relayed != 0 { "relayed" } else { "direct" },
            description
        );
    } else {
        log!(
            "peer {}: {}{}{}",
            peer.steam_id,
            route,
            address,
            if state.session_error != 0 { " (session error)" } else { "" }
        );
    }

    peer.last_state = state;
    peer.last_relayed = relayed;
    peer.has_state = true;
    if have_info {
        peer.last_ping_log = Some(Instant::now());
    }
}

pub fn run(steam: win::HMODULE, networking: *mut c_void) -> ! {
    let get_state = unsafe { proc::<GetP2PSessionStateFn>(steam, "SteamAPI_ISteamNetworking_GetP2PSessionState\0") };
    let get_messages = unsafe { proc::<InterfaceFn>(steam, "SteamAPI_SteamNetworkingMessages_SteamAPI_v002\0") };
    let get_info = unsafe {
        proc::<GetSessionConnectionInfoFn>(steam, "SteamAPI_ISteamNetworkingMessages_GetSessionConnectionInfo\0")
    };
    if get_state.is_none() && settings::monitor() {
        log!("steam_api.dll lacks GetP2PSessionState; route monitor off");
    }
    let messages = get_messages.map_or(core::ptr::null_mut(), |f| unsafe { f() });
    let api = get_state.map(|get_state| Api { networking, get_state, messages, get_info });

    let mut peers: Vec<Peer> = Vec::new();
    loop {
        std::thread::sleep(Duration::from_millis(settings::get().monitor_interval_ms as u64));
        enforce_config();
        let game = hooks::GAME_NETWORKING.load(Ordering::SeqCst);
        if !game.is_null() && !hooks::networking_hooks_intact(game) {
            hooks::hook_networking(game);
        }
        trace::report();
        let Some(api) = api.as_ref().filter(|_| settings::monitor()) else { continue };
        let count = PEER_COUNT.load(Ordering::Acquire);
        while peers.len() < count {
            let steam_id = PEER_IDS[peers.len()].load(Ordering::Acquire);
            peers.push(Peer { steam_id, ..Peer::default() });
        }
        for peer in &mut peers {
            check_peer(peer, api);
        }
    }
}
