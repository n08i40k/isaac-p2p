//! winmm.dll proxy for The Binding of Isaac: Repentance+.
//!
//! The game sends online co-op traffic through the legacy ISteamNetworking P2P API (SendP2PPacket/ReadP2PPacket).
//! Steam carries those sessions over Steam Networking Sockets, which decides per user whether a session may use a
//! direct UDP connection (ICE) or only Valve's relays (SDR). This DLL waits until the game has initialised Steam,
//! sets the global networking config so that sessions try a direct connection and prefer it over the relay, and
//! logs the route and ping of every peer the game talks to.
//!
//! It also hooks the packet calls of ISteamNetworking so that peers connected through the own UDP transport
//! (`transport`) exchange packets over it instead of Steam, and lets a transport client join the host's Steam lobby
//! through the game's own GameLobbyJoinRequested_t handler. Settings come from isaac_p2p.ini next to the game.

#![cfg(all(windows, target_arch = "x86"))]

mod console;
mod crypto;
mod exports;
mod hooks;
mod log;
mod monitor;
mod overlay;
mod pe;
mod settings;
mod steam;
mod stun;
mod trace;
mod transport;
mod win;

use core::ffi::c_void;
use core::ptr;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::time::Duration;

use steam::*;

/// Waits for Steam, applies the config, hooks the game's networking interface, then runs the route monitor.
fn worker() {
    if settings::get().console {
        console::start_input();
    }

    let mut steam: win::HMODULE = ptr::null_mut();
    let mut utils: *mut c_void = ptr::null_mut();
    let mut get_user: Option<GetHSteamUserFn> = None;
    let mut get_utils: Option<InterfaceFn> = None;
    // The game initialises Steam a moment after start-up; give up after two minutes.
    for _ in 0..2400 {
        if steam.is_null() {
            steam = unsafe { win::GetModuleHandleA(win::cstr("steam_api.dll\0")) };
            if !steam.is_null() {
                get_user = unsafe { proc(steam, "SteamAPI_GetHSteamUser\0") };
                get_utils = unsafe { proc(steam, "SteamAPI_SteamNetworkingUtils_SteamAPI_v004\0") };
                if get_user.is_none() || get_utils.is_none() {
                    log!("steam_api.dll lacks the networking flat API");
                    return;
                }
            }
        }
        if let (Some(get_user), Some(get_utils)) = (get_user, get_utils) {
            if unsafe { get_user() } != 0 {
                utils = unsafe { get_utils() };
                if !utils.is_null() {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if utils.is_null() {
        log!("Steam was not initialised; nothing changed");
        return;
    }

    let get_user_interface = unsafe { proc::<InterfaceFn>(steam, "SteamAPI_SteamUser_v023\0") };
    let get_steam_id = unsafe { proc::<GetSteamIdFn>(steam, "SteamAPI_ISteamUser_GetSteamID\0") };
    let user = get_user_interface.map_or(ptr::null_mut(), |f| unsafe { f() });
    match hooks::SteamTickets::new(steam, user) {
        Some(tickets) => transport::set_auth(Box::new(tickets)),
        None => log!("steam_api.dll lacks the session ticket API; transport players need a password"),
    }
    match get_steam_id {
        Some(get_steam_id) if !user.is_null() => transport::start(unsafe { get_steam_id(user) }),
        _ => log!("ISteamUser023 unavailable; transport off"),
    }

    if !settings::enabled() {
        log!("Steam networking config left untouched (enabled=0)");
    } else if !monitor::apply_config(steam, utils) {
        settings::ENABLED.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    hooks::hook_matchmaking(steam);
    // Prefer the object the game asked for; fall back to the flat API's when the game has not asked yet.
    let game_networking = || hooks::GAME_NETWORKING.load(std::sync::atomic::Ordering::SeqCst);
    for _ in 0..40 {
        if !game_networking().is_null() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if game_networking().is_null() {
        let get_networking = unsafe { proc::<InterfaceFn>(steam, "SteamAPI_SteamNetworking_v006\0") };
        let flat = get_networking.map_or(ptr::null_mut(), |f| unsafe { f() });
        if !flat.is_null() {
            hooks::hook_networking(flat);
        }
    }
    let networking = game_networking();
    if networking.is_null() {
        log!("ISteamNetworking006 unavailable; route monitor off");
        settings::MONITOR.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    monitor::run(steam, networking);
}

fn module_dir(instance: win::HMODULE) -> PathBuf {
    let mut buffer = [0u16; 1024];
    let len = unsafe { win::GetModuleFileNameW(instance, buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    let path = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len]));
    path.parent().map(PathBuf::from).unwrap_or_default()
}

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(instance: win::HMODULE, reason: u32, _reserved: *mut c_void) -> i32 {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason != DLL_PROCESS_ATTACH {
        return 1;
    }
    unsafe { win::DisableThreadLibraryCalls(instance) };
    let missing_exports = exports::load_real();

    settings::set_base_dir(module_dir(instance));
    let created_ini = settings::create_default();
    settings::load();
    if settings::get().console {
        console::open();
    }
    log::open(std::fs::File::create(settings::base_dir().join("isaac_p2p.log")).ok());
    log!("isaac-p2p loaded");
    if created_ini {
        log!("isaac_p2p.ini was missing; wrote one with the defaults");
    }
    match missing_exports {
        None => log!("system winmm.dll could not be loaded"),
        Some(missing) => {
            for name in missing {
                log!("system winmm.dll lacks {}; calls to it would crash", name);
            }
        }
    }
    hooks::hook_steam_imports();

    std::thread::spawn(worker);
    1
}
