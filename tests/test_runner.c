// Loads mock steam_api.dll and the proxy winmm.dll from its own directory and checks one scenario:
//   test_runner.exe <default|custom|norelay|disabled|nosteam>
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdio.h>
#include <string.h>

#define NO_VALUE -12345
static int failures;

#define CHECK(cond, ...) do { if (!(cond)) { failures++; printf("FAIL %s:%d: ", __FILE__, __LINE__); \
    printf(__VA_ARGS__); printf("\n"); } } while (0)

typedef void (*VoidFn)(void);
typedef int (*IntFn)(void);
typedef int (*IntIntFn)(int);
typedef const char *(*StrFn)(void);
typedef void (*EmitFn)(const char *);
typedef UINT(WINAPI *PeriodFn)(UINT);
typedef int (*SendFn)(unsigned long long);
typedef void (*AllowFn)(int);
typedef void (*SessionFn)(int, int, unsigned, unsigned short);
typedef void (*SocketsFn)(int, int, int);
typedef void (*OverwriteFn)(int, int);

static char dir[MAX_PATH];

static HMODULE load(const char *name) {
    char path[MAX_PATH];
    snprintf(path, sizeof path, "%s%s", dir, name);
    HMODULE module = LoadLibraryA(path);
    if (!module) {
        printf("FAIL: cannot load %s (%lu)\n", path, GetLastError());
        ExitProcess(2);
    }
    return module;
}

static int wait_for(IntFn count, int expected, int timeout_ms) {
    for (int waited = 0; waited < timeout_ms; waited += 20, Sleep(20))
        if (count() >= expected)
            return 1;
    return count() >= expected;
}

static int log_contains(const char *needle) {
    char path[MAX_PATH], buffer[16384] = {0};
    snprintf(path, sizeof path, "%sisaac_p2p.log", dir);
    FILE *file = fopen(path, "rb");
    if (!file)
        return 0;
    fread(buffer, 1, sizeof buffer - 1, file);
    fclose(file);
    return strstr(buffer, needle) != NULL;
}

static int wait_log(const char *needle, int timeout_ms) {
    for (int waited = 0; waited < timeout_ms; waited += 50, Sleep(50))
        if (log_contains(needle))
            return 1;
    return log_contains(needle);
}

