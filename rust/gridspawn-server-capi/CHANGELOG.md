# Changelog

## 0.4.0 (2026-10-24)

Mirrors `gridspawn-server` 0.5.0.
- **Behaviour change:** `gs_game_data_set_many` (and the publisher) no longer fail as a whole when one entry is refused. The others are stored; when every entry is refused, the first refusal is the error.
- Data publisher:
  - `gs_game_data_publisher_flush` and `_shutdown` (`timeout_ms`; `GS_STATUS_TIMEOUT` with the waiting keys in `*unsent_json_out`);
  - `_results_json` and `_dropped_results`;
  - `_keep_alive` and `_stop_keep_alive`;
  - `_seq_base`.
- `gs_game_data_publisher_free` still stops at once and loses what was not sent; call `_shutdown` first.
- After `_shutdown`, the other publisher calls report "publisher stopped".
- Event publisher: `GsEventPublisher` with `gs_game_event_publisher_spawn`, `_emit`, `_results_json`, `_queued`, `_last_error`, `_flush`, `_shutdown` and `_free`.
- ABI version unchanged (1): only functions and an opaque handle were added.

## 0.3.0 (2026-10-23)

- Live game data, mirroring `gridspawn-server` 0.4.0: `gs_game_data_set` / `_remove` / `_set_many` (`GsDataEntry`) / `_list` (JSON), `GsDataAck` with `gs_data_ack_accepted_len` / `_key`, `gs_data_ack_skipped_len` / `_key` / `_seq`, `gs_data_ack_quota` (`GsDataQuota`) and `gs_data_ack_free`, the publisher (`gs_game_data_publisher_spawn` / `_set` / `_remove` / `_skipped_json` / `_last_error` / `_free`), and `gs_game_event_emit`.
- Error codes `GS_ERROR_CODE_GAME_DATA_INVALID_VALUE`, `_MODULE_DISABLED`, `_QUOTA_EXCEEDED`, `_UNAVAILABLE` (32–35).
- `gs_error_retry_after_ms`: its comment no longer says that no service sends Retry-After.
- ABI version unchanged (1): only functions, structs and enum values were added.

## 0.2.0 (2026-10-23)

- Voice rules, mirroring `gridspawn-server` 0.3.0: `gs_voice_configure` (JSON), `gs_voice_player_state_init`, `gs_voice_set_player_state`, `gs_voice_mute`, `gs_voice_publish_positions`, and the position publisher (`gs_voice_publisher_spawn` / `_update` / `_remove` / `_last_error` / `_free`); `GsVoiceAck`.
- Error codes `GS_ERROR_CODE_VOICE_*` (added after `GS_ERROR_CODE_OTHER`), `gs_error_field` and `gs_error_reason`.
- ABI version unchanged (1): only functions, structs and enum values were added.
