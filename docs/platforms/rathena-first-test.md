# rAthena voice: first test

A checklist for a studio's first voice session with the rAthena companion:
build the pieces, set up the game in the Dev Hub, then four steps from one
test character to party and guild voice between real players. Plan about two
hours for the first run.

Background: `rathena-companion.md` (design, C surface) and
`rathena-packets.md` (map-server ↔ client packets).

## 1. Prerequisites (Windows build machine)

- **Visual Studio 2022** with the C++ tools for **x64** and **x86**
  (toolset v143). The server pieces and the companion are x64; the game-side
  DLL and the example client are x86.
- **Rust 1.91 or newer** through rustup, plus the 32-bit target:
  `rustup target add i686-pc-windows-msvc`.
- For the companion's voice build: **CMake**, **Python with meson**,
  **ninja**, and **LLVM** with `LIBCLANG_PATH` pointing at LLVM's `bin`
  folder. The build fetches some sources over the network (meson's abseil
  wrap).
- **The tested versions** (MSVC toolset, LLVM, meson, ...) are listed in
  `gridspawn-companion`'s README, under "Build (Windows)", together with the
  environment variables to set. Start from those: the voice build is
  sensitive to the MSVC and LLVM versions.
- **Use the tested toolchain.** Build the companion with MSVC toolset
  14.44.35207 (`vcvars64.bat -vcvars_ver=14.44`), LLVM/libclang 22.1.5 at
  `LIBCLANG_PATH`, meson and ninja 1.12.1 and Rust 1.95.0. Newer MSVC
  toolsets are not tested and need a newer Clang: with an older LLVM,
  bindgen stops with MSVC's `STL1000` error. Use one toolset for the whole
  build. After any toolset or LLVM change, run
  `cargo clean --release -p webrtc-audio-processing-sys`: the WebRTC meson
  build directory keeps the compiler it was first configured with, and a
  build that mixes toolsets fails, for example with C2440 in
  `rtc_base/checks.h`.
- **A short checkout path**, for example `C:\gs`. The WebRTC audio build
  compiles sources through deep paths under `target\`; under a long root,
  cl.exe stops with "Cannot open source file" (its 260-character limit).

## 2. Build order

Paths are relative to the SDK repository's `rust` folder. The examples end
up in `examples\windows\<name>\build\Release\` of their crate.

1. **Server library** (x64 Native Tools Command Prompt for VS 2022):
   ```bat
   cd gridspawn-server-capi
   cargo build --release
   msbuild examples\windows\rathena_voice\rathena_voice.vcxproj /p:Configuration=Release /p:Platform=x64
   ```
   The `Release` configuration copies `gridspawn_server_capi.dll` next to
   `rathena_voice.exe`. Compile your own map-server code that links the
   library with `/MD`.
2. **Game-side DLL** (x86 tools, for example `vcvarsall.bat x64_x86`):
   ```bat
   cd gridspawn-rathena-dll
   cargo build --release --target i686-pc-windows-msvc
   ```
3. **Companion** (x64 Native Tools prompt):
   ```bat
   cd gridspawn-companion
   set CC=cl
   set CXX=cl
   set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"
   cargo build --release
   ```
4. **Example game client** (x86). It needs steps 2 and 3 first: its
   post-build step copies `gridspawn_rathena.dll` and
   `gridspawn-companion.exe` next to `rathena_client.exe`.
   ```bat
   cd gridspawn-rathena-dll
   msbuild examples\windows\rathena_client\rathena_client.vcxproj /p:Configuration=Release /p:Platform=Win32
   ```

These are unsigned development builds, for your own testing only. Players
get the signed package (section 7).

## 3. Dev Hub setup (the game's owner, in the Gridspawn desktop app)

1. **Dev Hub → Game servers:** connect the game (**Connect a game server**).
   This creates the game's application; its App ID starts with `game-`.
2. **New server key.** Every new key is made for the **Production**
   environment and is shown once, together with the App ID. Store it in a
   file that only the map-server's account can read (NTFS permissions). Keys
   carry the scopes `club.post` and `session.manage`. Keep keys on the server,
   never in the game client.
3. **Sign-in and modules** (in the same game panel), with the **Production**
   chip selected:
   - **Modules:** turn on **Voice**.
   - **Guests:** turn on **Players without a Gridspawn account**, and under it
     guest **Voice** (and **Voice position** if you want positional voice).
   - **Save.** Only the game's owner can change these settings.

Use the **production** environment everywhere in this test: set
`GRIDSPAWN_ENVIRONMENT=production` for the server and the example client
(`GsRagConfig.environment` NULL also means production; the examples use
production when the variable is unset).

## 4. First session: four steps

1. **Build** (section 2) and **set up** the game (section 3).
2. **One character, one machine.** Get a ticket from the server example and
   attach it with the example client:
   ```bat
   set GRIDSPAWN_APP_ID=game-...
   set GRIDSPAWN_ENVIRONMENT=production
   set GRIDSPAWN_CREDENTIAL_FILE=C:\keys\gridspawn-server-key
   rathena_voice.exe prontera 150001 "Aria Swordsman" 42 7 --print-ticket
   rathena_client.exe <ticket> --window --log companion.log
   ```
   `--print-ticket` prints the party ticket once (testing only; a real
   map-server never prints tickets). The companion window opens; join voice
   and check that it shows the character name. A ticket is valid for 120
   seconds and for one use: attach it right away. A wrong but well-formed
   key passes connect and fails at the first session create with status 7,
   code `server/authentication_required`; a malformed or unreadable key
   file fails at `gs_credential_from_file` with status 2.
3. **Your map-server and client, two players in a party.** Add the packets
   of `rathena-packets.md` to your map-server and client. The opcodes
   `0x0CF0`–`0x0CF8` are **placeholders**: replace them with free opcodes in
   your packet database before you build. Two characters in one party, on
   two PCs with headsets, join party voice and hear each other.
4. **Switch and moderation.** One of them joins a guild's voice: the
   map-server admits them to the guild's session and sends a new ticket; the
   client attaches it (the party session is left first). Then try a server
   mute and unmute from the map-server.

Optional, before step 3: the companion's automated two-client test
(`gridspawn-companion\examples\two_client_voice.rs`) runs step 2 for two
characters and a party → guild switch, with a test tone instead of the
microphones. Build the companion with `--features testing` (before building
the example client), set `GRIDSPAWN_TEST_APP_ID` to your App ID and
`GRIDSPAWN_TEST_CREDENTIAL_FILE` to your key file, then run
`cargo run --release --features testing --example two_client_voice -- <path\to\rathena_client.exe>`
from `gridspawn-companion`. A testing build is for this test only, never for
players.

## 5. Limits

- **64 players per voice room.** A session has one voice room clients can
  join, and it holds at most 64 players in voice.
- **One session per voice group:** one per party, one per guild. A player is
  in one voice group at a time.
- **No load test yet** for thousands of concurrent group sessions. Plan a
  large launch with Gridspawn beforehand.

## 6. Not in this first test

- **Gridspawn sign-in and presence:** this test uses guests only.
- **Signed distribution:** see section 7.
- **A Linux map-server:** the server example here is a Windows x64 project.
- **Several map-servers** sharing voice groups.

## 7. Distribution

Development builds are unsigned and are not for players. The distributable
package (DLL and companion, signed with Gridspawn's code-signing certificate)
is made by `gridspawn-companion\package_windows.ps1`; ask Gridspawn for a
signed build when you are ready to ship.
