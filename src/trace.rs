//! Call tracing for the game's ISteamNetworking and ISteamMatchmaking objects: every vtable slot that is not
//! hooked otherwise gets a counting thunk, and `report()` logs the counts that changed.

use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use crate::{log, pe};

include!(concat!(env!("OUT_DIR"), "/trace_thunks.rs"));

const NETWORKING_NAMES: &[&str] = &[
    "SendP2PPacket", "IsP2PPacketAvailable", "ReadP2PPacket", "AcceptP2PSessionWithUser", "CloseP2PSessionWithUser",
    "CloseP2PChannelWithUser", "GetP2PSessionState", "AllowP2PPacketRelay", "CreateListenSocket",
    "CreateP2PConnectionSocket", "CreateConnectionSocket", "DestroySocket", "DestroyListenSocket", "SendDataOnSocket",
    "IsDataAvailableOnSocket", "RetrieveDataFromSocket", "IsDataAvailable", "RetrieveData", "GetSocketInfo",
    "GetListenSocketInfo", "GetSocketConnectionType", "GetMaxPacketSize",
];

const MATCHMAKING_NAMES: &[&str] = &[
    "GetFavoriteGameCount", "GetFavoriteGame", "AddFavoriteGame", "RemoveFavoriteGame", "RequestLobbyList",
    "AddRequestLobbyListStringFilter", "AddRequestLobbyListNumericalFilter", "AddRequestLobbyListNearValueFilter",
    "AddRequestLobbyListFilterSlotsAvailable", "AddRequestLobbyListDistanceFilter",
    "AddRequestLobbyListResultCountFilter", "AddRequestLobbyListCompatibleMembersFilter", "GetLobbyByIndex",
    "CreateLobby", "JoinLobby", "LeaveLobby", "InviteUserToLobby", "GetNumLobbyMembers", "GetLobbyMemberByIndex",
    "GetLobbyData", "SetLobbyData", "GetLobbyDataCount", "GetLobbyDataByIndex", "DeleteLobbyData",
    "GetLobbyMemberData", "SetLobbyMemberData", "SendLobbyChatMsg", "GetLobbyChatEntry", "RequestLobbyData",
    "SetLobbyGameServer", "GetLobbyGameServer", "SetLobbyMemberLimit", "GetLobbyMemberLimit", "SetLobbyType",
    "SetLobbyJoinable", "GetLobbyOwner", "SetLobbyOwner", "SetLinkedLobby",
];

struct TracedSlot {
    short_name: &'static str,
    method: &'static str,
    slot: usize,
    vtable: usize,
    reported: i32,
}

static TRACED: Mutex<Vec<TracedSlot>> = Mutex::new(Vec::new());

fn trace_vtable(vtable: *mut usize, interface: &str, short_name: &'static str, names: &'static [&'static str]) {
    let (own_start, own_end) = pe::own_image();
    let mut traced = TRACED.lock().unwrap_or_else(|e| e.into_inner());
    let mut added = 0;
    for (slot, &method) in names.iter().enumerate() {
        let current = unsafe { *vtable.add(slot) };
        let ours = TRACE_THUNK_FNS[..traced.len()].iter().any(|&f| f as *const () as usize == current);
        if ours || (own_start..own_end).contains(&current) {
            continue; // already traced, or hooked by this DLL
        }
        let index = match traced.iter().position(|t| t.vtable == vtable as usize && t.slot == slot) {
            Some(index) => index,
            None if traced.len() < TRACE_THUNKS => {
                traced.push(TracedSlot { short_name, method, slot, vtable: vtable as usize, reported: 0 });
                traced.len() - 1
            }
            None => break,
        };
        TRACE_TARGETS[index].store(current, Ordering::SeqCst);
        if unsafe { pe::patch_slot(vtable, slot, TRACE_THUNK_FNS[index] as *const () as usize) }.is_none() {
            continue;
        }
        let entry = &mut traced[index];
        entry.method = method;
        entry.short_name = short_name;
        added += 1;
    }
    drop(traced);
    log!("trace: {} {} methods counted (vtable {:p})", added, interface, vtable);
}

pub fn networking(object: *mut core::ffi::c_void) {
    if !object.is_null() {
        trace_vtable(unsafe { *(object as *mut *mut usize) }, "ISteamNetworking", "net", NETWORKING_NAMES);
    }
}

pub fn matchmaking(object: *mut core::ffi::c_void) {
    if !object.is_null() {
        trace_vtable(unsafe { *(object as *mut *mut usize) }, "ISteamMatchmaking", "mm", MATCHMAKING_NAMES);
    }
}

pub fn report() {
    let mut line = String::new();
    let mut traced = TRACED.lock().unwrap_or_else(|e| e.into_inner());
    for (i, t) in traced.iter_mut().enumerate() {
        let now = TRACE_COUNTS[i].load(Ordering::Relaxed);
        if now == t.reported {
            continue;
        }
        if !line.is_empty() {
            line.push_str(", ");
        }
        line.push_str(&format!("{}.{} +{}", t.short_name, t.method, now - t.reported));
        t.reported = now;
    }
    drop(traced);
    if !line.is_empty() {
        log!("calls: {}", line);
    }
}
