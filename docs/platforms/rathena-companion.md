# rAthena companion

Voice, and later Gridspawn sign-in and presence, for a Ragnarok private server
(rAthena map-server, C++) and its patched Ragexe client (Windows x86, 32-bit,
WARP payloads). The design keeps everything that needs threads, TLS or audio
out of the 32-bit game process: a 64-bit **companion** process does the work;
the game loads only a thin **x86 DLL** that talks to it over local pipes.

Status: design accepted 2026-10-25 with the decisions in §10. The DLL, the
companion and the Windows examples are built and tested live (guest voice
between two example clients). Not on crates.io yet. Server side:
`gridspawn-server-capi` on Windows x64, with the example
`examples/windows/rathena_voice`. The packet contract between map-server and
client is `rathena-packets.md`.

## 1. Pieces

| Piece | Runs in | Built from |
|---|---|---|
| `gridspawn_rathena.dll` (x86, C ABI, `__cdecl`) | the game client (loaded by the WARP patch) | crate `gridspawn-rathena-dll` (Rust, `i686-pc-windows-msvc`): no TLS, no audio, no async runtime |
| `gridspawn-companion.exe` (x64) | its own process, one per game process | crate `gridspawn-companion`: `gridspawn-client` (sign-in, player assertion, attach, presence) + `gridspawn-voice` with the `cpal` feature (microphone, speakers, devices) + an egui window |
| map-server | the game server | `gridspawn-server-capi` (Windows x64 now, Linux later): sessions, guest or player admission, tickets, voice rules |

32-bit voice inside the game process is **not needed** in this design; the DLL
never handles audio or network traffic to Gridspawn.

## 2. Process model

- **Start.** The game calls `gs_rag_init` after loading the DLL (never from
  `DllMain`). The DLL generates a 32-byte token (OS random), starts
  `gridspawn-companion.exe` (path from the config, default next to the DLL)
  and writes the token, the game's process id and the app settings to the
  child's **stdin** (not the command line, which other processes can read).
- **Channel.** The companion creates two pipes, one per direction, because a
  synchronous Windows pipe handle serialises reads and writes:
  - `\\.\pipe\gridspawn-rathena-<game pid>-in` (DLL → companion: requests)
    and `…-out` (companion → DLL: replies and events);
  - DACL: the current user only; remote clients rejected; one instance each;
  - the DLL opens both and sends `hello {v: 1, token}` on each. The companion
    compares the token in constant time and closes both on a mismatch. Only
    the process that started it can drive it.
- **Lifetime.** The companion opens a handle to the game process and exits
  when the game exits (also on a crash), after leaving voice and clearing
  presence. `gs_rag_shutdown(timeout_ms)` asks it to leave cleanly first.
- **One companion per game process.** A second game instance starts its own
  (its own pipe names and token).

## 3. Pipe protocol

- Frames: u32 little-endian length + UTF-8 JSON, at most 64 KiB.
- Requests `{id, op, args}` → one reply `{id, ok, result | error{code, message}}`.
- Events (companion → DLL) `{event, ...}`, unsolicited.
- Versioned by `hello.v`; the companion answers its own version, so a newer
  companion works with an older DLL.

## 4. DLL threading

- Every `gs_rag_*` call returns at once (`GS_RAG_OK` or an argument/state
  error); results arrive through callbacks.
- The DLL owns one reader thread and one **callback thread**. Callbacks run
  only on that thread, never on the caller's (the game loop), one at a time.
  A callback must return quickly and may call `gs_rag_*` functions.
- `gs_rag_init` and `gs_rag_shutdown` are the only blocking calls (bounded
  by their timeouts).
- Strings: UTF-8, NUL-terminated, borrowed for the call or the callback.

## 5. C surface (`gridspawn_rathena.h`, all `__cdecl`)

The header is the contract.

