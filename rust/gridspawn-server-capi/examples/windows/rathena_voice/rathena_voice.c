// Voice for a game server whose players have no Gridspawn account (for
// example an rAthena map-server), with one session per voice group: a party
// or a guild (docs/platforms/rathena-packets.md). The example admits
// one character into party voice, then switches them to guild voice: a ticket
// for the guild's session, removal from the party's (now empty, so ended),
// then a server mute and unmute.
//
//   set GRIDSPAWN_APP_ID=game-...
//   set GRIDSPAWN_ENVIRONMENT=production   (the key's environment; unset = production)
//   set GRIDSPAWN_CREDENTIAL_FILE=C:\keys\gridspawn-server-key
//   rathena_voice.exe <map-server> <char-id> <character-name> [party-id] [guild-id] [hold-seconds]
//                     [--print-ticket]
//
// The guest's externalPlayerId is "rathena-char-<char-id>": opaque, per
// character (the display name is the character's), never an email. The
// character name is the display name (1-64 characters, shown to players).
// Voice tickets are secrets for that one client: hand them to the game client
// over your own connection; this example prints only their length.
// --print-ticket is for local tests only: it prints the party ticket once, so
// it can be pasted into the example game client (rathena_client.exe of
// gridspawn-rathena-dll), and skips the switch and the mute.

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifdef _WIN32
#include <windows.h>
static void sleep_seconds(unsigned seconds) { Sleep(seconds * 1000u); }
#else
#include <unistd.h>
static void sleep_seconds(unsigned seconds) { sleep(seconds); }
#endif

#include "gridspawn_server.h"

static int fail(const char *what, GsStatus status, GsError *error) {
  fprintf(stderr, "%s failed: status %d", what, (int)status);
  if (error != NULL) {
    const char *code = gs_error_wire_code(error);
    fprintf(stderr, ", %s", gs_error_message(error));
    if (code != NULL) fprintf(stderr, " (code %s)", code);
    gs_error_free(error);
  }
  fputc('\n', stderr);
  return 1;
}

// One voice group (a party or a guild) and its session.
typedef struct VoiceGroup {
  char name[64];      // "party:42", also the voice channel
  char key[128];      // the session's idempotency key
  GsSession *session;
  GsRenewer *renewer; // freeing it ends the session (the lease lapses 45 s later)
} VoiceGroup;

// Puts one participant in the group's channel, or in no channel (channel
// NULL): with no channel, nobody hears them and they hear nobody. 02 has no
// route to remove a participant; this is how a player leaves a group's voice.
static GsStatus set_channel(VoiceGroup *group, const char *participant_id, const char *channel,
                            GsError **error) {
  GsVoicePlayerState state;
  gs_voice_player_state_init(&state);
  const char *channels[1] = {channel};
  state.channels = channel != NULL ? channels : NULL;
  state.channels_len = channel != NULL ? 1 : 0;
  GsVoiceAck ack = {0};
  GsStatus status = gs_voice_set_player_state(group->session, participant_id, &state, &ack, error);
  if (status == GS_STATUS_OK) {
    printf("  %s: %s -> %s (enforced now: %s)\n", group->name, participant_id,
           channel != NULL ? channel : "no channel", ack.enforced ? "yes" : "no");
  }
  return status;
}

