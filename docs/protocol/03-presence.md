# 03 Presence — Current

**Deployed 2026-10-22.** The routes below are live in the SDK edge and
`endpoints.presence` in the bootstrap points at them. This file is the
contract the server side implements.

Agreed requirements (2026-10-18, decisions 2026-10-22):

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

Every field is optional; `{}` shows just the game ("Playing <name>"). Unknown
fields are refused.

| Field | Rule |
|---|---|
| `details`, `state` | 1–128 characters after trimming, no control characters |
| `location` | 1–64 characters after trimming, no control characters |
| `startedAt` | Unix seconds, at most 5 minutes in the future and at most 30 days in the past; shown as time played |
| `party` | requires `presence.party`. `id` is 1–64 of `[A-Za-z0-9._:-]`. `1 ≤ size ≤ max ≤ 64`. The `id` is checked and then discarded: it is never stored or shown |

The game's name and icon come from the app registration.

**Text that is refused** (`presence/text_not_allowed`, with the field name).
The server refuses these rather than display them:

- email addresses;
- web addresses: anything with `://` or `www.`, or a dotted word ending in a
  common top-level domain such as `.com`, `.gg` or `.io`;
- player and Gridspawn identifiers: `gss_…`, `gsc_…`, `gsc1.…`, `gsp1.…`,
  `gsg1.…`, `gssn1.…`, `gpt1.…`, UUIDs, 28-character alphanumeric tokens, and
  the player's own `externalPlayerId` (4 characters or more) when the session
  has one.

There is no profanity filter today. Text may be refused with the same code by
a moderation filter later.

## Client path

```
PUT {presence}?appId=<appId>&environment=<env>
Authorization: Bearer <token>
Content-Type: application/json

<payload>
```

`<token>` is either the player's OAuth access token or a session presence grant
from 02 §6:

- **OAuth access token** (no game server needed): it must include
  `identity.basic` and `presence.write`, and must not be revoked.
- **Session presence grant**: its session must still be live (02 §2).

In both cases the app's `presence` module must be on.

```
DELETE {presence}?appId=<appId>&environment=<env>
Authorization: Bearer <token>
```

Responses:

- `PUT` answers `{"ok": true, "expiresAt": <Unix seconds>}`.
- `DELETE` answers `{"ok": true}`.

Presence expires **120 s** after the last update; re-send the same payload
every 60 s while playing.

Browsers may call the client path only from an origin registered for the
environment; a grant issued for a browser origin works only from that origin.

## Server path (authoritative)

```
PUT    {base}/v1/server/sessions/{sessionId}/participants/{participantId}/presence
DELETE {base}/v1/server/sessions/{sessionId}/participants/{participantId}/presence
Authorization: GridspawnServer <credential>
```

- The credential needs `session.manage`, the same scope as the other session
  calls. It is refused from a browser origin.
- Each write needs the session's lease to be live (02 §2), and the participant
  must belong to that session.
- The player must have consented to `presence.write`; a `party` needs
  `presence.party`.
- The payload, text rules and responses are the same as on the client path.

While a server-set presence is live for a player in an app, client writes for
that app are accepted but not displayed.

Server presence expires **120 s** after the last server write, like client
presence, or ends at once on `DELETE`. Re-send every 60 s while the player is
in the session. A server that stops renewing its lease also stops being able to
write, so its presence disappears within 120 s.

Guests (`server_verified_guest`) have no Gridspawn profile, so presence is
refused for them with `presence/guest_not_supported`, on both paths.

## Rate limits

- **Per player and app:** 5 writes (`PUT` or `DELETE`) per 20 s, counted
  separately for the client path and the server path, so a client cannot use
  up the server's budget.
- **Per app and environment:** 3,000 writes per minute, both paths together.

Over either limit: HTTP 429 `presence/rate_limited` with `Retry-After`
(seconds). The limits fail closed: when they cannot be checked, the answer is
503.

## Errors

| HTTP | Code | When |
|---|---|---|
| 400 | `presence/invalid_request` (`error.field` when known) | Bad payload, length, range, unknown field or query |
| 400 | `presence/text_not_allowed` (`error.field`) | Email, web address or identifier in text |
| 401 | `presence/token_invalid` | Client path: missing, malformed, expired or revoked token, or a token for another app or environment |
| 401 | `server/authentication_required` | Server path: bad credential |
| 403 | `presence/scope_denied` | No `presence.write`, a `party` without `presence.party`, or the `presence` module is off |
| 403 | `presence/guest_not_supported` | Guest participant or guest grant |
| 403 | `presence/account_unavailable` | The player's account is not active |
| 403 | `config/origin_not_allowed`, `server/browser_forbidden`, `server/scope_denied` | As elsewhere in the SDK |
| 404 | `config/app_not_found` | App or environment unknown or off |
| 404 | `session/not_found` | Session lease gone, or participant unknown or not in that session |
| 429 | `presence/rate_limited` + `Retry-After` | Rate limit |
| 503 | `service/not_ready` | Temporarily unavailable; the next 60 s re-send retries |

## Display

Shown on the player's Gridspawn profile, friends list, club member lists and
hover cards as `<app display name> · details · state (size/max)`, or
"Playing <app display name>" when there is neither details nor state.

What is shown is subject to the player's existing game-activity privacy:

- the share switch;
- a per-game hide setting for each SDK game;
- the player being online in Gridspawn (invisible or offline hides it).

Environments:

- `production` presence is shown to everyone the privacy settings allow.
- `development` and `staging` presence is shown **only to the player
  themselves**, so developers can test their integration.

Party size is shown; party IDs never are.

## Open questions for reviewers

1. Is a 120 s expiry with a 60 s re-send right for Godot's main loop, or do you
   prefer an explicit long-lived connection?
2. Do you need `party` join/invite flows in v1, or only display?
