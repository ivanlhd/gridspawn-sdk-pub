# Gridspawn for Godot

Player sign-in (through the Gridspawn app when it is open, otherwise the
system browser), player assertions for your game server, joining game
sessions, and in-game voice, for Godot 4.3 and later.

Status: 0.4.0. Builds for Windows x86_64 and Linux x86_64. Licence: MIT OR
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
| `sign_in()` | Asks the Gridspawn app when it is open, otherwise opens the browser → `signed_in` or `sign_in_failed` |
| `cancel_sign_in()` | `sign_in_failed("sign_in/cancelled", …)` |
| `sign_in_method() -> String` | Immediate: `"app"`, `"browser"`, or `""` (after `restore()`, or signed out) |
| `sign_out()` | `signed_out` |
| `is_signed_in() -> bool` | Immediate |
| `player_display_name() -> String` | Immediate; `""` when signed out |
| `sessions_persist() -> bool` | Whether sign-in is remembered across launches |
| `player_assertion(challenge)` | `assertion_ready(assertion)` or `assertion_failed(code, message)` |
| `attach(ticket)` | `attached(session_id, display_name, capabilities)` or `attach_failed(code, message)` |
| `renew()` | `grant_renewed(module)` or `grant_expired(module, code)` |
| `detach()` | Stops renewing the attached session's grants |
| `set_presence(details, state, location)` | `presence_set(expires_at)` or `presence_failed(code, message)` |
| `clear_presence()` | `presence_cleared` or `presence_failed(code, message)` |

Also `session_expired(code)`: the stored sign-in was refused and cleared, so
the player signs in again.

- **One login:** if the player has the Gridspawn desktop app open and signed
  in, `sign_in()` asks the app: the player confirms there and no browser
  opens. Otherwise, or if the app is not installed, the browser opens as
  before. You do not need to do anything for this.
  - **This needs a Gridspawn desktop app release that is not out yet.**
    Until it ships, `sign_in()` uses the browser exactly as before.
  - The player has 2 minutes to answer in the app; then `sign_in_failed`
    comes with `sign_in/timed_out`.
  - Declining in the app is `auth/access_denied`, as in the browser.
  - Set the `browser_only` property to always use the browser.
  - To show players: "If the Gridspawn app is open, you just confirm in the
    app. Otherwise your browser opens to sign in."
- **Identity:** show `display_name` only.
- **Offline:** with a stored sign-in, `restore()` may answer
  `sign_in_failed("network", …)` while `is_signed_in()` stays true. The game
  can carry on offline and show the player's name.
- **Grants:** an attached session's grants are renewed in the background
  until `detach()`.

## Presence

`set_presence("Ranked match", "In lobby", "Neon District")` shows what the
player is doing on their Gridspawn profile, friends list and club member
lists, as "<your game> · Ranked match · In lobby". Empty strings are left
out; `set_presence("", "", "")` shows "Playing <your game>".
- **Re-sending:** the presence is re-sent every 60 s until
  `clear_presence()` (it expires 120 s after the last update). If a re-send
  is refused later, `presence_failed` arrives.
- **Whose token:** the attached session's presence grant when its ticket
  included presence, otherwise the signed-in player. Then sign-in needs the
  `presence.write` scope: add it to `scopes`.
- **Guests** have no presence.
- **Limit:** 5 updates per 20 s per player, so set it when the activity
  changes, not every frame.
- **Refused text:** emails, web addresses and identifiers in the text
  (`presence/text_not_allowed`).
