# Third-party code in this addon

The libraries in `bin/` are built from the Gridspawn SDK (MIT OR Apache-2.0)
and include, with unmodified sources:

- **godot-rust (gdext)**, crate `godot` 0.5.5 and its `godot-*` crates:
  Mozilla Public License 2.0. Source: <https://github.com/godot-rust/gdext>
  and <https://crates.io/crates/godot/0.5.5>. Licence text:
  <https://mozilla.org/MPL/2.0/>.
- **WebRTC audio processing** (echo cancellation, noise suppression, gain),
  as packaged by <https://gitlab.freedesktop.org/pulseaudio/webrtc-audio-processing>
  2.1 and built by the Rust crate `webrtc-audio-processing-sys` 2.1.0 (its
  build script has small changes; the C++ sources do not):
  BSD-3-Clause-style licence, `LICENSE-webrtc-audio-processing.txt`.
- **Abseil** (`abseil-cpp` 20240722.0), used by WebRTC audio processing:
  Apache License 2.0, `LICENSE-APACHE`.
- **libopus** 1.6.1, built by the Rust crate `opusic-sys` 0.7.5:
  BSD-3-Clause, `LICENSE-libopus.txt`.
- Other Rust crates under permissive licences (MIT, Apache-2.0, BSD, ISC and
  similar), including webrtc-rs (`webrtc` and `rtc` 0.21.1, MIT OR
  Apache-2.0). The exact list and versions are in the SDK's
  `godot/gridspawn/Cargo.lock`.
