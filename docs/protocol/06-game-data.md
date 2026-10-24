# 06 Game data from game servers — **Current**

Deployed on the SDK edge on 2026-10-23. Live checks the same day, with the
reference crate `gridspawn-server` 0.4.0 on a test application's
development data:
- the writer's listing, including removed keys;
- writes with and without a TTL, numbered above every listed `seq` after
  each restart;
- a personal-data refusal, with the JSON path in `field`;
- an event, its repeat with the same `Idempotency-Key` (`duplicate: true`),
  and an event with a generated key;
- removal, leaving tombstones in the listing.

Sources: the platform's game-data design note (`docs/game-data.md`, §2 model
and quotas, §3 write path); reference client
`sdk/rust/gridspawn-server/src/game_data.rs`.

A game's own server publishes **live data** that the game's club can show on
its pages: player counts, the current round, a scoreboard, map markers. It
also publishes **events** ("round won", "boss defeated"). Clubs build their
pages from the game's keys with the club layout editor. Writing data changes
no page by itself.

- **No game session is needed.** Data belongs to the app and environment of
  the server credential.
- **Public by nature.** Production data appears on public club pages,
  readable by anyone. Publish display names only, never emails, account or
  participant ids, or anything private (§6).

## 1. Authentication — Current

```
Authorization: GridspawnServer <credential>
```
- **Scope:** `game_data.write`, given to the server key in the Dev Hub.
- **Module:** `game_data` must be switched on for the app and environment.
- **Origin:** any `Origin` header is refused (`data/browser_forbidden`).
  These routes are for game servers only.
- **Caching:** every answer has `Cache-Control: no-store`.

Base: the SDK base URL (`https://sdk.gridspawn.net`).

## 2. Documents — Current

A document is a named JSON value, kept per app and environment.

- **Key:** `^[a-z][a-z0-9_.-]{0,63}$`. Dots are only a naming convention.
- **Value:** any JSON. At most 4 KiB serialized, nesting depth ≤ 8, finite
  numbers.
- **`ttlSeconds`:** 10–86,400, optional. Without it, the document lives
  until it is removed.

### 2.1 Write (batched, all or nothing)

```
PUT /v1/server/game-data
{
  "set":    { "<key>": { "value": <JSON>, "seq": <int>, "ttlSeconds": <10..86400> }, … },
  "remove": [ { "key": "<key>", "seq": <int> }, … ]
}
```
- `set` and `remove` are both optional. Together they hold 1–40 entries;
  more is `400 data/invalid_value`, field `entries`, reason `count`.
- The body is at most 32 KiB.
- **All or nothing:** a validation error or a quota breach refuses the whole
  batch, and nothing is stored.

Answer:

```json
{
  "ok": true,
  "accepted": ["match.score"],
  "skipped":  [{ "key": "match.round", "seq": 57 }],
  "quota":    { "keys": 12, "maxKeys": 256, "bytes": 20480, "maxBytes": 262144 }
}
```

`skipped` gives the **stored** `seq` (§3).

### 2.2 Read, for the writer

```
GET /v1/server/game-data
```

```json
{
  "keys": [
    { "key": "match.score", "seq": 41, "bytes": 31, "ttlRemaining": null,
      "updatedAt": "2026-10-23T12:00:00Z" },
    { "key": "lobby.open", "seq": 39, "bytes": 0, "ttlRemaining": null,
      "updatedAt": "2026-10-23T11:58:00Z", "removed": true }
  ],
  "quota": { "keys": 12, "maxKeys": 256, "bytes": 20480, "maxBytes": 262144 }
}
```
- **Contents:** no values. Keys removed less than 24 h ago (tombstones) are
  included, with `"removed": true`.
- **`ttlRemaining`:** seconds, or `null`.
- **`updatedAt`:** server time, for information only.
- A writer calls it once at start-up (§3).

### 2.3 Markers — Current

