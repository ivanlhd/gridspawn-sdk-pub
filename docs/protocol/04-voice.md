# 04 Voice — Current

Voice uses Gridspawn's own mediasoup SFU. A client gets admitted with its
session voice grant, then speaks the mediasoup signaling protocol over
Socket.IO and carries Opus over WebRTC (ICE + DTLS-SRTP). The game server can
decide who hears whom (§4); clients see those decisions as SFU events (§5).

Sources:
- admission: `voice-gateway-rs/src/sdk_voice.rs` (route
  `/v1/sdk/voice/admissions`);
- SFU: `sfu/server.js`, `sfu/lib/sfu-auth.js`, `sfu/lib/sdk-audibility.js`,
  `sfu/README.md`;
- game-server voice routes: `sdk-api-edge-rs/src/http/voice.rs`;
- voice rules: `voice-gateway-rs/src/sdk_voice_control.rs`,
  `voice-gateway-rs/src/sdk_audibility.rs`;
- reference clients:
  - `sdk/rust/gridspawn-voice` covers §1–§5;
  - `sdk/typescript/packages/voice/src/{session,signaling,room}.ts` covers
    §1–§5.

Implementation note for native clients: Godot's `WebRTCPeerConnection` is
data-channel only. The native core must provide WebRTC media itself and build
the transports from mediasoup's ICE/DTLS/RTP parameters, as libmediasoupclient
does.

## 1. Admission — Current

```
POST {voice}/admissions
Authorization: Bearer <voice grant accessToken>
Content-Type: application/json

{ "room": "match", "endpointId": "gve1.…" }
```

`{voice}` is `endpoints.voice` (`https://voice-gateway.gridspawn.net/v1/sdk/voice`).
- **`endpointId`:** omit it on the first join; send the current one to
  renew.
- **Rooms:** the only room is `"match"`, one per game session. Named rooms
  are not available yet.
- **Origin:** native clients send no `Origin`. The gateway compares the
  grant's bound origin with the request's
  (`voice-gateway-rs/src/auth/sdk_grant.rs`):
  - a grant attached without an Origin (native) passes only when the
    admission request also has none;
  - a browser grant passes only with its exact registered Origin;
  - anything else is `sdk_voice_origin_mismatch` (test
    `native_voice_grant_rejects_an_injected_origin`).

Response, validated by the client (`core/src/sessions.ts`):

```json
{
  "sessionId": "…",
  "participant": { "participantId": "gsp1…", "identityKind": "gridspawn_user", "displayName": "…" },
  "endpointId": "gve1.<32 hex>",
  "room": { "name": "match", "roomId": "…" },
  "admissionExpiresAt": <Unix seconds>,
  "sfu": {
    "url": "https://…",
    "nodeId": "…",
    "roomId": "…",
    "token": "…",
    "expiresAt": <Unix seconds>,
    "limits": { "audioMaxBps": <positive number>, "maxIncomingBps": <positive number> }
  }
}
```

- `participant.displayName` is optional.
- `sfu.roomId` must equal `room.roomId`.
- `nodeId` matches `^[A-Za-z0-9_-]{3,128}$`.
- `sfu.url` must be HTTPS. The reference client also accepts plain HTTP on
  `localhost`/`127.0.0.1` for local testing.
- The response's `Date` header is exposed to browsers
  (`Access-Control-Expose-Headers: Date`). A client can therefore derive
  the SFU token's lifetime as `sfu.expiresAt` minus `Date`, without relying
  on its own clock.

Refusals: `{"ok": false, "error": "<code>"}`.

| HTTP | Code | When |
|---|---|---|
| 400 | `sdk_voice_room_not_supported` | A room other than `"match"` |
| 400 | `sdk_voice_endpoint_invalid` | `endpointId` is not `gve1.` + 32 hex |
| 400 | `sdk_voice_session_invalid` | The grant's session cannot be used for voice |
| 401 | `sdk_voice_authentication_required` | No `Bearer` grant |
| 401 | `sdk_voice_module_grant_invalid` | The grant is not a valid voice grant |
| 403 | `sdk_voice_origin_mismatch` | The request's Origin does not match the grant's (above) |
| 403 | `sdk_voice_admission_binding_mismatch` | A renewal for an endpoint admitted to another participant, session or room |
| 404 | `sdk_voice_admission_not_found` | A renewal for an `endpointId` the gateway does not know (expired): join again without it |
| 409 | `sdk_voice_room_full` | The room already has its `maxParticipants` distinct participants (a participant already in always fits) |
| 409 | `sdk_voice_reconnect_required` | A renewal whose room moved to another SFU node: reconnect cleanly (§2) |
| 429 | `sdk_voice_rate_limited` | Too many admissions for this participant |
| 503 | `sdk_voice_authentication_unavailable`, `sdk_voice_storage_unavailable`, `sdk_voice_token_unavailable`, `sdk_voice_no_sfu_nodes`, `sdk_voice_sfu_overloaded` | Temporarily unavailable; retry with back-off |

