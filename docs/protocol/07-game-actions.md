# 07 Game actions — **Proposed**

Not deployed. This is the wire contract of the edge routes built for game
actions (`sdk-api-edge-rs`, branch `game-actions-a1`) and of the reference
client `sdk/rust/gridspawn-server/src/actions.rs`. The design, its reasons and
the owner's decisions are in `docs/game-sdk/game-actions.md`.

The platform asks a game's own server to do something for a signed-in
player, for example a purchase from a club shop block. The game runs it and
answers with a result the player sees.

- **The game server polls.** It keeps one request open to the edge; when an
  action is ready, the edge answers it. All traffic is outbound from the game
  server.
- **Nothing runs without a confirmed ack.** The game server must ack each
  action and may start it only after `accepted: true`.
- **The game must deduplicate.** The same `actionId` can reach a handler
  again after a restart or on another instance. The game inserts the
  `actionId` into a uniquely keyed table **in the same database transaction
  as the charge**, before anything else, in storage every instance shares.
- **The game's clock is never trusted.** Every timer in a client is measured
  on its own monotonic clock from when the poll answer arrived. Only the
  difference `deadline - issuedAt` of the envelope is used, never the
  absolute times against the local clock.

## Server credential

All four routes use `Authorization: GridspawnServer <credential>` with the
scope `actions.receive`, no `Origin` header, and the `actions` module on for
the application and environment. The edge takes the application and
environment from the credential.

Errors use the edge's shape `{"ok": false, "error": {"code", "message"},
"requestId"}`.

| Code | Status | When |
|---|---|---|
| `actions/unavailable` | 503 | Actions are off on this edge, or its store is unreachable. Retry later. |
| `actions/disabled` | 403 | The `actions` module is off for the application and environment (poll). |
| `actions/invalid` | 400 | The body broke a rule below. |
| `actions/rate_limited` | 429 | Eight polls of this credential are already waiting. `Retry-After` in seconds. |
| `actions/not_found` | 404 | A result for an unknown action, or one another credential confirmed. |
| `actions/result_too_late` | 409 | The result window (60 s after the deadline) has closed. |
| `server/authentication_required`, `server/scope_denied` | 401, 403 | As for every server route. |

## POST /v1/server/actions/poll

```json
{ "instanceId": "inst-4b0c…", "names": ["shop.buy"], "max": 4, "waitSeconds": 20, "acked": [] }
```

- `instanceId`: `[A-Za-z0-9_-]{8,64}`, random per process (or per receiver).
- `names`: 1–16 distinct action names (letters, digits, `.` and `_`, 3–64).
- `max`: 1–8 actions per answer. Send the number of free handler slots.
- `waitSeconds`: 1–25. The edge waits at most the smaller of this and its
  own cap (20 s by default), then answers `{"actions": []}`.
- `acked`: optional, at most 32 ids, for the record only; it never starts
  anything.

Use a request timeout above the wait (the reference client uses the wait
plus 10 s). Each poll keeps the names live for 35 s: an action for a name
no instance polled for in that time is refused before it is sent.

Answer `200 {"actions": [<envelope>…]}`:

```json
{
  "actionId": "gsa1.<26 base32 chars>",
  "name": "shop.buy",
  "version": 1,
  "payload": { "itemId": "red_potion", "quantity": 3 },
  "actor": { "subject": "gss_…" },
  "source": { "kind": "club_block", "clubId": "…", "channelId": "…", "blockId": "…" },
  "issuedAt": "2026-11-04T10:00:00.000Z",
  "deadline": "2026-11-04T10:00:10.000Z",
  "attempt": 1
}
```

- `actor.subject` is the player's pairwise subject for this application and
  environment, as the game got it when the player signed in. Never a platform
  user id, an email or a name.
- `payload` is a JSON object of at most 4 KiB. Never trust it.
- `deadline - issuedAt` (10 s) is a hint of how long the player is likely to
  wait. Place it on your own clock from when the answer arrived. Do not
  compare either timestamp with your clock.
- `attempt` is 1, or 2 when the edge redelivers an unconfirmed action once.

## POST /v1/server/actions/ack

```json
{ "actionId": "gsa1.…", "attempt": 1, "instanceId": "inst-4b0c…" }
```

Send it the moment an action arrives. Answer `200 {"accepted": true}` or
`{"accepted": false}`.

- **Start the action only on `accepted: true`.** `false` means it was
  already decided: confirmed by another instance, or not confirmed within
  3 s and redelivered (or given up as not delivered).
- Retry on network errors, timeouts, 5xx and 429 until you get an answer.
  An ack that already won answers `true` again for the same `instanceId`.
  More than about 5 s after the deadline an ack can only answer `false`; stop
  retrying then.
- An action accepted after its deadline should not be run: answer it with
  `error` and `game/expired`.

## POST /v1/server/actions/{actionId}/result

```json
{ "status": "ok", "data": { "balance": 120 } }
{ "status": "declined", "code": "game/not_enough_gold", "userMessage": "Not enough gold.", "detail": "needs 30, has 12" }
{ "status": "error", "code": "game/database_unavailable", "detail": "inventory store timeout" }
```

- `status`: `ok`, `declined` (a game reason) or `error` (could not process).
- `code`: `game/snake_case`, 3–64 characters; required for `declined` and
  `error`.
- `userMessage`: plain text of at most 160 characters, shown to the player.
- `detail`: at most 500 characters, for developers in the Dev Hub only.
- `data`: an object of at most 1 KiB the block may show.
- The edge scans `userMessage` and `data` for emails and Gridspawn ids; on a
  hit the player sees a generic text and the original is kept in `detail`.

Only the credential that confirmed the action may post its result. Answer
`200 {"ok": true}`, or `{"ok": true, "duplicate": true}` when a result was
already taken (nothing changes). Retry on network errors, timeouts, 5xx and
429 until the window closes, 60 s after the deadline (`actions/result_too_late`).

A handler is never cancelled at the deadline: a slow result inside the
window still reaches the player.

## POST /v1/server/actions/stop

```json
{ "instanceId": "inst-4b0c…" }
```

On a clean stop. Answer `200 {"ok": true}`. Other instances that still poll
the same names keep them live.

## Reference client

`gridspawn-server` 0.6.0 (`Client::spawn_action_receiver`,
`blocking::Client::spawn_action_receiver_blocking`) and the C ABI
`gridspawn-server-capi` 0.5.0 (`gs_actions_start`, `gs_action_reply_*`).
