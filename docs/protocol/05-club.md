# 05 Club posts from a game server — **Current**

Deployed 2026-10-21/22 (UTC). Live checks, 2026-10-22:

- A game connected from the Dev Hub posted to its club's chat from its own
  Rust game server through `gridspawn-server` (development environment). A
  repeat with the same `Idempotency-Key` answered as a duplicate and did not
  post again.
- An application without a linked game got club grants. Its posts answered
  `404 feature_not_found`, as the table below says.

Sources are cited so the document can be checked against the code.

A game's own server posts news and chat messages into the game's **club**, shown
as the game. The server trades its server credential for a short **club grant**
at the SDK edge, then posts to Core with that grant. Core checks the grant,
finds where the post goes, and delivers it the way club webhooks do
(`docs/club_webhooks.md`).

```
game server                    SDK edge (sdk.gridspawn.net)          Core (presence-gateway …/core)
    | POST /v1/server/grants/club   |                                       |
    | GridspawnServer <credential>  |                                       |
    |------------------------------>|                                       |
    |      club grant (≤ 300 s)     |                                       |
    |<------------------------------|                                       |
    | POST /core/v1/game-servers/posts  Bearer <grant>  {feature, …}        |
    |---------------------------------------------------------------------->|
    |                         {"ok": true, "id": "…"}                       |
    |<----------------------------------------------------------------------|
```

Sources: edge `sdk-api-edge-rs/src/server_grants.rs`,
`sdk-api-edge-rs/src/http/server_grants.rs`; Core `gridspawn-core-rs/src/sdk_grants.rs`,
`gridspawn-core-rs/src/game_servers.rs`, `gridspawn-core-rs/src/http/game_server_routes.rs`;
chat `chat-gateway-rs/src/webhooks.rs`. Design and deploy:
`docs/game-server-integration.md`.

## Setup (Dev Hub, no API)

1. The game's owner opens **Dev Hub → Game servers** and connects the game.
   Core creates the game's SDK application, `game-<game id>` (or `game-` plus 24
   hex characters of the id's SHA-256 when the id does not fit
   `^[A-Za-z0-9_-]{3,128}$`), with `production` turned on.
2. The owner makes a **server key** for an environment (`production`, `staging`
   or `development`). It is shown once, with the App ID. At most 2 keys per
   environment are active, so a server can move to a new key before the old one
   is revoked. Keys carry the scopes `club.post` and `session.manage` (02).
3. The club's owner, or a member with **Manage Integrations**, chooses where each
   **feature** goes: a feature is a name the server sends with its posts
   (`patch-notes`, `match.results`; `^[a-z0-9][a-z0-9_.-]{0,47}$`), and its
   destination is one announcement board or one chat of the game's club. At most
   25 features per game.
4. Production servers post while **Game server can post to my club** is on.
   Development and staging servers post only while **Let development servers
   post (for testing)** is on (off by default).

The posting host is not in bootstrap (README §Bootstrap); servers configure it.

## 1. Get a club grant (SDK edge)

```
POST https://sdk.gridspawn.net/v1/server/grants/club
Authorization: GridspawnServer gsc1.<credential id>.<secret>
```

The body is ignored (send `{}` or nothing). No `Origin` header (02 §Server
authentication). Response `200`:

```json
{ "accessToken": "…", "tokenType": "Bearer", "expiresIn": 300, "scope": "club.post" }
```

- The edge keeps the last grant per credential and returns it again while more
  than 60 s remain, so `expiresIn` can be anywhere from 61 to 300
  (`SERVER_GRANT_REUSE_MIN_REMAINING`). Ask for a new grant when under 60 s
  remain; a busy server signs about once every 4 minutes.
- If the edge's Redis is down, every call signs a new grant (nothing is
  refused for that).

Errors (edge envelope, README §Conventions):

| Status | `error.code` | When |
|---|---|---|
| 401 | `server/authentication_required` | Malformed, unknown, revoked or expired credential, wrong secret, or the application/environment is turned off |
| 403 | `server/scope_denied` | The credential lacks `club.post` |
| 403 | `server/browser_forbidden` | The request has an `Origin` header |
| 503 | `service/not_ready` | Storage or signing unavailable |

Sources: `http/sessions.rs` (`server`, `server_auth_error`), `server_auth.rs`
(`authenticate_server`).

### The grant

An RS256 JWT signed with the module-grant keyset (README §Hosts,
`/.well-known/sdk-module-jwks.json`). Treat it as opaque; for reference, its
claims are snake_case like module grants (`ServerGrantClaims`):

