# Gridspawn SDK (public)

Server-side SDKs and the wire protocol for integrating a game with
[Gridspawn](https://gridspawn.net): game sessions, player and guest admission,
client tickets for voice and presence, and posting from your game server into
your game's club.

This repository is a read-only export. Changes are made upstream and synced
here; each commit names the upstream commit it came from.

## Contents

| Path | What |
|---|---|
| `rust/gridspawn-server` | Rust server SDK (async, tokio). Optional `blocking` feature. |
| `rust/gridspawn-server-capi` | C ABI of the same SDK for C, C++ and Unreal (`include/gridspawn_server.h`, static and shared library; Linux tested). |
| `docs/protocol` | The wire protocol: login (01), sessions and module grants (02), presence (03, proposed), voice (04), club posts from game servers (05). |

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
`rust/gridspawn-server-capi/README.md`.

Your server key comes from the Gridspawn Dev Hub (Game servers → New server
key). It is a secret for your game server only: never ship it in a game client.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

---

Synced from upstream commit `bf4d817de`.