| Area | Calls | Results / events |
|---|---|---|
| Lifecycle | `gs_rag_init(config, callbacks, user)`, `gs_rag_shutdown(timeout_ms)`, `gs_rag_version()` | `on_ready`, `on_companion_lost(reason)` |
| Sign-in (later; not needed for guests) | `gs_rag_sign_in()` (system browser now; the Gridspawn app flow of 01 §6 once the app ships), `gs_rag_sign_out()` | `on_signed_in(display_name)`, `on_signed_out` |
| Server proof | `gs_rag_player_assertion(request_id, challenge)` (01 §5) | `on_assertion(request_id, assertion)`: the game sends it to its server in a custom packet |
| Session | `gs_rag_attach(ticket)` (02 §5 ticket from the server), `gs_rag_detach()` | `on_attached(display_name, roster)`, `on_detached(reason)`: `"replaced"` or `"requested"` |
| Voice | `gs_rag_voice_join()`, `_leave()`, `_set_muted(b)`, `_set_deafened(b)`, `_set_transmit_mode(vad\|ptt)`, `_set_transmitting(b)` (push-to-talk key), `_set_volume(participant, 0..2)`, `_set_participant_muted(participant, b)` (local only), `_list_devices()`, `_select_device(input\|output, name)` | `on_voice_state(state)`, `on_participant(joined/left, id, display_name)`, `on_speaking(id, b)`, `on_local_speaking(b)`, `on_server_muted(id or self, b)`, `on_devices(kind, names, selected)` |
| Presence (signed-in players only) | `gs_rag_presence_set(details, state)`, `gs_rag_presence_clear()` | refused for guests (03: guests have no presence) |
| Errors | — | `on_error(code, message)`; codes are the SDK's (`voice/server_muted`, …) |

- **One session at a time.** `gs_rag_attach` while a session is attached
  first leaves its voice room and detaches it (`on_detached("replaced")`),
  then attaches the new ticket (`on_attached`). Voice joins again only when
  the game calls `gs_rag_voice_join()` (or immediately, when it was joined
  before the switch).
- `gs_rag_detach()` leaves voice and detaches (`on_detached("requested")`),
  for example when the server ends the player's group voice. It also cancels
  an attach still in progress. With no session attached it does nothing.
- `on_attached` delivers the voice room's roster (id + display name) as the
  companion knows it at that moment. The room is known only once voice is
  joined, so after a plain attach (and after a switch, which rejoins
  afterwards) the roster is empty; the players already in the room then
  arrive as `on_participant(joined)` right after `on_voice_state(joined)`.
- `on_devices` arrives once per kind after `on_ready`, and again after
  `gs_rag_voice_list_devices` and `gs_rag_voice_select_device`.
- Actions taken in the companion window go through the same code with no
  reply to the DLL; a failed window action shows in the window's status line,
  not as `on_error`.
- Everything a player sees uses display names (character names for guests,
  Gridspawn display names otherwise); ids stay internal.

## 6. Flows

The map-server keeps **one session per voice group** (a party, or a guild),
created when the group's first member joins voice and ended when it empties.
A player is in exactly one voice group at a time.

**Guest voice (no Gridspawn account).**
1. The map-server admits the player into the group's session as a guest
   (`externalPlayerId` = an opaque per-character id, display name = character
   name; `rathena-packets.md` §2),
   sets their voice state and issues a voice ticket (120 s, single use).
2. It sends the ticket in a custom packet; the client calls
   `gs_rag_attach(ticket)` then `gs_rag_voice_join()`.

**Switching group (party ↔ guild, GvG start/end).** The map-server admits the
player into the other group's session, issues a ticket for it and sends it.
The client calls `gs_rag_attach(new ticket)`: the companion leaves and
detaches the old session (`on_detached("replaced")`) and joins the new one.
The server stops renewing a group session once nobody is left in it.

**Gridspawn player (later).** `gs_rag_sign_in` → the server sends a challenge
→ `gs_rag_player_assertion` → the server admits the player
(`gs_admit_participant`) → ticket → attach, as above; presence becomes
available.

The packet contract: `rathena-packets.md`.

## 7. Companion window (first version)

A small window in the companion (egui): microphone and speaker pickers, mute
/ deafen, push-to-talk or voice activity, and the room's roster with display
names, speaking indicators and per-player volume. Plain and readable; no
nested panels. The game can drive all of it through the DLL too.
"Join voice" is available only while a session is attached. After a detach
the window shows "Not attached" until the game attaches the next ticket; the
window cannot attach by itself, since tickets come from the game server.

## 8. Example

A Visual Studio x86 project (`examples/windows/rathena_client`): load the DLL
(`LoadLibrary`), `gs_rag_init` with callbacks that print, attach with a ticket
given on the command line, join voice, toggle push-to-talk, shut down cleanly.
It runs against the server example of `gridspawn-server-capi`
(`examples/windows/rathena_voice`), which prints a ticket only with its
explicit test flag (`--print-ticket`).