```json
{ "iss": "https://sdk.gridspawn.net", "aud": "gridspawn-club", "sub": "<credential id>",
  "app_id": "game-…", "environment": "production", "scope": "club.post",
  "token_class": "server_grant", "iat": 0, "nbf": 0, "exp": 300, "jti": "…" }
```

Core accepts it only with that audience, issuer and token class, `club.post` in
`scope`, a lifetime of 1–305 s, `nbf` within 5 s of `iat`, a valid `app_id` and
environment, and a key from the JWKS (`sdk_grants.rs`, `check_claims`). Module
grants (02) are refused, and the edge's module-grant check refuses club grants,
so neither can stand in for the other.

## 2. Post (Core)

```
POST https://presence-gateway.gridspawn.net/core/v1/game-servers/posts
Authorization: Bearer <accessToken>
Content-Type: application/json
Idempotency-Key: match-8812-start          (optional, recommended)

{ "feature": "match.results", "title": "Match won", "content": "Arena · 8 players",
  "link": { "url": "https://…", "label": "Watch" } }
```

- **`feature`** is required. The rest is the club webhook body
  (`docs/club_webhooks.md` §Posting): `title`, `content` or `text`, `summary`,
  `link {url, label}`, or the Discord shape with `embeds[0]`.
- **Board posts:** the title comes from `title`, else `embeds[0].title`, else
  the first line of the text. Limits: title 160 characters, body 8000,
  `summary` 400. The post is written with the game's name as `author` and
  `source: "game_server"`. The app shows "<game> · via Game server".
- **Chat messages:** at most 2000 characters. The author is `game:<app id>`,
  shown with the game's name, and the origin is `{"platform": "game_server"}`.
  The app shows "via Game server" and no profile menu. They go through the
  same chat-gateway route as club webhook messages
  (`chat-gateway-rs/src/webhooks.rs`, `authorKind: "game_server"`).
- **Response `200`:** `{"ok": true, "id": "…"}`. A retry with the same
  `Idempotency-Key` answers `{"ok": true, "id": "<first id>", "duplicate": true}`
  and posts nothing.
- **Idempotency-Key:** 1–128 printable ASCII characters, no spaces. Its scope is
  the application, the feature and the key: the same key under another feature
  is a new post, and moving a feature to another destination does not re-post a
  key already used.
- Server to server only. A request with an `Origin` header is refused, and the
  route has no CORS headers; nginx answers `OPTIONS` with 405.

Errors (Core envelope: `{"error": "<code>"}`):

| Status | `error` | When |
|---|---|---|
| 400 | `invalid_game_server_json` | Body is not the JSON above |
| 400 | `invalid_idempotency_key` | Key too long or not printable ASCII |
| 400 | `missing_content`, `missing_title`, `title_too_long`, `content_too_long`, `summary_too_long`, `body_too_long`, `invalid_link_url`, `link_label_too_long` | Content refused (as webhooks) |
| 401 | `grant_required` | No `Authorization: Bearer` |
| 401 | `grant_invalid` | Bad signature, wrong audience/class/scope, expired, unknown key |
| 403 | `game_server_browser_forbidden` | An `Origin` header |
| 404 | `feature_not_found` | Unknown feature, posting turned off, environment not allowed, game not connected or without a club: one answer for all, so the reason is only in the Dev Hub log |
| 409 | `announcement_board_full` | The board is past its content limit |
| 409 | `announcement_board_busy` | The board changed during the post; retry |
| 409 | `club_chat_unavailable` | chat-gateway refused the conversation |
| 429 | `rate_limited` | A limit below, or chat-gateway's own limit |
| 503 | `grant_keys_unavailable` | Core could not fetch the JWKS and has none cached |
| 503 | `club_webhook_chat_unavailable`, `chat_unavailable` | Chat delivery not configured or unreachable |

Retry only on 429, 5xx or a network failure, with the same `Idempotency-Key`.
On `401 grant_invalid`, get a new grant once and retry.

## Limits

- 120 posts per minute per application, and 30 per minute per application and
  feature (Core, fixed one-minute windows). When Core's limiter is unavailable,
  posts are refused with 429, not let through.
- chat-gateway also limits each chat destination to 30 messages per minute
  (`chat-gateway-rs/src/webhooks.rs`, key `webhook:<destination id>`).
- No per-IP limit: the grant identifies the server.

## Turning things off

