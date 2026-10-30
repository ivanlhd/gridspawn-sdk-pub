/*
 * Gridspawn companion for rAthena / Ragexe clients: the game-side DLL.
 * gridspawn_rathena.dll (x86) starts gridspawn-companion.exe (x64) and drives
 * it over two local pipes; voice, sign-in and presence run in the companion.
 * Design: docs/platforms/rathena-companion.md.
 *
 * Rules:
 * - Every function is __cdecl. Strings are UTF-8, NUL-terminated, borrowed
 *   for the call (or, in callbacks, for the callback).
 * - gs_rag_init once, after loading the DLL, never from DllMain; then any
 *   gs_rag_* from any thread. Every call except gs_rag_init and
 *   gs_rag_shutdown returns at once; results arrive through callbacks.
 * - Callbacks run on one thread the DLL owns, one at a time, never on the
 *   caller's thread. Return quickly; calling gs_rag_* from a callback is
 *   allowed (gs_rag_shutdown too).
 * - A failed request is reported with on_error (code, message).
 * - Show players display names only (the names these callbacks deliver);
 *   participant ids are for calls such as gs_rag_voice_set_volume.
 */
#ifndef GRIDSPAWN_RATHENA_H
#define GRIDSPAWN_RATHENA_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32)
#define GS_RAG_CALL __cdecl
#if defined(GS_RAG_BUILDING)
#define GS_RAG_API __declspec(dllexport)
#else
#define GS_RAG_API __declspec(dllimport)
#endif
#else
#define GS_RAG_CALL
#define GS_RAG_API
#endif

/* Status codes. */
#define GS_RAG_OK 0
#define GS_RAG_NOT_INITIALIZED (-1)
#define GS_RAG_INVALID_ARGUMENT (-2)
#define GS_RAG_ALREADY_INITIALIZED (-3)
#define GS_RAG_COMPANION_START_FAILED (-4)
#define GS_RAG_COMPANION_LOST (-5)
#define GS_RAG_TIMEOUT (-6)
#define GS_RAG_INTERNAL (-7)

/* on_voice_state */
#define GS_RAG_VOICE_LEFT 0
#define GS_RAG_VOICE_JOINING 1
#define GS_RAG_VOICE_JOINED 2

/* gs_rag_voice_select_device / on_devices */
#define GS_RAG_DEVICE_INPUT 0
#define GS_RAG_DEVICE_OUTPUT 1

/* gs_rag_voice_set_transmit_mode */
#define GS_RAG_TRANSMIT_VOICE_ACTIVITY 0
#define GS_RAG_TRANSMIT_PUSH_TO_TALK 1

typedef struct GsRagParticipant {
  const char *id;           /* for calls; never show it */
  const char *display_name; /* show this */
} GsRagParticipant;

/* Zero-initialise, set struct_size = sizeof(GsRagConfig). */
typedef struct GsRagConfig {
  uint32_t struct_size;
  const char *app_id;         /* required: the Gridspawn application id */
  const char *environment;    /* "production" (NULL), "staging" or "development" */
  const char *sdk_base_url;   /* NULL = https://sdk.gridspawn.net */
  const char *companion_path; /* NULL = gridspawn-companion.exe next to the DLL */
  const char *log_path;       /* NULL = no log; else the companion appends its log here */
  int32_t show_window;        /* non-zero: show the companion's voice window (gs_rag_show_window) */
  uint32_t init_timeout_ms;   /* 0 = 10000 */
  int32_t test_tone;          /* testing only: a tone instead of the microphone, nothing played */
} GsRagConfig;

/* Zero-initialise, set struct_size = sizeof(GsRagCallbacks); any may be NULL. */
typedef struct GsRagCallbacks {
  uint32_t struct_size;
  void(GS_RAG_CALL *on_ready)(void *user);
  void(GS_RAG_CALL *on_companion_lost)(const char *reason, void *user);
  void(GS_RAG_CALL *on_signed_in)(const char *display_name, void *user);
  void(GS_RAG_CALL *on_signed_out)(void *user);
  /* A single-use assertion (60 s) for your server's challenge (protocol 01 §5). */
  void(GS_RAG_CALL *on_assertion)(uint32_t request_id, const char *assertion, void *user);
  /* The session of the ticket is attached; roster = the voice room's players
     known now (empty until voice is joined; then on_participant follows). */
  void(GS_RAG_CALL *on_attached)(const char *display_name, const GsRagParticipant *roster,
                                 uint32_t roster_len, void *user);
  /* reason: "replaced" (gs_rag_attach with another ticket) or "requested"
     (gs_rag_detach). */
  void(GS_RAG_CALL *on_detached)(const char *reason, void *user);
  void(GS_RAG_CALL *on_voice_state)(int32_t state, void *user);
  void(GS_RAG_CALL *on_participant)(int32_t joined, const char *id, const char *display_name,
                                    void *user);
  void(GS_RAG_CALL *on_speaking)(const char *id, int32_t speaking, void *user);
  void(GS_RAG_CALL *on_local_speaking)(int32_t speaking, void *user);
  /* id NULL = this player (unmuting locally is then refused). */
  void(GS_RAG_CALL *on_server_muted)(const char *id, int32_t muted, void *user);
  /* Once per kind after on_ready, then after each gs_rag_voice_list_devices
     and gs_rag_voice_select_device. selected NULL = the system default. */
  void(GS_RAG_CALL *on_devices)(int32_t kind, const char *const *names, uint32_t len,
                                const char *selected, void *user);
  void(GS_RAG_CALL *on_error)(const char *code, const char *message, void *user);
} GsRagCallbacks;

