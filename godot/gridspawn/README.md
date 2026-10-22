# gridspawn-godot

The Godot 4 GDExtension for Gridspawn. It is built on the Rust crates
`gridspawn-client` (`../../rust/gridspawn-client`) and `gridspawn-voice`
(`../../rust/gridspawn-voice`). The addon that game developers copy is
`addons/gridspawn`: its README is the user documentation.

Status: 0.2.0, source only. Licence: MIT OR Apache-2.0 (gdext, the Godot
bindings, is MPL-2.0: `addons/gridspawn/THIRD-PARTY.md`).
- Godot 4.3 or later (gdext `api-4-3`).
- Rust 1.94 or later for this crate.

## Build

The voice code compiles C and C++ (WebRTC audio processing, libopus), so
building needs the tools listed in `../../rust/gridspawn-voice/README.md`
("Building from source"): meson, ninja, CMake, libclang, and on Windows the
Visual Studio developer environment and a short checkout path.

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
cargo test --test headless -- --nocapture --test-threads=1
```

The tests:
- make a session, guests and voice tickets with `gridspawn-server`;
- copy the addon and the freshly built library into `../example`;
- import the project headless, then run it.

`headless_attach` checks restore, an assertion while signed out, attach,
renew, a forged ticket, and presence refused while signed out (guests get no
presence grant), through the GDScript API and signals.
`headless_voice` runs two Godot instances in one voice room. Each sends its
own tone through `input_source = Custom` (headless Godot has no microphone)
and checks that it receives and plays the other's tone; it also prints each
instance's join-time breakdown.

On Windows, use the `_console.exe` Godot build. Sign-in with a browser and a
real microphone and speakers are manual tests.

Run Cargo from this directory; its `target/` stays local.