## 2. SFU signaling — Current

Socket.IO v4 client to `sfu.url`:
- server `socket.io ^4.7.5`, reference client `socket.io-client 4.8.3`;
- WebSocket transport only, no automatic reconnection, 10 s timeout per
  request (`signaling.ts`);
- native clients connect **without** an `Origin` header (`sfu/README.md`).

Every request is an event emitted with an acknowledgement callback. An
acknowledgement of the form `{"error": "<code>"}` means rejection.

Join sequence (`session.ts`):

| # | Emit | Payload | Ack |
|---|---|---|---|
| 1 | `joinRoom` | `{roomId, peerId: endpointId, token: sfu.token}` | `{routerRtpCapabilities, producers: [ProducerDescriptor]}`, plus in game-session rooms `peers`, `voiceRoom`, `voiceAuthority` (§5.1) |
| 2 | `createTransport` (send) | `{roomId}` | `{id, iceParameters, iceCandidates, dtlsParameters}` |
| 3 | `createTransport` (recv) | `{roomId}` | same |
| 4 | `connectTransport` (once per transport, when its DTLS starts) | `{roomId, transportId, dtlsParameters}` | any non-error ack |
| 5 | `produce` | `{roomId, transportId, kind: "audio", rtpParameters, appData: {type: "voice", routeClass: "room"}}` | `{id}` |
| 6 | `consume` (per remote producer) | `{roomId, producerId, transportId, rtpCapabilities}` | `{consumerId, producerId, peerId?, ownerPeerId?, kind, type?, rtpParameters}` |

`ProducerDescriptor`: `{producerId, kind, peerId?, ownerPeerId?, type?, serverMuted?}`.
`serverMuted` is present in game-session rooms only: `true` while the game
server has muted that audio producer's player (§5.4).
Consume only `kind == "audio"` with `type` absent or `"voice"`.

Other requests:

| Emit | Payload | Purpose |
|---|---|---|
| `pauseProducer` / `resumeProducer` | `{roomId, producerId}` | mute / unmute own microphone. `resumeProducer` answers `{error: "server_muted"}` while the game server has muted this player (§5.4) |
| `pauseConsumer` / `resumeConsumer` | `{roomId, consumerId}` | stop / resume receiving one speaker. Ack `{status: "ok"}`, or `{error: "consumer-not-found"}`; `resumeConsumer` can also answer `{error: "not_audible"}` (§5.3) |
| `closeProducer` | `{roomId, producerId}` | leave |
| `refreshAuth` | `{roomId, token}` | swap in a renewed SFU token in place |

Server events:
- `newProducer` (ProducerDescriptor), `producerClosed` `{producerId}`,
  `consumerClosed` `{consumerId}`;
- in game-session rooms, also the §5.2 events.

**Renewal** (`session.ts`):
- about 20 s before `sfu.expiresAt`, call admission again with the same
  `endpointId`;
- if it answers `409 sdk_voice_reconnect_required`, or `endpointId`,
  `room.roomId` or `sfu.nodeId` changed, do a clean reconnect;
- otherwise emit `refreshAuth`.

## 3. Audio — Current router, recommended encoder

The SFU router offers Opus 48 kHz with in-band FEC (`sfu/server.js`).
- Bitrate and DTX are encoder-side.
- Recommended for games: mono, 20 ms frames, about 24 kbps,
  `useinbandfec=1`, `usedtx=1`, never above `limits.audioMaxBps`.

Decoding is per remote producer, so the client receives **raw
per-participant PCM**.
- In a room the game server has not configured, everyone hears everyone.
  Spatialisation, volume and mixing are done locally by the game (for
  example one `AudioStreamPlayer3D` per participant in Godot).
- In a room the game server configured (§4):
  - the server decides who is forwarded at all;
  - with `spatial: "server"`, each audible speaker's `gain` and `pan`
    arrive in `audibility` events (§5.2), and the client applies them
    before mixing.

## 4. Server-side voice control (game server) — Current

The game server decides who hears whom:
- it writes the room configuration, each player's state, positions and
  mutes;
- the voice service turns them into audible (listener, speaker) pairs;
- the SFU forwards only those (§5).

### 4.1 Routes

```
PUT  {base}/v1/server/sessions/{sessionId}/voice                                   RoomConfigs
PUT  {base}/v1/server/sessions/{sessionId}/participants/{participantId}/voice      PlayerState
POST {base}/v1/server/sessions/{sessionId}/voice/positions                         Positions
POST {base}/v1/server/sessions/{sessionId}/participants/{participantId}/voice/mute {"muted": bool}
Authorization: GridspawnServer <credential>
Content-Type: application/json
```

