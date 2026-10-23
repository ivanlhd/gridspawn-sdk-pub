# Gridspawn SDK wire protocol (v1)

This is the contract every Gridspawn SDK implements: the TypeScript packages in
`sdk/typescript`, and the Rust crates (`gridspawn-server`, its C ABI `gridspawn-server-capi`,
and the game-client crate `gridspawn-client`), and the Godot GDExtension
(`sdk/godot`). It is **extracted from the deployed services**,
not designed separately. When this document and the code disagree, the code
wins and this document is the bug.

Status legend used in every file:

- **Current** — implemented and deployed; cited to source.
- **Proposed** — not implemented; the shape is open for review before it is built.

| Part | Status | Services |
|---|---|---|
| [01 Login](01-login.md) | Current; §6 (sign in through the Gridspawn app) Proposed | `sdk-auth-rs`, the Gridspawn desktop app |
| [02 Game sessions and grants](02-sessions.md) | Current | `sdk-api-edge-rs` |
| [03 Presence](03-presence.md) | Current | `sdk-api-edge-rs` |
| [04 Voice](04-voice.md) | Current | `voice-gateway-rs`, `sfu/`, `sdk-api-edge-rs` |
| [05 Club posts from game servers](05-club.md) | Current | `sdk-api-edge-rs`, `gridspawn-core-rs`, `chat-gateway-rs` |
| [06 Game data from game servers](06-game-data.md) | Current | `sdk-api-edge-rs`, `app-rt-gateway-rs` |

## Hosts

| Purpose | URL |
|---|---|
| SDK base / OAuth issuer | `https://sdk.gridspawn.net` |
| OAuth metadata | `https://sdk.gridspawn.net/.well-known/oauth-authorization-server` |
| OAuth signing keys | `https://sdk.gridspawn.net/.well-known/jwks.json` |
| Module-grant signing keys | `https://sdk.gridspawn.net/.well-known/sdk-module-jwks.json` |
| Consent UI (browser only) | `https://auth.gridspawn.net` |
| Club posts from game servers (05) | `https://presence-gateway.gridspawn.net/core/v1/game-servers/posts` |

Clients must not hard-code anything beyond the SDK base URL. Every other
endpoint comes from bootstrap, except the club posting URL (05), which is not
in bootstrap yet: game servers configure it.

## Bootstrap (Current)

```
GET {base}/v1/bootstrap?appId=<appId>&environment=<env>&protocolVersion=1&sdkVersion=<semver>
```

Source: `sdk-api-edge-rs/src/http/mod.rs:32`, client `sdk/typescript/packages/core/src/client.ts:95-127`.

Response (shape verified live 2026-10-18; the ids are examples):

```json
{
  "ok": true,
  "requestId": "…",
  "application": { "appId": "game-example", "displayName": "Example Game" },
  "environment": "development",
  "protocol": { "selected": 1, "minimum": 1, "maximum": 1 },
  "modules": ["interakt", "presence", "voice"],
  "endpoints": {
    "api": "https://sdk.gridspawn.net/v1",
    "oauth": "https://sdk.gridspawn.net/oauth",
    "presence": "https://sdk.gridspawn.net/v1/presence",
    "voice": "https://voice-gateway.gridspawn.net/v1/sdk/voice",
    "interakt": "https://sdk.gridspawn.net/v1/interakt",
    "mediaResolver": "https://sdk.gridspawn.net/v1/media/resolve"
  }
}
```

A client must reject bootstrap unless `ok` is `true`, `application.appId` and
`environment` equal what it asked for, and `protocol.selected` is `1`.
`endpoints.presence` is advertised but **not served yet**: it returns HTTP 404
(verified 2026-10-18). See 03.

## Conventions (Current)

- `appId`: `^[A-Za-z0-9_-]{3,128}$`. `environment`: `development` | `staging` | `production`.
- JSON bodies are UTF-8; OAuth endpoints use `application/x-www-form-urlencoded`.
- Error envelope from the SDK edge (`sdk-api-edge-rs/src/errors.rs:72-84`):
  `{"ok": false, "error": {"code": "…", "message": "…"}, "requestId": "…"}`.
  OAuth endpoints use RFC 6749 `{"error": "…", "error_description": "…"}`.
  The request ID is also returned in the `x-gridspawn-request-id` header.
- Retry only idempotent requests, and only on HTTP 429/5xx or network failure
  (`sdk/typescript/packages/core/src/http.ts`). Exponential backoff from 50 ms, 3 attempts.
- Times in responses are seconds unless the field name says otherwise.
  `expiresIn` is relative seconds; `*ExpiresAt` fields are either RFC 3339
  strings or Unix seconds, as stated per field.

## Native (non-browser) clients

Native clients are a designed case, not a workaround:

- They send **no `Origin` header**. OAuth token/revoke accept that
  (`sdk-auth-rs/src/http/handlers.rs:520-525`); session attach treats Origin as
  optional (`sdk-api-edge-rs/src/http/sessions.rs:340-361`); the voice gateway
  accepts an Origin-less grant only from an Origin-less request
  (`voice-gateway-rs/src/auth/sdk_grant.rs:252-256`); and the SFU requires
  native SDK voice tokens to arrive *without* a browser Origin (`sfu/README.md`).
- They are OAuth **public clients**: `token_endpoint_auth_methods_supported` is
  `["none"]`. A game binary must never contain a client secret or a server credential.

## Development application

Gridspawn's own test application and its server credential are internal and
not part of this contract. Game developers use their own application: the
game's owner connects it and makes server keys in the Dev Hub (05 §Setup).