int main(int argc, char **argv) {
    const char *scenario = argc > 1 ? argv[1] : "default";
    GetModuleFileNameA(NULL, dir, MAX_PATH);
    strrchr(dir, '\\')[1] = '\0';

    HMODULE steam = load("steam_api.dll");
    VoidFn init_steam = (VoidFn)GetProcAddress(steam, "mock_init_steam");
    IntFn int_count = (IntFn)GetProcAddress(steam, "mock_int_count");
    IntIntFn int_value = (IntIntFn)GetProcAddress(steam, "mock_int_value");
    IntFn string_key = (IntFn)GetProcAddress(steam, "mock_string_key");
    StrFn string_value = (StrFn)GetProcAddress(steam, "mock_string_value");
    IntFn debug_level = (IntFn)GetProcAddress(steam, "mock_debug_level");
    EmitFn emit_debug = (EmitFn)GetProcAddress(steam, "mock_emit_debug");

    SendFn game_send = (SendFn)GetProcAddress(steam, "mock_game_send");
    AllowFn game_allow_relay = (AllowFn)GetProcAddress(steam, "mock_game_allow_relay");
    IntFn sent_count = (IntFn)GetProcAddress(steam, "mock_sent_count");
    IntFn relay_allowed = (IntFn)GetProcAddress(steam, "mock_relay_allowed");
    SessionFn set_session = (SessionFn)GetProcAddress(steam, "mock_set_session");
    SocketsFn set_sockets = (SocketsFn)GetProcAddress(steam, "mock_set_sockets");

    OverwriteFn steam_overwrite = (OverwriteFn)GetProcAddress(steam, "mock_steam_overwrite");

    VoidFn restore_vtable = (VoidFn)GetProcAddress(steam, "mock_restore_vtable");
    restore_vtable(); // remember the original vtable

    HMODULE winmm = load("winmm.dll");
    PeriodFn begin = (PeriodFn)GetProcAddress(winmm, "timeBeginPeriod");
    PeriodFn end = (PeriodFn)GetProcAddress(winmm, "timeEndPeriod");
    CHECK(begin && end && GetProcAddress(winmm, "timeGetDevCaps"), "winmm exports missing");
    CHECK(begin && begin(1) == 0, "timeBeginPeriod not forwarded");
    CHECK(end && end(1) == 0, "timeEndPeriod not forwarded");

    // Steam comes up after the DLL, as in the game.
    Sleep(300);
    CHECK(int_count() == 0, "config set before Steam was initialised");
    if (strcmp(scenario, "nosteam") != 0)
        init_steam();

    if (!strcmp(scenario, "default")) {
        CHECK(wait_for(int_count, 4, 3000), "expected 4 int settings, got %d", int_count());
        CHECK(int_value(104) == 6, "ICE_Enable = %d", int_value(104));
        CHECK(int_value(105) == 0, "ICE_Penalty = %d", int_value(105));
        CHECK(int_value(106) == 20, "SDR_Penalty = %d", int_value(106));
        CHECK(int_value(17) == 5, "LogLevel_P2PRendezvous = %d", int_value(17));
        CHECK(string_key() == 0, "STUN list set without stun_servers");
        CHECK(debug_level() == 5, "debug output not hooked");
        emit_debug("route selected: ICE");
        CHECK(log_contains("steam: route selected: ICE"), "steam debug output not logged");
        CHECK(log_contains("P2P_Transport_ICE_Enable = 6: ok"), "settings not logged");

        // The game talks to a peer through the hooked vtable; the monitor reports the route as it changes.
        CHECK(wait_log("SendP2PPacket ok, AllowP2PPacketRelay ok", 5000), "hooks not installed");
        game_allow_relay(1);
        CHECK(relay_allowed() == 1, "AllowP2PPacketRelay not passed through");
        CHECK(log_contains("game called AllowP2PPacketRelay(true)"), "AllowP2PPacketRelay not logged");
        set_session(1, 1, 0, 0);
        CHECK(game_send(76561198000000001ULL) && sent_count() == 1, "SendP2PPacket not passed through");
        game_send(76561198000000001ULL);
        CHECK(log_contains("peer 76561198000000001: game opened a session"), "peer not recorded");
        CHECK(wait_log("peer 76561198000000001: RELAY", 5000), "relay route not logged");
        set_session(1, 0, 0x5DB8D822, 27015);
        set_sockets(3, 42, 0);
        CHECK(wait_log("peer 76561198000000001: DIRECT 93.184.216.34:27015 ping 42 ms (sockets: direct; mock session)", 5000),
            "direct route not logged");

        // Proton rebuilds the vtable: the hooks must come back, and packets must flow through them again.
        restore_vtable();
        CHECK(wait_log("hooks applied again (the vtable was restored)", 5000), "restored vtable not re-hooked");
        int sent_before = sent_count();
        game_send(76561198000000001ULL);
        CHECK(sent_count() == sent_before + 1, "SendP2PPacket lost after re-hooking");

        steam_overwrite(104, -1);
        CHECK(wait_log("P2P_Transport_ICE_Enable is -1 (result 1), expected 6; setting again", 5000),
            "overwritten value not detected");
        CHECK(int_value(104) == 6, "ICE_Enable not restored: %d", int_value(104));
    } else if (!strcmp(scenario, "custom")) {
        CHECK(wait_for(int_count, 3, 3000), "expected 3 int settings, got %d", int_count());
        Sleep(200);
        CHECK(int_count() == 3, "log disabled but got %d settings", int_count());
        CHECK(int_value(104) == 3, "ICE_Enable = %d", int_value(104));
        CHECK(int_value(105) == 5, "ICE_Penalty = %d", int_value(105));
        CHECK(int_value(106) == 50, "SDR_Penalty = %d", int_value(106));
        CHECK(int_value(17) == NO_VALUE, "LogLevel set with log=0");
        CHECK(debug_level() == 0, "debug output hooked with log=0");
        CHECK(string_key() == 103 && !strcmp(string_value(), "stun:stun.example.org:3478"),
            "STUN list = %d '%s'", string_key(), string_value());
    } else if (!strcmp(scenario, "norelay")) {
        CHECK(wait_for(relay_allowed, 0, 3000) && relay_allowed() == 0, "relay not disabled at start");
        game_allow_relay(1);
        CHECK(relay_allowed() == 0, "game re-enabled the relay");
        CHECK(log_contains("AllowP2PPacketRelay(true), forced to false"), "forced relay not logged");
    } else if (!strcmp(scenario, "disabled") || !strcmp(scenario, "nosteam")) {
        Sleep(800);
        CHECK(int_count() == 0, "settings applied: %d", int_count());
    } else {
        printf("unknown scenario %s\n", scenario);
        return 2;
    }

    printf("%s: %s\n", scenario, failures ? "FAILED" : "ok");
    return failures ? 1 : 0;
}
