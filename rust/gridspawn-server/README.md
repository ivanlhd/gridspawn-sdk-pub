# gridspawn-server

Rust server SDK for Gridspawn: game sessions, player and guest admission,
client tickets, player presence, voice rules (who hears whom), posting to
your game's club, and live game data for your club's pages. Server-side only
— the credential must never ship in a game client.

It speaks the wire protocol one to one: `02-sessions.md` (sessions,
admission, tickets), `05-club.md` (club posts) and `06-game-data.md` (game
data) in the protocol docs (`docs/protocol/` in the public repository).
Requests never carry an `Origin` header.

Status: 0.4.0, source only, not on crates.io yet. Licence: MIT OR Apache-2.0
(see the end of this file).
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

## Show what a player is doing (presence)

Your server sets a signed-in player's presence: it shows on their Gridspawn
profile, friends list and club member lists as "<your game> · details ·
state (size/max)" (protocol 03, server path).

```rust
use gridspawn_server::{Party, Presence};

let presence = Presence::new()
    .details("Ranked match")
    .state("In lobby")
    .party(Party::new("lobby-42", 2, 4));
participant.set_presence(&presence).await?;   // or session.participant(id).set_presence(…)
// …every 60 s while the player is in the session: it expires 120 s after
// the last update.
participant.clear_presence().await?;          // when the player leaves
```

- The crate does not re-send for you: call `set_presence` every 60 s while
  the player is in the session. Presence expires 120 s after the last write,
  and at once on `clear_presence`.
- Needs the key's `session.manage` scope, a live lease, and the player's
  `presence.write` consent (`presence.party` for a party). Guests have no
  Gridspawn profile: `PresenceGuestNotSupported`.
- While your server's presence is live, the player's own client updates for
  your game are accepted but not shown.
- Texts are trimmed and checked for length and control characters before
  sending. The service refuses emails, web addresses and identifiers in them
  (`PresenceTextNotAllowed`, with `ApiError::field`).
- Limits: 5 writes per 20 s per player, and 3,000 per minute per app and
  environment. Over them: `RateLimited` with `retry_after`.

## Decide who hears whom (voice rules)

Players in a session talk in its voice rooms (the game client joins with
`gridspawn-voice` or the Godot addon). Your server decides who hears whom: the
voice service forwards nothing for a pair that may not hear each other, so a
modified client cannot listen in.

```rust
use gridspawn_server::{Module, Rolloff, VoiceConfig, VoicePlayerState, VoiceRoom};

let voice = session.voice();
voice.configure(&VoiceConfig::new().room(
    "match",
    VoiceRoom::hybrid(2.0, 25.0).rolloff(Rolloff::Inverse).hysteresis(2.0),
)).await?;

// For each player, BEFORE their voice ticket:
voice.set_player_state(&guest.id, &VoicePlayerState::new().channels(["team:red"])).await?;
let ticket = guest.issue_ticket(&[Module::Voice]).await?;

// Positions (proximity and hybrid rooms), from your game loop:
let positions = voice.spawn_position_publisher(10);       // sends ≤ 10 times a second
positions.update(&guest.id, [x, y, z], Some(yaw))?;        // cheap: every tick
// 2D game: positions.update_planar(&guest.id, x, y, None)?   (sent as [x, 0, y])

voice.mute(&guest.id, true).await?;                        // authoritative mute
```

- **Rooms:** `VoiceRoom::open()`, `channels()`, `proximity(min, max)` or
  `hybrid(min, max)` (a shared channel at full volume, or range). Distances
  are in metres; full volume up to `min`, silent beyond `max`
  (`+ hysteresis` to leave range). `spatial_client()` lets the game client
  place voices itself: no range culling, so it is **not secrecy-safe** (for
  co-op and social games). `max_audible_speakers` (default 16) keeps the
  closest. Dead and spectating players: `Ghosts` (by default they hear the
  living and speak only to each other).
- **Rooms today:** players can join only the room `match` until named rooms
  are enabled on the voice service. Configuring `match` puts it under your
  rules.
- **The player state is required.** In a room you configured, a player
  without a state hears no one and is heard by no one. Call
  `set_player_state` before `issue_ticket` with `Module::Voice`. After a
  session handle has configured voice, the crate logs a warning when it
  issues a voice ticket for a player whose state it never sent.
