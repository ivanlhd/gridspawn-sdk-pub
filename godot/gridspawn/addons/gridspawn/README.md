# Gridspawn for Godot

Player sign-in with the system browser, player assertions for your game
server, and joining game sessions, for Godot 4.3 and later.

Status: 0.1.0. Builds for Windows x86_64 and Linux x86_64. Licence: MIT OR
Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`). The bundled Rust bindings for
Godot are MPL-2.0: see `THIRD-PARTY.md`.

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
