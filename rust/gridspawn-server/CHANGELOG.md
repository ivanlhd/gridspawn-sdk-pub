# Changelog

## 0.3.0 (2026-10-23)

- Voice rules: `Session::voice()` gives a `VoiceControl` with `configure` (rooms: open, channels, proximity, hybrid; server or client spatial; ghosts; caps), `set_player_state` (required before a player's voice ticket in a configured room; a warning is logged otherwise), `mute`, `publish_positions` and `spawn_position_publisher`. Updates carry increasing sequence numbers; `VoiceAck` reports `enforced` and `overtaken`. `VoicePosition::planar` for 2D games. `VoiceConfig::from_json`.
- Voice error codes (`VoiceInvalidRequest`, `VoiceStaleSequence`, `VoiceParticipantNotFound`, `VoiceModuleDisabled`, `VoiceUnavailable`; `voice/browser_forbidden`, `voice/session_not_found` and `voice/rate_limited` map to the existing codes) and `ApiError::reason`.
- Blocking facade: `voice_configure`, `voice_set_player_state`, `voice_mute`, `voice_publish_positions`.

## 0.2.0 (2026-10-22)

- Presence on the server path (protocol 03): `Participant::set_presence` / `clear_presence`, `Session::participant(id)`, typed presence error codes and `ApiError::field`.
