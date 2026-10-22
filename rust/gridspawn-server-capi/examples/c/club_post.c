/*
 * Post to your game's club from C: the two-call quickstart.
 *
 *   cc -std=c99 club_post.c -I../../include ../../target/release/libgridspawn_server_capi.a \
 *      -lpthread -ldl -lm -lrt -o club_post
 *   GRIDSPAWN_APP_ID=game-… GRIDSPAWN_ENVIRONMENT=development \
 *   GRIDSPAWN_CREDENTIAL_FILE=/path/to/0600-key-file ./club_post patch-notes
 */
#include <stdio.h>
#include <stdlib.h>

#include "gridspawn_server.h"

static int fail(const char *what, GsStatus status, GsError *error) {
  fprintf(stderr, "%s failed: status %d", what, (int)status);
  if (error != NULL) {
    const char *wire = gs_error_wire_code(error);
    fprintf(stderr, ": %s (http %u, code %s)", gs_error_message(error),
            (unsigned)gs_error_http_status(error), wire != NULL ? wire : "-");
    gs_error_free(error);
  }
  fputc('\n', stderr);
  return 1;
}

int main(int argc, char **argv) {
  const char *app_id = getenv("GRIDSPAWN_APP_ID");
  const char *key_file = getenv("GRIDSPAWN_CREDENTIAL_FILE");
  const char *environment = getenv("GRIDSPAWN_ENVIRONMENT");
  const char *feature = argc > 1 ? argv[1] : "patch-notes";
  const char *key = argc > 2 ? argv[2] : "patch-1.2-notes";
  if (app_id == NULL || key_file == NULL) {
    fprintf(stderr, "set GRIDSPAWN_APP_ID and GRIDSPAWN_CREDENTIAL_FILE\n");
    return 2;
  }
  if (gs_abi_version() != GS_ABI_VERSION) {
    fprintf(stderr, "library ABI %u, header ABI %u\n", (unsigned)gs_abi_version(),
            (unsigned)GS_ABI_VERSION);
    return 2;
  }

  GsError *error = NULL;
  GsClientOptions options;
  gs_client_options_init(&options);
  options.app_id = app_id;
  GsEnvironment parsed;
  GsStatus status = gs_environment_parse(environment != NULL ? environment : "development",
                                         &parsed, &error);
  if (status != GS_STATUS_OK) return fail("environment", status, error);
  options.environment = (uint32_t)parsed;
  options.sdk_base_url = getenv("GRIDSPAWN_SDK_BASE_URL");     // NULL = production
  options.club_posts_url = getenv("GRIDSPAWN_CLUB_POSTS_URL"); // NULL = production

  GsCredential *credential = NULL;
  status = gs_credential_from_file(key_file, &credential, &error);
  if (status != GS_STATUS_OK) return fail("credential", status, error);

  GsClient *client = NULL;
  status = gs_client_connect(&options, credential, &client, &error); // consumes credential
  if (status != GS_STATUS_OK) return fail("connect", status, error);

  GsPost post = {.title = "Patch 1.2 is out", .content = "Fixes and a new map."};
  char *id = NULL;
  bool duplicate = false;
  status = gs_post(client, feature, &post, key, &id, &duplicate, &error);
  if (status != GS_STATUS_OK) {
    int code = fail("post", status, error);
    gs_client_free(client);
    return code;
  }
  printf("posted %s (duplicate: %s)\n", id, duplicate ? "true" : "false");
  gs_string_free(id);
  gs_client_free(client);
  return 0;
}
