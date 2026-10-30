# Changelog

## 0.1.1 (unreleased)

- `gs_rag_show_window(int32_t visible)`: shows or hides the companion's voice
  window. Its close button now only hides the window (companion 0.1.1).
  Additive: the struct layouts and the other functions are unchanged; a
  companion older than 0.1.1 answers it with `on_error("companion/unknown_op")`.
- The `rathena_client` example takes `--show-window-file <file>` to show or
  hide the window from a test, and `--no-shutdown` to exit without
  `gs_rag_shutdown`.

## 0.1.0 (unreleased)

- First version: `gridspawn_rathena.dll` for x86 Windows, `__cdecl` C ABI in
  `include/gridspawn_rathena.h` (lifecycle, sign-in, player assertion, attach
  with detach-first semantics, `gs_rag_detach`, voice, devices, presence,
  14 callbacks).
- Starts `gridspawn-companion.exe` and talks to it over two named pipes
  (protocol version 1).
- Visual Studio x86 example `examples/windows/rathena_client`.
