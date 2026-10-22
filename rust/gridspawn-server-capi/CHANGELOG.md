# Changelog

## 0.2.0 (2026-10-23)

- Voice rules, mirroring `gridspawn-server` 0.3.0: `gs_voice_configure` (JSON), `gs_voice_player_state_init`, `gs_voice_set_player_state`, `gs_voice_mute`, `gs_voice_publish_positions`, and the position publisher (`gs_voice_publisher_spawn` / `_update` / `_remove` / `_last_error` / `_free`); `GsVoiceAck`.
- Error codes `GS_ERROR_CODE_VOICE_*` (added after `GS_ERROR_CODE_OTHER`), `gs_error_field` and `gs_error_reason`.
- ABI version unchanged (1): only functions, structs and enum values were added.
