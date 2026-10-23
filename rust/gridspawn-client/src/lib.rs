//! Rust client SDK for Gridspawn, for game clients.
//!
//! It speaks the wire protocol one to one:
//!
//! - player sign-in through the Gridspawn app when it is open (the player
//!   confirms there), otherwise with the system browser (OAuth 2.0
//!   authorization code + PKCE S256, RFC 8252 loopback redirect), token
//!   refresh, player assertions for the game server, and sign-out —
//!   `docs/protocol/01-login.md`;
//! - attaching to a game session with the participant ticket the game server
//!   issued, and module grants — `docs/protocol/02-sessions.md`;
//! - the player's presence in your game, re-sent every 60 s —
//!   `docs/protocol/03-presence.md`.
//!
//! A game client holds no secret: Gridspawn applications are OAuth public
//! clients. Server credentials belong on game servers only (`gridspawn-server`).
//!
//! ```no_run
//! use gridspawn_client::{Client, Environment, Restore, SignIn};
//!
//! # async fn run() -> Result<(), gridspawn_client::Error> {
//! let client = Client::builder("game-neon-apex", Environment::Production)
//!     .connect()
//!     .await?;
//! let user = match client.restore().await? {
//!     Restore::SignedIn(user) => user,
//!     _ => client.sign_in(SignIn::new(["identity.basic"])).await?,
//! };
//! println!("Signed in as {}", user.display_name);
//!
//! // Joining a game server: answer its challenge with an assertion …
//! let assertion = client.player_assertion("challenge-from-the-game-server-0123456789").await?;
//! # let _ = assertion;
//! // … and attach with the ticket it sends back.
//! # let ticket = "";
//! let session = client.attach(ticket).await?;
//! let _renewer = session.spawn_renewer();
//! # Ok(()) }
//! ```
//!
//! Requests never carry an `Origin` header. Tokens, codes, verifiers, state,
//! assertions, tickets and grants are never logged and are redacted in `Debug`.

mod auth;
mod broker;
mod config;
mod error;
mod http;
mod loopback;
mod pkce;
mod presence;
mod session;
mod storage;

pub use auth::{AuthEvent, PendingSignIn, PlayerAssertion, Restore, SignIn, SignInMethod, User};
pub use config::{Client, ClientBuilder, Environment, Module};
pub use error::{ApiError, Error, ErrorCode, Service, SignInError};
pub use presence::{Party, Presence, PresenceAck, PresenceEvent, PresenceUpdater};
pub use session::{
    AttachedSession, GrantEvent, IdentityKind, ModuleGrant, Participant, RenewerHandle,
};
#[cfg(feature = "os-store")]
pub use storage::OsTokenStore;
pub use storage::{MemoryTokenStore, StoreError, StoredSession, TokenStore};
