// Stand-in for steam_api.dll: records the networking config calls made by winmm.dll.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define EXPORT __declspec(dllexport)

typedef void(__cdecl *DebugOutputFn)(int, const char *);

static volatile int steam_user;
static int utils_object;
static int int_keys[32], int_values[32], int_count;
static int string_key;
static char string_value[1024];
static DebugOutputFn debug_output;
static int debug_level;

EXPORT int __cdecl SteamAPI_GetHSteamUser(void) { return steam_user; }
EXPORT void *__cdecl SteamAPI_SteamNetworkingUtils_SteamAPI_v004(void) { return steam_user ? &utils_object : 0; }

EXPORT bool __cdecl SteamAPI_ISteamNetworkingUtils_SetGlobalConfigValueInt32(void *self, int key, int value) {
    if (self != &utils_object || int_count == 32)
        return false;
    int_keys[int_count] = key;
    int_values[int_count] = value;
    int_count++;
    return true;
}

EXPORT int __cdecl SteamAPI_ISteamNetworkingUtils_GetConfigValue(void *self, int key, int scope, intptr_t object,
    int *type, void *result, size_t *size) {
    (void)object;
    if (self != &utils_object || scope != 1 || *type != 1 || *size < 4)
        return -1;
    for (int i = int_count - 1; i >= 0; i--)
        if (int_keys[i] == key) {
            memcpy(result, &int_values[i], 4);
            return 1;
        }
    return -3;
}

// Steam replacing a value with its own, as when it loads its serialized config.
EXPORT void mock_steam_overwrite(int key, int value) {
    for (int i = 0; i < int_count; i++)
        if (int_keys[i] == key)
            int_values[i] = value;
}

EXPORT bool __cdecl SteamAPI_ISteamNetworkingUtils_SetGlobalConfigValueString(void *self, int key, const char *value) {
    if (self != &utils_object)
        return false;
    string_key = key;
    strncpy(string_value, value, sizeof string_value - 1);
    return true;
}

EXPORT void __cdecl SteamAPI_ISteamNetworkingUtils_SetDebugOutputFunction(void *self, int level, DebugOutputFn fn) {
    if (self == &utils_object) {
        debug_level = level;
        debug_output = fn;
    }
}

// Test controls.
EXPORT void mock_init_steam(void) { steam_user = 1; }
EXPORT int mock_int_count(void) { return int_count; }
EXPORT int mock_int_value(int key) {
    for (int i = int_count - 1; i >= 0; i--)
        if (int_keys[i] == key)
            return int_values[i];
    return -12345;
}
EXPORT int mock_string_key(void) { return string_key; }
EXPORT const char *mock_string_value(void) { return string_value; }
EXPORT int mock_debug_level(void) { return debug_level; }
EXPORT void mock_emit_debug(const char *message) {
    if (debug_output)
        debug_output(5, message);
}

// ---------------------------------------------------------------------------------------------------------------
// ISteamNetworking006 object, as the game sees it, plus the flat functions the monitor uses.

#include <stdint.h>

typedef struct {
    uint8_t connection_active, connecting, session_error, using_relay;
    int32_t bytes_queued, packets_queued;
    uint32_t remote_ip;
    uint16_t remote_port;
} P2PSessionState;

static uint64_t sent_to;
static int sent_count;
static int relay_allowed = -1;
static P2PSessionState session;
static int sockets_state, sockets_ping, sockets_flags;

static bool __thiscall net_send(void *self, uint64_t id, const void *data, uint32_t size, int type, int channel) {
    (void)self; (void)data; (void)size; (void)type; (void)channel;
    sent_to = id;
    sent_count++;
    return true;
}
static bool __thiscall net_allow_relay(void *self, bool allow) {
    (void)self;
    relay_allowed = allow;
    return true;
}
static bool __thiscall net_unused(void *self) { (void)self; return false; }
static bool __thiscall net_is_available(void *self, uint32_t *size, int channel) {
    (void)self; (void)size; (void)channel;
    return false;
}
static bool __thiscall net_read(void *self, void *dest, uint32_t capacity, uint32_t *size, uint64_t *from,
    int channel) {
    (void)self; (void)dest; (void)capacity; (void)size; (void)from; (void)channel;
    return false;
}
static bool __thiscall net_session(void *self, uint64_t id) { (void)self; (void)id; return true; }
static bool __thiscall net_get_state(void *self, uint64_t id, P2PSessionState *out);

