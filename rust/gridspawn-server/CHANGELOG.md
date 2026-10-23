# Changelog

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
