# Changelog

## 0.2.1 (2026-10-23)

- A speaker whose receive negotiation fails is never dropped silently: the round is retried once (`Warning` `voice/receive_retried`, naming the participant); if it fails again the section is closed, the SFU consumer is closed, and the new non-fatal `VoiceEvent::ReceiveFailed { participant, code, message }` is sent. The session goes on (before, the session ended).
- Live tests print the host's load and each join's admission and consume times at the start and the end.

## 0.2.0 (2026-10-23)

- Server-side voice rules: `VoiceEvent::Audibility` (the server's gain and pan per speaker, applied before mixing; the jitter buffer starts over when a speaker becomes audible again), `RoomConfig` (and `room_config()`), `Authority` (and `authority_available()`), `ServerMuted`, `LocalServerMuted` (and `is_server_muted()`); `ParticipantInfo` gains `audible`, `gain`, `pan`, `server_muted` and `locally_muted`.
- **Participant ids are now the game session's participant ids** (`gsp1.…`, from the SFU's roster), with display names, and participants appear when they join, not when they first speak. Against a voice service without a roster, endpoint ids as before.
- `VoiceOptions::room` (default `match`); `set_participant_muted` (pauses that participant's audio on the SFU for this client only); `pull_participant_stereo` and `pull_mix_stereo` (equal-power pan); `participant(id)`.
- Error codes `voice/room_not_supported`, `voice/room_not_configured`, `voice/room_full`, `voice/server_muted` (previously `voice/admission_…` / `voice/sfu_…` forms).
- A producer announced twice is consumed once; the producer never sends `allowedPeerIds` (tested).
- SFU token renewal is timed from the server's own clock (`Date`) on the monotonic clock: no dependence on the local wall clock.

## 0.1.1 (2026-10-22)

- Fix: after a listener's last remote speaker left, the next speaker to join could not be received (the receive renegotiation was refused, "invalid proposed signaling state transition … have-remote-offer") and the session ended with `voice/transport_failed`. The receive offer now keeps its transport when every consumer has closed, a refused renegotiation is rolled back, and a failed removal is reported as a `Warning` event instead of being ignored.
- `rust-version` corrected to 1.91; 1.85 never built with the locked dependencies.