static void *net_vtable[] = {
    (void *)net_send, (void *)net_is_available, (void *)net_read, (void *)net_session, (void *)net_session,
    (void *)net_unused, (void *)net_get_state, (void *)net_allow_relay,
    // Slots 8-21: the socket API, unused by the game.
    (void *)net_unused, (void *)net_unused, (void *)net_unused, (void *)net_unused, (void *)net_unused,
    (void *)net_unused, (void *)net_unused, (void *)net_unused, (void *)net_unused, (void *)net_unused,
    (void *)net_unused, (void *)net_unused, (void *)net_unused, (void *)net_unused,
};
_Static_assert(sizeof net_vtable / sizeof net_vtable[0] == 22, "ISteamNetworking006 has 22 methods");
static struct { void **vtable; } net_object = { net_vtable };
static int messages_object;

EXPORT void *__cdecl SteamAPI_SteamNetworking_v006(void) { return steam_user ? &net_object : 0; }
EXPORT void *__cdecl SteamAPI_SteamNetworkingMessages_SteamAPI_v002(void) { return steam_user ? &messages_object : 0; }

static bool __thiscall net_get_state(void *self, uint64_t id, P2PSessionState *out) {
    if (self != &net_object || id != sent_to)
        return false;
    *out = session;
    return true;
}

// Like steam_api.dll, the flat function goes through the object's vtable.
EXPORT bool __cdecl SteamAPI_ISteamNetworking_GetP2PSessionState(void *self, uint64_t id, P2PSessionState *out) {
    return ((bool(__thiscall *)(void *, uint64_t, P2PSessionState *))(*(void ***)self)[6])(self, id, out);
}

EXPORT int __cdecl SteamAPI_ISteamNetworkingMessages_GetSessionConnectionInfo(void *self, const int32_t *identity,
    uint8_t *info, int32_t *status) {
    if (self != &messages_object || identity[0] != 16 || identity[1] != 8 || *(const uint64_t *)(identity + 2) != sent_to)
        return 0;
    if (info) {
        *(int32_t *)(info + 440) = sockets_flags;
        strcpy((char *)info + 312, "mock session");
    }
    if (status) {
        status[0] = sockets_state;
        status[1] = sockets_ping;
    }
    return sockets_state;
}

// Test controls: act like the game, and set what Steam reports.
EXPORT bool mock_game_send(uint64_t id) {
    void **vtable = net_object.vtable;
    return ((bool(__thiscall *)(void *, uint64_t, const void *, uint32_t, int, int))vtable[0])(&net_object, id, "x", 1, 0, 0);
}
EXPORT void mock_game_allow_relay(bool allow) {
    ((bool(__thiscall *)(void *, bool))net_object.vtable[7])(&net_object, allow);
}
EXPORT int mock_sent_count(void) { return sent_count; }
EXPORT int mock_relay_allowed(void) { return relay_allowed; }
EXPORT void mock_set_session(int active, int relay, uint32_t ip, uint16_t port) {
    session.connection_active = (uint8_t)active;
    session.using_relay = (uint8_t)relay;
    session.remote_ip = ip;
    session.remote_port = port;
}
EXPORT void mock_set_sockets(int state, int ping, int flags) {
    sockets_state = state;
    sockets_ping = ping;
    sockets_flags = flags;
}

// ---------------------------------------------------------------------------------------------------------------
// ISteamUser and callback dispatch.

#include <stdlib.h>

static int user_object;

EXPORT void *__cdecl SteamAPI_SteamUser_v023(void) { return steam_user ? &user_object : 0; }
EXPORT uint64_t __cdecl SteamAPI_ISteamUser_GetSteamID(void *self) {
    (void)self;
    const char *id = getenv("MOCK_STEAM_ID");
    return id ? strtoull(id, 0, 10) : 76561198000000099ULL;
}

typedef struct { void **vtable; uint8_t flags; int id; } CallbackBase;

static CallbackBase *callbacks[32];
static int callback_ids[32], callback_count;
static uint8_t posted[8][64];
static int posted_ids[8], posted_count;

EXPORT void __cdecl SteamAPI_RegisterCallback(void *callback, int id) {
    if (callback_count < 32) {
        callbacks[callback_count] = callback;
        callback_ids[callback_count++] = id;
        ((CallbackBase *)callback)->flags |= 1;
        ((CallbackBase *)callback)->id = id;
    }
}