| Change (Dev Hub) | Takes effect |
|---|---|
| Turn off "Game server can post to my club" | Next post (404). The SDK application is turned off too, so its keys get `401 server/authentication_required` for new grants and sessions |
| Turn off "Let development servers post" | Next development/staging post (404) |
| Remove a feature's destination | Next post for that feature (404) |
| Revoke a key | No new grants at once. **A grant already issued keeps working for up to 5 minutes**: Core checks the grant, not the key, on each post |

## Rust sketch (`reqwest`, `serde_json`)

```rust
use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};

const GRANT_URL: &str = "https://sdk.gridspawn.net/v1/server/grants/club";
const POST_URL: &str = "https://presence-gateway.gridspawn.net/core/v1/game-servers/posts";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Grant {
    access_token: String,
    expires_in: u64,
}

pub struct ClubPoster {
    http: Client,          // reqwest sends no Origin header
    credential: String,    // gsc1.… from the server's secret store, never from the game client
    grant: Option<(String, Instant)>,
}

impl ClubPoster {
    pub fn new(credential: String) -> Self {
        Self { http: Client::new(), credential, grant: None }
    }

    async fn grant(&mut self) -> reqwest::Result<String> {
        if let Some((token, expires)) = &self.grant {
            if Instant::now() + Duration::from_secs(60) < *expires {
                return Ok(token.clone());
            }
        }
        let grant: Grant = self
            .http
            .post(GRANT_URL)
            .header("Authorization", format!("GridspawnServer {}", self.credential))
            .json(&json!({}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let expires = Instant::now() + Duration::from_secs(grant.expires_in);
        self.grant = Some((grant.access_token.clone(), expires));
        Ok(grant.access_token)
    }

    /// Posts once per `key`: a retry with the same key returns `duplicate: true`.
    pub async fn post(&mut self, feature: &str, key: &str, body: Value) -> reqwest::Result<Value> {
        let mut body = body;
        body["feature"] = json!(feature);
        for attempt in 0..2 {
            let grant = self.grant().await?;
            let response = self
                .http
                .post(POST_URL)
                .bearer_auth(grant)
                .header("Idempotency-Key", key)
                .json(&body)
                .send()
                .await?;
            if response.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                self.grant = None; // revoked or rotated signing key: get a new grant once
                continue;
            }
            return response.error_for_status()?.json().await;
        }
        unreachable!()
    }
}
```

## With the Rust crate

The Rust crate `gridspawn-server` does the grant, its reuse, the one refresh
on `grant_invalid` and the retries. Add it from the public SDK repository
(`gridspawn-server = { git = "https://github.com/ivanlhd/gridspawn-sdk-pub", rev = "<commit>" }`; MIT OR
Apache-2.0; the C ABI for C, C++ and Unreal is
`gridspawn-server-capi` next to it):

```rust
use gridspawn_server::{Client, Credential, Environment, IdempotencyKey, Post};

let client = Client::builder("game-…", Environment::Production,
                             Credential::from_file("/etc/my-game/gridspawn-key")?)
    .connect()
    .await?;
let receipt = client
    .post("match.results", Post::titled("Match won", "Arena · 8 players"),
          &IdempotencyKey::new("match-8812-result")?)
    .await?;
// receipt.duplicate is true when this key had already posted
```

Errors arrive as `gridspawn_server::Error::Api` with a typed `ErrorCode`
(`FeatureNotFound`, `RateLimited`, …) for every code in the tables above.

## curl walkthrough

The credential is read from a file so it never appears in shell history or
`ps`:

```sh
GSC_FILE=/path/to/0600-file          # holds gsc1.<id>.<secret>
GRANT=$(curl -sS -X POST https://sdk.gridspawn.net/v1/server/grants/club \
  -H @<(printf 'Authorization: GridspawnServer %s\n' "$(cat "$GSC_FILE")") \
  | jq -r .accessToken)

post() {  # $1 = feature, $2 = Idempotency-Key
  curl -sS -w ' [%{http_code}]\n' -X POST https://presence-gateway.gridspawn.net/core/v1/game-servers/posts \
    -H @<(printf 'Authorization: Bearer %s\n' "$GRANT") \
    -H 'Content-Type: application/json' -H "Idempotency-Key: $2" \
    -d "{\"feature\":\"$1\",\"title\":\"Match won\",\"content\":\"Arena · 8 players\"}"
}
post match.results smoke-1     # {"ok":true,"id":"…"} [200]
post match.results smoke-1     # {"ok":true,"id":"…","duplicate":true} [200]
post no-such-feature smoke-2   # {"error":"feature_not_found"} [404]
```