- **Positions:** right-handed, y up; `yaw` in radians, 0 facing −z and
  growing to the left (Godot's `rotation.y`). The pan is computed in the x/z
  plane, so 2D games send `[x, 0, y]` (`VoicePosition::planar`,
  `update_planar`). A position counts fully for 1 s after it arrives and
  fades out over the next 2 s: the publisher re-sends every player's latest
  position each tick (use 2 Hz or more).
- **`VoiceAck::enforced`** is false until the first player of the session
  joins voice (no voice server is assigned before that), or while the voice
  server's link to the rules is down. It is information, not an error.
- **Keep the lease alive** (`spawn_renewer`): when it lapses, the rules go
  stale and everyone is silent 3 s later.
- **Sessions longer than 24 hours:** today the renewals keep the rules alive
  for 24 h after the last `configure`. Call `configure` again before that if
  your game sends no other voice updates.
- **Ordering:** state and position updates carry an increasing sequence
  number (the wall clock in ms, or one more than the last), so an older
  update never replaces a newer one: it answers `VoiceStaleSequence`. A
  retried update that meets it reports `VoiceAck::overtaken` instead of an
  error.
- **Limits:** configure 5 per 10 s per session; state 10/s per player and
  200/s per session; positions 20 batches per 2 s per session (1–256
  players each); mute 20/s per session. Over them: `RateLimited` with
  `retry_after`; the publisher waits it out by itself.
- `VoiceConfig::from_json` reads a configuration in its wire shape, for
  example from a file. The `blocking` client has `voice_configure`,
  `voice_set_player_state`, `voice_mute` and `voice_publish_positions` (no
  background publisher there).

## Show live game data on your club

Your server keeps named values current (player count, round, scoreboard,
map markers) and sends events ("round won"). Your club shows them on its
pages through the club layout editor (protocol `06-game-data.md`). **No game
session is needed:** the data belongs to your app and environment. Values
are `serde_json::Value` (add `serde_json = "1"`).

```rust
use gridspawn_server::DataBatch;
use serde_json::json;
use std::time::Duration;

let game = client.game();
let ack = game.data().set("match.score", json!({"red": 3, "blue": 1})).await?;
game.data().set_with_ttl("lobby.open", json!(true), Duration::from_secs(300)).await?;
game.data().remove("lobby.open").await?;
game.data().set_many(&DataBatch::new()                 // 1–40 entries, all or nothing
    .set("match.round", json!(4))
    .remove("lobby.banner")).await?;
let listing = game.data().list().await?;               // keys, seq, sizes, removals, quota

// Values that change every frame: the latest per key, one batch per tick.
let live = game.spawn_data_publisher(4);     // 1–5 Hz
live.set("match.clock", json!(812))?;        // cheap: every frame
// live.skipped(): keys another writer owns; live.last_error(); drop(live) stops it

game.events().emit("match.ended", json!({"winner": "Red team"})).await?;
game.events().emit_with_id("round.won", json!({"round": 4}), "round-4-of-match-81").await?;
```

- **`DataAck`:** `accepted` (the keys stored), `skipped` (keys another
  writer owns, with their stored sequence number) and `quota` (`keys`,
  `max_keys`, `bytes`, `max_bytes`).
- **Events:** every event carries an `Idempotency-Key`, so a retry is never
  counted twice. `emit` generates one; `emit_with_id` takes yours
  (`[A-Za-z0-9_-]{8,64}`). Use your own when the same event may be sent again
  after a restart: a repeat within 24 h answers `duplicate: true`.
- **TTL:** whole seconds, 10 s to 24 h. Without one, a document lives until
  it is removed. A removal is kept 24 h, so an older delayed write cannot
  bring the key back.
- **Checks before sending** (`Error::InvalidInput`): key `^[a-z][a-z0-9_.-]{0,63}$`,
  values up to 4 KiB of JSON and 8 levels deep, 1–40 entries and 32 KiB per
  batch, event names `^[a-z0-9][a-z0-9_.-]{0,47}$`, event payloads a JSON
  object up to 4 KiB.
- **The publisher** retries what can succeed later (network errors, 5xx,
  429 after `Retry-After`) with the newest values. A refusal that cannot
  succeed by retrying — an invalid or personal-data value, or
  `GameDataQuotaExceeded` — drops those entries and is kept in
  `last_error()`; set the key again once the cause is fixed.
- **`list()` is for start-up and checks, not for polling** (10 per 10 s).
- The `blocking` client has `game_data_set`, `game_data_remove`,
  `game_data_set_many`, `game_data_list` and `game_event_emit` (no background
  publisher there).

- **Public by nature.** Production game data appears on public club pages,
  readable by anyone. **Publish display names only, never emails, account
  ids or participant ids, and nothing private.** The service refuses email
  shapes and Gridspawn id prefixes (`GameDataInvalidValue` with reason
  `personal_data`, and the JSON path in `field`), but that check is a safety
  net, not a filter to rely on.
- **One writer per key.** Every write carries a sequence number from one
  in-process counter per `Client`; the service skips a write that is not
  newer than the key's stored one. Servers that run side by side (shards,
  regions) write disjoint keys, for example `shard-eu.players`. The first
  write reads your stored keys once and starts above them, so a restarted
  server carries on where it stopped; no clock is involved. A key that is
  still skipped after one resend belongs to another writer and comes back in
  `DataAck::skipped`.
- **Setup:** give the server key the `game_data.write` scope and switch game
  data on for the app and environment, in the Dev Hub. What your club shows
  is the club's choice; writing data changes no page by itself.
- **Limits** (per app and environment): 20 entries a second (bursts of 40)
  and 5 a second per key; up to 40 entries and 32 KiB per batch; 256 keys
  and 256 KiB in total; values up to 4 KiB; 10 events a second; the listing
  10 per 10 s. The publisher stays within them by itself.

