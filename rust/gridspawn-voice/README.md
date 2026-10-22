# gridspawn-voice

Voice for game clients. It joins the voice room of a game session attached
with `gridspawn-client`, through the protocol in `docs/protocol/04-voice.md`:
- admission with the session's voice grant;
- the SFU's Socket.IO signaling;
- WebRTC audio (Opus) both ways.

The microphone goes through WebRTC's audio processing (echo cancellation,
noise suppression, automatic gain). Each remote speaker gets a jitter buffer
with Opus in-band FEC and loss concealment. Your engine talks to it through a
plain PCM API (mono, 48 kHz). With the `cpal` feature, it can use the
operating system's audio devices itself.

Status: 0.2.1, source only, not on crates.io yet. Licence: MIT OR Apache-2.0.
WebRTC's audio processing is vendored with build fixes: `../vendor/README.md`.

## Who hears whom

Your game server decides, with `gridspawn-server`'s `session.voice()`:
channels (teams, parties), proximity, dead and spectating players, and the
authoritative mute. The voice service forwards nothing for a pair that may
not hear each other, so a modified client cannot listen in. This crate
applies what the service sends:
- **gain** (0–1, multiplied with your local volume) and **pan** (−1 left …
  1 right) per speaker: `VoiceEvent::Audibility`,
  `ParticipantInfo::{audible, gain, pan}`, and the stereo pulls;
- **the room's rules**: `room_config()` (mode, spatial placement,
  `secrecy_safe`, size; not the distances, which stay on the server) and
  `VoiceEvent::RoomConfig`;
- **server mute**: `VoiceEvent::ServerMuted` for others,
  `VoiceEvent::LocalServerMuted` and `is_server_muted()` for this client
  (unmuting is refused with `voice/server_muted` meanwhile);
- **fail closed**: `VoiceEvent::Authority { available: false }` while the
  service's rules are unreachable (nothing is forwarded then).

Without rules from the game server, everyone in the room hears everyone.

**Client-spatialized rooms** (`spatial: "client"`, `secrecy_safe: false`):
the service sends every allowed stream with gain 1 and no pan. Place each
participant's audio in your own scene (`pull_participant`), and tell players
that the room is not secrecy-safe.

## Participants

- `participants()` lists everyone else in the room, including people who are
  not speaking, with `display_name` (show only this) and `id`.
- `id` is the game session's participant id (`gsp1.…`), the same id your
  game server got when it admitted the player, so you can match voices to
  players. Never show it. `local_id()` is this client's.
- A player on two devices is one participant; their audio is summed.
- Against a voice service without a roster, ids are SFU endpoint ids, there
  are no display names, and participants appear when they first speak.

## Use

```rust
use gridspawn_voice::{TransmitMode, VoiceEvent, VoiceOptions, VoiceSession};

// `client` is a connected gridspawn_client::Client; `session` was attached
// with a ticket from your game server that includes the voice module.
let voice = VoiceSession::join(&client, &session, VoiceOptions::default()).await?;
// Another room the game server configured: VoiceOptions::default().room("lobby")
let mut events = voice.events();

// Your audio callback or game loop, every 10–20 ms:
voice.push_capture(&microphone_samples);        // mono, 48 kHz
voice.pull_mix_stereo(&mut speaker_frames);     // what to play (interleaved L, R; or pull_mix for mono)
voice.push_playback_reference(&what_you_played); // echo reference, mono (include game audio)

// Controls
voice.set_muted(true).await?;                    // nothing is sent; the SFU producer pauses
voice.set_transmit_mode(TransmitMode::PushToTalk);
voice.set_transmitting(key_down);                // push-to-talk key
voice.set_participant_volume(&participant_id, 0.5);
voice.set_participant_muted(&participant_id, true).await?; // for this client only
voice.set_deafened(true);
voice.leave().await;
```

- **Transmitting:** voice activation is the default (open microphone, sent
  while speech is detected). Push-to-talk and continuous are the
  alternatives.
- **Events:** `ParticipantJoined`, `ParticipantLeft`, `SpeakingStarted`,
  `SpeakingStopped`, `LocalSpeaking`, `Audibility`, `RoomConfig`,
  `Authority`, `ServerMuted`, `LocalServerMuted`, `ReceiveFailed` (a
  speaker could not be received; the session goes on), `Warning`,
  `Error { code, … }`, `Left`. Events before you subscribe are not
  replayed: read `participants()`, `room_config()` and
  `authority_available()` after joining.
- **Per-participant audio:** `pull_participant(id, …)` (mono) or
  `pull_participant_stereo(id, …)` gives one participant's audio, for
  example to place it in your engine. Use the per-participant pulls **or**
  the mix pulls, not both.
- **Local mute** (`set_participant_muted`): the SFU stops sending that
  participant's audio to this client. Unmuting someone the game server
  currently keeps inaudible is not an error; they are heard again when the
  server allows. Deafening (`set_deafened`) only stops playback.
- **Rooms:** `match` exists in every session. Other rooms exist when the game
  server configured them and the voice service has named rooms on; otherwise
  joining answers `voice/room_not_supported` or `voice/room_not_configured`.
  A full room answers `voice/room_full`.
- **Threading:** the PCM calls never wait on the network. Call them from your
  audio or game thread. Everything else runs on the tokio runtime that called
  `join`.
- **Authorization renewal** (SFU token and voice grant) is automatic. If the
  backend moves the session to another node, you get
  `Error { code: "voice/reconnect_required" }`: join again.

## Building from source

**Players of the Godot addon get prebuilt binaries and never need any of
this.** To build this crate yourself:

- **Rust 1.91 or newer.** The WebRTC crates (rtc 0.21) need it: rcgen
  0.14.8 and later require 1.88, and rtc-mdns uses the standard library's
  `ip_from` feature, stable since 1.91.
- **Linux:** meson and ninja, CMake (libopus), a C++17 compiler, and
  libclang (for bindgen).
- **Windows:** Visual Studio 2022 with the x64 developer environment
  (`vcvars64`), CMake, Python with meson, ninja, and LLVM (libclang, for
  bindgen).
  Keep the checkout path short (or enable long paths): the bundled sources
  sit deep under Cargo's `OUT_DIR`, and paths over 260 characters fail.
- **Both:** network access at build time: meson fetches abseil through its
  wrap.

## Tests

```sh
cargo test --lib -- --nocapture
GRIDSPAWN_TEST_APP_ID=… GRIDSPAWN_TEST_CREDENTIAL_FILE=/path/to/server-key \
cargo test --test live -- --nocapture --test-threads=1
```

- **Unit tests** (no network): the SDP bridge, signaling frames, admission
  checks, the roster and audibility events, the pan law, voice-activity
  detection, echo cancellation through the pipeline (ERLE), and the
  impairment gate. The gate simulates loss and jitter in front of the real
  Opus encoder, the jitter buffer and the decoder.
- **Live tests:** headless clients with different tones on the live SFU,
  through `gridspawn-server` (application with guests and voice). They
  check that:
  - everyone hears exactly the other two;
  - a muted speaker delivers zero packets;
  - leaving and rejoining work;
  - with the game server's rules (channels, range, mutes): inaudible pairs
    deliver zero packets, the range gain follows the curve, server and local
    mutes work, a player without a state is silent both ways, and the
    roster carries the game server's ids and display names.
- With `GRIDSPAWN_TEST_VOICE_LONG=1`, a 5.5-minute session must cross SFU
  token renewals and a voice grant renewal.

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