// Creates the group's session when its first member asks for voice: lease
// renewed by the library, one "match" room in channels mode.
static GsStatus open_group(GsClient *client, const char *kind, const char *id, VoiceGroup *group,
                           GsError **error) {
  memset(group, 0, sizeof *group);
  snprintf(group->name, sizeof group->name, "%s:%s", kind, id);
  char external_id[96];
  snprintf(external_id, sizeof external_id, "rathena-%s-%s", kind, id);
  // A new key per creation (here a timestamp; a server keeps a generation
  // counter per group): a retried create returns the same session.
  snprintf(group->key, sizeof group->key, "%s-%lld", external_id, (long long)time(NULL));
  GsCreateSession create = {0};
  create.external_session_id = external_id;
  create.idempotency_key = group->key;
  create.max_participants = 64; // a voice room holds 64 players (04 §4.2)
  GsStatus status = gs_session_create(client, &create, &group->session, error);
  if (status != GS_STATUS_OK) return status;
  printf("%s: session %s\n", group->name, gs_session_id(group->session));

  // Renews every 15 s (45 s lease); renewals also keep the voice rules fresh (04 §4.4).
  status = gs_session_spawn_renewer(group->session, &group->renewer, error);
  if (status != GS_STATUS_OK) return status;

  // Players hear the players they share a channel with; every member gets the
  // group's channel when admitted.
  const char *config =
      "{\"rooms\": {\"match\": {\"mode\": \"channels\", \"spatial\": \"client\","
      " \"maxParticipants\": 64, \"maxAudibleSpeakers\": 16}}}";
  GsVoiceAck ack = {0};
  status = gs_voice_configure(group->session, config, &ack, error);
  if (status == GS_STATUS_OK) {
    printf("%s: voice room configured (enforced now: %s)\n", group->name,
           ack.enforced ? "yes" : "no");
  }
  return status;
}

// Ends the group's voice once nobody is left: stop renewing.
static void close_group(VoiceGroup *group) {
  if (group->renewer != NULL) gs_renewer_free(group->renewer);
  if (group->session != NULL) gs_session_free(group->session);
  memset(group, 0, sizeof *group);
}

// Admits the character as a guest, gives them the group's channel and issues a
// voice ticket (120 s, single use). Idempotent per (session, character).
static GsStatus admit(VoiceGroup *group, const char *char_id, const char *character,
                      bool print_ticket, GsParticipant **player, GsError **error) {
  char external_player[64], admit_key[200];
  snprintf(external_player, sizeof external_player, "rathena-char-%s", char_id);
  snprintf(admit_key, sizeof admit_key, "%s-%s", group->key, char_id);
  GsAdmitGuest guest = {0};
  guest.external_player_id = external_player;
  guest.display_name = character;
  guest.idempotency_key = admit_key;
  GsStatus status = gs_admit_guest(group->session, &guest, player, error);
  if (status != GS_STATUS_OK) return status;
  const char *participant = gs_participant_id(*player);
  printf("%s: admitted %s as %s\n", group->name, gs_participant_display_name(*player), participant);

  // The state comes before the ticket: in a configured room, a player with no
  // state hears no one and is heard by no one.
  status = set_channel(group, participant, group->name, error);
  if (status != GS_STATUS_OK) return status;

  char *ticket = NULL;
  uint64_t expires_in_ms = 0;
  status = gs_issue_ticket(*player, GS_MODULE_VOICE, &ticket, &expires_in_ms, error);
  if (status != GS_STATUS_OK) return status;
  printf("%s: voice ticket issued (%zu characters, expires in %llu ms): send it to the client\n",
         group->name, strlen(ticket), (unsigned long long)expires_in_ms);
  if (print_ticket) {
    // Testing only (see the top of the file). Never log tickets in a server.
    printf("TICKET %s\n", ticket);
    fflush(stdout);
  }
  gs_string_free(ticket);
  return GS_STATUS_OK;
}

