//! Rust server SDK for Gridspawn.
//!
//! Server-side only: the server credential must never ship in a game client.
//! It speaks the wire protocol one to one:
//!
//! - game sessions, player and guest admission, and client tickets —
//!   `docs/protocol/02-sessions.md`;
//! - player presence set by the game server —
//!   `docs/protocol/03-presence.md`;
//! - posts to the game's club with a club grant —
//!   `docs/protocol/05-club.md`.
//!
//! ```no_run
//! use gridspawn_server::{Client, Credential, Environment, IdempotencyKey, Post};
//!
//! # async fn run() -> Result<(), gridspawn_server::Error> {
//! let client = Client::builder(
//!     "game-neon-apex",
//!     Environment::Production,
//!     Credential::from_file("/etc/my-game/gridspawn-key")?,
//! )
//! .connect()
//! .await?;
//! let receipt = client
//!     .post(
//!         "patch-notes",
//!         Post::titled("Patch 1.2 is out", "Fixes and a new map."),
//!         &IdempotencyKey::new("patch-1.2-notes")?,
//!     )
//!     .await?;
//! println!("posted {} (duplicate: {})", receipt.id, receipt.duplicate);
//! # Ok(()) }
//! ```
//!
//! Requests never carry an `Origin` header. Network errors, timeouts and 5xx
//! answers are retried with the same Idempotency-Key ([`RetryPolicy`]); 429 is
//! returned to the caller.

mod client;
mod club;
mod credential;
mod error;
mod http;
mod presence;
mod session;
mod types;

#[cfg(feature = "blocking")]
pub mod blocking;

pub use client::{Client, ClientBuilder};
pub use club::{Link, Post, PostReceipt};
pub use credential::Credential;
pub use error::{ApiError, Error, ErrorCode, Service};
pub use presence::{ParticipantRef, Party, Presence, PresenceAck};
pub use session::{
    AdmitGuest, AdmitParticipant, ClientTicket, CreateSession, IdentityKind, Lease, Participant,
    ParticipantId, RenewerHandle, Session, SessionId,
};
pub use types::{Environment, IdempotencyKey, Module, RetryPolicy};
