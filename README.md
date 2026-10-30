# Gridspawn SDK (public)

SDKs and the wire protocol for integrating a game with
[Gridspawn](https://gridspawn.net).
- On the game server: game sessions, player and guest admission, client
  tickets for voice and presence, player presence, and posting into your
  game's club.
- On the game client: player sign-in with the system browser, player
  assertions, attaching to game sessions, player presence, and in-game
  voice, in Rust or as a Godot 4.3+ addon.
- For 32-bit Windows game clients (rAthena / Ragexe): a small x86 DLL with a
  C ABI and a 64-bit companion process that runs voice for the game.

This repository is a read-only export. Changes are made upstream and synced
here; each commit names the upstream commit it came from.

## Contents

| Path | What |
|---|---|
| `rust/gridspawn-server` | Rust server SDK (async, tokio): sessions, admission, tickets, player presence, club posts. Optional `blocking` feature. |
| `rust/gridspawn-server-capi` | C ABI of the same SDK for C, C++ and Unreal (`include/gridspawn_server.h`, static and shared library; Linux and Windows x64 (MSVC) tested). Includes a Visual Studio example for an rAthena-style map-server (`examples/windows/rathena_voice`). |
| `rust/gridspawn-client` | Rust client SDK for game clients: sign-in (PKCE, loopback), player assertions, session attach and module grants, and player presence (re-sent every 60 s). |
| `rust/gridspawn-voice` | Rust voice for game clients: joins an attached session's voice room (WebRTC audio, Opus, echo cancellation, noise suppression, jitter buffer) behind a PCM API. Building it compiles C and C++: see its README. |
| `rust/gridspawn-rathena-dll` | `gridspawn_rathena.dll`: the x86 (32-bit) game-side DLL for rAthena / Ragexe clients, C ABI (`__cdecl`, `include/gridspawn_rathena.h`). It starts the companion and drives it over local pipes, with no network or audio code of its own. Includes a Visual Studio x86 example (`examples/windows/rathena_client`). |
| `rust/gridspawn-companion` | `gridspawn-companion.exe`: the 64-bit companion the DLL starts. It runs session attach and voice (and later sign-in and presence) for the game, with an optional voice window. Windows only. |
| `rust/vendor` | Third-party crates the voice crate builds with small build changes (see Third-party code). |
| `godot/gridspawn` | Godot 4.3+ addon (GDExtension) over the client SDKs: the `Gridspawn` autoload with sign-in, assertions, attach and presence as signals, and the `GridspawnVoice` node for in-game voice. Windows x86_64 and Linux x86_64. Prebuilt addon zips (Windows, Linux) with SHA-256 sums: [release godot-v0.4.0](https://github.com/ivanlhd/gridspawn-sdk-pub/releases/tag/godot-v0.4.0) (with voice, the game server's voice rules, and presence; [godot-v0.1.0](https://github.com/ivanlhd/gridspawn-sdk-pub/releases/tag/godot-v0.1.0) has sign-in and attach only); or build it from source (`godot/gridspawn/README.md`). |
| `godot/example` | Minimal Godot project used by the addon's headless tests. |
| `docs/protocol` | The wire protocol: login (01), sessions and module grants (02), presence (03), voice (04), club posts from game servers (05), live game data (06). |
| `docs/platforms` | Platform guides: `rathena-companion.md` (the DLL and companion design and C surface) and `rathena-packets.md` (the proposed packet contract between an rAthena map-server and its client for party and guild voice). |

## Install (Rust)

Not on crates.io yet. Use a git dependency pinned to a commit of this
repository; Cargo finds the package by name:

```toml
[dependencies]
gridspawn-server = { git = "https://github.com/ivanlhd/gridspawn-sdk-pub", rev = "<commit>" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Quickstart, sessions, retries and the error table:
`rust/gridspawn-server/README.md`. C, C++ and Unreal:
`rust/gridspawn-server-capi/README.md`. Game clients:
`rust/gridspawn-client/README.md`; voice: `rust/gridspawn-voice/README.md`.
Godot: `godot/gridspawn/addons/gridspawn/README.md`. rAthena / Ragexe:
`docs/platforms/rathena-companion.md`, `docs/platforms/rathena-packets.md`,
`rust/gridspawn-rathena-dll/README.md` and `rust/gridspawn-companion/README.md`.

Your server key comes from the Gridspawn Dev Hub (Game servers → New server
key). It is a secret for your game server only: never ship it in a game client.

## Third-party code

`rust/vendor/` holds copies of third-party crates, with the small build
changes listed in `rust/vendor/README.md`. They keep their own licences, not
this repository's:

| Path | Crate and upstream | Licence |
|---|---|---|
| `rust/vendor/webrtc-audio-processing-sys` | `webrtc-audio-processing-sys` 2.1.0, <https://github.com/tonarino/webrtc-audio-processing> | BSD-3-Clause (`COPYING`) |
| `rust/vendor/webrtc-audio-processing` | `webrtc-audio-processing` 2.1.0, <https://github.com/tonarino/webrtc-audio-processing> | BSD-3-Clause (`COPYING`) |

Prebuilt Godot addon libraries also contain other third-party code (the Godot
bindings, WebRTC audio processing, Abseil, libopus):
`godot/gridspawn/addons/gridspawn/THIRD-PARTY.md` lists it.

## Licence

Everything except `rust/vendor/` is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

---

Synced from upstream commit `04402157c`.
