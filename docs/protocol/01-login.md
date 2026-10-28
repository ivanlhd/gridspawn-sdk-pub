# 01 Login — Current (§6 Proposed)

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

## 6. Sign in through the Gridspawn app — Proposed

Proposed; the released desktop app (0.1.12+604) answers it. This section
specifies the app-brokered flow.

When the player has the Gridspawn desktop app open and signed in, a native
game can ask the app instead of opening the browser. The player confirms in
the app, and the game gets an authorization code, which it exchanges as in
§2 with its own PKCE verifier. If the app is not there, or cannot help, the
game uses the browser flow of §1 without telling the player.

Browser games cannot use this: there is no local pipe in a browser.

### 6.1 Transport

The app listens on the local RPC pipe that Discord-compatible games already
use:
- **Windows:** `\\.\pipe\discord-ipc-0` … `-9`.
- **Linux and macOS:** a Unix socket `discord-ipc-0` … `-9` in the first
  existing directory among `$XDG_RUNTIME_DIR`, `$TMPDIR`, `$TMP`, `$TEMP`
  and `/tmp`. The app does not host one there yet.

**Frames:** an opcode (u32, little-endian), a length (u32, little-endian),
then a UTF-8 JSON object of at most 64 KiB. The opcodes are 0 handshake,
1 frame, 2 close, 3 ping, 4 pong.

Discord may own one of these pipes, usually `-0`. A client tries the slots
in order, and uses a pipe only after the Gridspawn handshake answer (§6.2).

### 6.2 Handshake

Client → app, opcode 0:

```json
{ "v": 1, "client_id": "<appId>", "gridspawn": { "protocol": 1 } }
```

App → client, opcode 1:

```json
{ "cmd": "DISPATCH", "evt": "READY", "nonce": null,
  "data": { "v": 1, "gridspawn": { "protocol": 1 } } }
```

- **Accept only the Gridspawn READY.** A client uses the pipe only if
  `data.gridspawn.protocol` is `1`. Anything else ends that pipe and moves
  on to the next slot: another READY, a close (opcode 2), an error, or no
  answer within 500 ms.
- **No user data:** the Gridspawn READY carries none. Nothing about the
  player is shared before they confirm.
- **Time budget:** the search over all slots takes at most 1.5 s.

### 6.3 Authorize

Client → app, opcode 1, once per connection:

```json
{
  "cmd": "AUTHORIZE",
  "nonce": "<request id, echoed back>",
  "args": {
    "client_id": "<appId>",
    "environment": "production",
    "scopes": ["identity.basic"],
    "code_challenge": "<BASE64URL(SHA256(verifier))>",
    "code_challenge_method": "S256",
    "state": "<random>"
  }
}
```

- `environment`: `development`, `staging` or `production`.
- `scopes`, `code_challenge` and `state` follow §1's rules.
- There is no OAuth `nonce` on the pipe: `nonce` is the frame's request id.
- **The player's answer:** the app shows a confirmation naming the game and
  the requested scopes. If the player has already agreed to these scopes for
  this game, the app answers without asking and shows a short notice. The
  app dismisses an unanswered confirmation after 5 minutes. `gridspawn-client`
  waits at most 120 s, or its sign-in timeout when that is shorter
  (`sdk/rust/gridspawn-client/src/broker.rs:26`, `auth.rs:435`), then closes
  the connection, which dismisses the confirmation.
- **Cancelling:** if the client closes the connection, the app dismisses the
  confirmation.
- **One request per connection:** one `AUTHORIZE` per connection, and the
  client closes the connection after the answer. The app shows one
  confirmation at a time and queues the others.

**Approved:**

```json
{ "cmd": "AUTHORIZE", "evt": null, "nonce": "<same>",
  "data": { "code": "<code>", "state": "<same state>" } }
```

**Refused:**

```json
{ "cmd": "AUTHORIZE", "evt": "ERROR", "nonce": "<same>",
  "data": { "code": 4000, "message": "…", "error": "access_denied" } }
```

The OAuth `error` decides what happens (§6.4). The numeric `code` is 4000 on
every refusal. 4009 is the answer of an app or connection that does not
support this.

### 6.4 Using the answer

- **Approved:**
  - check that `state` matches; if it does not, stop (do not fall back to
    the browser);
  - exchange the code as in §2, with
    `redirect_uri=urn:gridspawn:app-broker` and the client's own
    `code_verifier`;
  - the code's lifetime and single use are as in §1.
  - The URN is reserved for codes issued through the app: it is accepted
    for every application with no registration, and `/authorize` refuses
    it.
- **`error: "access_denied"`:** the player declined, or the request did not
  come from the player's own Windows session and user. Stop, exactly as
  when they press Deny in the browser. Do not open the browser.
- **Fall back to the browser (§1), without telling the player:**
  - `data.code` 4009: an app that does not support this;
  - `error: "login_required"`: the app is signed out, or the account still
    needs setup, which the browser pages can do;
  - `error: "temporarily_unavailable"`;
  - a broken connection, or an answer that does not match the request.
- **`error: "invalid_request"`:** a malformed `AUTHORIZE`, or a second one
  on the same connection. Stop, and report it.
- **Any other `error`** (for example `invalid_client`, `invalid_scope`):
  stop, and report it, as the browser flow would.
