# rAthena voice packets — Proposed

The packet contract between an rAthena map-server and a Ragexe client that
loads `gridspawn_rathena.dll` (`rathena-companion.md`). It covers guest voice
per party or guild. Opcodes are **placeholders**: the rAthena team assigns
them for their packet version.

Protocol references: 01 §5 (player assertion), 02 §3/§4/§5 (admission and
tickets), 04 §4 (server-side voice control). rAthena references are from
`rathena/rathena` master (`src/common/{mmo.hpp,socket.cpp}`,
`src/config/packets.hpp`, `src/map/{party,guild,map,chrif,clif}.hpp`,
`src/map/clif.cpp`), read 2026-10-25.

## 1. Model

- **One Gridspawn session per voice group** (a party or a guild). It is
  created when the group's first member asks for voice, and ended when the
  last one leaves: its renewer is stopped and the lease lapses 45 s later
  (02 §2; 02 has no end-session call).
- **A player is in at most one voice group at a time.** Joining another
  group's voice is a switch: the player gets a ticket for the other session,
  and the client's `gs_rag_attach` detaches the previous one first
  (`on_detached("replaced")`) and joins voice again.
- **Voice owner.** One component holds the server credential and the
  sessions, using `gridspawn-server` (Rust) or `gridspawn-server-capi` (C).
  - With a single map-server, it can live in the map-server process.
  - With several map-servers it must be one component for the whole
    cluster (the char-server or a sidecar), because a party or guild spans
    map-servers.
  - Below, "the server" is the map-server that receives the client's
    packet, and "the owner" is this component. The map-server ↔ owner link
    is the rAthena team's choice.
- **Each session's voice room** is configured once, at creation (04 §4.1, §4.2):

  ```json
  {"rooms": {"match": {"mode": "channels", "spatial": "client",
                       "maxParticipants": 64, "maxAudibleSpeakers": 16}}}
  ```

  - Every member gets the state `{"channels": ["party:<party_id>"]}` (or
    `guild:<guild_id>`) when admitted. In a configured room, a member with
    no state is neither heard nor hearing (04 §4.3).
  - The configured room also gives fail-closed behaviour: if the owner
    stops renewing, the room goes silent 3 s after the lease expires
    (04 §4.4).
- **Removing a player** uses a state write with no channels,
  `{"channels": []}`. 02 has no route to remove a participant. In
  `channels` mode two living players hear each other only when they share a
  channel (04 §4.3), and an empty list shares nothing, so nobody hears them
  and they hear nobody. The write is accepted like any other state write
  (checked live). It applies on the voice service's side, so it holds even
  if a modified client stays connected to the SFU.

## 2. Identities

| Gridspawn field | rAthena value | Why |
|---|---|---|
| `externalSessionId` | `rathena-party-<party_id>` / `rathena-guild-<guild_id>` | readable in logs |
| session `Idempotency-Key` | `rathena-party-<party_id>-<generation>` | a new key per (re)creation; the owner increments `generation` each time it creates the group's session |
| guest `externalPlayerId` | `rathena-char-<char_id>` | per character, because the display name is the character's (02 §4: never an email, never equal to the display name) |
| guest `displayName` | the character name (`NAME_LENGTH` 24 with NUL, within 02 §4's 1–64) | shown to the other players |
| admit `Idempotency-Key` | `<session key>-<char_id>` | a repeated admit for the same character in the same session returns the same participant |

A character name that 02 §4 refuses (it looks like an email) cannot get guest
voice: the owner answers `ZC_GS_VOICE_REFUSED` with reason 4.

## 3. Packets

Little-endian, packed, rAthena style. **Opcodes are placeholders**
(`0x0Cxx`). The map-server's packet table only goes up to `MAX_PACKET_DB`
`0xCFF` (`clif.hpp`), so the real values must fall within it and be unused in
the packet version in use. Client → server packets must be registered in the
map-server's packet table. Server → client packets are handled by the
client patch that hooks the client's packet dispatch: the DLL has no network
code and never sees packets; the patch calls `gs_rag_*`.

| Placeholder | Name | Dir | Layout (bytes) |
|---|---|---|---|
| `0x0CF0` | `CZ_GS_VOICE_JOIN` | C→S | `u16 type; u8 group_kind` (0 party, 1 guild) — 3 |
| `0x0CF1` | `ZC_GS_VOICE_TICKET` | S→C | `u16 type; u16 len; u8 group_kind; u32 group_id; u8 reason; char ticket[len-10]` (ASCII, no NUL) |
| `0x0CF2` | `ZC_GS_VOICE_REFUSED` | S→C | `u16 type; u8 group_kind; u8 reason` — 4 |
| `0x0CF3` | `CZ_GS_VOICE_STATE` | C→S | `u16 type; u8 group_kind; u32 group_id; u8 state` (0 left, 2 joined) — 8 |
| `0x0CF4` | `CZ_GS_VOICE_LEAVE` | C→S | `u16 type` — 2 |
| `0x0CF5` | `ZC_GS_VOICE_END` | S→C | `u16 type; u8 group_kind; u32 group_id; u8 reason` — 8 |
| `0x0CF6` | `ZC_GS_VOICE_GROUP_INFO` (optional) | S→C | `u16 type; u8 group_kind; u32 group_id; u8 in_voice` — 8 |

Values:
- `ZC_GS_VOICE_TICKET.reason`: 0 asked for (`CZ_GS_VOICE_JOIN`), 1 switched
  by the server (for example at GvG start), 2 reissued (after a
  reconnect).
- `ZC_GS_VOICE_REFUSED.reason`:
  - 0 not in such a group;
  - 1 the group's voice is full (64 in voice: 04 §4.2's room cap; a guild
    can have up to `MAX_GUILD` = 16+10*6 = 76 members);
  - 2 voice unavailable now (a Gridspawn call failed; the client may retry
    after a few seconds);
  - 3 voice is off for this server (no credential, guests or voice
    disabled);
  - 4 this character name cannot be used for guest voice.
