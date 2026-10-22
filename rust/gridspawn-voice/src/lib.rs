//! Gridspawn voice for game clients.
//!
//! Joins the voice room of an attached game session (protocol 04): admission
//! with the session's voice grant, the SFU's Socket.IO signaling, and WebRTC
//! audio (webrtc-rs, Opus) to and from the SFU. The microphone goes through
//! WebRTC's audio processing (echo cancellation, noise suppression, gain);
//! each remote speaker gets a jitter buffer with in-band FEC and loss
//! concealment. The engine side is a plain PCM API (mono, 48 kHz):
//!
//! ```no_run
//! # async fn run(client: gridspawn_client::Client, session: gridspawn_client::AttachedSession)
//! # -> Result<(), gridspawn_voice::VoiceError> {
//! use gridspawn_voice::{VoiceOptions, VoiceSession};
//!
//! let voice = VoiceSession::join(&client, &session, VoiceOptions::default()).await?;
//! let mut events = voice.events();
//! // Every 10–20 ms, from the audio or game thread:
//! # let microphone = [0f32; 960];
//! voice.push_capture(&microphone);              // what the microphone heard
//! let mut speakers = [0f32; 960];
//! voice.pull_mix(&mut speakers);                // what to play
//! voice.push_playback_reference(&speakers);     // what was played (echo reference)
//! # let _ = events.try_recv();
//! voice.leave().await;
//! # Ok(()) }
//! ```
//!
//! Limits of this version (the backend's, today): one room per game session
//! where everyone hears everyone; participants are opaque ids with no display
//! names; deafening is local; no server mute or audibility events.

mod admission;
mod audio;
mod error;
mod jitter;
mod media;
mod sdp;
mod session;
mod signaling;

#[cfg(feature = "cpal")]
pub mod devices;

pub use audio::AudioOptions;
pub use error::VoiceError;
pub use session::{
    ParticipantInfo, TransmitMode, VoiceEvent, VoiceOptions, VoiceSession, VoiceStats,
};
