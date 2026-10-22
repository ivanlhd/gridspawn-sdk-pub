# 02 Game sessions and module grants — Current

The **game server** is the authority. It creates a session, admits players
(Gridspawn accounts or guests), and issues each one a short single-use
**participant ticket**. The client redeems the ticket for per-module grants
(presence, voice, interakt), each a separate token with its own audience.

Sources: routes `sdk-api-edge-rs/src/http/mod.rs:37-61`, logic
`sdk-api-edge-rs/src/session_service.rs`, lifetimes
`sdk-auth-contracts-rs/src/policy.rs:15-19`, reference clients
`sdk/typescript/packages/server/src/{api,sessions}.ts` and
`sdk/typescript/packages/core/src/sessions.ts`.

```
game server                    Gridspawn SDK edge                game client
    |  POST /v1/server/sessions        |                              |
    |--------------------------------->|                              |
    |                                  |      challenge (game channel)|
    |----------------------------------------------------------------->|
    |                                  |  POST /v1/auth/player-assertions (01 §5)
    |              assertion (game channel)                           |
    |<-----------------------------------------------------------------|
    |  POST …/participants (assertion) |                              |
    |  POST …/tickets                  |                              |
    |--------------------------------->|                              |
    |              ticket (game channel)                              |
    |----------------------------------------------------------------->|
    |                                  |   POST /v1/sessions/attach   |
    |                                  |<-----------------------------|
    |                                  |   grants {presence, voice}   |
```

Guests skip the assertion: the server calls `…/guests` instead.

## Server authentication

All `/v1/server/...` calls:

```
Authorization: GridspawnServer <server credential>
Content-Type: application/json
Idempotency-Key: <8–128 chars of [A-Za-z0-9._:~-]>   (where marked)
```

- The credential is bound to one app and environment.
- Requests carrying an `Origin` header are **rejected** ("Server credentials
  cannot be used from a browser origin", `sdk-api-edge-rs/src/http/sessions.rs:297-300`).
- A server credential must never ship in a client build.

## 1. Create a session (idempotent)

```
POST {base}/v1/server/sessions
Idempotency-Key: …

{ "externalSessionId": "match-123", "region": "us-east", "maxParticipants": 16,
  "metadata": { "map": "arena" } }
```

`externalSessionId` 1–256 chars; `region` optional 1–64; `maxParticipants`
optional 1–10000; `metadata` optional, ≤ 16 entries, key 1–64, value 0–256, no
control characters. Response:

```json
{ "sessionId": "…", "leaseExpiresAt": "2026-10-18T12:00:45Z", "renewAfterSeconds": 15 }
```

## 2. Keep the session alive

The lease lasts **45 s**; renew every **15 s** (`SESSION_AUTHORITY_LEASE_TTL`,
`SESSION_AUTHORITY_RENEW_AFTER`). On failure, retry after 1–5 s.

```
POST {base}/v1/server/sessions/{sessionId}/renew
{}
```

Same response shape as create.

## 3. Admit a signed-in player (idempotent)

```
POST {base}/v1/server/sessions/{sessionId}/participants
Idempotency-Key: …

{ "playerAssertion": "…", "challenge": "<the challenge you gave the client>",
  "externalPlayerId": "your-player-id", "role": "player", "team": "red" }
```

`challenge` 32–256 of `[A-Za-z0-9._~-]`; `externalPlayerId` 1–256; `role`,
`team` optional 1–64. The edge verifies the assertion's signature, app,
environment, challenge and expiry, and rejects reuse
(`session_service.rs:146-181`). Response:

```json
{ "participantId": "gsp1…", "identityKind": "gridspawn_user", "subject": "…" }
```

## 4. Admit a guest (idempotent)

Only when the app/environment enables guests (test app: yes, Voice only).

```
POST {base}/v1/server/sessions/{sessionId}/guests
Idempotency-Key: …

{ "externalPlayerId": "opaque-id", "displayName": "Wanderer", "role": "player" }
```

`displayName` 1–64 after trimming. Rejected if `displayName` or
`externalPlayerId` looks like an email, or if they are equal
(`server/src/sessions.ts:317-334`). Response has
`"identityKind": "server_verified_guest"` and always a `displayName`.

## 5. Issue a client ticket

```
POST {base}/v1/server/sessions/{sessionId}/participants/{participantId}/tickets

{ "modules": ["presence", "voice"] }
```

`modules`: 1–3 unique values from `presence`, `voice`, `interakt`. A module is
granted only if the app enables it and the player consented to its base scope
(`presence.write`, `voice.join`, `interakt.receive`;
`session_service.rs:800-818`). Response:

```json
{ "participantTicket": "gpt1.<43 chars>", "tokenType": "urn:gridspawn:participant-ticket", "expiresIn": 120 }
```

The ticket lives **120 s** and is single-use. Send it to the client over the
game's own connection.

## 6. Client: attach (redeem the ticket)

```
POST {api}/sessions/attach?appId=<appId>&environment=<env>
Content-Type: application/json

{ "participantTicket": "gpt1.…" }
```

`{api}` is `endpoints.api` (`https://sdk.gridspawn.net/v1`). No `Origin` from
native clients. Response:

```json
{
  "sessionId": "…",
  "participant": { "participantId": "gsp1…", "identityKind": "gridspawn_user",
                   "subject": "…", "displayName": "…" },
  "modules": ["voice"],
  "grants": {
    "voice": { "accessToken": "…", "tokenType": "Bearer", "expiresIn": 300, "scope": "voice.join voice.position" }
  }
}
```

`participant.displayName` is optional for `gridspawn_user` and always present
for `server_verified_guest`. Every module in `modules` must have a grant with `tokenType == "Bearer"` and
`expiresIn > 0`; reject otherwise (`core/src/sessions.ts:178-229`). Grants
live **5 minutes** (`MODULE_GRANT_TTL`). Each grant has a distinct audience
(`gridspawn-presence`, `gridspawn-voice`, `gridspawn-interakt`), so a voice
grant cannot be used for presence.

## 7. Client: renew a module grant

Renew when under 60 s remain (`core/src/sessions.ts:141`):

```
POST {api}/sessions/{sessionId}/grants/{module}/renew?appId=<appId>&environment=<env>
Authorization: Bearer <current grant accessToken>
```

Response: one grant object (`accessToken`, `tokenType`, `expiresIn`, `scope`).
