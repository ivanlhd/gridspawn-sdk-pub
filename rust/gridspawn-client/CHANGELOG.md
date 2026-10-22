# Changelog

## 0.2.0 (2026-10-22)

- Presence on the client path (protocol 03): `Client::presence()` / `AttachedSession::presence()` give a `PresenceUpdater` that re-sends every 60 s; `Client::voice_url()` and `presence_url()`; typed presence error codes and `ApiError::field`.
