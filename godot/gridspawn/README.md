# gridspawn-godot

The Godot 4 GDExtension for Gridspawn. It is built on the Rust crate
`gridspawn-client` (`../../rust/gridspawn-client`). The addon that game
developers copy is `addons/gridspawn`: its README is the user documentation.

Status: 0.1.0, source only. Licence: MIT OR Apache-2.0 (gdext, the Godot
bindings, is MPL-2.0: `addons/gridspawn/THIRD-PARTY.md`).
- Godot 4.3 or later (gdext `api-4-3`).
- Rust 1.94 or later for this crate.

## Build

- Linux x86_64: `cargo build --release` gives `target/release/libgridspawn_godot.so`.
- Windows x86_64: build on Windows with the MSVC toolchain. `cargo build
  --release` gives `target\release\gridspawn_godot.dll`.
- `./package.sh [gridspawn_godot.dll]` assembles
  `dist/gridspawn-godot-<version>/addons/gridspawn` with both libraries, and
  a zip.

## Tests

```sh
cargo build    # first: `cargo test` does not build the cdylib itself
GRIDSPAWN_GODOT_BIN=/path/to/Godot_v4.3-stable_linux.x86_64 \
GRIDSPAWN_TEST_APP_ID=… GRIDSPAWN_TEST_CREDENTIAL_FILE=/path/to/server-key \
cargo test --test headless -- --nocapture
```

The test:
- makes a session, a guest and a voice ticket with `gridspawn-server`;
- copies the addon and the freshly built library into `../example`;
- imports the project headless, then runs it.

Through the GDScript API and signals, it checks restore, an assertion while
signed out, attach, renew, and a forged ticket. On Windows, use the
`_console.exe` Godot build. Sign-in with a browser is a manual test.

Run Cargo from this directory; its `target/` stays local.
