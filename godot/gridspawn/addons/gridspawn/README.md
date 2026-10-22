# Gridspawn for Godot

Player sign-in with the system browser, player assertions for your game
server, joining game sessions, and in-game voice, for Godot 4.3 and later.

Status: 0.1.0. Builds for Windows x86_64 and Linux x86_64. Licence: MIT OR
Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`). The bundled Rust bindings for
Godot are MPL-2.0, and the voice code includes WebRTC audio processing,
Abseil and libopus: see `THIRD-PARTY.md`.

## Install

1. Copy `addons/gridspawn` into your project, with its `bin/` folder.
2. Project → Project Settings → Autoload: add
   `res://addons/gridspawn/gridspawn.tscn` with the name **`Gridspawn`**.
3. Open `gridspawn.tscn` and set `app_id` and `environment` (in the Inspector).

Your application needs a loopback redirect (`http://127.0.0.1/callback`) and
the scopes you request registered in its client settings. Without them,
sign-in fails with `auth/invalid_client` or `auth/invalid_request`.

## Use

```gdscript
func _ready() -> void:
    Gridspawn.signed_in.connect(func(name): $Hello.text = "Signed in as %s" % name)
    Gridspawn.sign_in_failed.connect(func(code, message): push_warning("%s: %s" % [code, message]))
    Gridspawn.restore()

func _on_play_pressed() -> void:
    if not Gridspawn.is_signed_in():
        Gridspawn.sign_in()
        await Gridspawn.signed_in
    # The game server sends a challenge; answer with an assertion.
    Gridspawn.player_assertion(challenge_from_server)
    var assertion: String = await Gridspawn.assertion_ready
    send_to_game_server(assertion)
    # The server admits the player and returns a ticket.
    Gridspawn.attach(ticket_from_server)
```

Every method returns at once. Results arrive as signals on the main thread.

| Method | Result |
|---|---|
| `restore()` | `signed_in(display_name)`, `signed_out`, or `sign_in_failed(code, message)` |
| `sign_in()` | Opens the browser → `signed_in` or `sign_in_failed` |
| `cancel_sign_in()` | `sign_in_failed("sign_in/cancelled", …)` |
| `sign_out()` | `signed_out` |
| `is_signed_in() -> bool` | Immediate |
| `player_display_name() -> String` | Immediate; `""` when signed out |
| `sessions_persist() -> bool` | Whether sign-in is remembered across launches |
| `player_assertion(challenge)` | `assertion_ready(assertion)` or `assertion_failed(code, message)` |
| `attach(ticket)` | `attached(session_id, display_name, capabilities)` or `attach_failed(code, message)` |
| `renew()` | `grant_renewed(module)` or `grant_expired(module, code)` |
| `detach()` | Stops renewing the attached session's grants |

Also `session_expired(code)`: the stored sign-in was refused and cleared, so
the player signs in again.

- **Identity:** show `display_name` only.
- **Offline:** with a stored sign-in, `restore()` may answer
  `sign_in_failed("network", …)` while `is_signed_in()` stays true. The game
  can carry on offline and show the player's name.
- **Grants:** an attached session's grants are renewed in the background
  until `detach()`.

## Voice

Players in an attached game session talk to each other in the game. Godot's
own audio is used: the microphone, your buses, and the game's output device.

1. Project → Project Settings → Autoload: add
   `res://addons/gridspawn/gridspawn_voice.tscn` with the name
   **`GridspawnVoice`**, below `Gridspawn`.
2. Project Settings → Audio → Driver: turn on **Enable Input** (the
   microphone). Without it, `join()` fails with
   `voice/microphone_unavailable`.

```gdscript
func _on_attached(_session_id, _display_name, capabilities) -> void:
    if "voice" in capabilities:
        GridspawnVoice.join()

func _ready() -> void:
    Gridspawn.attached.connect(_on_attached)
    GridspawnVoice.joined.connect(func(_room): $VoiceIcon.show())
    GridspawnVoice.speaking_started.connect(func(id): highlight(id, true))
    GridspawnVoice.speaking_stopped.connect(func(id): highlight(id, false))
    GridspawnVoice.error.connect(func(code, message): push_warning("%s: %s" % [code, message]))
```

| Method | Result |
|---|---|
| `join()` | `joined(room)` or `error(code, message)` |
| `leave()` | `left(reason)` |
| `is_joined() -> bool` | Immediate |
| `set_muted(muted)`, `is_muted()` | This player's microphone |
| `set_deafened(deafened)` | Silences everyone else for this player |
| `set_push_to_talk(enabled)`, `set_transmitting(on)` | Push-to-talk and the key's state |
| `set_player_volume(participant_id, gain)` | 0.0 to 2.0, this player's preference |
| `get_participants() -> Array[Dictionary]` | `participant_id`, `display_name`, `speaking`, `gain` |
| `get_local_participant_id() -> String` | This player's own id in the speaking signals |
| `get_input_devices()`, `set_input_device(name)` | Godot's microphone selection |
| `get_output_devices()`, `set_output_device(name)` | Godot's output device (the whole game's) |
| `get_stats() -> Dictionary` | Counters for diagnostics |

Also `participant_joined(participant_id, display_name)`,
`participant_left(participant_id)`, `speaking_started(participant_id)` and
`speaking_stopped(participant_id)`, for other players and for this player.

Inspector properties:
- `echo_cancellation`, `noise_suppression`, `auto_gain`: on by default. Echo
  cancellation uses everything the game plays (the Master bus) as its
  reference, so when a player uses speakers instead of a headset, the others
  do not hear themselves, or the game's sounds, coming back through that
  player's microphone.
- `push_to_talk`: off by default, so the microphone is open and sends while
  the player speaks.
- `input_bus` (created and muted if missing) and `output_bus` (`Master`).
- `input_source`: **Custom** sends samples you push with `push_input()`
  (mono, 48 kHz) instead of the microphone.

Limits of this version:
- One voice room per game session, where everyone hears everyone.
- `participant_id` is opaque and never for display. `display_name` is still
  empty; names come with a later server update.
- Deafening is local: the audio still arrives, it is not played.
- Proximity, groups and server mute come later.

Voice error codes start with `voice/` (for example `voice/not_attached`,
`voice/microphone_unavailable`, `voice/module_unavailable`,
`voice/transport_failed`), or are `network`.

## Where the sign-in is kept

The `token_storage` property defaults to **Os Secure** in the Inspector. It
uses the operating system's credential store:
- Windows Credential Manager;
- the macOS Keychain;
- the Secret Service on Linux.

Where none is available, the session is kept in memory and the player signs
in on each launch. That happens on Linux without a keyring daemon; plan for
it on Steam Deck in Game Mode too. `sessions_persist()` tells you which case
applies. Sign-in is never stored in files or save data.

## Error codes

Codes are the service's (`auth/invalid_grant`, `session/ticket_invalid`, …) or
local (`sign_in/cancelled`, `sign_in/timed_out`, `sign_in/browser_unavailable`,
`sign_in/state_mismatch`, `sign_in/required`, `network`, `timeout`). Messages
are for logs and never contain secrets.