- **Credential:** needs `session.manage`. Any `Origin` header is refused.
- **Session:** the lease must be live (02 §2).
  - On the two participant routes, the participant must be admitted to
    that session.
  - `PUT …/voice` replaces the whole configuration. It needs the voice
    module on for the app and environment.
- **Positions:** they name participants by id format only. Ids unknown to
  the room are ignored.
- **Mute:** it is separate from the player state, so a state write never
  unmutes anyone.
- **Answer:** `200 {"ok": true, "enforced": bool}` with
  `Cache-Control: no-store`.
  - `enforced` is true once the session's voice room is live on an SFU node
    that follows the rules.
  - Before that, the write is stored and applies when players join.
- **A refused write changes nothing.**

Body limits: configuration 8 KiB, state 2 KiB, positions 32 KiB, mute 1 KiB.

### 4.2 Shapes

Unknown fields are refused everywhere. Numbers must be finite, with
|x| ≤ 1e6.

```
RoomConfigs { rooms: { <name>: RoomConfig } }        // 1–4 rooms; name ^[a-z][a-z0-9_-]{0,31}$
RoomConfig {
  mode: "open" | "channels" | "proximity" | "hybrid",
  spatial: "server" | "client",
  proximity?: { minDistance, maxDistance, rolloff: "linear" | "inverse" | "exponential",
                rolloffFactor, hysteresis },         // metres; 0 ≤ min < max ≤ 1e4
  ghosts?: { hearAlive = true, aliveHearGhosts = false, spectatorsHearDead = true },
  maxParticipants: 1..64 = 64,
  maxAudibleSpeakers: 1..64 = 16
}
PlayerState { seq?, rooms?: [name], channels: [≤ 8 × ^[a-z0-9:_-]{1,48}$],
              alive = true, spectator = false, scene?: string (≤ 48), canSpeak = true }
Positions   { seq, positions: [{ participantId, p: [x, y, z], yaw? }] }   // ≤ 256 entries; yaw in radians
```

- **Rooms:** clients can join only `"match"` (§1), so configure `match`.
  Other room names are accepted in the configuration, but nobody can be
  admitted to them yet.
- **Ordering:** `seq` (state and positions) is at most 2⁵³ − 1. A `seq` not
  above the last accepted one is refused with `voice/stale_sequence`.

### 4.3 Who hears whom

Per room, `audible(L, S)` holds when all of these are true:
- L ≠ S, and both are in the room;
- S `canSpeak` and is not muted;
- the authority is fresh (§4.4);
- the base rule holds:
  - `open`: everyone;
  - `channels`: a shared channel;
  - `proximity`: in range, with `spatial: "server"` only;
  - `hybrid`: either of the two;
- the ghost rule holds (from `ghosts`):
  - the living are heard by the dead and spectators when `hearAlive`;
  - the dead and spectators are heard by the living only with
    `aliveHearGhosts`;
  - the dead are heard by spectators with `spectatorsHearDead`;
  - two non-living players hear each other;
  - two living players follow the base rule.

Further rules:
- **Scene:** different `scene` values make a pair inaudible.
- **No state:** a participant with no state in a configured room is neither
  heard nor hearing. Set each player's state when you admit them.
- **Range:**
  - entering needs d ≤ `maxDistance`; leaving needs
    d > `maxDistance` + `hysteresis`;
  - a position is held for 1 s, fades from 1 s to 3 s, and after 3 s grants
    nothing.
- **Cap:** each listener hears at most `maxAudibleSpeakers`, closest first.
  A shared channel counts as distance 0.
- **Gain and pan:** gain is quantised to 1/32, and pan to 1/16 (`spatial:
  "server"` only). A channel or ghost path gives gain 1.

### 4.4 Lease and staleness

- **Stale point:** the game server's authority is stale once
  `now > leaseExpiresAt + 3 s` by the voice service's clock. That happens
  when there is no lease renewal and no voice write. Every pair in that
  session's configured rooms is then inaudible until fresh authority
  arrives.
- **Renewals keep it fresh:** while the session has a voice configuration,
  each lease renewal (02 §2) also refreshes the voice authority. A game
  that sends no voice writes (for example channels only) therefore stays
  fresh by renewing as usual.
- **Configured rooms never reopen by themselves.** If the configuration
  disappears or drops a room while players are in it, nobody in that room
  is heard.

### 4.5 Rate limits

Fixed windows, per session unless stated:

| Route | Limit |
|---|---|
| configuration | 5 per 10 s |
| state | 10/s per participant, 200/s per session |
| positions | 20 per 2 s (10 Hz sustained, bursts of 20) |
| mute | 20/s |

