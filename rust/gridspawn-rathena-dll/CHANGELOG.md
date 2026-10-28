# Changelog

## 0.1.0 (unreleased)

- First version: `gridspawn_rathena.dll` for x86 Windows, `__cdecl` C ABI in
  `include/gridspawn_rathena.h` (lifecycle, sign-in, player assertion, attach
  with detach-first semantics, `gs_rag_detach`, voice, devices, presence,
  14 callbacks).
- Starts `gridspawn-companion.exe` and talks to it over two named pipes
  (protocol version 1).
- Visual Studio x86 example `examples/windows/rathena_client`.