Testing-only switches (not for shipped clients): `test_tone` in
`GsRagConfig` (`--tone`: a 440 Hz tone instead of the microphone, nothing
played, the received level logged each second) and the example's
`--next-ticket-file` (stands in for the map-server's switch packet).
`gridspawn-companion/examples/two_client_voice.rs` uses both: two guests in
one party session hear each other, then one switches to a guild session.
In a testing build, two environment variables (inherited from the game
process) change the test signal: `GRIDSPAWN_TEST_SIGNAL=<wav>` sends a looped
16-bit, mono, 48 kHz WAV instead of the tone, and
`GRIDSPAWN_TEST_PROCESSING=off` joins voice with echo cancellation, noise
suppression and automatic gain off (gridspawn-voice's high-pass filter stays
on). The companion logs the RMS sent and received each second.

Measured 2026-10-25 (two example clients, a development app with guest voice,
received at one client vs sent by the other, over the seconds both were in
the party session):

| Signal | Capture processing | Received vs sent |
|---|---|---|
| 440 Hz tone, RMS 0.141 | on (default) | −13.0 / −12.3 dB: it starts near 0.07 and settles at 0.031 within about 4 s |
| 440 Hz tone | off | −0.4 dB |
| speech (synthesised phrase, RMS 0.098) | on (default) | −0.8 dB both ways |
| speech | off | −0.9 / −1.0 dB |

Speech comes through at the expected level with the default processing, so
the tone's attenuation is expected processing behaviour, not an audio fault.
The likely cause is that noise suppression and/or automatic gain treat a
steady tone as noise; which of the two is responsible was not isolated.

The tone exists only in companion builds with the `testing` cargo feature;
the signed package is built without it, and `package_windows.ps1` checks
both packaged binaries for the tone code and the `--next-ticket-file`
switch and stops if either is present.

Building the companion on Windows: keep the checkout path short, for example
a top-level folder on the build drive. The WebRTC audio build compiles
sources through deep relative paths under `target\`. Under a long checkout
path, the release build stopped with "Cannot open source file" for an abseil
source that exists; from a short path the same build succeeded. That is
consistent with cl.exe's 260-character path limit.

## 9. Limits and sizing (current protocol)

- **One voice room per session, 64 players.** Clients join only `match`
  (04 §1); a room holds at most 64 players in voice (04 §4.2). The
  per-voice-group layout fits parties (12) and GvG guilds (20–30). If GvG
  ever needs more than 64 players in one room, the path is a protocol change
  (more joinable rooms per session, or larger rooms), with server-side work
  at Gridspawn.
- **Per session, the server library does:**
  - create once (02 §1), admit per member (02 §3/§4), one ticket per member
    and join (02 §5);
  - one lease renewal every 15 s while the session lives (`gs_session_spawn_renewer`,
    one task per session; 02 §2): 1,000 live group sessions are about 67
    renewals a second from the voice owner's process (`rathena-packets.md`
    §1);
  - a voice configuration once (5 per 10 s allowed), and a voice state write
    per member change (10/s per participant, 200/s per session; 04 §4.5);
  - to end a session, stop its renewer: the lease lapses 45 s later (02 has
    no end-session call).
- **Capacity at scale:** thousands of concurrent group sessions and their
  voice players have not been load-tested yet. Plan a large launch with
  Gridspawn beforehand.

## 10. Decisions (2026-10-25)

1. Session layout: one session per voice group (§6). More than 64 players
   in one room needs the protocol change in §9.
2. Companion window: egui.
3. Distribution: unsigned builds for development only; production builds are
   signed with Gridspawn's Azure Trusted Signing certificate (same as the
   desktop app). `package_windows.ps1` in the `gridspawn-companion` crate produces
   the distributable: it builds both binaries, signs `gridspawn_rathena.dll`
   and `gridspawn-companion.exe` (signtool, Trusted Signing dlib + metadata,
   timestamp `http://timestamp.digicert.com`), verifies each with
   `signtool verify /pa`, and zips them with the header and licences. The
   dlib and metadata paths are parameters or environment variables only and
   are never printed or committed.
