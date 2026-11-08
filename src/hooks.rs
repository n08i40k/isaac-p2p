//! Hooks on the game's Steam interfaces and imports.
//!
//! ISteamNetworking's packet calls are hooked so that peers connected through the own transport exchange packets
//! over it instead of Steam. The game's SteamAPI_RunCallbacks / SteamAPI_RegisterCallback imports are hooked so
//! that a transport client can join the host's Steam lobby through the game's own GameLobbyJoinRequested_t
//! handler, on the game thread, right after the game pumps callbacks.

use core::ffi::{c_char, c_void};
use core::ptr;
use std::cell::UnsafeCell;
use std::ffi::CStr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicUsize, Ordering};

use crate::steam::*;
use crate::{log, monitor, pe, settings, trace, transport, win};

/// A function pointer taken from a patched slot or import.
pub struct Real(AtomicUsize);

impl Real {
    pub const fn new() -> Real {
        Real(AtomicUsize::new(0))
    }

    pub fn set(&self, address: usize) {
        self.0.store(address, Ordering::SeqCst);
    }

    /// # Safety
    /// `F` must be the function pointer type of what was stored, and something must have been stored.
    pub unsafe fn get<F: Copy>(&self) -> F {
        let address = self.0.load(Ordering::SeqCst);
        debug_assert!(address != 0);
        unsafe { core::mem::transmute_copy(&address) }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// ISteamNetworking.

static REAL_SEND: Real = Real::new();
static REAL_ALLOW_RELAY: Real = Real::new();
static REAL_IS_AVAILABLE: Real = Real::new();
static REAL_READ: Real = Real::new();
static REAL_ACCEPT: Real = Real::new();
static REAL_CLOSE: Real = Real::new();
static REAL_SESSION_STATE: Real = Real::new();

unsafe extern "thiscall" fn hook_send_p2p_packet(
    this: *mut c_void,
    steam_id: u64,
    data: *const c_void,
    size: u32,
    send_type: i32,
    channel: i32,
) -> bool {
    monitor::remember_peer(steam_id);
    if transport::owns(steam_id) {
        let bytes = if data.is_null() || size == 0 {
            &[][..]
        } else {
            unsafe { core::slice::from_raw_parts(data.cast::<u8>(), size as usize) }
        };
        return transport::send(steam_id, bytes, send_type, channel);
    }
    unsafe { REAL_SEND.get::<SendP2PPacketFn>()(this, steam_id, data, size, send_type, channel) }
}

unsafe extern "thiscall" fn hook_is_p2p_packet_available(this: *mut c_void, size: *mut u32, channel: i32) -> bool {
    if let Some(available) = transport::available(channel) {
        if !size.is_null() {
            unsafe { *size = available };
        }
        return true;
    }
    unsafe { REAL_IS_AVAILABLE.get::<IsP2PPacketAvailableFn>()(this, size, channel) }
}

unsafe extern "thiscall" fn hook_read_p2p_packet(
    this: *mut c_void,
    dest: *mut c_void,
    capacity: u32,
    size: *mut u32,
    from: *mut u64,
    channel: i32,
) -> bool {
    if let Some(message) = transport::read(channel) {
        unsafe {
            let copy = message.data.len().min(capacity as usize);
            if copy > 0 {
                ptr::copy_nonoverlapping(message.data.as_ptr(), dest.cast::<u8>(), copy);
            }
            if !size.is_null() {
                *size = message.data.len() as u32;
            }
            if !from.is_null() {
                *from = message.from;
            }
        }
        return true;
    }
    unsafe { REAL_READ.get::<ReadP2PPacketFn>()(this, dest, capacity, size, from, channel) }
}

unsafe extern "thiscall" fn hook_accept_p2p_session(this: *mut c_void, steam_id: u64) -> bool {
    transport::owns(steam_id) || unsafe { REAL_ACCEPT.get::<SessionWithUserFn>()(this, steam_id) }
}

unsafe extern "thiscall" fn hook_close_p2p_session(this: *mut c_void, steam_id: u64) -> bool {
    transport::owns(steam_id) || unsafe { REAL_CLOSE.get::<SessionWithUserFn>()(this, steam_id) }
}

unsafe extern "thiscall" fn hook_get_p2p_session_state(
    this: *mut c_void,
    steam_id: u64,
    state: *mut P2PSessionState,
) -> bool {
    if let Some(own) = transport::session_state(steam_id) {
        unsafe { *state = own };
        return true;
    }
    unsafe { REAL_SESSION_STATE.get::<GetP2PSessionStateMethod>()(this, steam_id, state) }
}

unsafe extern "thiscall" fn hook_allow_p2p_packet_relay(this: *mut c_void, allow: u8) -> bool {
    let allow = allow != 0;
    let effective = !settings::get().force_no_relay && allow;
    log!("game called AllowP2PPacketRelay({}){}", allow, if effective != allow { ", forced to false" } else { "" });
    unsafe { REAL_ALLOW_RELAY.get::<AllowP2PPacketRelayFn>()(this, effective) }
}

/// The ISteamNetworking object the game uses. It comes from the game's own SteamInternal_FindOrCreateUserInterface
/// call, which can differ from the flat API's object when another steam_api.dll wraps the interfaces.
pub static GAME_NETWORKING: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static NETWORKING_LOCK: Mutex<()> = Mutex::new(());

unsafe fn vtable_of(object: *mut c_void) -> *mut usize {
    unsafe { *(object as *mut *mut usize) }
}

pub fn networking_hooks_intact(networking: *mut c_void) -> bool {
    unsafe {
        let vtable = vtable_of(networking);
        *vtable.add(SLOT_SEND_P2P_PACKET) == hook_send_p2p_packet as *const () as usize
            && *vtable.add(SLOT_READ_P2P_PACKET) == hook_read_p2p_packet as *const () as usize
            && *vtable.add(SLOT_IS_P2P_PACKET_AVAILABLE) == hook_is_p2p_packet_available as *const () as usize
    }
}

/// Points the slot at the hook. Already hooked slots are left as they are; otherwise `original` takes the slot's
/// current function, so hooks can be applied again after something restored the vtable.
fn hook_slot(vtable: *mut usize, slot: usize, hook: usize, original: &Real) -> bool {
    if unsafe { *vtable.add(slot) } == hook {
        return true;
    }
    match unsafe { pe::patch_slot(vtable, slot, hook) } {
        Some(previous) => {
            original.set(previous);
            true
        }
        None => false,
    }
}

/// Hooks the object's vtable. Safe to call again: Proton's steamclient bridge rebuilds interface vtables when the
/// game creates an interface, which drops hooks applied earlier.
pub fn hook_networking(networking: *mut c_void) {
    let guard = NETWORKING_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let game = GAME_NETWORKING.load(Ordering::SeqCst);
    let vtable = unsafe { vtable_of(networking) };
    if !game.is_null() && vtable != unsafe { vtable_of(game) } {
        log!(
            "another ISteamNetworking object {:p} (vtable {:p}) besides the hooked {:p}; left alone",
            networking,
            vtable,
            game
        );
        return;
    }
    let again = !game.is_null();
    GAME_NETWORKING.store(networking, Ordering::SeqCst);
    if again && networking_hooks_intact(networking) {
        return;
    }
    let send = hook_slot(vtable, SLOT_SEND_P2P_PACKET, hook_send_p2p_packet as *const () as usize, &REAL_SEND);
    let relay = hook_slot(vtable, SLOT_ALLOW_P2P_PACKET_RELAY, hook_allow_p2p_packet_relay as *const () as usize, &REAL_ALLOW_RELAY);
    let packets = hook_slot(vtable, SLOT_IS_P2P_PACKET_AVAILABLE, hook_is_p2p_packet_available as *const () as usize, &REAL_IS_AVAILABLE)
        && hook_slot(vtable, SLOT_READ_P2P_PACKET, hook_read_p2p_packet as *const () as usize, &REAL_READ)
        && hook_slot(vtable, SLOT_ACCEPT_P2P_SESSION, hook_accept_p2p_session as *const () as usize, &REAL_ACCEPT)
        && hook_slot(vtable, SLOT_CLOSE_P2P_SESSION, hook_close_p2p_session as *const () as usize, &REAL_CLOSE)
        && hook_slot(vtable, SLOT_GET_P2P_SESSION_STATE, hook_get_p2p_session_state as *const () as usize, &REAL_SESSION_STATE);
    let ok = |b: bool| if b { "ok" } else { "FAILED" };
    log!(
        "ISteamNetworking {:p} (vtable {:p}) hooks{}: SendP2PPacket {}, AllowP2PPacketRelay {}, transport {}",
        networking,
        vtable,
        if again { " applied again (the vtable was restored)" } else { "" },
        ok(send),
        ok(relay),
        ok(packets)
    );
    drop(guard);
    trace::networking(networking);
    if relay && settings::get().force_no_relay {
        unsafe { REAL_ALLOW_RELAY.get::<AllowP2PPacketRelayFn>()(networking, false) };
    }
}

// ---------------------------------------------------------------------------------------------------------------
// ISteamMatchmaking009::JoinLobby, hooked only to log what the game asks Steam for.

static REAL_JOIN_LOBBY: Real = Real::new();

unsafe extern "thiscall" fn hook_join_lobby(this: *mut c_void, lobby_id: u64) -> u64 {
    log!("game called JoinLobby({})", lobby_id);
    unsafe { REAL_JOIN_LOBBY.get::<JoinLobbyFn>()(this, lobby_id) }
}

pub fn hook_matchmaking(steam: win::HMODULE) {
    let get = unsafe { proc::<InterfaceFn>(steam, "SteamAPI_SteamMatchmaking_v009\0") };
    let matchmaking = get.map_or(ptr::null_mut(), |f| unsafe { f() });
    let ok = !matchmaking.is_null()
        && hook_slot(unsafe { vtable_of(matchmaking) }, SLOT_JOIN_LOBBY, hook_join_lobby as *const () as usize, &REAL_JOIN_LOBBY);
    log!("ISteamMatchmaking hook: JoinLobby {}", if ok { "ok" } else { "FAILED" });
}

// ---------------------------------------------------------------------------------------------------------------
// Steam callbacks.
//
// CCallbackBase in this build (MSVC puts overloaded virtuals in reverse order):
//   slot 0  void Run(void *param, bool io_failure, SteamAPICall_t call)
//   slot 1  void Run(void *param)
//   slot 2  int GetCallbackSizeBytes()
// followed by u8 flags at +4 and int callback id at +8.

#[repr(C)]
struct OwnCallback {
    vtable: *const *const c_void,
    flags: u8,
    callback_id: i32,
    size: i32,
}

/// Callback objects handed to Steam, which writes their flags and id.
struct OwnCallbacks(UnsafeCell<[OwnCallback; 4]>);
unsafe impl Sync for OwnCallbacks {}

struct Vtable([*const c_void; 3]);
unsafe impl Sync for Vtable {}

static OWN_CALLBACK_VTABLE: Vtable =
    Vtable([
    own_callback_run_call_result as *const c_void,
    own_callback_run as *const c_void,
    own_callback_size as *const c_void,
]);
// Every one of these callbacks is 24 bytes (ValidateAuthTicketResponse_t: u64 id, u32 response, pad, u64 owner).
static OWN_CALLBACKS: OwnCallbacks = OwnCallbacks(UnsafeCell::new(
    [const { OwnCallback { vtable: OWN_CALLBACK_VTABLE.0.as_ptr(), flags: 0, callback_id: 0, size: 24 } }; 4],
));
const OWN_CALLBACK_IDS: [i32; 4] = [
    CALLBACK_LOBBY_ENTER,
    CALLBACK_LOBBY_DATA_UPDATE,
    CALLBACK_LOBBY_CHAT_UPDATE,
    CALLBACK_VALIDATE_AUTH_TICKET_RESPONSE,
];

// Every lobby callback starts with the lobby's Steam ID. LobbyEnter_t carries the join result at +16
// (EChatRoomEnterResponse, 1 = success).
unsafe extern "thiscall" fn own_callback_run(this: *mut c_void, param: *mut c_void) {
    unsafe {
        let id = ptr::read_volatile(&(*this.cast::<OwnCallback>()).callback_id);
        if id == CALLBACK_VALIDATE_AUTH_TICKET_RESPONSE {
            let steam_id = ptr::read_unaligned(param.cast::<u64>());
            let response = ptr::read_unaligned(param.cast::<u8>().add(8).cast::<u32>());
            transport::auth_result(steam_id, response);
            return;
        }
        let lobby_id = ptr::read_unaligned(param.cast::<u64>());
        if id == CALLBACK_LOBBY_ENTER {
            let response = ptr::read_unaligned(param.cast::<u8>().add(16).cast::<u32>());
            log!(
                "Steam: entered lobby {}, response {}{}",
                lobby_id,
                response,
                if response == 1 { " (success)" } else { "" }
            );
            if response != 1 {
                return;
            }
        }
        transport::set_lobby(lobby_id);
    }
}

unsafe extern "thiscall" fn own_callback_run_call_result(this: *mut c_void, param: *mut c_void, _io_failure: u8, _call: u64) {
    unsafe { own_callback_run(this, param) }
}

unsafe extern "thiscall" fn own_callback_size(this: *mut c_void) -> i32 {
    unsafe { (*this.cast::<OwnCallback>()).size }
}

static REAL_RUN_CALLBACKS: Real = Real::new();
static REAL_REGISTER_CALLBACK: Real = Real::new();
static GAME_JOIN_REQUESTED_CALLBACK: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static REGISTERED: AtomicBool = AtomicBool::new(false);
static WAITING_LOGGED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn hook_register_callback(callback: *mut c_void, callback_id: i32) {
    if callback_id == CALLBACK_GAME_LOBBY_JOIN_REQUESTED {
        GAME_JOIN_REQUESTED_CALLBACK.store(callback, Ordering::SeqCst);
    }
    unsafe { REAL_REGISTER_CALLBACK.get::<RegisterCallbackFn>()(callback, callback_id) }
}

unsafe extern "C" fn hook_run_callbacks() {
    unsafe { REAL_RUN_CALLBACKS.get::<RunCallbacksFn>()() };

    if !REGISTERED.swap(true, Ordering::SeqCst) {
        let callbacks = OWN_CALLBACKS.0.get().cast::<OwnCallback>();
        for (i, id) in OWN_CALLBACK_IDS.iter().enumerate() {
            unsafe { REAL_REGISTER_CALLBACK.get::<RegisterCallbackFn>()(callbacks.add(i).cast(), *id) };
        }
    }

    if let Some(lobby) = steam_lobby() {
        transport::lobby_tick(lobby);
    }

    if !game_listens_for_joins() {
        if transport::join_pending() && !WAITING_LOGGED.swap(true, Ordering::Relaxed) {
            log!("transport: waiting for the game's menu to accept the lobby join (get past the title screen)");
        }
        return;
    }
    WAITING_LOGGED.store(false, Ordering::Relaxed);

    let Some((lobby_id, host_id)) = transport::take_join() else { return };
    let callback = GAME_JOIN_REQUESTED_CALLBACK.load(Ordering::SeqCst);
    if callback.is_null() {
        log!("transport: the game has no lobby join handler registered; cannot join lobby {}", lobby_id);
        return;
    }
    let mut request = [lobby_id, host_id]; // GameLobbyJoinRequested_t
    unsafe {
        let fields = callback as *const usize;
        let run = *vtable_of(callback).add(1);
        log!(
            "transport: asking the game to join lobby {} (handler {:#x}, object {:#x}, method {:#x})",
            lobby_id,
            run,
            *fields.add(3),
            *fields.add(4)
        );
        let run: CallbackRunFn = core::mem::transmute(run);
        run(callback, request.as_mut_ptr().cast());
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Steam session tickets for the transport's player verification (ISteamUser, flat API).

pub struct SteamTickets {
    user: usize,
    get_ticket: GetAuthSessionTicketFn,
    begin: BeginAuthSessionFn,
    end: EndAuthSessionFn,
    cancel: CancelAuthTicketFn,
}

impl SteamTickets {
    pub fn new(steam: win::HMODULE, user: *mut c_void) -> Option<SteamTickets> {
        unsafe {
            Some(SteamTickets {
                user: (!user.is_null()).then_some(user as usize)?,
                get_ticket: proc(steam, "SteamAPI_ISteamUser_GetAuthSessionTicket\0")?,
                begin: proc(steam, "SteamAPI_ISteamUser_BeginAuthSession\0")?,
                end: proc(steam, "SteamAPI_ISteamUser_EndAuthSession\0")?,
                cancel: proc(steam, "SteamAPI_ISteamUser_CancelAuthTicket\0")?,
            })
        }
    }
}

impl transport::SteamAuth for SteamTickets {
    fn ticket(&self) -> Option<(u32, Vec<u8>)> {
        let mut buffer = vec![0u8; 1024];
        let mut len = 0u32;
        let handle = unsafe {
            (self.get_ticket)(self.user as *mut c_void, buffer.as_mut_ptr(), buffer.len() as i32, &mut len, ptr::null())
        };
        if handle == 0 || len == 0 {
            return None;
        }
        buffer.truncate(len as usize);
        Some((handle, buffer))
    }

    fn begin(&self, ticket: &[u8], steam_id: u64) -> bool {
        // k_EBeginAuthSessionResultOK; the verdict follows as ValidateAuthTicketResponse_t.
        unsafe { (self.begin)(self.user as *mut c_void, ticket.as_ptr(), ticket.len() as i32, steam_id) == 0 }
    }

    fn end(&self, steam_id: u64) {
        unsafe { (self.end)(self.user as *mut c_void, steam_id) };
    }

    fn cancel(&self, handle: u32) {
        unsafe { (self.cancel)(self.user as *mut c_void, handle) };
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Lobby data for the transport's rendezvous, through the flat API (CSteamID returns stay plain u64 there).

type GetLobbyDataFn = unsafe extern "C" fn(*mut c_void, u64, *const c_char) -> *const c_char;
type SetLobbyDataFn = unsafe extern "C" fn(*mut c_void, u64, *const c_char, *const c_char) -> bool;
type GetNumLobbyMembersFn = unsafe extern "C" fn(*mut c_void, u64) -> i32;
type GetLobbyMemberByIndexFn = unsafe extern "C" fn(*mut c_void, u64, i32) -> u64;
type GetLobbyMemberDataFn = unsafe extern "C" fn(*mut c_void, u64, u64, *const c_char) -> *const c_char;
type SetLobbyMemberDataFn = unsafe extern "C" fn(*mut c_void, u64, *const c_char, *const c_char);
type GetLobbyOwnerFn = unsafe extern "C" fn(*mut c_void, u64) -> u64;

const LOBBY_KEY: &str = "isaac_p2p\0";

struct SteamLobby {
    matchmaking: usize,
    get_data: GetLobbyDataFn,
    set_data: SetLobbyDataFn,
    member_count: GetNumLobbyMembersFn,
    member_by_index: GetLobbyMemberByIndexFn,
    get_member_data: GetLobbyMemberDataFn,
    set_member_data: SetLobbyMemberDataFn,
    owner: GetLobbyOwnerFn,
}

fn text(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

fn c_string(value: &str) -> std::ffi::CString {
    std::ffi::CString::new(value).unwrap_or_default()
}

impl SteamLobby {
    fn object(&self) -> *mut c_void {
        self.matchmaking as *mut c_void
    }
}

impl transport::LobbyApi for SteamLobby {
    fn data(&self, lobby: u64) -> String {
        text(unsafe { (self.get_data)(self.object(), lobby, win::cstr(LOBBY_KEY).cast()) })
    }

    fn set_data(&self, lobby: u64, value: &str) {
        let value = c_string(value);
        unsafe { (self.set_data)(self.object(), lobby, win::cstr(LOBBY_KEY).cast(), value.as_ptr()) };
    }

    fn members(&self, lobby: u64) -> Vec<u64> {
        let count = unsafe { (self.member_count)(self.object(), lobby) }.clamp(0, 64);
        (0..count).map(|i| unsafe { (self.member_by_index)(self.object(), lobby, i) }).filter(|&id| id != 0).collect()
    }

    fn member_data(&self, lobby: u64, member: u64) -> String {
        text(unsafe { (self.get_member_data)(self.object(), lobby, member, win::cstr(LOBBY_KEY).cast()) })
    }

    fn set_member_data(&self, lobby: u64, value: &str) {
        let value = c_string(value);
        unsafe { (self.set_member_data)(self.object(), lobby, win::cstr(LOBBY_KEY).cast(), value.as_ptr()) };
    }

    fn owner(&self, lobby: u64) -> u64 {
        unsafe { (self.owner)(self.object(), lobby) }
    }
}

/// The lobby API, looked up on first use; `None` when steam_api.dll lacks part of it.
fn steam_lobby() -> Option<&'static SteamLobby> {
    static LOBBY: std::sync::OnceLock<Option<SteamLobby>> = std::sync::OnceLock::new();
    LOBBY
        .get_or_init(|| unsafe {
            let steam = win::GetModuleHandleA(win::cstr("steam_api.dll\0"));
            if steam.is_null() {
                return None;
            }
            let found = (|| {
                let matchmaking = proc::<InterfaceFn>(steam, "SteamAPI_SteamMatchmaking_v009\0")?();
                Some(SteamLobby {
                    matchmaking: (!matchmaking.is_null()).then_some(matchmaking as usize)?,
                    get_data: proc(steam, "SteamAPI_ISteamMatchmaking_GetLobbyData\0")?,
                    set_data: proc(steam, "SteamAPI_ISteamMatchmaking_SetLobbyData\0")?,
                    member_count: proc(steam, "SteamAPI_ISteamMatchmaking_GetNumLobbyMembers\0")?,
                    member_by_index: proc(steam, "SteamAPI_ISteamMatchmaking_GetLobbyMemberByIndex\0")?,
                    get_member_data: proc(steam, "SteamAPI_ISteamMatchmaking_GetLobbyMemberData\0")?,
                    set_member_data: proc(steam, "SteamAPI_ISteamMatchmaking_SetLobbyMemberData\0")?,
                    owner: proc(steam, "SteamAPI_ISteamMatchmaking_GetLobbyOwner\0")?,
                })
            })();
            if found.is_none() {
                log!("steam_api.dll lacks the lobby data API; players connect by IP only");
            }
            found
        })
        .as_ref()
}

// The game's GameLobbyJoinRequested_t handler defers the request to a task that hands the lobby to every listener
// in an intrusive list; the listeners belong to the menus and only exist past the title screen, so a request made
// earlier is lost. The list head is read from that task's code:
//   push [ebp+0Ch]; lea ecx,[ebp-18h]; call ...; mov edi,eax; mov dword [ebp-4],0; mov ecx,[list]; mov esi,[ecx];
//   cmp esi,ecx
const JOIN_LISTENERS_SIGNATURE: [i16; 30] = [
    0xFF, 0x75, 0x0C, 0x8D, 0x4D, 0xE8, 0xE8, -1, -1, -1, -1, 0x8B, 0xF8, 0xC7, 0x45, 0xFC, 0x00, 0x00, 0x00, 0x00,
    0x8B, 0x0D, -1, -1, -1, -1, 0x8B, 0x31, 0x3B, 0xF1,
];
const JOIN_LISTENERS_ADDRESS_AT: usize = 22;

/// Address of the game's pointer to the list head; 0 when not found.
static JOIN_LISTENERS: AtomicUsize = AtomicUsize::new(0);

fn find_join_listeners(game: win::HMODULE) {
    let matches = |window: &[u8]| JOIN_LISTENERS_SIGNATURE.iter().zip(window).all(|(&s, &b)| s < 0 || s as u8 == b);
    let mut hits = 0;
    let mut hit = None;
    for section in unsafe { pe::code_sections(game as *const u8) } {
        for window in section.windows(JOIN_LISTENERS_SIGNATURE.len()) {
            if matches(window) {
                if hits == 0 {
                    hit = Some(window);
                }
                hits += 1;
            }
        }
    }
    match hit {
        Some(window) if hits == 1 => {
            let at = JOIN_LISTENERS_ADDRESS_AT;
            let address = u32::from_le_bytes(window[at..at + 4].try_into().unwrap()) as usize;
            JOIN_LISTENERS.store(address, Ordering::SeqCst);
            log!("lobby join listeners at {:#x}", address);
        }
        _ => log!("lobby join listener list not found ({} matches); join requests are sent right away", hits),
    }
}

/// True when the game has a menu listening for lobby join requests (or when that can't be told).
fn game_listens_for_joins() -> bool {
    let listeners = JOIN_LISTENERS.load(Ordering::SeqCst) as *const *const usize;
    if listeners.is_null() {
        return true;
    }
    unsafe {
        let head = ptr::read_volatile(listeners);
        !head.is_null() && ptr::read_volatile(head) != head as usize
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Imports of isaac-ng.exe.

static REAL_FIND_OR_CREATE_USER_INTERFACE: Real = Real::new();

unsafe extern "C" fn hook_find_or_create_user_interface(user: i32, version: *const c_char) -> *mut c_void {
    let object = unsafe { REAL_FIND_OR_CREATE_USER_INTERFACE.get::<FindOrCreateUserInterfaceFn>()(user, version) };
    let name = if version.is_null() { None } else { Some(unsafe { CStr::from_ptr(version) }.to_string_lossy()) };
    let vtable = if object.is_null() { ptr::null_mut() } else { unsafe { vtable_of(object) } };
    log!("game requested {}: {:p} (vtable {:p})", name.as_deref().unwrap_or("(null)"), object, vtable);
    if !object.is_null() {
        match name.as_deref() {
            Some("SteamNetworking006") => {
                hook_networking(object);
                trace::networking(object);
            }
            Some("SteamMatchMaking009") => trace::matchmaking(object),
            _ => {}
        }
    }
    object
}

// GDI32!SwapBuffers presents every OpenGL frame; the overlay draws its HUD right before.
type SwapBuffersFn = unsafe extern "system" fn(win::HDC) -> i32;
static REAL_SWAP_BUFFERS: Real = Real::new();

unsafe extern "system" fn hook_swap_buffers(dc: win::HDC) -> i32 {
    crate::overlay::on_swap(dc);
    unsafe { REAL_SWAP_BUFFERS.get::<SwapBuffersFn>()(dc) }
}

// EOS P2P, counted to tell which backend carries the game's traffic.
type EosSendFn = unsafe extern "system" fn(*mut c_void, *const c_void) -> i32;
type EosReceiveFn = unsafe extern "system" fn(
    *mut c_void,
    *const c_void,
    *mut c_void,
    *mut c_void,
    *mut c_void,
    *mut c_void,
    *mut c_void,
) -> i32;
static REAL_EOS_SEND: Real = Real::new();
static REAL_EOS_RECEIVE: Real = Real::new();
static EOS_SENDS: AtomicI32 = AtomicI32::new(0);
static EOS_RECEIVES: AtomicI32 = AtomicI32::new(0);

unsafe extern "system" fn hook_eos_send(handle: *mut c_void, options: *const c_void) -> i32 {
    let count = EOS_SENDS.fetch_add(1, Ordering::Relaxed) + 1;
    if count == 1 || count % 1000 == 0 {
        log!("game sent EOS P2P packet #{}", count);
    }
    unsafe { REAL_EOS_SEND.get::<EosSendFn>()(handle, options) }
}

unsafe extern "system" fn hook_eos_receive(
    handle: *mut c_void,
    options: *const c_void,
    peer: *mut c_void,
    socket: *mut c_void,
    channel: *mut c_void,
    data: *mut c_void,
    written: *mut c_void,
) -> i32 {
    let result =
        unsafe { REAL_EOS_RECEIVE.get::<EosReceiveFn>()(handle, options, peer, socket, channel, data, written) };
    if result == 0 {
        // EOS_Success
        let count = EOS_RECEIVES.fetch_add(1, Ordering::Relaxed) + 1;
        if count == 1 || count % 1000 == 0 {
            log!("game received EOS P2P packet #{}", count);
        }
    }
    result
}

fn import(game: win::HMODULE, dll: &str, name: &str, hook: usize, original: &Real) -> bool {
    match unsafe { pe::hook_import(game, dll, name, hook) } {
        Some(previous) => {
            original.set(previous);
            true
        }
        None => false,
    }
}

pub fn hook_steam_imports() {
    let game = unsafe { win::GetModuleHandleA(ptr::null()) };
    find_join_listeners(game);
    let ok = |b: bool| if b { "ok" } else { "FAILED" };
    let lookup = import(
        game,
        "steam_api.dll",
        "SteamInternal_FindOrCreateUserInterface",
        hook_find_or_create_user_interface as *const () as usize,
        &REAL_FIND_OR_CREATE_USER_INTERFACE,
    );
    let eos = import(game, "EOSSDK-Win32-Shipping.dll", "_EOS_P2P_SendPacket@8", hook_eos_send as *const () as usize, &REAL_EOS_SEND)
        && import(
            game,
            "EOSSDK-Win32-Shipping.dll",
            "_EOS_P2P_ReceivePacket@28",
            hook_eos_receive as *const () as usize,
            &REAL_EOS_RECEIVE,
        );
    let swap = import(game, "GDI32.dll", "SwapBuffers", hook_swap_buffers as *const () as usize, &REAL_SWAP_BUFFERS);
    log!("import hooks: FindOrCreateUserInterface {}, EOS P2P {}, SwapBuffers (HUD) {}", ok(lookup), ok(eos), ok(swap));
    let run = import(game, "steam_api.dll", "SteamAPI_RunCallbacks", hook_run_callbacks as *const () as usize, &REAL_RUN_CALLBACKS);
    let reg = import(
        game,
        "steam_api.dll",
        "SteamAPI_RegisterCallback",
        hook_register_callback as *const () as usize,
        &REAL_REGISTER_CALLBACK,
    );
    log!("steam_api import hooks: RunCallbacks {}, RegisterCallback {}", ok(run), ok(reg));
    if !run || !reg {
        // Lobby hand-off needs both; leave the game's imports as they are.
        unsafe {
            if run {
                pe::hook_import(game, "steam_api.dll", "SteamAPI_RunCallbacks", REAL_RUN_CALLBACKS.0.load(Ordering::SeqCst));
            }
            if reg {
                pe::hook_import(
                    game,
                    "steam_api.dll",
                    "SteamAPI_RegisterCallback",
                    REAL_REGISTER_CALLBACK.0.load(Ordering::SeqCst),
                );
            }
        }
    }
}
