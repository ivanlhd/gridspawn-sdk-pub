# Changelog

## 0.2.1 (2026-10-22)

- Fix (gridspawn-voice 0.1.1): after the last other player in a voice room left, the next one to join was not heard and the voice session ended with `voice/transport_failed`. Players can now leave and join again freely.

## 0.2.0 (2026-10-22)

- In-game voice (`GridspawnVoice` node, `gridspawn_voice.tscn`) and presence (`Gridspawn.set_presence` / `clear_presence`); the addon ships the voice code's third-party notices.
