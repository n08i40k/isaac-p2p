//! Steam API (steam_api.dll) types and constants. Flat functions are cdecl; interface methods are thiscall.

use core::ffi::{c_char, c_void};

pub const CONFIG_LOG_LEVEL_P2P_RENDEZVOUS: i32 = 17;
pub const CONFIG_P2P_STUN_SERVER_LIST: i32 = 103;
pub const CONFIG_P2P_TRANSPORT_ICE_ENABLE: i32 = 104;
pub const CONFIG_P2P_TRANSPORT_ICE_PENALTY: i32 = 105;
pub const CONFIG_P2P_TRANSPORT_SDR_PENALTY: i32 = 106;

pub const DEBUG_OUTPUT_MSG: i32 = 5;
pub const CONFIG_SCOPE_GLOBAL: i32 = 1;
pub const CONFIG_TYPE_INT32: i32 = 1;
pub const GET_CONFIG_OK: i32 = 1;
pub const GET_CONFIG_OK_INHERITED: i32 = 2;

// ISteamNetworking006 vtable slots.
pub const SLOT_SEND_P2P_PACKET: usize = 0;
pub const SLOT_IS_P2P_PACKET_AVAILABLE: usize = 1;
pub const SLOT_READ_P2P_PACKET: usize = 2;
pub const SLOT_ACCEPT_P2P_SESSION: usize = 3;
pub const SLOT_CLOSE_P2P_SESSION: usize = 4;
pub const SLOT_GET_P2P_SESSION_STATE: usize = 6;
pub const SLOT_ALLOW_P2P_PACKET_RELAY: usize = 7;
// ISteamMatchmaking009.
pub const SLOT_JOIN_LOBBY: usize = 14;

pub const SEND_RELIABLE: i32 = 2;
pub const SEND_RELIABLE_WITH_BUFFERING: i32 = 3;

pub const IDENTITY_TYPE_STEAM_ID: i32 = 16;
pub const CONNECTION_STATE_CONNECTED: i32 = 3;
pub const CONNECTION_FLAG_RELAYED: i32 = 16;

pub const CALLBACK_VALIDATE_AUTH_TICKET_RESPONSE: i32 = 143;
pub const CALLBACK_GAME_LOBBY_JOIN_REQUESTED: i32 = 333;
pub const CALLBACK_LOBBY_ENTER: i32 = 504;
pub const CALLBACK_LOBBY_DATA_UPDATE: i32 = 505;
pub const CALLBACK_LOBBY_CHAT_UPDATE: i32 = 506;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct P2PSessionState {
    pub connection_active: u8,
    pub connecting: u8,
    pub session_error: u8,
    pub using_relay: u8,
    pub bytes_queued_for_send: i32,
    pub packets_queued_for_send: i32,
    pub remote_ip: u32,
    pub remote_port: u16,
}

#[repr(C, align(8))]
pub struct IdentityData(pub [u8; 128]);

#[repr(C)]
pub struct NetworkingIdentity {
    pub kind: i32,
    pub size: i32,
    pub data: IdentityData,
}

#[repr(C)]
pub struct ConnectionInfo {
    pub remote: NetworkingIdentity,
    pub user_data: i64,
    pub listen_socket: u32,
    pub remote_address: [u8; 18],
    pub pad: u16,
    pub pop_remote: u32,
    pub pop_relay: u32,
    pub state: i32,
    pub end_reason: i32,
    pub end_debug: [c_char; 128],
    pub description: [c_char; 128],
    pub flags: i32,
    pub reserved: [u32; 63],
}

#[repr(C)]
pub struct RealTimeStatus {
    pub state: i32,
    pub ping: i32,
    pub quality_local: f32,
    pub quality_remote: f32,
    pub out_packets_per_sec: f32,
    pub out_bytes_per_sec: f32,
    pub in_packets_per_sec: f32,
    pub in_bytes_per_sec: f32,
    pub send_rate_bytes_per_sec: i32,
    pub pending_unreliable: i32,
    pub pending_reliable: i32,
    pub sent_unacked_reliable: i32,
    pub queue_time: i64,
    pub reserved: [u32; 16],
}

