# Changelog

## 0.1.1 (2026-10-22)

- Fix: after a listener's last remote speaker left, the next speaker to join could not be received (the receive renegotiation was refused, "invalid proposed signaling state transition … have-remote-offer") and the session ended with `voice/transport_failed`. The receive offer now keeps its transport when every consumer has closed, a refused renegotiation is rolled back, and a failed removal is reported as a `Warning` event instead of being ignored.
- `rust-version` corrected to 1.91; 1.85 never built with the locked dependencies.
