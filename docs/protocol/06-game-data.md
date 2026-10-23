# 06 Game data from game servers — **Proposed**

Not implemented. This is the shape under review before it is built. Source:
the platform's game-data design note (`docs/game-data.md`, §2 model and
quotas, §3 write path).

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

## 1. Authentication — Proposed

```
Authorization: GridspawnServer <credential>
```
- **Scope:** `game_data.write`, given to the server key in the Dev Hub.
- **Module:** `game_data` must be switched on for the app and environment.
- **Origin:** any `Origin` header is refused (`data/browser_forbidden`).
  These routes are for game servers only.
- **Caching:** every answer has `Cache-Control: no-store`.

Base: the SDK base URL (`https://sdk.gridspawn.net`).

## 2. Documents — Proposed

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

## 3. Ordering — Proposed

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

## 4. Events — Proposed

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

## 5. Limits — Proposed

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

## 6. Public by nature — Proposed

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

## 7. Errors — Proposed

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

## 8. SDKs — Proposed

`gridspawn-server` 0.4.0 (Rust) and its C ABI are planned to cover this
document:
- `client.game().data()`: `set`, `set_with_ttl`, `remove`, `set_many` and
  `list`.
- `spawn_data_publisher(hz)`: the latest value per key, one batch per tick.
- `client.game().events().emit` and `emit_with_id`: an `Idempotency-Key` on
  every request, either generated or the caller's.
- The ordering of §3, including the start-up read and the resend.

Game clients and the Godot addon have nothing to do here: this is a
server-side feature.
