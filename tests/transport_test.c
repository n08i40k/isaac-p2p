// Two copies of this program, a host and a client, talk through winmm.dll's UDP transport:
//   transport_test.exe host | client                 correctness test (reliable + unreliable exchange)
//   transport_test.exe host idle <s> | client reconnect  the host serves its lobby for <s> seconds and exits
//                                                    without a goodbye; the client must reconnect once a host is
//                                                    back on the port
//   transport_test.exe client lobby                  the client knows no IP: it enters the host's lobby (shared
//                                                    through MOCK_LOBBY_DIR), connects through the host's offer,
//                                                    then runs the correctness test with a plain host
//                                                    ("client lobby bench [s]": the benchmark instead)
//   transport_test.exe client refused                the client's password is wrong: it must give up without
//                                                    being asked to join a lobby
//   transport_test.exe host bench | client bench [s] latency benchmark: the client sends 60 Hz unreliable
//                                                    pings and a 4 KB reliable message every 250 ms, the host
//                                                    echoes both; the client prints RTT percentiles and loss
// Each acts like the game: it registers a GameLobbyJoinRequested_t handler, pumps SteamAPI_RunCallbacks through
// its import table, and sends / reads packets through the ISteamNetworking vtable.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

__declspec(dllimport) void __cdecl SteamAPI_RunCallbacks(void);
__declspec(dllimport) void __cdecl SteamAPI_RegisterCallback(void *, int);
__declspec(dllimport) void mock_init_steam(void);
__declspec(dllimport) void mock_post_callback(int, const void *, int);
__declspec(dllimport) void *__cdecl SteamInternal_FindOrCreateUserInterface(int, const char *);
__declspec(dllimport) int __cdecl SteamAPI_GetHSteamUser(void);

#define HOST_ID 76561198000000001ULL
#define CLIENT_ID 76561198000000002ULL
#define LOBBY_ID 109775240000000001ULL
#define BIG 50000

static int failures;
#define CHECK(cond, ...) do { if (!(cond)) { failures++; printf("FAIL %s:%d: ", __FILE__, __LINE__); \
    printf(__VA_ARGS__); printf("\n"); } } while (0)

// The game's GameLobbyJoinRequested_t handler.
typedef struct { void **vtable; uint8_t flags; int id; } CallbackBase;
static uint64_t join_lobby, join_friend;
static void __thiscall join_run(void *self, void *param) {
    (void)self;
    memcpy(&join_lobby, param, 8);
    memcpy(&join_friend, (uint8_t *)param + 8, 8);
}
static void __thiscall join_run3(void *self, void *param, bool failure, uint64_t call) {
    (void)failure; (void)call;
    join_run(self, param);
}
static int __thiscall join_size(void *self) { (void)self; return 16; }
static void *join_vtable[] = {(void *)join_run3, (void *)join_run, (void *)join_size};
static CallbackBase join_handler = {join_vtable, 0, 0};

typedef bool(__thiscall *SendFn)(void *, uint64_t, const void *, uint32_t, int, int);
typedef bool(__thiscall *AvailableFn)(void *, uint32_t *, int);
typedef bool(__thiscall *ReadFn)(void *, void *, uint32_t, uint32_t *, uint64_t *, int);

static void *net;
static bool send_packet(uint64_t to, const void *data, uint32_t size, int type, int channel) {
    return ((SendFn)(*(void ***)net)[0])(net, to, data, size, type, channel);
}
static bool read_packet(void *dest, uint32_t capacity, uint32_t *size, uint64_t *from, int channel) {
    uint32_t available = 0;
    if (!((AvailableFn)(*(void ***)net)[1])(net, &available, channel))
        return false;
    return ((ReadFn)(*(void ***)net)[2])(net, dest, capacity, size, from, channel);
}

static uint8_t big[BIG], buffer[BIG + 16];

static void fill(uint8_t seed) {
    for (int i = 0; i < BIG; i++)
        big[i] = (uint8_t)(i * 31 + seed);
}

// Receives `count` reliable messages on channel 1 (each `big` filled with seed + index) and one marker on
// channel 2; checks order, contents and sender.
static void receive_all(uint64_t sender, uint8_t seed, int count, int timeout_ms) {
    int received = 0, unreliable = 0;
    for (int waited = 0; waited < timeout_ms && received < count; waited += 5) {
        SteamAPI_RunCallbacks();
        uint32_t size;
        uint64_t from;
        while (read_packet(buffer, sizeof buffer, &size, &from, 1)) {
            fill((uint8_t)(seed + received));
            CHECK(from == sender, "message from %llu", (unsigned long long)from);
            CHECK(size == BIG && !memcmp(buffer, big, BIG), "reliable message %d corrupted (size %u)", received, size);
            received++;
        }
        while (read_packet(buffer, sizeof buffer, &size, &from, 2))
            unreliable++;
        Sleep(5);
    }
    CHECK(received == count, "got %d of %d reliable messages", received, count);
    printf("  received %d reliable, %d unreliable\n", received, unreliable);
}

