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

Status: 0.1.1, source only, not on crates.io yet. Licence: MIT OR Apache-2.0.
WebRTC's audio processing is vendored with build fixes: `../vendor/README.md`.

## What this version does not do yet

These are limits of the voice backend today, not of this crate:
- **One room per game session, everyone hears everyone.** Groups, teams and
  proximity need server-side audibility, which is not built yet.
- **Participants are opaque ids with no display names.** The SFU does not
  send a roster yet. Never show these ids to players.
- **Deafening is local.** Playback stops, but remote audio keeps arriving:
  the SFU has no per-listener pause yet.
- **No server mute and no audibility or gain events.**

## Use

```rust
use gridspawn_voice::{TransmitMode, VoiceEvent, VoiceOptions, VoiceSession};

// `client` is a connected gridspawn_client::Client; `session` was attached
// with a ticket from your game server that includes the voice module.
let voice = VoiceSession::join(&client, &session, VoiceOptions::default()).await?;
let mut events = voice.events();

// Your audio callback or game loop, every 10–20 ms:
voice.push_capture(&microphone_samples);        // mono, 48 kHz
voice.pull_mix(&mut speaker_samples);           // what to play
voice.push_playback_reference(&what_you_played); // echo reference (include game audio)

// Controls
voice.set_muted(true).await?;                    // nothing is sent; the SFU producer pauses
voice.set_transmit_mode(TransmitMode::PushToTalk);
voice.set_transmitting(key_down);                // push-to-talk key
voice.set_participant_volume(&participant_id, 0.5);
voice.set_deafened(true);
voice.leave().await;
```

- **Transmitting:** voice activation is the default (open microphone, sent
  while speech is detected). Push-to-talk and continuous are the
  alternatives.
- **Events:** `ParticipantJoined`, `ParticipantLeft`, `SpeakingStarted`,
  `SpeakingStopped`, `LocalSpeaking`, `Error { code, … }`, `Left`.
- **Per-participant audio:** `pull_participant(id, …)` gives one
  participant's audio (for 3D placement in your engine). Use it **or**
  `pull_mix`, not both.
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
  checks, voice-activity detection, echo cancellation through the pipeline
  (ERLE), and the impairment gate. The gate simulates loss and jitter in
  front of the real Opus encoder, the jitter buffer and the decoder.
- **Live test:** three headless clients with different tones on the live
  SFU, through `gridspawn-server` (application with guests and voice). It
  checks that:
  - everyone hears exactly the other two;
  - a muted speaker delivers zero packets;
  - leaving and rejoining work.
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