// Callbacks can be posted from any thread (BeginAuthSession runs on the transport's network thread).
static SRWLOCK posted_lock = SRWLOCK_INIT;

// Delivers posted callbacks through slot 1, Run(void *param), as steam_api.dll does.
EXPORT void __cdecl SteamAPI_RunCallbacks(void) {
    uint8_t taken[8][64];
    int taken_ids[8], count;
    AcquireSRWLockExclusive(&posted_lock);
    count = posted_count;
    memcpy(taken, posted, sizeof taken);
    memcpy(taken_ids, posted_ids, sizeof taken_ids);
    posted_count = 0;
    ReleaseSRWLockExclusive(&posted_lock);
    for (int p = 0; p < count; p++)
        for (int c = 0; c < callback_count; c++)
            if (callback_ids[c] == taken_ids[p])
                ((void(__thiscall *)(void *, void *))callbacks[c]->vtable[1])(callbacks[c], taken[p]);
}

EXPORT void mock_post_callback(int id, const void *data, int size) {
    AcquireSRWLockExclusive(&posted_lock);
    if (posted_count < 8 && size <= 64) {
        memcpy(posted[posted_count], data, (size_t)size);
        posted_ids[posted_count++] = id;
    }
    ReleaseSRWLockExclusive(&posted_lock);
}

// Session tickets: "MOCKTICKET:<steam id>" (MOCK_BAD_TICKET=1 makes it invalid). BeginAuthSession posts
// ValidateAuthTicketResponse_t (143): response 0 when the ticket names the Steam ID, 5 (invalid) otherwise.
EXPORT uint32_t __cdecl SteamAPI_ISteamUser_GetAuthSessionTicket(void *self, void *ticket, int max, uint32_t *size,
    const void *identity) {
    (void)self; (void)identity;
    char text[64];
    snprintf(text, sizeof text, "MOCKTICKET:%llu", (unsigned long long)SteamAPI_ISteamUser_GetSteamID(0));
    if (getenv("MOCK_BAD_TICKET"))
        strcpy(text, "BADTICKET");
    int len = (int)strlen(text);
    if (len > max)
        return 0;
    memcpy(ticket, text, (size_t)len);
    *size = (uint32_t)len;
    return 1;
}

EXPORT int __cdecl SteamAPI_ISteamUser_BeginAuthSession(void *self, const void *ticket, int size, uint64_t id) {
    (void)self;
    char expected[64];
    snprintf(expected, sizeof expected, "MOCKTICKET:%llu", (unsigned long long)id);
    uint8_t response[24] = {0};
    uint32_t verdict = size == (int)strlen(expected) && !memcmp(ticket, expected, (size_t)size) ? 0 : 5;
    memcpy(response, &id, 8);
    memcpy(response + 8, &verdict, 4);
    memcpy(response + 16, &id, 8);
    mock_post_callback(143, response, sizeof response);
    return 0;
}

EXPORT void __cdecl SteamAPI_ISteamUser_EndAuthSession(void *self, uint64_t id) { (void)self; (void)id; }
EXPORT void __cdecl SteamAPI_ISteamUser_CancelAuthTicket(void *self, uint32_t handle) { (void)self; (void)handle; }

EXPORT void *mock_networking(void) { return &net_object; }

// Puts the original methods back into the vtable, as Proton's steamclient bridge does when an interface is created.
EXPORT void mock_restore_vtable(void) {
    static void *pristine[22];
    static int saved;
    if (!saved) {
        memcpy(pristine, net_vtable, sizeof pristine);
        saved = 1;
        return;
    }
    memcpy(net_vtable, pristine, sizeof pristine);
}

// How the game gets its interfaces.
EXPORT void *__cdecl SteamInternal_FindOrCreateUserInterface(int user, const char *version) {
    if (!user || !version)
        return 0;
    if (!strcmp(version, "SteamNetworking006"))
        return &net_object;
    if (!strcmp(version, "SteamUser023"))
        return &user_object;
    return 0;
}

// ---------------------------------------------------------------------------------------------------------------
// ISteamMatchmaking009: lobby data and member data, shared between processes through files in MOCK_LOBBY_DIR
// (lobby.txt, member_<steam id>.txt). Every process that sets member data counts as a lobby member. Without
// MOCK_LOBBY_DIR the lobby is empty and keeps nothing.



static void *mm_vtable[40];
static struct { void **vtable; } mm_object = { mm_vtable };
static bool __thiscall mm_unused(void *self) { (void)self; return false; }

