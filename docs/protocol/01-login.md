# 01 Login — Current

OAuth 2.0 authorization code with PKCE (RFC 7636, S256 only) for public
clients, with RFC 8252 loopback redirects for native apps. It is **not**
OpenID Connect: there is no `id_token`; the token response carries a `user`
object instead.

Sources: `sdk-auth-rs/src/http/mod.rs`, `sdk-auth-rs/src/service.rs`,
`sdk-auth-contracts-rs/src/{validation,policy}.rs`, reference client
`sdk/typescript/packages/core/src/auth.ts`.

Live metadata (`/.well-known/oauth-authorization-server`, verified 2026-10-18):
issuer `https://sdk.gridspawn.net`; `response_types_supported: ["code"]`;
`grant_types_supported: ["authorization_code","refresh_token"]`;
`code_challenge_methods_supported: ["S256"]`;
`token_endpoint_auth_methods_supported: ["none"]`.

## Scopes

`identity.basic presence.write presence.party voice.join voice.position interakt.receive`.
Client-side validation (`auth.ts:295-305`): 1–32 scopes, each `^[A-Za-z0-9.\-_:]+$`
and ≤ 128 chars, deduplicated and sorted. Server limits are tighter:
`MAX_SCOPES = 24`, `MAX_SCOPE_LENGTH = 96` (`policy.rs:21-22`).

## 1. Authorize (system browser)

The native client starts a loopback HTTP listener, then opens the system browser at:

```
GET {oauth}/authorize
  ?response_type=code
  &client_id=<appId>
  &environment=<env>
  &redirect_uri=http://127.0.0.1:<port>/callback
  &scope=<space-separated>
  &state=<random>
  &nonce=<random>
  &code_challenge=<BASE64URL(SHA256(verifier))>
  &code_challenge_method=S256
```

`{oauth}` is `endpoints.oauth` from bootstrap (`https://sdk.gridspawn.net/oauth`).

Redirect URI rules (`sdk-auth-contracts-rs/src/validation.rs:75-96`):

- Exact string match against a registered URI, **or** RFC 8252 loopback match:
  both registered and requested are `http`, host is the IP literal `127.0.0.1`
  or `[::1]`, identical path and query, no fragment, no userinfo. **Only the
  port may differ.**
- `localhost` is **not** treated as loopback. Use `127.0.0.1`.
- Max length 2048.

`verifier`: 43–128 chars of `[A-Za-z0-9-._~]` (`validation.rs:64-73`).
`state` and `nonce`: ≤ 512 chars each (`policy.rs:24-25`). The authorization
request lives 10 minutes (`AUTHORIZATION_REQUEST_TTL`).

The user signs in and consents at `auth.gridspawn.net`. The browser is then
redirected to the loopback URI with either `?code=<code>&state=<state>` or
`?error=<oauth_error>&state=<state>`.

The client must verify, before using the code (`auth.ts:101-117`): the callback
path and query match the redirect URI it sent, and `state` equals what it sent.
The code lives **60 seconds** (`AUTHORIZATION_CODE_TTL`) and is single-use.

## 2. Token exchange

```
POST {oauth}/token?client_id=<appId>&environment=<env>
Content-Type: application/x-www-form-urlencoded

grant_type=authorization_code&client_id=<appId>&code=<code>
&redirect_uri=<same redirect_uri>&code_verifier=<verifier>
```

No `Origin` header and no client authentication. Response 200:

```json
{
  "access_token": "…",
  "token_type": "Bearer",
  "expires_in": 600,
  "refresh_token": "…",
  "scope": "identity.basic voice.join",
  "user": {
    "subject": "…",
    "displayName": "…",
    "avatarObjectPath": "…"
  }
}
```

- `avatarObjectPath` is optional and is an **object path**, not a URL. Resolve
  it through `endpoints.mediaResolver`; never store CDN URLs.
- `subject` is an opaque stable identifier. It must never be shown in public UI;
  show `displayName` only.
- Reject the response unless `token_type == "Bearer"`, both tokens are non-empty
  strings, `expires_in > 0`, and `user.subject`/`user.displayName` are strings (`auth.ts:267-293`).

## 3. Refresh

```
POST {oauth}/token?client_id=<appId>&environment=<env>
Content-Type: application/x-www-form-urlencoded

grant_type=refresh_token&client_id=<appId>&refresh_token=<refresh_token>
```

Same response shape. Policy (`policy.rs:5-14`):

| Token | Lifetime |
|---|---|
| Access token | 10 minutes |
| Refresh token | rotated on every use; 30 days inactivity, 90 days absolute |
| Rotation grace | the superseded refresh token is accepted once more for 5 minutes if its replacement was never used |

The reference client refreshes when the access token has under 30 s left.
On a non-retryable refresh error, clear stored tokens and require interactive
sign-in again. Refresh tokens must be stored in OS-protected storage
(Windows DPAPI / macOS Keychain / Linux Secret Service), never in plain files
or in the game's save data.

## 4. Revoke (sign out on this device)

```
POST {oauth}/revoke?client_id=<appId>&environment=<env>
Content-Type: application/x-www-form-urlencoded

token=<refresh_token>&client_id=<appId>
```

Clear local storage before the call. Account-wide sign-out is not available
(`auth.ts:166-172`).

## 5. Player assertion (prove the player to the game server)

Used when a signed-in player joins a game server (see 02).

```
POST {base}/v1/auth/player-assertions?client_id=<appId>&environment=<env>
Authorization: Bearer <access_token>
Content-Type: application/json

{ "audience": "game_server", "challenge": "<from the game server>" }
```

`challenge`: 32–256 chars of `[A-Za-z0-9._~-]`, generated by the game server.
Response:

```json
{ "playerAssertion": "…", "tokenType": "…", "expiresIn": 60 }
```

The assertion lives 60 seconds (`PLAYER_ASSERTION_TTL`), is single-use, and is
bound to the app, environment and challenge. The client sends it to its game
server over the game's own connection; the server redeems it (02 §3).
