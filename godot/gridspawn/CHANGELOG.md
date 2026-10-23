# Changelog

## 0.4.0 (2026-10-23)

- **The in-app sign-in needs a Gridspawn desktop app release that is not out yet.** Until then, `sign_in()` uses the browser exactly as before.
- One login (gridspawn-client 0.3.0): `sign_in()` asks the Gridspawn app when it is open, and the player just confirms there; otherwise the browser opens as before. The player has 2 minutes to answer in the app (`sign_in/timed_out` after that); declining is `auth/access_denied`.
- `browser_only` property (always use the browser); `sign_in_method()` returns `"app"`, `"browser"` or `""`. `cancel_sign_in()` also closes the app's confirmation.

## 0.3.1 (2026-10-23)

- Players the game server muted before this player joined are reported right after `joined`: `server_mute_changed(participant_id, true)`, and `server_muted` is true in `get_participants()` (gridspawn-voice 0.2.2). Before, they showed as unmuted until the next mute change.
- An unmute refused with `voice/server_muted` leaves `is_muted()` true and sends `server_mute_changed` with this player's id.

## 0.3.0 (2026-10-23)

- Voice rules from the game server: `audibility_changed(participant_id, audible, gain, pan)`, `room_config_changed(config)`, `authority_changed(available)`, `server_mute_changed(participant_id, muted)`; `get_room_config()`, `is_secrecy_safe()`; voices play in stereo with the server's pan.
- Players are listed with their display names (in `participant_joined` and `get_participants()`) as soon as they join, and `participant_id` is the id your game server got when it admitted the player (gridspawn-voice 0.2.0).
- `get_participants()`: `volume` is this player's own setting (it was called `gain`); `gain` and `pan` are now the game server's; new `audible`, `server_muted`, `locally_muted`.
- `room` property for `join()`; `set_player_muted(participant_id, muted)`; client-spatialized rooms: `bind_participant(participant_id, node)` / `unbind_participant(participant_id)` (an `AudioStreamPlayer3D` following the node), `voice/not_client_spatial` elsewhere.

## 0.2.1 (2026-10-22)

- Fix (gridspawn-voice 0.1.1): after the last other player in a voice room left, the next one to join was not heard and the voice session ended with `voice/transport_failed`. Players can now leave and join again freely.

## 0.2.0 (2026-10-22)

- In-game voice (`GridspawnVoice` node, `gridspawn_voice.tscn`) and presence (`Gridspawn.set_presence` / `clear_presence`); the addon ships the voice code's third-party notices.
