# gridspawn-server

Rust server SDK for Gridspawn: game sessions, player and guest admission,
client tickets, and posting to your game's club. Server-side only — the
credential must never ship in a game client.

It speaks the wire protocol one to one: `02-sessions.md` (sessions,
admission, tickets) and `05-club.md` (club posts) in the protocol docs
(`docs/protocol/` in the public repository). Requests never carry an `Origin`
header.

Status: 0.1.0, source only. **No licence yet: all rights reserved until one is
chosen.** crates.io follows the licence decision.
MSRV: Rust 1.83 for the library (the tests' dev-dependencies need a newer
toolchain).

## Install

Not on crates.io yet. Add it from the public SDK repository, pinned to a commit:

```toml
[dependencies]
gridspawn-server = { git = "https://github.com/ivanlhd/gridspawn-sdk-pub", rev = "<commit>" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Cargo finds the `gridspawn-server` package by name inside that repository (it
lives in `rust/gridspawn-server`). Inside the Gridspawn monorepo, use a path
dependency instead:

```toml
gridspawn-server = { path = "<checkout>/sdk/rust/gridspawn-server" }
```

## Post to your club

1. Dev Hub → Game servers: connect the game, make a server key, and add a
   destination for a feature (for example `patch-notes`).
2. Put the key in a file only your server user can read (`chmod 600`).

```rust
use gridspawn_server::{Client, Credential, Environment, IdempotencyKey, Post};

#[tokio::main]
async fn main() -> Result<(), gridspawn_server::Error> {
    let client = Client::builder(
        "game-neon-apex",
        Environment::Production,
        Credential::from_file("/etc/my-game/gridspawn-key")?,
    )
    .connect()
    .await?;

    let receipt = client
        .post(
            "patch-notes",
            Post::titled("Patch 1.2 is out", "Fixes and a new map."),
            &IdempotencyKey::new("patch-1.2-notes")?,
        )
        .await?;
    println!("posted {} (duplicate: {})", receipt.id, receipt.duplicate);
    Ok(())
}
```

Reusing the same `IdempotencyKey` never posts twice. Retries after a
network error use it automatically. The same program is
`examples/club_post.rs`:

```sh
GRIDSPAWN_APP_ID=game-… GRIDSPAWN_ENVIRONMENT=development \
GRIDSPAWN_CREDENTIAL_FILE=/path/to/key cargo run --example club_post -- patch-notes "Title" "Text"
```

`Credential::from_file` refuses a key file that group or others can read
(fix: `chmod 600 <path>`; or use `Credential::from_file_unchecked`).
`Credential::from_env("GRIDSPAWN_SERVER_CREDENTIAL")` reads it from the
environment. `Debug`/`Display` never show the secret.

## Run a game session

```rust
use gridspawn_server::{AdmitGuest, CreateSession, IdempotencyKey, Module};

let session = client
    .create_session(CreateSession::new("match-123", IdempotencyKey::new("match-123")?)
        .region("us-east").max_participants(16))
    .await?;
let _renewer = session.spawn_renewer();          // keeps the 45 s lease alive

let guest = session
    .admit_guest(AdmitGuest::new("player-7f3a", "Wanderer", IdempotencyKey::new("match-123:p7f3a")?))
    .await?;
let ticket = guest.issue_ticket(&[Module::Voice]).await?;
// send ticket.as_str() to that player's client over your game connection
```

Signed-in players: `session.admit_participant(AdmitParticipant::new(assertion,
challenge, external_player_id, key))` with the assertion the client got
(protocol 01 §5).

The renewer runs on the current tokio runtime and stops when its handle, or
every clone of the session or the client, is dropped. With the `blocking`
feature, `gridspawn_server::blocking::Client` offers the same calls
synchronously (no background renewer there: call `renew` yourself). Do not
call it from async code or a game's frame loop.

## Retries

- Network errors, timeouts and HTTP 5xx are retried (`RetryPolicy`: 3
  attempts, 200 ms doubling, ±20 % jitter), always with the same
  Idempotency-Key and body.
- 429 is not retried: you get `ErrorCode::RateLimited` and decide. Neither
  service sends `Retry-After` today (`ApiError::retry_after` reads it if it
  appears); the posting limits are fixed one-minute windows.
- Ticket issue is never retried (each call issues a new ticket).
- A post that gets `grant_invalid` gets a new club grant once and is
  retried once.

## Errors

`Error::Api` carries an `ApiError` with `service`, `status`, `code`
(`ErrorCode`), `wire_code`, `message`, `request_id` and `retry_after`.
`Error::is_retryable()` tells whether a later retry with the same key makes
sense.

| `ErrorCode` | Wire code | What to do |
|---|---|---|
| `FeatureNotFound` | `feature_not_found` (404) | Add a destination for the feature in the Dev Hub, turn posting on, or allow test servers |
| `RateLimited` | `rate_limited` (429) | Wait; limits are 120 posts/min per app and 30/min per feature |
| `GrantInvalid` | `grant_invalid` (401) | Key revoked or app turned off (one automatic refresh already happened) |
| `AuthenticationRequired` | `server/authentication_required` (401) | Wrong, revoked or expired key, or the environment is off |
| `ScopeDenied` | `server/scope_denied` (403) | The key lacks the scope (`club.post` / `session.manage`) |
| `CapabilityDenied` | `session/capability_denied` (403) | A module the app or the player may not use |
| `BoardFull` / `BoardBusy` | `announcement_board_full` / `_busy` (409) | Full: remove old posts. Busy: retry |
| `ContentRejected(code)` | `title_too_long`, … (400) | Fix the post |
| `ChatUnavailable` | `club_*chat_unavailable`, `chat_unavailable` | Chat delivery is down; retry later |
| `IdempotencyConflict` | `request/idempotency_conflict` (409) | Same key, different body: use a new key |
| `SessionNotFound` | `session/not_found` (404) | The session ended or the id is wrong |
| `ServiceNotReady` | `service/not_ready` (503) | Retried automatically; try again later |
| `Other(code)` | anything else | Kept verbatim |

The full list is in the `ErrorCode` docs and the protocol docs (02, 05).

## Tests

```sh
cargo test                                   # unit + mock server, no network
GRIDSPAWN_TEST_APP_ID=… GRIDSPAWN_TEST_CREDENTIAL_FILE=/path/to/key cargo test --test live -- --nocapture
GRIDSPAWN_TEST_GAME_CREDENTIAL_FILE=… GRIDSPAWN_TEST_GAME_APP_ID=game-… \
GRIDSPAWN_TEST_FEATURE=smoke cargo test --test live   # posts a real test message
```

Run Cargo from this directory; its `target/` stays local.