static void send_all(uint64_t to, uint8_t seed, int count) {
    for (int i = 0; i < count; i++) {
        fill((uint8_t)(seed + i));
        CHECK(send_packet(to, big, BIG, 2, 1), "reliable send %d refused", i);
        char ping[16] = "unreliable";
        send_packet(to, ping, sizeof ping, 0, 2);
        SteamAPI_RunCallbacks();
        Sleep(10);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Benchmark.

#define BENCH_MAX 100000
#define REL_SIZE 4096
static double unreliable_rtt[BENCH_MAX], reliable_rtt[BENCH_MAX];
static LARGE_INTEGER frequency;

static double now_ms(void) {
    LARGE_INTEGER t;
    QueryPerformanceCounter(&t);
    return (double)t.QuadPart * 1000.0 / (double)frequency.QuadPart;
}

static int compare(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}

static void report(const char *name, double *rtt, int count, int sent) {
    qsort(rtt, (size_t)count, sizeof *rtt, compare);
    if (!count) {
        printf("  %-10s sent %d, none came back\n", name, sent);
        return;
    }
    printf("  %-10s sent %5d, back %5d (loss %.1f%%), rtt ms: min %.1f p50 %.1f p90 %.1f p99 %.1f max %.1f\n",
        name, sent, count, 100.0 * (sent - count) / sent, rtt[0], rtt[count / 2], rtt[count * 9 / 10],
        rtt[count * 99 / 100], rtt[count - 1]);
}

// Echoes channels 3 (unreliable) and 4 (reliable) until the client has been quiet for 5 s.
static void bench_host(void) {
    double last = now_ms() + 60000; // wait up to a minute for the client
    while (now_ms() - last < 5000) {
        SteamAPI_RunCallbacks();
        uint32_t size;
        uint64_t from;
        while (read_packet(buffer, sizeof buffer, &size, &from, 3)) {
            send_packet(from, buffer, size, 0, 3);
            last = now_ms();
        }
        while (read_packet(buffer, sizeof buffer, &size, &from, 4)) {
            send_packet(from, buffer, size, 2, 4);
            last = now_ms();
        }
        Sleep(1);
    }
    printf("  bench host done\n");
}

static void bench_client(int seconds) {
    int sent_u = 0, sent_r = 0, back_u = 0, back_r = 0;
    double start = now_ms(), next_u = start, next_r = start, end = start + seconds * 1000.0;
    uint8_t message[REL_SIZE];
    memset(message, 0x5A, sizeof message);
    while (now_ms() < end + 2000) { // 2 s extra for stragglers
        double t = now_ms();
        if (t < end && t >= next_u && sent_u < BENCH_MAX) {
            send_packet(HOST_ID, &t, sizeof t, 0, 3);
            sent_u++;
            next_u += 1000.0 / 60;
        }
        if (t < end && t >= next_r && sent_r < BENCH_MAX) {
            memcpy(message, &t, sizeof t);
            CHECK(send_packet(HOST_ID, message, sizeof message, 2, 4), "reliable send refused");
            sent_r++;
            next_r += 250;
        }
        SteamAPI_RunCallbacks();
        uint32_t size;
        uint64_t from;
        double sent_at;
        while (read_packet(buffer, sizeof buffer, &size, &from, 3))
            if (size == sizeof sent_at && back_u < BENCH_MAX) {
                memcpy(&sent_at, buffer, sizeof sent_at);
                unreliable_rtt[back_u++] = now_ms() - sent_at;
            }
        while (read_packet(buffer, sizeof buffer, &size, &from, 4))
            if (size == REL_SIZE && back_r < BENCH_MAX) {
                memcpy(&sent_at, buffer, sizeof sent_at);
                reliable_rtt[back_r++] = now_ms() - sent_at;
            }
        Sleep(1);
    }
    printf("  bench %d s:\n", seconds);
    report("unreliable", unreliable_rtt, back_u, sent_u);
    report("reliable", reliable_rtt, back_r, sent_r);
    CHECK(back_r == sent_r, "%d of %d reliable messages came back", back_r, sent_r);
}

static bool log_has(const char *needle) {
    char path[MAX_PATH], text[65536] = {0};
    GetModuleFileNameA(NULL, path, MAX_PATH);
    strcpy(strrchr(path, '\\') + 1, "isaac_p2p.log");
    FILE *file = fopen(path, "rb");
    if (!file)
        return false;
    fread(text, 1, sizeof text - 1, file);
    fclose(file);
    return strstr(text, needle) != NULL;
}

int main(int argc, char **argv) {
    bool host = argc > 1 && !strcmp(argv[1], "host");
    bool bench = argc > 2 && !strcmp(argv[2], "bench");
    QueryPerformanceFrequency(&frequency);
    char dir[MAX_PATH], path[MAX_PATH];
    GetModuleFileNameA(NULL, dir, MAX_PATH);
    strrchr(dir, '\\')[1] = '\0';
    snprintf(path, sizeof path, "%swinmm.dll", dir);
    if (!LoadLibraryA(path)) {
        printf("FAIL: cannot load winmm.dll\n");
        return 2;
    }
    SteamAPI_RegisterCallback(&join_handler, 333);
    mock_init_steam();
    net = SteamInternal_FindOrCreateUserInterface(SteamAPI_GetHSteamUser(), "SteamNetworking006");

    if (host) {
        // The game creates a lobby: Steam posts LobbyEnter_t.
        uint8_t lobby_enter[24] = {0};
        uint64_t lobby = LOBBY_ID;
        uint32_t success = 1; // EChatRoomEnterResponse at +16
        memcpy(lobby_enter, &lobby, 8);
        memcpy(lobby_enter + 16, &success, 4);
        for (int i = 0; i < 40; i++, Sleep(25))
            SteamAPI_RunCallbacks();
        mock_post_callback(504, lobby_enter, sizeof lobby_enter);
        SteamAPI_RunCallbacks();

        if (argc > 3 && !strcmp(argv[2], "idle")) {
            for (int i = 0, end = atoi(argv[3]) * 100; i < end; i++, Sleep(10))
                SteamAPI_RunCallbacks();
            printf("transport host (idle): ok\n");
            return 0;
        }
        if (bench) {
            bench_host();
            printf("%s: %s\n", "transport host", failures ? "FAILED" : "ok");
            return failures ? 1 : 0;
        }
        // Wait for the client's first message, then exchange data both ways.
        receive_all(CLIENT_ID, 100, 20, 30000);
        send_all(CLIENT_ID, 200, 20);
        for (int i = 0; i < 600; i++, Sleep(10))
            SteamAPI_RunCallbacks(); // keep acking until the client is done
        CHECK(join_lobby == 0, "host was asked to join a lobby");
    } else {
        if (argc > 2 && !strcmp(argv[2], "lobby")) {
            uint8_t lobby_enter[24] = {0};
            uint64_t lobby = LOBBY_ID;
            uint32_t success = 1;
            memcpy(lobby_enter, &lobby, 8);
            memcpy(lobby_enter + 16, &success, 4);
            for (int i = 0; i < 40; i++, Sleep(25))
                SteamAPI_RunCallbacks();
            mock_post_callback(504, lobby_enter, sizeof lobby_enter);
            bool connected = false;
            for (int waited = 0; waited < 20000 && !connected; waited += 10, Sleep(10)) {
                SteamAPI_RunCallbacks();
                connected = log_has("connected to host");
            }
            CHECK(connected, "client did not connect through the lobby");
            CHECK(join_lobby == 0, "client already in the lobby was asked to join it");
            if (connected && argc > 3 && !strcmp(argv[3], "bench")) {
                bench_client(argc > 4 ? atoi(argv[4]) : 30);
                printf("transport client (lobby): %s\n", failures ? "FAILED" : "ok");
                return failures ? 1 : 0;
            }
            send_all(HOST_ID, 100, 20);
            receive_all(HOST_ID, 200, 20, 30000);
            printf("transport client (lobby): %s\n", failures ? "FAILED" : "ok");
            return failures ? 1 : 0;
        }
        if (argc > 2 && !strcmp(argv[2], "refused")) {
            bool gave_up = false;
            for (int waited = 0; waited < 15000 && !gave_up; waited += 50, Sleep(50)) {
                SteamAPI_RunCallbacks();
                gave_up = log_has("no answer from");
            }
            CHECK(gave_up, "client did not give up");
            CHECK(join_lobby == 0, "client was asked to join lobby %llu", (unsigned long long)join_lobby);
            CHECK(!log_has("connected to host"), "client connected with a wrong password");
            printf("transport client (wrong password): %s\n", failures ? "FAILED" : "ok");
            return failures ? 1 : 0;
        }
        for (int waited = 0; waited < 15000 && !join_lobby; waited += 10, Sleep(10))
            SteamAPI_RunCallbacks();
        CHECK(join_lobby == LOBBY_ID && join_friend == HOST_ID, "join request lobby %llu friend %llu",
            (unsigned long long)join_lobby, (unsigned long long)join_friend);
        if (argc > 2 && !strcmp(argv[2], "reconnect")) {
            bool back = false;
            for (int waited = 0; waited < 40000 && !back; waited += 50, Sleep(50)) {
                SteamAPI_RunCallbacks();
                back = log_has("reconnected to host");
            }
            CHECK(back, "client did not reconnect");
            printf("transport client (reconnect): %s\n", failures ? "FAILED" : "ok");
            return failures ? 1 : 0;
        }
        if (bench) {
            bench_client(argc > 3 ? atoi(argv[3]) : 30);
            printf("%s: %s\n", "transport client", failures ? "FAILED" : "ok");
            return failures ? 1 : 0;
        }
        send_all(HOST_ID, 100, 20);
        receive_all(HOST_ID, 200, 20, 30000);
    }
    printf("%s: %s\n", host ? "transport host" : "transport client", failures ? "FAILED" : "ok");
    return failures ? 1 : 0;
}
