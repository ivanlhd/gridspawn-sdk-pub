# 07 Game actions (**Current**)

Deployed on the SDK edge on 2026-11-05; a club form's email and password
fields since 2026-11-07 (UTC). This is the wire contract of the edge routes
for game actions (`sdk-api-edge-rs`) and of the reference client
`sdk/rust/gridspawn-server/src/actions.rs`.

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
  environment. Never a platform user id, an email or a name. It is the same
  on every press by this player and on the SDK sign-in that follows. An
  action can be the first time the game sees the subject: the player may
  have shared their identity from a club block without opening the game.
- One game account per player is your rule to keep. Key accounts on
  `actor.subject`. Answer a repeat with `declined` and your own
  `userMessage` (for example "You already have an account here"). Never key
  accounts on a name the player typed into the payload. The platform enforces
  nothing beyond its own limits.
- `payload` is a JSON object of at most 4 KiB (serialized) and depth 8
  (a scalar counts 0, each object or array 1). Never trust it. The platform
  scans every key and string in it and refuses the press when one holds an
  email address or a Gridspawn id, with two exceptions, both under keys a
  club's form declares (see "Actions from club pages"):
- Under the one key a club's form declares as an email field, `payload`
  holds one email address the player typed for your game's account: one
  address and nothing else, at most 254 characters, never their Gridspawn
  email, and not verified. Treat it as personal data: keep it only as long
  as the account needs it, and never show it to other players. The platform
  logs it only as `"[email]"`.
- Under the one key a club's form declares as a password field, `payload`
  holds the password the player chose for your game's account: 8 to 128
  characters, no whitespace at either end, otherwise any text (it is not
  scanned for emails; a Gridspawn id in it is refused), never their
  Gridspawn password. The platform logs it only as `"[password]"`. Hash it
  at once with a password hash (Argon2id, scrypt or bcrypt), never store or
  log it as given, and never send it back: not in `detail`, `data` or
  `userMessage`.
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

## Actions from club pages

A club page sends a game action **by name**, and what a name means is up to
your game: register an account, buy an item, claim a reward, join an event,
vote, redeem a code, anything. You choose the names, add them in the Dev Hub
and handle them on your server; the club's page creator puts them on the
page. The player sees your result: `ok`, `declined` with your `userMessage`,
or a failure.

**Two places on a page send one.** Both send the `name` and `version` the
page's creator entered, with `source.kind` `club_block` and the `clubId`,
`channelId` and `blockId` of the block that sent it.

- **A Form block.** One action when the player presses the form's button.
  `payload` has one key per answered question, **exactly as the club
  manager named the field**:
  - short and long text and email: the text, trimmed;
  - password: exactly as typed, never trimmed;
  - number: a number (a whole number when the field takes whole numbers);
  - checkbox: `true` or `false` (always sent);
  - choice: the chosen option's value, or a list of values when the field
    takes several;
  - an optional question left empty is not in the payload.

  A form is the only place that can declare an email field and a password
  field (one of each), the two exceptions to the payload scan above.
- **The "Send game action" node action** (a button or any other node of the
  page). `payload` has up to 12 values the page's creator wires, each under a
  key they name, read from text they typed, a value of the page (including
  your game's published data) or a property of another node. Text is
  trimmed, numbers, booleans and lists are sent as they are, and an empty
  value is left out. There is no email or password field here: an email
  anywhere in it is refused.

Keys are lower-case letters, digits and `_`, start with a letter, and have at
most 32 characters. They are the creator's choice, so agree on them with the
club, or read the ones a name needs and answer `declined` when one is
missing.

**Environment.** A press is sent to `production` unless the page's setting
names `development` or `staging`, which only the club's managers can send
(for testing). A form made in the form editor always uses `production`.

**What your server does for every name.**

- Key the player on `actor.subject`, never on a name typed into the payload.
  The subject is the same on every press by this player and on the SDK
  sign-in that follows; an action can be the first time your game sees it.
- Deduplicate on `actionId`, as for every action (the same press can arrive
  twice).
- Answer `ok` when it worked, `declined` with your own `userMessage` when
  your rules say no, and `error` when you could not process it.

**Example: a registration form.** A form with the questions "Nome de
usuário", "Senha", "E-mail" and "Aceito as regras" sends, under the name the
club manager entered (for example `player.register`):

```json
{ "nome_usuario": "zoro", "senha": "…", "email": "zoro@example.com", "aceito_regras": true }
```

Create the game account for `actor.subject`; hash `senha` at once and never
log it or send it back; keep `email` only as long as the account needs it.
When the subject already has an account, or the name is taken, answer
`declined` with a `userMessage` such as "You already have an account here."
The form shows its success text on `ok` and its declined text on `declined`,
each followed by your `userMessage` when you send one.

**Example: a shop button.** A "Send game action" on a button sends
`shop.buy` with `{"itemId": "red_potion", "quantity": 3}`. Charge the
player's gold in the same transaction as the `actionId` insert, and answer
`ok` with `{"balance": 120}`, or `declined` with `game/not_enough_gold` and
"Not enough gold." The reference crate's `examples/action_shop.rs` answers
`shop.buy` this way, with the gold kept in memory.

**Setup.**

In the Dev Hub, for your application:

1. A server key with the scope `actions.receive` (05, Setup).
2. The `actions` module on for each environment you serve.
3. Every action name added to the names your server accepts. A press for a
   name that is not added ends with `actions/disabled`.
4. Production needs Gridspawn's approval for your application. Until then a
   press in `production` ends with `actions/disabled`.
5. Your server polling for the name: a press for a name no instance polled
   for in the last 35 s ends with `actions/no_server`.

In the club, your game must be connected with its data shown in that
environment; otherwise the press is refused before it reaches your server.
The club's creator then puts the action on the page: in a Form block's Sending
tab (the game, "Action name", "Version", then "Taking answers" on), or as a
"Send game action" on a node.

## Reference client

`gridspawn-server` 0.6.0 (`Client::spawn_action_receiver`,
`blocking::Client::spawn_action_receiver_blocking`) and the C ABI
`gridspawn-server-capi` 0.5.0 (`gs_actions_start`, `gs_action_reply_*`).
