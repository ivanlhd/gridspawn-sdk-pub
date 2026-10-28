// The game side of the Gridspawn companion, as a 32-bit client would use it:
// load gridspawn_rathena.dll, initialise it with callbacks, attach the ticket
// your map-server sent, join voice, then shut down cleanly.
//
//   set GRIDSPAWN_APP_ID=game-...
//   set GRIDSPAWN_ENVIRONMENT=development
//   rathena_client.exe <ticket> [--seconds N] [--ptt] [--window] [--tone] [--log <file>]
//                      [--next-ticket-file <file>]
//
// gridspawn_rathena.dll and gridspawn-companion.exe sit next to this exe.
// --tone is for tests: a tone instead of the microphone, nothing played.
// --next-ticket-file stands in for the map-server's "switch group" packet:
// when the file appears, its ticket is attached (the previous session is
// detached first and voice is joined again in the new one).

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <windows.h>

#include "gridspawn_rathena.h"

static volatile LONG g_attached = 0;
static volatile LONG g_lost = 0;
static volatile LONG g_detached = 0;

/* A tiny id -> display name map, so speaking lines show names. */
#define MAX_PLAYERS 64
static char g_ids[MAX_PLAYERS][96];
static char g_names[MAX_PLAYERS][80];
static CRITICAL_SECTION g_lock;

static void remember(const char *id, const char *name) {
  EnterCriticalSection(&g_lock);
  for (int i = 0; i < MAX_PLAYERS; i++) {
    if (g_ids[i][0] == '\0' || strcmp(g_ids[i], id) == 0) {
      strncpy(g_ids[i], id, sizeof g_ids[i] - 1);
      strncpy(g_names[i], name, sizeof g_names[i] - 1);
      break;
    }
  }
  LeaveCriticalSection(&g_lock);
}

static const char *name_of(const char *id) {
  static char copy[80];
  strcpy(copy, "a player");
  EnterCriticalSection(&g_lock);
  for (int i = 0; i < MAX_PLAYERS; i++) {
    if (strcmp(g_ids[i], id) == 0) {
      strncpy(copy, g_names[i], sizeof copy - 1);
      break;
    }
  }
  LeaveCriticalSection(&g_lock);
  return copy;
}

static void GS_RAG_CALL on_ready(void *user) {
  (void)user;
  printf("companion ready\n");
}

static void GS_RAG_CALL on_companion_lost(const char *reason, void *user) {
  (void)user;
  printf("companion lost: %s\n", reason);
  InterlockedExchange(&g_lost, 1);
}

static void GS_RAG_CALL on_attached(const char *display_name, const GsRagParticipant *roster,
                                    uint32_t roster_len, void *user) {
  (void)user;
  printf("attached as %s (%u in the roster)\n", display_name, roster_len);
  for (uint32_t i = 0; i < roster_len; i++) remember(roster[i].id, roster[i].display_name);
  InterlockedExchange(&g_attached, 1);
}

static void GS_RAG_CALL on_detached(const char *reason, void *user) {
  (void)user;
  printf("detached: %s\n", reason);
  InterlockedExchange(&g_detached, 1);
}

static void GS_RAG_CALL on_voice_state(int32_t state, void *user) {
  (void)user;
  printf("voice: %s\n", state == GS_RAG_VOICE_JOINED    ? "joined"
                        : state == GS_RAG_VOICE_JOINING ? "joining"
                                                        : "left");
}

static void GS_RAG_CALL on_participant(int32_t joined, const char *id, const char *display_name,
                                       void *user) {
  (void)user;
  if (joined) remember(id, display_name);
  printf("player %s: %s\n", joined ? "joined" : "left", display_name);
}

static void GS_RAG_CALL on_speaking(const char *id, int32_t speaking, void *user) {
  (void)user;
  printf("%s %s speaking\n", name_of(id), speaking ? "started" : "stopped");
}

static void GS_RAG_CALL on_local_speaking(int32_t speaking, void *user) {
  (void)user;
  printf("you %s speaking\n", speaking ? "started" : "stopped");
}

static void GS_RAG_CALL on_server_muted(const char *id, int32_t muted, void *user) {
  (void)user;
  printf("%s %s by the server\n", id == NULL ? "you were" : name_of(id), muted ? "muted" : "unmuted");
}

static void GS_RAG_CALL on_devices(int32_t kind, const char *const *names, uint32_t len,
                                   const char *selected, void *user) {
  (void)user;
  printf("%s devices: %u (selected: %s)\n", kind == GS_RAG_DEVICE_INPUT ? "input" : "output", len,
         selected != NULL ? selected : "system default");
  (void)names;
}

static void GS_RAG_CALL on_error(const char *code, const char *message, void *user) {
  (void)user;
  printf("error %s: %s\n", code, message);
}

