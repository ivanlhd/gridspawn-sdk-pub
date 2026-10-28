# gridspawn-companion

The 64-bit companion process for 32-bit games (rAthena / Ragexe clients).
`gridspawn_rathena.dll` (crate `gridspawn-rathena-dll`) starts it, and it runs
`gridspawn-client` and `gridspawn-voice` for the game: session attach, voice
(microphone, speakers, Opus, the voice service), Gridspawn sign-in and
presence.

Design and pipe protocol: `docs/platforms/rathena-companion.md`.

- It talks only to the DLL that started it: two named pipes for the current
  user only, local clients only, and a random token from the DLL checked on
  both pipes.
- It exits with the game process.
- With `show_window` it shows a small voice window (egui): join or leave, mute,
  deafen, voice activity or push-to-talk, input and output devices, and the
  players in voice by display name, with a volume and "Mute for me" each.

Windows only. Development builds are unsigned. The distributable comes from
`package_windows.ps1`, which builds the DLL and the companion, signs both
(Azure Trusted Signing: `-AzureTrustedSigningDlibPath`,
`-AzureTrustedSigningMetadataPath`, or the environment variables
`GRIDSPAWN_TRUSTED_SIGNING_DLIB` / `GRIDSPAWN_TRUSTED_SIGNING_METADATA`),
verifies them with `signtool verify /pa`, and zips them into `dist\`.
`-SkipSigning` only checks the build and staging; its output is named
`-UNSIGNED` and is not for players.

## Build (Windows)

The voice crate builds Opus and WebRTC audio processing, so it needs the same
tools as the Godot voice build: Visual Studio 2022 (x64), meson and ninja on
`PATH`, and `LIBCLANG_PATH` pointing at LLVM's `bin`.

```
cargo build --release
```

The executable is `target\release\gridspawn-companion.exe`; put it next to
`gridspawn_rathena.dll`.

Keep the checkout path short (for example `C:\gs\rust`). The WebRTC audio
build reaches sources through deep relative paths under `target\`, and under
a long root cl.exe stops with "Cannot open source file" (its 260-character
path limit).

## Live test

`examples/two_client_voice.rs` plays the map-server (with `gridspawn-server`)
and runs two copies of the x86 example client: two guests in one party session
hear each other's test tone, then one of them switches to a guild session.

```
set GRIDSPAWN_TEST_APP_ID=...
set GRIDSPAWN_TEST_CREDENTIAL_FILE=...
cargo build --release --features testing
cargo run --release --features testing --example two_client_voice -- <path\to\rathena_client.exe>
```

The app must have guests enabled with voice.

## Testing-only switches

- `testTone` in the launch line (set by the DLL from `GsRagConfig.test_tone`):
  a 440 Hz tone instead of the microphone, nothing played, and the level of
  what is received written to the log each second. Only builds with the
  `testing` cargo feature have it; other builds ignore the flag (with a log
  warning) and use the microphone. `package_windows.ps1` builds without the
  feature and refuses binaries that contain the tone code.
- With the `testing` feature only, two environment variables:
  - `GRIDSPAWN_TEST_SIGNAL=<file.wav>` (16-bit PCM, mono, 48 kHz, looped)
    replaces the tone;
  - `GRIDSPAWN_TEST_PROCESSING=off` turns off echo cancellation, noise
    suppression and automatic gain.
  The log then shows the RMS sent and received each second. With the default
  processing a steady tone arrives about 13 dB down while speech arrives
  within 1 dB (see the design doc, §8).

## Licence

MIT OR Apache-2.0.