- **When your game server also sets presence**, the server's is shown.
- **Not yet in GDScript:** the start time (shown as time played) and the
  party size. A dictionary-based `set_presence_ex(dict)` with them is planned
  for the next version. Game servers can set both today (`gridspawn-server`).

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
| `join()` | Enters the room `room` → `joined(room)` or `error(code, message)` |
| `leave()` | `left(reason)` |
| `is_joined() -> bool` | Immediate |
| `set_muted(muted)`, `is_muted()` | This player's microphone |
| `set_deafened(deafened)` | Silences everyone else for this player (the audio still arrives) |
| `set_push_to_talk(enabled)`, `set_transmitting(on)` | Push-to-talk and the key's state |
| `set_player_volume(participant_id, volume)` | 0.0 to 2.0, this player's preference |
| `set_player_muted(participant_id, muted)` | Mutes one player for this player only; the voice server stops sending their audio here |
| `get_participants() -> Array[Dictionary]` | `participant_id`, `display_name`, `speaking`, `volume`, `audible`, `gain`, `pan`, `server_muted`, `locally_muted` |
| `get_local_participant_id() -> String` | This player's own id in the speaking signals |
| `get_room_config() -> Dictionary` | `managed`, and with rules `mode`, `spatial`, `secrecy_safe`, `max_participants` |
| `is_secrecy_safe() -> bool` | False only in client-spatialized rooms |
| `bind_participant(participant_id, node)`, `unbind_participant(participant_id)` | Client-spatialized rooms: play a player's voice from a `Node3D` |
| `get_input_devices()`, `set_input_device(name)` | Godot's microphone selection |
| `get_output_devices()`, `set_output_device(name)` | Godot's output device (the whole game's) |
| `get_stats() -> Dictionary` | Counters for diagnostics |

Also `participant_joined(participant_id, display_name)`,
`participant_left(participant_id)`, `speaking_started(participant_id)` and
`speaking_stopped(participant_id)`, for other players and for this player;
and the game server's rules (below): `audibility_changed(participant_id,
audible, gain, pan)`, `room_config_changed(config)`,
`authority_changed(available)` and `server_mute_changed(participant_id,
muted)`.

**Players:**
- Show `display_name` only. `participant_id` is the id your game server got
  when it admitted the player, so you can match voices to players; never
  show it.
- Everyone in the room is listed, also players who are not speaking.

**Who hears whom** is decided by your game server (`gridspawn-server`:
channels such as teams, proximity, dead and spectating players, and an
authoritative mute). The voice server sends nothing for a pair that may not
hear each other, so a modified client cannot listen in. Each player gets a
`gain` and, with positions and a facing, a `pan` per speaker; the addon plays
voices in stereo with them. Without rules from your game server, everyone in
the room hears everyone.
- `room`: `match` exists in every session. Other rooms exist when your game
  server configured them (and the voice service has named rooms on).
  Otherwise `join()` fails with `voice/room_not_supported` or
  `voice/room_not_configured`; a full room answers `voice/room_full`.
- `server_mute_changed(id, muted)`: the game server muted or unmuted a
  player. Players muted before this one joined are reported right after
  `joined` (and `server_muted` is set in `get_participants()`).
- `server_mute_changed(id, true)` with this player's own id: the game server
  muted this player. Unmuting is refused (`voice/server_muted`) until it
  unmutes.
- `authority_changed(false)`: the rules cannot be reached for a while, and
  nothing is forwarded (it fails closed). `authority_changed(true)` when
  they are back.
- **Client-spatialized rooms** (`spatial = "client"`): the voice server sends
  every stream the channels allow, at gain 1 without pan.
  `bind_participant(id, node)` plays that player through an
  `AudioStreamPlayer3D` that follows `node`, so Godot's 3D audio places it.
  These rooms are **not secrecy-safe** (`is_secrecy_safe()` is false): a
  modified client hears everyone its channels allow, so say so in your UI.
  In other rooms `bind_participant` fails with `voice/not_client_spatial`.

Inspector properties:
- `room`: the voice room `join()` enters (`match`).
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

Voice error codes start with `voice/` (for example `voice/not_attached`,
`voice/microphone_unavailable`, `voice/module_unavailable`,
`voice/room_not_configured`, `voice/room_full`, `voice/server_muted`,
`voice/not_client_spatial`, `voice/transport_failed`), or are `network`.

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

Codes are the service's (`auth/invalid_grant`, `session/ticket_invalid`,
`presence/scope_denied`, …) or
local (`sign_in/cancelled`, `sign_in/timed_out`, `sign_in/browser_unavailable`,
`sign_in/state_mismatch`, `sign_in/required`, `network`, `timeout`). Messages
are for logs and never contain secrets.
