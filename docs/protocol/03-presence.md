# 03 Presence — Proposed

**Not implemented.** Today the edge already issues presence grants (audience
`gridspawn-presence`, base scope `presence.write`;
`sdk-api-edge-rs/src/session_service.rs:812-837`) and bootstrap advertises
`endpoints.presence`, but **no route serves it**. This file is the proposed
contract for review by the SDK implementers before the server side is built.

Agreed requirements (2026-10-18):

- Accept presence from **both** the game server (authoritative) and the client
  (for games without a server), with rate limits.
- Tiny surface: set and clear.
- The game is identified by the registered app, never by client-supplied text,
  so a game cannot claim to be another game.

## Payload

```json
{
  "details": "Ranked match",
  "state": "In lobby",
  "location": "Neon District",
  "startedAt": 1792310400,
  "party": { "id": "opaque-party-id", "size": 2, "max": 4 }
}
```

| Field | Rule |
|---|---|
| `details`, `state` | optional, 1–128 chars, no control characters |
| `location` | optional, 1–64 chars |
| `startedAt` | optional Unix seconds; shown as elapsed time |
| `party` | optional; requires `presence.party`. `id` opaque 1–64 chars of `[A-Za-z0-9._:-]`; `1 ≤ size ≤ max ≤ 64` |

Game name and icon come from the app registration. No URLs, emails or player
IDs are accepted in text fields; the server rejects them rather than
displaying them.

## Client path

```
PUT {presence}?appId=<appId>&environment=<env>
Authorization: Bearer <token>
Content-Type: application/json

<payload>
```

`<token>` is either the player's OAuth access token with `presence.write`
(no game server needed) or a session presence grant from 02 §6.

```
DELETE {presence}?appId=<appId>&environment=<env>
Authorization: Bearer <token>
```

Response: `{"ok": true, "expiresAt": <Unix seconds>}`. Presence expires
**120 s** after the last update; clients re-send the same payload every 60 s
while playing. Rate limit: 5 writes per 20 s per player per app; HTTP 429 with
`Retry-After` when exceeded.

## Server path (authoritative)

```
PUT    {base}/v1/server/sessions/{sessionId}/participants/{participantId}/presence
DELETE {base}/v1/server/sessions/{sessionId}/participants/{participantId}/presence
Authorization: GridspawnServer <credential>
```

Same payload and limits. While a server-set presence is active for a player
in an app, client writes for that app are accepted but not displayed. Server
presence ends when the session lease lapses (02 §2) or on DELETE.

Guests (`server_verified_guest`) have no Gridspawn profile, so presence is
rejected for them with `presence/guest_not_supported`.

## Display

Shown on the player's Gridspawn profile and friends list as
`<app display name> · details · state`, subject to the player's existing game
activity privacy setting. Party size is shown; party IDs never are.

## Open questions for reviewers

1. Is a 120 s expiry with a 60 s heartbeat right for Godot's main loop, or do
   you prefer an explicit long-lived connection?
2. Do you need `party` join/invite flows in v1, or only display?