int main(int argc, char **argv) {
  const char *args[6] = {0};
  int count = 0;
  bool print_ticket = false;
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "--print-ticket") == 0) print_ticket = true;
    else if (count < 6) args[count++] = argv[i];
  }
  if (count < 3) {
    fprintf(stderr,
            "usage: %s <map-server> <char-id> <character-name> [party-id] [guild-id] "
            "[hold-seconds] [--print-ticket]\n",
            argv[0]);
    return 2;
  }
  const char *map_server = args[0];
  const char *char_id = args[1];
  const char *character = args[2];
  const char *party_id = count > 3 ? args[3] : "1";
  const char *guild_id = count > 4 ? args[4] : "1";
  unsigned hold = count > 5 ? (unsigned)strtoul(args[5], NULL, 10) : 0;

  const char *app_id = getenv("GRIDSPAWN_APP_ID");
  const char *key_file = getenv("GRIDSPAWN_CREDENTIAL_FILE");
  const char *environment = getenv("GRIDSPAWN_ENVIRONMENT");
  if (app_id == NULL || key_file == NULL) {
    fprintf(stderr, "set GRIDSPAWN_APP_ID and GRIDSPAWN_CREDENTIAL_FILE\n");
    return 2;
  }
  if (gs_abi_version() != GS_ABI_VERSION) {
    fprintf(stderr, "library ABI %u, header ABI %u\n", (unsigned)gs_abi_version(),
            (unsigned)GS_ABI_VERSION);
    return 2;
  }
  printf("gridspawn-server-capi %s (map-server %s)\n", gs_version(), map_server);

  // 1. Connect (bootstrap checks the app id, the environment and the protocol;
  //    the key is checked by the first authenticated call).
  GsError *error = NULL;
  GsClientOptions options;
  gs_client_options_init(&options);
  options.app_id = app_id;
  GsEnvironment parsed;
  GsStatus status = gs_environment_parse(environment != NULL ? environment : "production",
                                         &parsed, &error);
  if (status != GS_STATUS_OK) return fail("environment", status, error);
  options.environment = (uint32_t)parsed;
  options.sdk_base_url = getenv("GRIDSPAWN_SDK_BASE_URL"); // NULL = production

  GsCredential *credential = NULL;
  status = gs_credential_from_file(key_file, &credential, &error);
  if (status != GS_STATUS_OK) return fail("credential", status, error);
  GsClient *client = NULL;
  status = gs_client_connect(&options, credential, &client, &error); // consumes the credential
  if (status != GS_STATUS_OK) return fail("connect", status, error);

  VoiceGroup party = {0}, guild = {0};
  GsParticipant *in_party = NULL, *in_guild = NULL;
  const char *what = "party voice";

  // 2. The character asks for party voice: the party's session is created
  //    (first member), the character admitted with the party channel, and a
  //    ticket goes to their client.
  status = open_group(client, "party", party_id, &party, &error);
  if (status != GS_STATUS_OK) goto failed;
  status = admit(&party, char_id, character, print_ticket, &in_party, &error);
  if (status != GS_STATUS_OK) goto failed;

  if (!print_ticket) {
    // 3. Switch to guild voice (for example at GvG): a ticket for the guild's
    //    session; the client's gs_rag_attach detaches the party session first.
    //    (Within one session, moving the player to another channel with
    //    set_channel is also possible; this layout keeps one group per session.)
    what = "guild voice";
    status = open_group(client, "guild", guild_id, &guild, &error);
    if (status != GS_STATUS_OK) goto failed;
    status = admit(&guild, char_id, character, false, &in_guild, &error);
    if (status != GS_STATUS_OK) goto failed;

    // 4. Remove them from the party's voice (no channel), and end the party's
    //    session: nobody is left in it.
    what = "leaving party voice";
    status = set_channel(&party, gs_participant_id(in_party), NULL, &error);
    if (status != GS_STATUS_OK) goto failed;
    gs_participant_free(in_party);
    in_party = NULL;
    close_group(&party);
    printf("party:%s: empty, session no longer renewed (the lease lapses in 45 s)\n", party_id);

    // 5. Server mute in the guild's session (stays after reconnects), then lift it.
    what = "server mute";
    const char *participant = gs_participant_id(in_guild);
    status = gs_voice_mute(guild.session, participant, true, NULL, &error);
    if (status != GS_STATUS_OK) goto failed;
    printf("  %s muted by the server\n", participant);
    status = gs_voice_mute(guild.session, participant, false, NULL, &error);
    if (status != GS_STATUS_OK) goto failed;
    printf("  %s unmuted\n", participant);
  }

  if (hold > 0) {
    printf("holding for %u s (the renewers keep the leases)\n", hold);
    sleep_seconds(hold);
  }

  if (in_party != NULL) gs_participant_free(in_party);
  if (in_guild != NULL) gs_participant_free(in_guild);
  close_group(&party);
  close_group(&guild);
  gs_client_free(client);
  printf("done\n");
  return 0;

failed: {
  int code = fail(what, status, error);
  if (in_party != NULL) gs_participant_free(in_party);
  if (in_guild != NULL) gs_participant_free(in_guild);
  close_group(&party);
  close_group(&guild);
  gs_client_free(client);
  return code;
}
}