EXPORT void *__cdecl SteamAPI_SteamMatchmaking_v009(void) {
    for (int i = 0; i < 40; i++)
        if (!mm_vtable[i])
            mm_vtable[i] = (void *)mm_unused;
    return steam_user ? &mm_object : 0;
}

static bool lobby_path(char *out, size_t size, const char *name) {
    const char *dir = getenv("MOCK_LOBBY_DIR");
    if (!dir)
        return false;
    snprintf(out, size, "%s\\%s", dir, name);
    return true;
}

static const char *read_text(const char *name) {
    static char text[4][1024];
    static int next;
    char *out = text[next++ % 4];
    char path[MAX_PATH];
    out[0] = 0;
    if (!lobby_path(path, sizeof path, name))
        return out;
    FILE *file = fopen(path, "rb");
    if (file) {
        size_t n = fread(out, 1, 1023, file);
        out[n] = 0;
        fclose(file);
    }
    return out;
}

static void write_text(const char *name, const char *value) {
    char path[MAX_PATH], temp[MAX_PATH];
    if (!lobby_path(path, sizeof path, name))
        return;
    snprintf(temp, sizeof temp, "%s.tmp", path);
    FILE *file = fopen(temp, "wb");
    if (file) {
        fputs(value, file);
        fclose(file);
        MoveFileExA(temp, path, MOVEFILE_REPLACE_EXISTING);
    }
}

EXPORT const char *__cdecl SteamAPI_ISteamMatchmaking_GetLobbyData(void *self, uint64_t lobby, const char *key) {
    (void)self; (void)lobby;
    return strcmp(key, "isaac_p2p") ? "" : read_text("lobby.txt");
}

EXPORT bool __cdecl SteamAPI_ISteamMatchmaking_SetLobbyData(void *self, uint64_t lobby, const char *key,
    const char *value) {
    (void)self; (void)lobby;
    if (!strcmp(key, "isaac_p2p"))
        write_text("lobby.txt", value);
    return true;
}

static int member_ids(uint64_t *out, int max) {
    char pattern[MAX_PATH];
    if (!lobby_path(pattern, sizeof pattern, "member_*.txt"))
        return 0;
    WIN32_FIND_DATAA found;
    HANDLE find = FindFirstFileA(pattern, &found);
    int count = 0;
    if (find == INVALID_HANDLE_VALUE)
        return 0;
    do {
        if (count < max)
            out[count++] = strtoull(found.cFileName + 7, 0, 10);
    } while (FindNextFileA(find, &found));
    FindClose(find);
    return count;
}

EXPORT int __cdecl SteamAPI_ISteamMatchmaking_GetNumLobbyMembers(void *self, uint64_t lobby) {
    (void)self; (void)lobby;
    uint64_t ids[16];
    return member_ids(ids, 16);
}

EXPORT uint64_t __cdecl SteamAPI_ISteamMatchmaking_GetLobbyMemberByIndex(void *self, uint64_t lobby, int index) {
    (void)self; (void)lobby;
    uint64_t ids[16];
    int count = member_ids(ids, 16);
    return index >= 0 && index < count ? ids[index] : 0;
}

EXPORT const char *__cdecl SteamAPI_ISteamMatchmaking_GetLobbyMemberData(void *self, uint64_t lobby, uint64_t user,
    const char *key) {
    (void)self; (void)lobby;
    char name[64];
    snprintf(name, sizeof name, "member_%llu.txt", (unsigned long long)user);
    return strcmp(key, "isaac_p2p") ? "" : read_text(name);
}

EXPORT void __cdecl SteamAPI_ISteamMatchmaking_SetLobbyMemberData(void *self, uint64_t lobby, const char *key,
    const char *value) {
    (void)self; (void)lobby;
    char name[64];
    snprintf(name, sizeof name, "member_%llu.txt", (unsigned long long)SteamAPI_ISteamUser_GetSteamID(0));
    if (!strcmp(key, "isaac_p2p"))
        write_text(name, value);
}

// The lobby's owner: MOCK_LOBBY_OWNER (a Steam ID), or nobody.
EXPORT uint64_t __cdecl SteamAPI_ISteamMatchmaking_GetLobbyOwner(void *self, uint64_t lobby) {
    (void)self; (void)lobby;
    const char *owner = getenv("MOCK_LOBBY_OWNER");
    return owner ? strtoull(owner, 0, 10) : 0;
}