- `ZC_GS_VOICE_END.reason`: 0 the player left or was removed from the
  group, 1 the group was disbanded, 2 removed by a GM.

**The ticket is a secret** (02 §5: single use, 120 s). Send it only to that
player's connection (never to the area or the party), and never log it on
either side.

### Transport

The Ragnarok client connection is plain TCP: rAthena's socket layer
(`src/common/socket.cpp`) has no TLS. Its only packet transform,
`PACKET_OBFUSCATION` (`src/config/packets.hpp`), XORs the **packet ID of
client → server packets** (`clif_parse`, `src/map/clif.cpp`: "Decrypt the
current packet ID"). Payloads are untouched, and so are server → client
packets. `ZC_GS_VOICE_TICKET` therefore crosses the network readable.

What already limits a captured ticket:
- **Single use:** the first attach consumes it (02 §5); any later attach
  with it is refused.
- **Bound:** it is issued for one participant of one session (02 §5), and
  an attach for another app or environment is refused.
- **Short-lived:** 120 s (02 §5).
- **Sent to one connection:** only that player's, never broadcast.

The ticket is bound to the app, the environment, the session and the
participant only, not to an IP address, a device, the game connection or a
client key, so whoever attaches first within the 120 s holds the
participant's grants.

Residual risk:
- **Who can exploit it:** an attacker on the network path who reads the
  ticket and attaches before the player's client.
- **What they get:** they are attached as that participant, with that
  character's display name, in that group's voice.
- **What the player sees:** their client's attach fails with
  `401 session/ticket_invalid` ("The participant ticket is invalid,
  expired, or already attached."; the attach of 02 §6), and the client
  patch sends
  `CZ_GS_VOICE_JOIN` again (§3, client handling).
- **What the re-join does and does not do:** it gets a new ticket for the
  **same** participant (the admit is idempotent, §2), so it does not cut the
  attacker off.
- **What the server can do:** mute the participant (04 §4.1) or write
  `channels: []` for them. Both apply to the participant, so they affect
  the attacker and the player alike.
- **Scale:** an attacker who can read this TCP stream can read and inject
  the rest of the player's game traffic too. The ticket adds voice
  impersonation within one group to that existing exposure.

Recommendation: if the rAthena team's client patch already encrypts sensitive
packets (beyond `PACKET_OBFUSCATION`), use the same protection for
`ZC_GS_VOICE_TICKET` (and for `ZC_GS_VOICE_CHALLENGE` / `CZ_GS_VOICE_ASSERTION`
in §6). This document does not assume any such protection exists.

### Client handling (the client patch)

| Packet / event | The patch does |
|---|---|
| player chooses "party voice" / "guild voice" | send `CZ_GS_VOICE_JOIN(kind)` |
| `ZC_GS_VOICE_TICKET` | `gs_rag_attach(ticket)`; then, if voice is not joined yet, `gs_rag_voice_join()`. On a switch the companion rejoins by itself (`rathena-companion.md` §5) |
| `on_voice_state(joined / left)` | send `CZ_GS_VOICE_STATE(kind, id, 2 / 0)` for the group of the ticket last attached |
| `on_companion_lost` | send `CZ_GS_VOICE_STATE(kind, id, 0)` |
| player chooses "leave voice" | `gs_rag_detach()`, then send `CZ_GS_VOICE_LEAVE` |
| `ZC_GS_VOICE_END` for the attached group | `gs_rag_detach()` (leaves voice and detaches); show "Party voice ended" (or guild) |
| `ZC_GS_VOICE_REFUSED` | show the reason in words; never show ids |
| `on_error` with an attach failure (ticket expired) | send `CZ_GS_VOICE_JOIN(kind)` again |
| map-server login done (after `clif_parse_LoadEndAck`) while voice was on | send `CZ_GS_VOICE_JOIN(kind)` again (§4, reconnect) |

## 4. What the server does

| Event (rAthena) | Owner actions | Sent |
|---|---|---|
| `CZ_GS_VOICE_JOIN(kind)` | Check the character's `party_id` / `guild_id` (0 → reason 0). If the player is in another group's voice, remove them there first (state `[]`). Ensure the group's session: create, spawn renewer, configure. Check that fewer than 64 are in voice. Admit the guest (§2) and set state `{"channels": ["<kind>:<id>"]}`. Issue a voice ticket. Count the player as *pending* until `CZ_GS_VOICE_STATE(joined)` or 120 s | `ZC_GS_VOICE_TICKET(reason 0)`, or `ZC_GS_VOICE_REFUSED` |
| server-initiated switch (GvG start/end) | as above for the target group | `ZC_GS_VOICE_TICKET(reason 1)` |
| `CZ_GS_VOICE_STATE(kind, id, state)` | for the player's current group only (others are stale, ignored): joined → *in voice*; left → not counted, audibility kept, so a rejoin from the companion window works while the session lives | — |
| `CZ_GS_VOICE_LEAVE` | state `[]` in the current group; drop the player from it | — |
| `party_member_withdraw` (leave, kick) / `guild_member_withdraw` | if that character is in the group's voice: state `[]`, drop | `ZC_GS_VOICE_END(reason 0)` to them if online |
| `party_broken` / `guild_broken` | state `[]` for every member in voice; stop the session's renewer | `ZC_GS_VOICE_END(reason 1)` to each online member |
| `party_member_added` / `guild_member_added` | nothing (voice is opt-in) | optional `ZC_GS_VOICE_GROUP_INFO` |
| map change on the same map-server (`clif_changemap`) | nothing: voice follows the group, not the map | — |
| map-server change (`chrif_changemapserver`) | nothing with a cluster-wide owner; the client keeps its attachment | — |
| `map_quit` (logout, disconnect) | start a quit grace (proposed 10 s); when it ends: state `[]`, drop. A `CZ_GS_VOICE_JOIN` from the same character during the grace cancels it (a map-server change, a reconnect) | — |
| GM mute | `mute` route (04 §4.1); the client gets `on_server_muted` from the SFU | — |
| group voice empty (no *pending* or *in voice* for a grace, proposed 30 s) | stop the renewer; the lease lapses 45 s later. The next join creates a new session (`generation` + 1) | — |

Reconnect: a join for the group the player is already in re-admits
(idempotent, same participant) and issues a new ticket
(`ZC_GS_VOICE_TICKET(reason 2)`). The client's attach replaces its attachment.

Not verified here: whether the old map-server calls `map_quit` on a
map-server change. The quit grace makes either case safe.

## 5. Rates and sizing (04 §4.5, 02 §2)

- **Per switch:** two state writes (old group `[]`, new group channel), well
  within 10/s per participant.
- **Guild disband:** up to 64 state writes in one session (only members in
  voice), within 200/s per session. The owner should pace them and retry
  `429 voice/rate_limited` after `Retry-After`.
- **Configuration:** once per session creation (5 per 10 s allowed).
- **Renewals:** one every 15 s per live group session, about 67 a second per
  1,000 sessions (`rathena-companion.md` §9). That scale has not been
  load-tested yet.
- **Room cap:** 64 in voice per group session (04 §4.2). A larger GvG voice
  room needs option (b) (`rathena-companion.md` §9).

## 6. Signed-in players (later)

Same packets, plus a challenge round before the ticket (01 §5, 02 §3):

| Placeholder | Name | Dir | Layout |
|---|---|---|---|
| `0x0CF7` | `ZC_GS_VOICE_CHALLENGE` | S→C | `u16 type; u16 len; u32 request_id; char challenge[len-8]` (32–256 of `[A-Za-z0-9._~-]`) |
| `0x0CF8` | `CZ_GS_VOICE_ASSERTION` | C→S | `u16 type; u16 len; u32 request_id; char assertion[len-8]` |

The client patch calls `gs_rag_player_assertion(request_id, challenge)` and
sends the result from `on_assertion`. The owner admits the player with
`playerAssertion` + `challenge` (02 §3) instead of as a guest, and the rest is
unchanged. The assertion lives 60 s and is single use.

## 7. Open items

- Opcode assignment and the map-server ↔ owner link: the rAthena team.
- The grace values (quit 10 s, empty group 30 s) are proposals.
