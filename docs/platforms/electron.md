# Electron integration

Status: **`@gridspawn/core` 0.2.1 and `@gridspawn/voice` 0.3.1 on npm. A studio has run them in an Electron renderer with `contextIsolation` on: bootstrap, session attach and joining Voice work. Joining Voice needs core 0.2.1 (0.2.0 failed there with "Illegal invocation").** The same packages serve the game's plain browser build (§6).

The renderer uses the browser packages as they are; they import no Node modules. Everything Electron-specific stays in the app's main process and preload script: serving the game, opening the sign-in page, and passing the OAuth callback back to the page over the app's own IPC.

## 1. Serve the game from a standard, secure scheme

The SDK needs a secure context. Core's PKCE uses Web Crypto (`crypto.subtle`; without it sign-in fails with `platform/crypto_unavailable`) and Voice uses `navigator.mediaDevices.getUserMedia` (without it, `voice/browser_media_unavailable`). Both exist only in secure contexts. A page loaded from `file://` is not an option either: `file` is not an accepted origin scheme (protocol 01, "From a browser page").

Serve the game from the app's own scheme, registered before the app is ready:

```js
// main.js
const { app, net, protocol } = require('electron');
const path = require('node:path');
const { pathToFileURL } = require('node:url');

protocol.registerSchemesAsPrivileged([
  { scheme: 'app', privileges: { standard: true, secure: true, supportFetchAPI: true } },
]);

const root = path.join(__dirname, 'game');
app.whenReady().then(() => {
  protocol.handle('app', (request) => {
    const file = path.join(root, decodeURIComponent(new URL(request.url).pathname));
    if (!file.startsWith(root + path.sep)) return new Response('Not found', { status: 404 });
    return net.fetch(pathToFileURL(file).toString());
  });
  // mainWindow.loadURL('app://localhost/index.html');
});
```

- `standard: true`: relative URLs resolve as on the web, and web storage works (Electron disables it for non-standard schemes).
- `secure: true`: Electron adds the scheme to Chromium's secure schemes, so the page is a secure context. Check that `window.isSecureContext` is `true` in the renderer.
- `registerSchemesAsPrivileged` can be called only once, before `app` is ready.
- The page's origin is what the Gridspawn services check. Read `window.location.origin` in the renderer (`app://localhost` above): that exact value is the origin to register.

## 2. What to register

- **Browser origin:** `app://localhost` (the page's `window.location.origin`), for each environment the game uses, plus the https origin of the plain browser build. The game's owner adds them in the Dev Hub, in the game's client settings (Sign-in and modules, "Browser origins"). Until it is registered, bootstrap answers 403 `config/origin_not_allowed`.
- **Redirect URI**, in the same Dev Hub client settings, one of:
  - a reverse-domain scheme the game owns, such as `com.studio.game:/oauth` (a scheme with a dot, then `:/` and a path);
  - Desktop sign-in (the loopback switch): `http://127.0.0.1<path>`, any port.

  The page origin is not a redirect URI: `app://localhost/callback` is refused with `scheme`.
- **Scopes and modules** as for any client: `identity.basic` to sign in, `voice.join` for Voice.

## 3. Sign in from the renderer

```ts
import { ExternalAuthorizationDriver, Gridspawn } from '@gridspawn/core';
import { withVoice } from '@gridspawn/voice';

const authorization = new ExternalAuthorizationDriver({
  open: (url) => window.gridspawnAuth.open(url),
});
window.gridspawnAuth.onCallback((url) => authorization.completeAuthorization(url));

const gridspawn = withVoice(await Gridspawn.create({
  appId: 'your_app_id',
  environment: 'production',
  redirectUri: 'com.studio.game:/oauth',
  authorizationDriver: authorization,
}));

const user = await gridspawn.auth.signIn({ scopes: ['identity.basic', 'voice.join'] });
```

`signIn` builds the PKCE challenge and state as in a browser, calls `open` with the authorization URL and waits. `completeAuthorization(url)` returns `true` when the URL belongs to the pending sign-in (its `state` matches) and sign-in continues: the redirect check, then the token exchange. Any other URL returns `false` and is ignored. One sign-in waits at a time; starting another ends the first. A sign-in ends with:

| Code | When |
|---|---|
| `auth/authorization_cancelled` | the app called `cancelAuthorization()`, for example when the user closed the sign-in window |
| `auth/authorization_superseded` | a newer sign-in replaced it |
| `auth/authorization_timeout` | no callback within `timeoutMs` (default 10 minutes) |
| `auth/authorization_open_failed` | `open` threw or rejected (the cause is kept) |
| `auth/<oauth error>` | the callback carried `error`, for example `auth/access_denied` |

The default token store is memory-only, so the player signs in again after a restart. To keep the session, pass a `tokenStorage` whose `load`, `save` and `clear` go to the main process, which keeps the record with Electron's `safeStorage`; refresh tokens belong in OS-protected storage (protocol 01 §3).

## 4. The IPC relay

The renderer never sees Node or Electron; the preload script exposes two narrow functions (one per IPC message, never `ipcRenderer` itself).

```js
// preload.js
const { contextBridge, ipcRenderer } = require('electron');
contextBridge.exposeInMainWorld('gridspawnAuth', {
  open: (url) => ipcRenderer.invoke('gridspawn-auth:open', url),
  onCallback: (listener) => ipcRenderer.on('gridspawn-auth:callback', (_event, url) => listener(url)),
});
```

```js
// main.js: the system browser, and the callback on a reverse-domain scheme
const { app, ipcMain, shell } = require('electron');
const SCHEME = 'com.studio.game';
let mainWindow; // the game's BrowserWindow

if (!app.requestSingleInstanceLock()) app.quit();
app.setAsDefaultProtocolClient(SCHEME);
const relay = (url) => {
  if (typeof url === 'string' && url.startsWith(`${SCHEME}:`)) {
    mainWindow?.webContents.send('gridspawn-auth:callback', url);
  }
};
app.on('open-url', (event, url) => { event.preventDefault(); relay(url); }); // macOS
app.on('second-instance', (_event, argv) => relay(argv.find((arg) => arg.startsWith(`${SCHEME}:`)))); // Windows, Linux
ipcMain.handle('gridspawn-auth:open', (_event, url) => {
  if (new URL(url).origin !== 'https://sdk.gridspawn.net') throw new Error('Not a Gridspawn sign-in page');
  return shell.openExternal(url);
});
```

- The main process opens only Gridspawn's sign-in origin (the origin of bootstrap's `endpoints.oauth`).
- Register `open-url` early, before waiting for `ready`, so a callback is not missed.
- On macOS and Linux the scheme works only in the packaged app, and the packager must declare it (`Info.plist`, the `.desktop` file); Electron Forge and Electron Packager have a `protocols` option. In development on Windows with the default Electron executable, call `app.setAsDefaultProtocolClient(SCHEME, process.execPath, [path.resolve(process.argv[1])])`.