## Retries

- Network errors, timeouts and HTTP 5xx are retried (`RetryPolicy`: 3
  attempts, 200 ms doubling, ±20 % jitter), always with the same
  Idempotency-Key and body.
- 429 is not retried: you get `ErrorCode::RateLimited` and decide.
  `ApiError::retry_after` carries `Retry-After` when sent (presence limits
  send it, as do voice and game data; the posting limits are fixed one-minute
  windows without it). The game-data publisher waits it out by itself.
- Game-data writes need no Idempotency-Key: their sequence numbers make a
  repeat harmless. Events always carry one.
- Ticket issue is never retried (each call issues a new ticket).
- A post that gets `grant_invalid` gets a new club grant once and is
  retried once.

## Errors

`Error::Api` carries an `ApiError` with `service`, `status`, `code`
(`ErrorCode`), `wire_code`, `message`, `request_id`, `retry_after`, `field`
and `reason`.
`Error::is_retryable()` tells whether a later retry with the same key makes
sense.

| `ErrorCode` | Wire code | What to do |
|---|---|---|
| `FeatureNotFound` | `feature_not_found` (404) | Add a destination for the feature in the Dev Hub, turn posting on, or allow test servers |
| `RateLimited` | `rate_limited`, `presence/rate_limited`, `voice/rate_limited`, `data/rate_limited` (429) | Wait (`retry_after` when sent); posts: 120/min per app and 30/min per feature; presence: 5 writes per 20 s per player; voice and game data: see their limits |
| `GameDataInvalidValue` | `data/invalid_value` (400) | Fix the field in `ApiError::field` (`reason` says why). `ApiError::is_personal_data()`: an email or a Gridspawn id was found; publish display names only |
| `GameDataModuleDisabled` | `data/module_disabled` (403) | Turn game data on for the app and environment in the Dev Hub |
| `GameDataQuotaExceeded` | `data/quota_exceeded` (409) | Remove keys (`field`: `keys` or `bytes`) |
| `GameDataUnavailable` | `data/unavailable` (503) | Retried automatically; nothing was stored; try again later |
| `PresenceInvalidRequest` / `PresenceTextNotAllowed` | `presence/invalid_request` / `presence/text_not_allowed` (400) | Fix the field in `ApiError::field` |
| `PresenceScopeDenied` | `presence/scope_denied` (403) | No `presence.write` (or `presence.party`) consent, or the presence module is off |
| `PresenceGuestNotSupported` | `presence/guest_not_supported` (403) | Guests have no presence |
| `PresenceAccountUnavailable` | `presence/account_unavailable` (403) | The player's account is not active |
| `VoiceInvalidRequest` | `voice/invalid_request` (400) | Fix the field in `ApiError::field` (`reason` says why) |
| `VoiceStaleSequence` | `voice/stale_sequence` (409) | A newer update was already accepted; nothing to do |
| `VoiceParticipantNotFound` | `voice/participant_not_found` (404) | The player is not admitted to this session (or left) |
| `VoiceModuleDisabled` | `voice/module_disabled` (403) | Turn voice on for the app and environment in the Dev Hub |
| `VoiceUnavailable` | `voice/unavailable` (503) | Retried automatically; try again later |
| `GrantInvalid` | `grant_invalid` (401) | Key revoked or app turned off (one automatic refresh already happened) |
| `AuthenticationRequired` | `server/authentication_required` (401) | Wrong, revoked or expired key, or the environment is off |
| `ScopeDenied` | `server/scope_denied` (403) | The key lacks the scope (`club.post` / `session.manage` / `game_data.write`) |
| `CapabilityDenied` | `session/capability_denied` (403) | A module the app or the player may not use |
| `BoardFull` / `BoardBusy` | `announcement_board_full` / `_busy` (409) | Full: remove old posts. Busy: retry |
| `ContentRejected(code)` | `title_too_long`, … (400) | Fix the post |
| `ChatUnavailable` | `club_*chat_unavailable`, `chat_unavailable` | Chat delivery is down; retry later |
| `IdempotencyConflict` | `request/idempotency_conflict` (409) | Same key, different body: use a new key |
| `SessionNotFound` | `session/not_found`, `voice/session_not_found` (404) | The session ended or the id is wrong |
| `ServiceNotReady` | `service/not_ready` (503) | Retried automatically; try again later |
| `Other(code)` | anything else | Kept verbatim |

`data/browser_forbidden` maps to `BrowserForbidden`. The full list is in the
`ErrorCode` docs and the protocol docs (02, 05, 06).

## Tests

```sh
cargo test                                   # unit + mock server, no network
GRIDSPAWN_TEST_APP_ID=… GRIDSPAWN_TEST_CREDENTIAL_FILE=/path/to/key cargo test --test live -- --nocapture
GRIDSPAWN_TEST_GAME_CREDENTIAL_FILE=… GRIDSPAWN_TEST_GAME_APP_ID=game-… \
GRIDSPAWN_TEST_FEATURE=smoke cargo test --test live   # posts a real test message
```

Run Cargo from this directory; its `target/` stays local.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