A document whose value has this shape is a set of **markers**. Club pages can
draw it on a "Game markers" layer of their world view: a live map of players,
objectives or events.

```json
{
  "markers": [
    { "id": "flag-red", "label": "Red flag", "x": 120.5, "y": -40, "kind": "objective" },
    { "id": "p-17", "label": "Ana", "x": 98, "y": 12.25 }
  ]
}
```

- **Shape:**
  - at most 64 markers;
  - `id` and `label` are text;
  - `x` and `y` are finite numbers;
  - `kind` is optional, at most 32 characters.
  - Any other shape is an ordinary document.
  - The value limit of §2 still applies: 64 markers must fit in 4 KiB,
    about 60 bytes each. Keep ids and labels short.
- **Coordinates:** `x` and `y` are in the game's own world units. There is
  no fixed unit, origin or axis direction.
  - The club's creator places them on the page with the layer's "Game area"
    (minimum and maximum x and y) and a "Y points up" switch.
  - Publish in one stable coordinate space, and say in your own docs which
    one, so creators can set the area once.
- **One layer, one document:** a "Game markers" layer draws exactly one
  document (one key). A document cannot span several keys, and it does not
  carry its own area: the "Game area" and "Y points up" are page settings.
  For several groups (teams, objectives), publish one key per group; the
  creator adds one layer for each.
- **`id`:** stable for the same marker across updates. The page follows
  markers by id from one update to the next.
- **`label`:** public, like all game data (§6). Display names only, never
  account ids or emails.
- **A live picture:**
  - Every update carries the whole set. Send it at most a few times a
    second; the reference publisher sends 1–5 per second.
  - When nothing should show, remove the document or send an empty list.
  - A removed or expired document draws nothing.
- **`kind`:** free text, for example `objective`, `ally`, `enemy`. Club pages
  may style markers by kind later; for now each layer draws one style.
- **Status:** storing and reading markers is live. Drawing them on club pages
  comes with the next Gridspawn app release.

## 3. Ordering — Current

- **`seq`:** each `set` and `remove` entry carries its own integer, chosen
  by the writer, up to 2⁵³ − 1. Per key, it must strictly increase.
- **Stale entries are skipped, not refused.** An entry whose `seq` is not
  above the key's stored one is listed in `skipped` with the stored `seq`.
  The rest of the batch still applies.
- **Removal:** `remove` keeps a tombstone with its `seq` for 24 h, so a
  delayed older `set` cannot bring the key back.
- **One writer per key.** Game-server processes that run side by side
  (shards, regions) write disjoint keys, for example under a `shard-eu.`
  prefix.
- **Start-up and restarts:**
  - A writer reads `GET /v1/server/game-data` once. It starts its counter
    above the highest `seq` listed, tombstones included.
  - As a safety net (a tombstone older than the listing, or another
    writer), an entry that is still skipped is sent once more, with a `seq`
    above the stored one.
  - If it is skipped again, another writer owns the key.
- **Retries:** a retried write that already landed comes back in `skipped`,
  with the `seq` it was sent with. It is not an error.
- **No clock is involved.** Server times in answers are for information
  only.

## 4. Events — Current

```
POST /v1/server/game-events
Idempotency-Key: <[A-Za-z0-9_-]{8,64}>

{ "name": "<name>", "payload": { … } }
→ 201 { "ok": true, "id": "<server sequence>", "duplicate": false }
```
- **`name`:** `^[a-z0-9][a-z0-9_.-]{0,47}$`.
- **`payload`:** a JSON object, at most 4 KiB.
- **`Idempotency-Key`:** **required**, `[A-Za-z0-9_-]{8,64}`.
  - A missing header is `400 data/invalid_value`, field `Idempotency-Key`,
    reason `missing`. Any other length or character gives reason
    `idempotency_key`. The server never generates a key.
  - A repeat with the same key within 24 h returns the first answer, with
    `"duplicate": true`, so retries are safe.
