# Changelog

## 0.5.0 (2026-10-24)

Breaking:
- **A refused entry no longer fails the whole write.** `GameData::set_many` (and `set` / `remove`) stores the other entries and lists the refused ones in the new `DataAck::rejected` (`RejectedKey`: `key`, `field`, `reason`). Only when every entry is refused is the first refusal the error, as before for a single entry. Rate limits, the quota and other failures still refuse the whole write.
  - Writes send `Gridspawn-Partial: 1`, and one parser reads both answers: the partial one (`accepted` as a count, `skipped[].storedSeq`, `rejected`, and a 400 with `rejected` when every entry is refused) and today's all-or-nothing one.
  - From an all-or-nothing edge, the crate drops the entry the refusal names (`set.<key>…` or `remove[<i>]…`) and sends the rest again with the same numbers: at most one extra request per refused entry.
- **`DataPublisher::stop()` and dropping a publisher still stop at once, losing what was not sent (removals included); that is now documented, and they are no longer the way to end a publisher cleanly.** Use the new `shutdown(timeout)`, which sends what is pending first.

Added:
- `DataPublisher`:
  - `flush(timeout)` and `shutdown(timeout)`. `Unsent` lists what was still waiting at the timeout.
  - `results()`: the final outcome per key (`DataResult`; `DataOutcome::Stored`, `Removed`, `OwnedElsewhere` or `Refused`). The last 256 are kept; `dropped_results()` counts the rest.
  - `keep_alive(key)` / `stop_keep_alive(key)`. The latest value is sent again with a new sequence number every TTL/2 after it was stored, never more often than every 5 s; it needs a TTL (10 s at least).
  - `seq_base()`.
- The publisher reads the listing once when it starts and logs the sequence base.
  - A refused entry drops only itself.
  - Over the quota (409), the batch's removals go again alone and its sets are refused.
  - `last_error()` is cleared only by a batch that goes through without a refusal.
- `EventPublisher` (`Game::spawn_event_publisher()`):
  - `emit` / `emit_with_id` check and queue (at most 1000) and return the `Idempotency-Key`.
  - Events go in order, at most 10 a second. Network errors, 5xx and 429 (after `Retry-After`) are retried with the same key, backing off up to 30 s.
  - `results()` (`EventResult`), `queued()`, `last_error()`, `flush`, `shutdown`, `stop`.
- `GameData::seq_base()`.

## 0.4.1 (2026-10-23)

- `Markers` / `Marker` for live maps (protocol 06 §2.3): `Markers::new().push(Marker::new(id, label, x, y).kind(k))`, `to_value()` checks at most 64 markers, a non-empty id and label, finite coordinates, a kind of at most 32 characters and 4 KiB in all (`Error::InvalidInput` with the field); `DataPublisher::set_markers(key, &markers)`.

## 0.4.0 (2026-10-23)

- Live game data for the club's pages (protocol 06), no game session needed: `Client::game()` gives a `Game` (one writer per client) with `data()` — `set`, `set_with_ttl`, `remove`, `set_many(DataBatch)` (1–40 entries, all or nothing), `list()` (the writer's listing; for start-up and checks, not polling) — `events()` — `emit` (generated Idempotency-Key) and `emit_with_id` — and `spawn_data_publisher(hz)` (1–5 Hz; the latest value per key, oldest change first, within the service's entry limits; `skipped()`, `last_error()`).
- Ordering without clocks: every entry carries a number from one in-process counter, seeded once (single-flight) above every stored number, tombstones included; a skipped entry is resent once above its stored number, and a key still skipped is reported in `DataAck::skipped` (another writer owns it). A retried write that already landed counts as accepted.
- Error codes `GameDataInvalidValue` (with `field` and `reason`; `ApiError::is_personal_data()`), `GameDataModuleDisabled`, `GameDataQuotaExceeded`, `GameDataUnavailable`; `data/browser_forbidden` and `data/rate_limited` map to `BrowserForbidden` and `RateLimited`.
- Blocking facade: `game_data_set`, `game_data_remove`, `game_data_set_many`, `game_data_list`, `game_event_emit`.

## 0.3.0 (2026-10-23)

- Voice rules: `Session::voice()` gives a `VoiceControl` with `configure` (rooms: open, channels, proximity, hybrid; server or client spatial; ghosts; caps), `set_player_state` (required before a player's voice ticket in a configured room; a warning is logged otherwise), `mute`, `publish_positions` and `spawn_position_publisher`. Updates carry increasing sequence numbers; `VoiceAck` reports `enforced` and `overtaken`. `VoicePosition::planar` for 2D games. `VoiceConfig::from_json`.
- Voice error codes (`VoiceInvalidRequest`, `VoiceStaleSequence`, `VoiceParticipantNotFound`, `VoiceModuleDisabled`, `VoiceUnavailable`; `voice/browser_forbidden`, `voice/session_not_found` and `voice/rate_limited` map to the existing codes) and `ApiError::reason`.
- Blocking facade: `voice_configure`, `voice_set_player_state`, `voice_mute`, `voice_publish_positions`.

## 0.2.0 (2026-10-22)

- Presence on the server path (protocol 03): `Participant::set_presence` / `clear_presence`, `Session::participant(id)`, typed presence error codes and `ApiError::field`.
