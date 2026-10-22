# Third-party code in this addon

The libraries in `bin/` are built from the Gridspawn SDK (MIT OR Apache-2.0)
and include, unmodified:

- **godot-rust (gdext)**, crate `godot` 0.5.5 and its `godot-*` crates:
  Mozilla Public License 2.0. Source: <https://github.com/godot-rust/gdext>
  and <https://crates.io/crates/godot/0.5.5>. Licence text:
  <https://mozilla.org/MPL/2.0/>.
- Other Rust crates under permissive licences (MIT, Apache-2.0, BSD, ISC and
  similar). The exact list and versions are in the SDK's
  `godot/gridspawn/Cargo.lock`.
