# gridspawn-client

Rust client SDK for Gridspawn, for game clients:
- player sign-in with the system browser (OAuth 2.0 authorization code + PKCE
  S256, RFC 8252 loopback redirect), with token refresh and sign-out;
- player assertions for your game server;
- attaching to a game session with the ticket your game server issued.

A game client holds no secret: Gridspawn applications are OAuth public clients.
Server keys belong on game servers only (`gridspawn-server`).

It speaks the wire protocol one to one: `01-login.md` and `02-sessions.md` in
the protocol docs (`docs/protocol/` in the public repository). Requests never
carry an `Origin` header.

Status: 0.1.0, source only, not on crates.io yet. Licence: MIT OR Apache-2.0
(see the end of this file).
MSRV: Rust 1.83 for the library (the tests' dev-dependencies need a newer
toolchain). Presence (03) and voice (04) are not in this crate yet.

## Install

Not on crates.io yet. Add it from the public SDK repository, pinned to a commit:

```toml
[dependencies]
gridspawn-client = { git = "https://github.com/ivanlhd/gridspawn-sdk-pub", rev = "<commit>" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Features (both on by default): `browser` opens the system browser;
`os-store` keeps the session in the operating system's credential store.

## Application settings

Per environment, your application needs:
- the loopback redirect `http://127.0.0.1/callback` registered (any port
  matches, as RFC 8252 requires; `localhost` is not loopback);
- the scopes you request allowed (for example `identity.basic`);
- for attach, the modules enabled (and guest access, for guests).

These client settings are not yet editable in the Dev Hub. Without them,
sign-in fails with `auth/invalid_client` or `auth/invalid_request`, and tickets
with `session/capability_denied`.

## Sign in

```rust
use gridspawn_client::{Client, Environment, Restore, SignIn};

#[tokio::main]
async fn main() -> Result<(), gridspawn_client::Error> {
    let client = Client::builder("game-neon-apex", Environment::Production)
        .connect()
        .await?;
    let user = match client.restore().await? {
        Restore::SignedIn(user) => user,
        Restore::SignedOut | Restore::InteractionRequired { .. } => {
            client.sign_in(SignIn::new(["identity.basic"])).await?
        }
    };
    println!("Signed in as {}", user.display_name);
    Ok(())
}
```

- `sign_in` listens on `127.0.0.1` on a port the OS picks, opens the browser,
  waits (default 5 minutes, at most 10), and exchanges the code. The browser
  tab then says whether it worked. Other requests to the listener (favicon,
  probes) get a 404; a callback with the wrong `state`, or for another host
  name, ends the sign-in.
- `begin_sign_in` does the same in two steps:
  - with `SignIn::open_browser(false)`, show
    `PendingSignIn::authorization_url()` to the player yourself;
  - `finish()` waits for the callback;
  - `cancel()`, or dropping the pending sign-in, closes the listener.
- Show `User::display_name` only. `subject` is an opaque id that never belongs
  in UI, and `avatar_object_path` is an object path, not a URL.
- `examples/sign_in.rs` is the manual test with a real browser:
  `GRIDSPAWN_APP_ID=game-… cargo run --example sign_in`.

## Where the session is kept

Protocol 01 §3 requires refresh tokens to live in OS-protected storage, never
in plain files or save data.

| Store | Where | Notes |
|---|---|---|
| `OsTokenStore` (default when available) | Windows Credential Manager (DPAPI), macOS Keychain, Linux Secret Service (GNOME Keyring, KWallet) | Entry service `gridspawn`, account `<app id>/<environment>` |
| `MemoryTokenStore` (automatic fallback) | This process only | The player signs in on each launch |

The fallback applies where no OS store answers:
- Linux sessions without a Secret Service daemon: minimal window managers,
  containers, WSL without a keyring daemon;
- **Steam Deck in Game Mode**: plan for the memory fallback there (the
  probe decides at run time).

`Client::persists_sessions()` tells which case applies, so the game can say
"you will need to sign in again next time". `OsTokenStore::available()` checks
directly. A store failure while saving is logged and the session stays in
memory for this run. `ClientBuilder::token_store` accepts your own
`TokenStore` implementation; it must meet the same rule.

