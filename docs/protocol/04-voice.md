# 04 Voice — Current, plus Proposed additions

Voice uses Gridspawn's own mediasoup SFU. A client gets admitted with its
session voice grant, then speaks the mediasoup signaling protocol over
Socket.IO and carries Opus over WebRTC (ICE + DTLS-SRTP).

Sources: admission `voice-gateway-rs/src/sdk_voice.rs` (route
`/v1/sdk/voice/admissions`, `voice-gateway-rs/src/http.rs:53`), SFU
`sfu/server.js`, `sfu/lib/sfu-auth.js`, `sfu/README.md`, reference client
`sdk/typescript/packages/voice/src/{session,signaling}.ts`.

Implementation note for native clients: Godot's `WebRTCPeerConnection` is
data-channel only, so the native core must provide WebRTC media itself and
build the transports from mediasoup's ICE/DTLS/RTP parameters, as
libmediasoupclient does.

## 1. Admission — Current

```
POST {voice}/admissions
Authorization: Bearer <voice grant accessToken>
Content-Type: application/json

{ "room": "match", "endpointId": "gve1.…" }
```

`{voice}` is `endpoints.voice` (`https://voice-gateway.gridspawn.net/v1/sdk/voice`).
Omit `endpointId` on first join; send the current one to renew. The only room
is `"match"`, one per game session. Native clients send no `Origin`. The
gateway compares the grant's bound origin with the request's
(`voice-gateway-rs/src/auth/sdk_grant.rs:252-256`): a grant attached without
an Origin (native) passes only when the admission request also has none; a
browser grant passes only with its exact registered Origin. Anything else is
`sdk_voice_origin_mismatch` (test `native_voice_grant_rejects_an_injected_origin`).

Response, validated by the client (`core/src/sessions.ts:250-283`):

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

`participant.displayName` is optional. `sfu.roomId` must equal `room.roomId`;
`nodeId` matches `^[A-Za-z0-9_-]{3,128}$`. `sfu.url` must be HTTPS; the
reference client also accepts plain HTTP on `localhost`/`127.0.0.1` for local testing.

## 2. SFU signaling — Current

Socket.IO v4 client to `sfu.url` (server `socket.io ^4.7.5`, reference client
`socket.io-client 4.8.3`), WebSocket transport only, no automatic
reconnection, 10 s timeout per request (`signaling.ts:12-19`). Native clients
connect **without** an `Origin` header (`sfu/README.md`). Every request is an
event emitted with an acknowledgement callback. An acknowledgement of the form
`{"error": "<code>"}` means rejection.

Join sequence (`session.ts:191-256`):

| # | Emit | Payload | Ack |
|---|---|---|---|
| 1 | `joinRoom` | `{roomId, peerId: endpointId, token: sfu.token}` | `{routerRtpCapabilities, producers: [ProducerDescriptor]}` |
| 2 | `createTransport` (send) | `{roomId}` | `{id, iceParameters, iceCandidates, dtlsParameters}` |
| 3 | `createTransport` (recv) | `{roomId}` | same |
| 4 | `connectTransport` (once per transport, when its DTLS starts) | `{roomId, transportId, dtlsParameters}` | any non-error ack |
| 5 | `produce` | `{roomId, transportId, kind: "audio", rtpParameters, appData: {type: "voice", routeClass: "room"}}` | `{id}` |
| 6 | `consume` (per remote producer) | `{roomId, producerId, transportId, rtpCapabilities}` | `{consumerId, producerId, peerId?, ownerPeerId?, kind, type?, rtpParameters}` |

`ProducerDescriptor`: `{producerId, kind, peerId?, ownerPeerId?, type?}`.
Consume only `kind == "audio"` with `type` absent or `"voice"`.

Other requests:

| Emit | Payload | Purpose |
|---|---|---|
| `pauseProducer` / `resumeProducer` | `{roomId, producerId}` | mute / unmute own microphone |
| `closeProducer` | `{roomId, producerId}` | leave |
| `refreshAuth` | `{roomId, token}` | swap in a renewed SFU token in place |

Server events: `newProducer` (ProducerDescriptor), `producerClosed`
`{producerId}`, `consumerClosed` `{consumerId}`.

Renewal (`session.ts:390-437`): about 20 s before `sfu.expiresAt`, call
admission again with the same `endpointId`. If `endpointId`, `room.roomId` or
`sfu.nodeId` changed, do a clean reconnect; otherwise emit `refreshAuth`.

## 3. Audio — Current router, recommended encoder

The SFU router offers Opus 48 kHz with in-band FEC (`sfu/server.js:117-132`).
Bitrate and DTX are encoder-side. Recommended for games: mono, 20 ms frames,
about 24 kbps, `useinbandfec=1`, `usedtx=1`, never above `limits.audioMaxBps`.

Decoding is per remote producer, so the client receives **raw per-participant
PCM**. Spatialisation, volume and "who do I hear" mixing are done locally by
the game (for example one `AudioStreamPlayer3D` per participant in Godot).

## 4. Proposed additions (not implemented)

Needed for proximity voice where the game decides who each listener hears:

### 4.1 Per-listener subscription

```
pauseConsumer  {roomId, consumerId}  -> {}
resumeConsumer {roomId, consumerId}  -> {}
```

The SFU stops or resumes forwarding one producer to this listener only,
saving bandwidth for out-of-range players. Today a listener can only close a
consumer; there is no pause/resume.

### 4.2 Authoritative mute (game server)

```
POST {base}/v1/server/sessions/{sessionId}/participants/{participantId}/voice/mute
Authorization: GridspawnServer <credential>

{ "muted": true }
```

The gateway tells the SFU to pause that participant's voice producers and to
refuse `resumeProducer` while muted, so a muted player's audio is never
forwarded. Clients receive `producerPaused {producerId, reason: "server_mute"}`
and `producerResumed {producerId}`. No moderation mute exists today.

### 4.3 Position

`voice.position` is a granted scope but nothing consumes it. v1 keeps
positions local to the game (4.1 plus local spatialisation). Server-side
distance culling is out of scope for v1.