**Loopback instead of a scheme.** When sign-in starts, the main process listens on `127.0.0.1` port 0 (`node:http`), returns the port over IPC, and the renderer calls `signIn({ scopes, redirectUri: \`http://127.0.0.1:${port}/callback\` })` (the loopback switch accepts any port). The server relays `http://127.0.0.1:${port}${request.url}` with `gridspawn-auth:callback`, answers with a short "You can return to the game" page and closes.

**A sign-in window instead of the system browser.** RFC 8252 §8.12 asks native apps not to use embedded user agents for authorization, so prefer the system browser. A game that still opens the page in its own `BrowserWindow` does this in the `gridspawn-auth:open` handler instead of `shell.openExternal`, and catches the callback before Chromium tries to load the scheme:

```js
const signInWindow = new BrowserWindow({ parent: mainWindow, width: 540, height: 760 });
const catchCallback = (event) => {
  if (!event.url.startsWith(`${SCHEME}:`)) return;
  event.preventDefault();
  relay(event.url);
  signInWindow.close();
};
signInWindow.webContents.on('will-redirect', catchCallback); // a server redirect
signInWindow.webContents.on('will-navigate', catchCallback); // a navigation by the page
signInWindow.loadURL(url).catch(() => {});
```

With a window, no operating-system scheme registration is needed. Tell the renderer when the user closes the window, so it can call `cancelAuthorization()`.

## 5. Voice

- `@gridspawn/voice` runs in the renderer as in a browser. mediasoup-client 3.22 picks its WebRTC handler from the Chromium version it reads from `navigator.userAgentData` (the `Chromium` brand) or `Chrome/<n>` in the user agent; Chromium 111 and later use its `Chrome111` handler. A custom user agent must keep `Chrome/<n>`.
- Autoplay: Electron's default `autoplayPolicy` is `no-user-gesture-required`, so remote voice plays without a click. With `user-gesture-required` or `document-user-activation-required`, and in the plain browser build, playback can be blocked: the session reports `voice/playback_blocked`; call `resumePlayback()` from a click or key press.
- Microphone: `getUserMedia` needs the secure context of §1. On macOS the app must set `NSMicrophoneUsageDescription` in its `Info.plist`, and can ask with `systemPreferences.askForMediaAccess('microphone')` before joining. If the app installs `session.setPermissionRequestHandler`, it must allow `media` for its own page.
- The edge binds the registered origin into the voice grant, and the Voice gateway and the SFU accept that grant only from the same origin (protocol 04): a voice token issued to the game page works only from that page.

## 6. The plain browser build

The browser build uses the same packages from its https origin, registered like the Electron one (protocol 01, "From a browser page"), with the popup or redirect sign-in and a callback page on that origin (`BrowserPopupAuthorizationDriver`, `completeAuthorizationPopup`; the `@gridspawn/core` README).
