# Changelog

## 0.3.0 (2026-10-23)

- **The released Gridspawn desktop app on Windows** (0.1.12+604, stable since 2026-10-27) answers the in-app sign-in; with an older app (or on Linux/macOS, where the app hosts no pipe) the browser is used as before.
- One login (protocol 01 §6): `sign_in()` first asks the Gridspawn desktop app over its local pipe (`discord-ipc-0..9`; a Gridspawn handshake, accepted only from a READY carrying `gridspawn.protocol == 1`; at most 1.5 s, 500 ms per pipe). The player confirms in the app and the code is exchanged with `redirect_uri=urn:gridspawn:app-broker` and the client's own PKCE verifier. No app, an older app (4009), `login_required`, `temporarily_unavailable` or a broken connection fall back to the browser flow silently.
- The player has 2 minutes to answer in the app (the request's timeout when shorter); then the connection is closed, which closes the app's confirmation, and the sign-in ends with `sign_in/timed_out`. A decline is `auth/access_denied`, as in the browser; `invalid_request` and other OAuth errors are final.
- `SignIn::browser_only(bool)`; `Client::sign_in_method() -> Option<SignInMethod>` (`App` or `Browser`; `None` after `restore()` and `sign_out()`). `sign_in()` still returns `User`; `begin_sign_in()` stays browser-only.

## 0.2.0 (2026-10-22)

- Presence on the client path (protocol 03): `Client::presence()` / `AttachedSession::presence()` give a `PresenceUpdater` that re-sends every 60 s; `Client::voice_url()` and `presence_url()`; typed presence error codes and `ApiError::field`.
