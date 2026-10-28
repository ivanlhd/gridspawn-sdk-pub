# gridspawn-rathena-dll

The game-side half of the Gridspawn companion for rAthena / Ragexe clients.
`gridspawn_rathena.dll` is a small 32-bit (x86) Windows DLL with a C ABI
(`__cdecl`). It starts `gridspawn-companion.exe` (64-bit, the
`gridspawn-companion` crate) and drives it over two local named pipes. Voice,
Gridspawn sign-in and presence run in the companion, so the game process gets
no audio threads, no async runtime and no TLS.

Design and process model: `docs/platforms/rathena-companion.md`.
The contract is the hand-written header `include/gridspawn_rathena.h`.

## Build (Windows)

From this directory, with the Visual Studio 2022 x86 tools installed:

```
rustup target add i686-pc-windows-msvc
cargo build --release --target i686-pc-windows-msvc
```

The DLL is `target\i686-pc-windows-msvc\release\gridspawn_rathena.dll`. Put it
next to `gridspawn-companion.exe` (or set `GsRagConfig.companion_path`).

Development builds are unsigned. Players get the signed package made by
`gridspawn-companion/package_windows.ps1` (DLL and companion, Azure Trusted
Signing).

## Use

1. `LoadLibraryW(L"gridspawn_rathena.dll")` and `GetProcAddress` (the header
   has the function pointer types), never from `DllMain`.
2. `gs_rag_init(&config, &callbacks, user)`: starts the companion and connects
   (blocking, bounded by `init_timeout_ms`).
3. `gs_rag_attach(ticket)` with the ticket your map-server issued
   (protocol 02 §5). Attaching another ticket detaches the current session
   first (`on_detached("replaced")`) and joins voice again if it was joined.
4. `gs_rag_voice_join()`, then the voice calls; results arrive through the
   callbacks, on one thread the DLL owns.
5. `gs_rag_shutdown(timeout_ms)` before unloading.

Show players display names only (the names the callbacks deliver).
Participant ids are for calls such as `gs_rag_voice_set_volume`.

## Example

`examples/windows/rathena_client` is a Visual Studio x86 project that shows
load → init → callbacks → attach → voice → shutdown:

```
set GRIDSPAWN_APP_ID=game-...
set GRIDSPAWN_ENVIRONMENT=development
rathena_client.exe <ticket> [--seconds N] [--ptt] [--window] [--tone] [--log file]
                   [--next-ticket-file file]
```

A ticket for a local test comes from the server example
`gridspawn-server-capi/examples/windows/rathena_voice` with `--print-ticket`.

## Testing-only switches

- `GsRagConfig.test_tone` (`--tone` in the example): the companion sends a
  440 Hz tone instead of the microphone and plays nothing. For automated
  tests; never set it in a shipped client. Only a companion built with its
  `testing` feature honours it; the signed package's companion ignores it.
- `--next-ticket-file` (example only): when the file appears, its ticket is
  attached, standing in for the map-server's "switch group" packet.

## Licence

MIT OR Apache-2.0.