- Over a limit: `429 voice/rate_limited` with `Retry-After` (seconds).
- The limits fail closed: when they cannot be checked, the answer is 503.

### 4.6 Errors

Body: `{"ok": false, "error": {"code", "message", "field"?, "reason"?}, "requestId"}`.

| HTTP | Code | When |
|---|---|---|
| 400 | `voice/invalid_request` (`field`, `reason` when known, e.g. `proximity`/`distance`, `seq`/`range`) | Malformed body, unknown field, a value out of bounds |
| 401 | `server/authentication_required` | Bad credential |
| 403 | `voice/browser_forbidden` | Any `Origin` header |
| 403 | `voice/module_disabled` | Configuration write with voice off for the app and environment |
| 403 | `server/scope_denied` | No `session.manage` |
| 404 | `voice/session_not_found` | Lease gone |
| 404 | `voice/participant_not_found` | Participant not admitted to that session |
| 409 | `voice/stale_sequence` | `seq` not above the last accepted |
| 429 | `voice/rate_limited` + `Retry-After` | §4.5 |
| 503 | `voice/unavailable` | Voice control temporarily unavailable. Retry; nothing was stored |

## 5. Audibility on the client — Current

What a client sees in a game-session room on the SFU (§2).

### 5.1 Join

The `joinRoom` ack adds:

```
peers:          [{ endpointId, participantId, displayName, identityKind }]   // the others; display names only
voiceRoom:      { mode, spatial, secrecySafe, maxParticipants } | null       // null while the room is not configured
voiceAuthority: { available: bool }
```

- `joinRoom` answers `{"error": "room_full"}` past `maxParticipants`
  distinct participants.
- `secrecySafe` is false for `spatial: "client"`:
  - the server does no range culling there (proximity grants nothing);
  - every pair allowed by `open`, channels and the ghost rules is
    forwarded, whatever the distance;
  - the game spatialises locally. Label such rooms in the game's UI.

### 5.2 Events

| Event | Payload | When |
|---|---|---|
| `peerJoined` | `{endpointId, participantId, displayName, identityKind}` | another player joined |
| `peerLeft` | `{endpointId}` | a player left |
| `audibility` | `{producerId, audible: true, gain, pan}` or `{producerId, audible: false}` | this listener's pair for that producer changed. `gain` in [0, 1]; `pan` in [−1, 1] or `null` |
| `voiceRoom` | `{mode, spatial, secrecySafe, maxParticipants}` | the room's configuration changed |
| `voiceAuthority` | `{available: bool}` | `false` when the voice rules have been unreachable for 5 s; `true` when they are back |
| `producerPaused` | `{producerId, reason: "server_mute"}` | the game server muted that speaker |
| `producerResumed` | `{producerId}` | unmuted |

### 5.3 Consumers

- **In a configured room:**
  - a consumer is created paused, and resumed only while its pair is
    audible;
  - an inaudible pair stays paused: no RTP reaches the listener;
  - a pair that changes is one pause or resume, with no renegotiation and no
    `consumerClosed`.
- **A room the game server has not configured** forwards to everyone. Its
  `audibility` events say `audible: true`, `gain` 1, `pan` `null`.
- **Client pause:**
  - a consumer forwards only while its pair is audible **and** the client
    has not paused it;
  - `resumeConsumer` answers `{error: "not_audible"}` while the pair is not
    audible. It still clears the client's pause, so the consumer forwards as
    soon as the pair becomes audible.
- **Gain and pan:** apply `audibility`'s values to that producer's playback
  when `spatial` is `"server"`.

### 5.4 Server authority over producers

- `produce` with a non-empty `appData.allowedPeerIds` answers
  `{error: "sdk_server_authority"}`. So does `updateProducerAllowedUsers`.
- **Server-muted speaker:**
  - their audio producers are paused;
  - `resumeProducer` answers `{error: "server_muted"}`;
  - after a reconnect, the new producer starts paused;
  - a player who joins later sees the mute in the producer's descriptor
    (`serverMuted: true` in `joinRoom`'s `producers` and in `newProducer`).
    `producerPaused` / `producerResumed` report only later changes.

### 5.5 Fail closed

- If the voice rules are unreachable for 5 s:
  - every consumer in configured rooms pauses;
  - clients get `voiceAuthority {available: false}`.
- When they are back, every pair is restored and clients get
  `voiceAuthority {available: true}`.
- A shorter interruption is not visible to clients.

### 5.6 What a listener can infer

- A listener can tell "audible" from "not audible" (packets, `audibility`).
  So range-limited audibility reveals "within hearing range", as hearing
  someone does.
- It never reveals speech of inaudible speakers or coordinates: gain and pan
  are quantised, and pan is sent only with `spatial: "server"`.
