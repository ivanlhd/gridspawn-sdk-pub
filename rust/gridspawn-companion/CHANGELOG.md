# Changelog

## 0.1.1 (unreleased)

- The voice window's close button hides the window instead of closing it.
  Closing it used to leave a window that stopped responding. Voice and the
  attached session go on and end with the game.
- New request `show_window` (`{visible}`), sent by `gs_rag_show_window`,
  shows or hides the window again; it works while the window is hidden. It
  is refused with `companion/no_window` when the game started the companion
  without a window.
- The process ends when the companion's work ends (game exit,
  `gs_rag_shutdown`, pipe closed), whatever the window's state; it no
  longer waits for the window to close.

## 0.1.0 (unreleased)

- First version: the x64 companion for `gridspawn_rathena.dll` (pipe protocol
  version 1): attach and detach, voice with device selection and local
  per-player mute and volume, sign-in, player assertion, presence.
- Optional voice window (egui).
- Exits with the game process.
- Testing-only tone mode.
- Live test `examples/two_client_voice.rs`.