#define LOAD(type, name)                                                \
  type name = (type)(void *)GetProcAddress(dll, #name);                 \
  if (name == NULL) {                                                   \
    fprintf(stderr, "gridspawn_rathena.dll has no %s\n", #name);       \
    return 2;                                                           \
  }

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  const char *ticket = NULL;
  const char *log_path = NULL;
  const char *next_ticket_file = NULL;
  int seconds = 20, ptt = 0, window = 0, tone = 0;
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "--seconds") == 0 && i + 1 < argc) seconds = atoi(argv[++i]);
    else if (strcmp(argv[i], "--ptt") == 0) ptt = 1;
    else if (strcmp(argv[i], "--window") == 0) window = 1;
    else if (strcmp(argv[i], "--tone") == 0) tone = 1;
    else if (strcmp(argv[i], "--log") == 0 && i + 1 < argc) log_path = argv[++i];
    else if (strcmp(argv[i], "--next-ticket-file") == 0 && i + 1 < argc) next_ticket_file = argv[++i];
    else ticket = argv[i];
  }
  const char *app_id = getenv("GRIDSPAWN_APP_ID");
  if (ticket == NULL || app_id == NULL) {
    fprintf(stderr, "usage: set GRIDSPAWN_APP_ID, then %s <ticket> [--seconds N] [--ptt] "
                    "[--window] [--tone] [--log file] [--next-ticket-file file]\n", argv[0]);
    return 2;
  }
  InitializeCriticalSection(&g_lock);

  // 1. Load the DLL (a patched client does this from its WARP payload).
  HMODULE dll = LoadLibraryW(L"gridspawn_rathena.dll");
  if (dll == NULL) {
    fprintf(stderr, "could not load gridspawn_rathena.dll (error %lu)\n", GetLastError());
    return 2;
  }
  LOAD(gs_rag_version_fn, gs_rag_version);
  LOAD(gs_rag_init_fn, gs_rag_init);
  LOAD(gs_rag_attach_fn, gs_rag_attach);
  LOAD(gs_rag_void_fn, gs_rag_detach);
  LOAD(gs_rag_void_fn, gs_rag_voice_join);
  LOAD(gs_rag_int_fn, gs_rag_voice_set_transmit_mode);
  LOAD(gs_rag_int_fn, gs_rag_voice_set_transmitting);
  LOAD(gs_rag_void_fn, gs_rag_voice_list_devices);
  LOAD(gs_rag_shutdown_fn, gs_rag_shutdown);
  printf("gridspawn_rathena %s\n", gs_rag_version());

  // 2. Initialise: starts the companion and connects (blocking, bounded).
  GsRagConfig config;
  memset(&config, 0, sizeof config);
  config.struct_size = sizeof config;
  config.app_id = app_id;
  config.environment = getenv("GRIDSPAWN_ENVIRONMENT");
  config.sdk_base_url = getenv("GRIDSPAWN_SDK_BASE_URL");
  config.log_path = log_path;
  config.show_window = window;
  config.test_tone = tone;

  GsRagCallbacks callbacks;
  memset(&callbacks, 0, sizeof callbacks);
  callbacks.struct_size = sizeof callbacks;
  callbacks.on_ready = on_ready;
  callbacks.on_companion_lost = on_companion_lost;
  callbacks.on_attached = on_attached;
  callbacks.on_detached = on_detached;
  callbacks.on_voice_state = on_voice_state;
  callbacks.on_participant = on_participant;
  callbacks.on_speaking = on_speaking;
  callbacks.on_local_speaking = on_local_speaking;
  callbacks.on_server_muted = on_server_muted;
  callbacks.on_devices = on_devices;
  callbacks.on_error = on_error;

  int32_t status = gs_rag_init(&config, &callbacks, NULL);
  if (status != GS_RAG_OK) {
    fprintf(stderr, "gs_rag_init failed: %d\n", (int)status);
    return 1;
  }
  gs_rag_voice_list_devices();

  // 3. Attach the session of the ticket, then join voice.
  gs_rag_attach(ticket);
  for (int waited = 0; !g_attached && !g_lost && waited < 15000; waited += 50) Sleep(50);
  if (!g_attached) {
    fprintf(stderr, "not attached\n");
    gs_rag_shutdown(5000);
    return 1;
  }
  if (ptt) gs_rag_voice_set_transmit_mode(GS_RAG_TRANSMIT_PUSH_TO_TALK);
  gs_rag_voice_join();

  // 4. Stay in voice. With --ptt the "key" is held for 2 s out of every 4.
  for (int t = 0; t < seconds * 10 && !g_lost; t++) {
    if (ptt && t % 20 == 0) gs_rag_voice_set_transmitting((t / 20) % 2 == 0);
    if (next_ticket_file != NULL && t % 5 == 0) {
      FILE *file = fopen(next_ticket_file, "rb");
      if (file != NULL) {
        static char next[4096];
        size_t len = fread(next, 1, sizeof next - 1, file);
        fclose(file);
        next[len] = '\0';
        while (len > 0 && (next[len - 1] == '\n' || next[len - 1] == '\r')) next[--len] = '\0';
        if (len > 0) {
          remove(next_ticket_file);
          printf("switching group\n");
          gs_rag_attach(next);
          next_ticket_file = NULL;
        }
      }
    }
    Sleep(100);
  }

  // 5. Leave voice and detach (as on the server's "voice ended" packet), then
  //    stop the companion.
  InterlockedExchange(&g_detached, 0);
  gs_rag_detach();
  for (int waited = 0; !g_detached && !g_lost && waited < 3000; waited += 50) Sleep(50);
  status = gs_rag_shutdown(5000);
  printf("shutdown: %d\n", (int)status);
  FreeLibrary(dll);
  DeleteCriticalSection(&g_lock);
  return 0;
}