## Tokens, offline, sign-out

- Access tokens live 10 minutes. The client refreshes on demand, when a call
  needs a token with less than 30 s left; it never refreshes on a timer, so
  idle players cost nothing. Refreshes are shared between concurrent callers
  and retried once after a network error or 5xx.
- "Signed in" means a session is held. Offline, `restore()` keeps it:
  - with a still-valid access token, it returns `SignedIn`;
  - otherwise it returns `Error::Network`, while `is_signed_in()` stays true
    and `current_user()` still works.
- Only a refused refresh token (`auth/invalid_grant`, `auth/invalid_client`)
  ends the session. The store is cleared, then
  `AuthEvent::SessionExpired` is sent (`subscribe()`).
- `sign_out()` clears the store first, then revokes the refresh token. A
  failed revoke is returned, but the local session is gone either way.

## Join a game server

```rust
// 1. The game server sends a challenge over your game connection.
let assertion = client.player_assertion(&challenge).await?;   // single use, 60 s
// 2. Send assertion.as_str() back; the server admits the player and issues a ticket.
// 3. Attach with the ticket.
let session = client.attach(&ticket).await?;
println!("joined as {:?}", session.participant().display_name);
let _renewer = session.spawn_renewer();          // keeps the module grants fresh
let voice = session.grant(gridspawn_client::Module::Voice).await?;
```

- Assertions and attach are never retried: the assertion and the ticket are
  single-use.
- Grants live 5 minutes:
  - `grant(module)` renews one that has less than 60 s left;
  - `spawn_renewer()` renews each grant at 60 s left and retries network
    errors, 429 and 5xx until it expires;
  - when a grant can no longer be renewed (the session ended, or it expired),
    you get `GrantEvent::Expired`.
- Guests are attached the same way; their `display_name` is always set.

## Errors

`Error::code()` gives a stable string for UI and logs: the wire code
(`auth/invalid_grant`, `session/ticket_invalid`, …) or a local one
(`sign_in/cancelled`, `network`, `timeout`, …). `Error::Api` carries an
`ApiError` with `service`, `status`, `code`, `wire_code`, `message`,
`request_id` and `retry_after`.

| Code | When | What to do |
|---|---|---|
| `sign_in/cancelled`, `sign_in/timed_out` | The flow was cancelled or not finished in time | Offer sign-in again |
| `sign_in/browser_unavailable` | No system browser could be opened | Show the URL (`begin_sign_in`) |
| `sign_in/state_mismatch`, `sign_in/redirect_mismatch` | A callback that is not ours | Sign in again |
| `auth/access_denied` | The player declined | Continue as a guest, or ask later |
| `auth/invalid_client`, `auth/invalid_scope` | The app, environment or scope is not set up | Check the application settings |
| `auth/invalid_grant` | Code expired or reused; refresh token revoked or expired | Sign in again |
| `auth/login_required`, `auth/account_setup_required` | The account must finish setup in the browser | Sign in again |
| `auth/rate_limited` | The application's token budget is used up | Wait and retry |
| `session/ticket_invalid` | The ticket expired (120 s), was used, or is forged | Ask the game server for a new ticket |
| `session/capability_denied` | A module the app or player may not use | Configuration |
| `session/grant_invalid`, `session/not_found` | The game session ended | Rejoin |
| `service/not_ready`, `network`, `timeout` | Temporary | Retry later; the session is kept |

## Tests

```sh
cargo test                     # unit + mock (a scripted browser signs in), no network
GRIDSPAWN_TEST_APP_ID=… GRIDSPAWN_TEST_CREDENTIAL_FILE=/path/to/server-key \
cargo test --test live -- --nocapture
```

The live test uses `gridspawn-server` to create a session, admit a guest and
issue a voice ticket, then attaches, renews, and waits for one renewer cycle
(about 4 minutes). Interactive sign-in needs a person: `examples/sign_in.rs`.

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