/* Lifecycle */
GS_RAG_API int32_t GS_RAG_CALL gs_rag_init(const GsRagConfig *config,
                                           const GsRagCallbacks *callbacks, void *user);
GS_RAG_API int32_t GS_RAG_CALL gs_rag_shutdown(uint32_t timeout_ms);
GS_RAG_API const char *GS_RAG_CALL gs_rag_version(void);

/* Gridspawn sign-in (not needed for guests) */
GS_RAG_API int32_t GS_RAG_CALL gs_rag_sign_in(void);
GS_RAG_API int32_t GS_RAG_CALL gs_rag_sign_out(void);
GS_RAG_API int32_t GS_RAG_CALL gs_rag_player_assertion(uint32_t request_id,
                                                       const char *challenge);

/* Session: the ticket your game server issued (protocol 02 §5, 120 s, single use).
   Attaching while a session is attached detaches it first (on_detached). */
GS_RAG_API int32_t GS_RAG_CALL gs_rag_attach(const char *ticket);
/* Leaves voice and detaches the session (on_detached("requested")); nothing
   happens when none is attached. */
GS_RAG_API int32_t GS_RAG_CALL gs_rag_detach(void);

/* Voice */
GS_RAG_API int32_t GS_RAG_CALL gs_rag_voice_join(void);
GS_RAG_API int32_t GS_RAG_CALL gs_rag_voice_leave(void);
GS_RAG_API int32_t GS_RAG_CALL gs_rag_voice_set_muted(int32_t muted);
GS_RAG_API int32_t GS_RAG_CALL gs_rag_voice_set_deafened(int32_t deafened);
GS_RAG_API int32_t GS_RAG_CALL gs_rag_voice_set_transmit_mode(int32_t mode);
GS_RAG_API int32_t GS_RAG_CALL gs_rag_voice_set_transmitting(int32_t pressed);
GS_RAG_API int32_t GS_RAG_CALL gs_rag_voice_set_volume(const char *participant, float volume);
GS_RAG_API int32_t GS_RAG_CALL gs_rag_voice_set_participant_muted(const char *participant,
                                                                  int32_t muted);
GS_RAG_API int32_t GS_RAG_CALL gs_rag_voice_list_devices(void);
GS_RAG_API int32_t GS_RAG_CALL gs_rag_voice_select_device(int32_t kind, const char *name);

/* Presence (signed-in players only) */
GS_RAG_API int32_t GS_RAG_CALL gs_rag_presence_set(const char *details, const char *state);
GS_RAG_API int32_t GS_RAG_CALL gs_rag_presence_clear(void);

/* Companion window (show_window non-zero at init). Its close button only
   hides it: voice and the attached session go on, and end with the game
   (gs_rag_shutdown, or the game process ending). visible non-zero shows it
   again (for example from a game menu or key), 0 hides it. on_error
   "companion/no_window" when show_window was 0 at init;
   "companion/unknown_op" from a companion older than 0.1.1. */
GS_RAG_API int32_t GS_RAG_CALL gs_rag_show_window(int32_t visible);

/* Function pointer types for LoadLibrary / GetProcAddress. */
typedef int32_t(GS_RAG_CALL *gs_rag_init_fn)(const GsRagConfig *, const GsRagCallbacks *, void *);
typedef int32_t(GS_RAG_CALL *gs_rag_shutdown_fn)(uint32_t);
typedef const char *(GS_RAG_CALL *gs_rag_version_fn)(void);
typedef int32_t(GS_RAG_CALL *gs_rag_attach_fn)(const char *);
typedef int32_t(GS_RAG_CALL *gs_rag_void_fn)(void);
typedef int32_t(GS_RAG_CALL *gs_rag_int_fn)(int32_t);

#ifdef __cplusplus
}
#endif

#endif /* GRIDSPAWN_RATHENA_H */