const _: () = assert!(size_of::<NetworkingIdentity>() == 136);
const _: () = assert!(size_of::<ConnectionInfo>() == 696);
const _: () = assert!(size_of::<RealTimeStatus>() == 120);
const _: () = assert!(size_of::<P2PSessionState>() == 20);

pub type GetHSteamUserFn = unsafe extern "C" fn() -> i32;
pub type GetSteamIdFn = unsafe extern "C" fn(*mut c_void) -> u64;
pub type InterfaceFn = unsafe extern "C" fn() -> *mut c_void;
pub type SetInt32Fn = unsafe extern "C" fn(*mut c_void, i32, i32) -> bool;
pub type SetStringFn = unsafe extern "C" fn(*mut c_void, i32, *const c_char) -> bool;
pub type DebugOutputFn = unsafe extern "C" fn(i32, *const c_char);
pub type SetDebugOutputFn = unsafe extern "C" fn(*mut c_void, i32, DebugOutputFn);
pub type GetConfigValueFn =
    unsafe extern "C" fn(*mut c_void, i32, i32, isize, *mut i32, *mut c_void, *mut usize) -> i32;
pub type GetP2PSessionStateFn = unsafe extern "C" fn(*mut c_void, u64, *mut P2PSessionState) -> bool;
pub type GetSessionConnectionInfoFn =
    unsafe extern "C" fn(*mut c_void, *const NetworkingIdentity, *mut ConnectionInfo, *mut RealTimeStatus) -> i32;

pub type SendP2PPacketFn = unsafe extern "thiscall" fn(*mut c_void, u64, *const c_void, u32, i32, i32) -> bool;
pub type AllowP2PPacketRelayFn = unsafe extern "thiscall" fn(*mut c_void, bool) -> bool;
pub type IsP2PPacketAvailableFn = unsafe extern "thiscall" fn(*mut c_void, *mut u32, i32) -> bool;
pub type ReadP2PPacketFn =
    unsafe extern "thiscall" fn(*mut c_void, *mut c_void, u32, *mut u32, *mut u64, i32) -> bool;
pub type SessionWithUserFn = unsafe extern "thiscall" fn(*mut c_void, u64) -> bool;
pub type GetP2PSessionStateMethod = unsafe extern "thiscall" fn(*mut c_void, u64, *mut P2PSessionState) -> bool;
pub type JoinLobbyFn = unsafe extern "thiscall" fn(*mut c_void, u64) -> u64;

pub type RunCallbacksFn = unsafe extern "C" fn();
pub type RegisterCallbackFn = unsafe extern "C" fn(*mut c_void, i32);
pub type CallbackRunFn = unsafe extern "thiscall" fn(*mut c_void, *mut c_void);
pub type GetAuthSessionTicketFn = unsafe extern "C" fn(*mut c_void, *mut u8, i32, *mut u32, *const c_void) -> u32;
pub type BeginAuthSessionFn = unsafe extern "C" fn(*mut c_void, *const u8, i32, u64) -> i32;
pub type EndAuthSessionFn = unsafe extern "C" fn(*mut c_void, u64);
pub type CancelAuthTicketFn = unsafe extern "C" fn(*mut c_void, u32);
pub type FindOrCreateUserInterfaceFn = unsafe extern "C" fn(i32, *const c_char) -> *mut c_void;

/// `GetProcAddress` typed as `F`; `name` ends with a NUL.
///
/// # Safety
/// `F` must be a function pointer type matching the export.
pub unsafe fn proc<F: Copy>(module: crate::win::HMODULE, name: &'static str) -> Option<F> {
    let address = unsafe { crate::win::GetProcAddress(module, crate::win::cstr(name)) };
    (!address.is_null()).then(|| unsafe { core::mem::transmute_copy(&address) })
}