- **`id`:** the server's sequence per app and environment, as a string.
- **Retention:** the last 100 events, none older than 7 days.

## 5. Limits — Current

Per app and environment. Every limit fails closed (503 when it cannot be
checked).

| What | Limit |
|---|---|
| document entries (`set` and `remove`, counted per entry) | 20/s (bursts of 40); per key 5/s (bursts of 10) |
| events | 10/s |
| the writer's listing (`GET`, §2.2) | 10 per 10 s. It is for start-up, not for polling |
| live keys | 256 |
| total value bytes | 256 KiB (262,144) |

- Tombstones do not count toward the key or byte quota.
- A batch holds at most 40 entries, matching the burst.

## 6. Public by nature — Current

- **What to publish:** production game data is shown on public club pages.
  Publish display names only. Never publish emails, account ids,
  participant ids or anything private.
- **What the server refuses** (a safety net, not a filter to rely on):
  - **Checked:** every string and object key in a value or payload, data
    keys, and event names.
  - **Refused:** email shapes, and the Gridspawn id and credential prefixes
    `gsp1.`, `gssn1.`, `gve1.`, `gsc1.` and `gsc_`.
  - **Answer:** `400 data/invalid_value` with reason `personal_data`, and
    `field` set to the JSON path, for example
    `set.scoreboard.value.players[3].name`.

## 7. Errors — Current

Body: `{"ok": false, "error": {"code", "message", "field"?, "reason"?}, "requestId"}`.

| HTTP | Code | When |
|---|---|---|
| 400 | `data/invalid_value` (`field`, `reason`: `key`, `name`, `json`, `depth`, `size`, `count`, `ttl`, `seq`, `missing`, `idempotency_key`, `personal_data`) | Malformed or out of bounds. A single value over 4 KiB is `size`; more than 40 entries is `count`; a missing, negative, non-integer or too large `seq` is `seq`; a missing or malformed event `Idempotency-Key` is `missing` or `idempotency_key` |
| 401 | `server/authentication_required` | Bad credential |
| 403 | `server/scope_denied` | The key lacks `game_data.write` |
| 403 | `data/browser_forbidden` | Any `Origin` |
| 403 | `data/module_disabled` | Game data is off for the app and environment |
| 409 | `data/quota_exceeded` (`field`: `keys` or `bytes`) | The batch would pass 256 live keys or 256 KiB |
| 429 | `data/rate_limited` + `Retry-After` (seconds) | §5 |
| 503 | `data/unavailable` | Store or limiter unreachable. Nothing was stored; retry |

There is no `stale_sequence` error: stale entries are skipped (§3).

## 8. SDKs — Current

`gridspawn-server` 0.4.1 (Rust) and its C ABI `gridspawn-server-capi` 0.3.0
cover this document:
- `client.game().data()`: `set`, `set_with_ttl`, `remove`, `set_many` and
  `list`.
- 0.4.1: `Markers` / `Marker` build a §2.3 markers document with its limits
  checked; `DataPublisher::set_markers` sends one.
- `spawn_data_publisher(hz)` (1–5 Hz):
  - It sends the latest value per key, one batch per tick, oldest change
    first, within §5. `set` and `set_with_ttl` are cheap, so call them every
    frame.
  - It retries what can succeed later (network errors, 5xx, 429 after
    `Retry-After`) with the newest values.
  - A refusal that retrying cannot fix drops those entries and is reported
    as the last error: an invalid or personal-data value, or
    `data/quota_exceeded`.
- `client.game().events().emit` and `emit_with_id`: an `Idempotency-Key` on
  every request, either generated or the caller's.
- The ordering of §3: one in-process counter, seeded once from the listing,
  and one resend. No clock is involved.
- The C ABI mirrors all of this (`gs_game_data_*`, `gs_game_event_emit`).

Game clients and the Godot addon have nothing to do here: this is a
server-side feature.
