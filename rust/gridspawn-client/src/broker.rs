//! Sign in through the Gridspawn desktop app (protocol 01 §6): the app's
//! local RPC pipe, a Gridspawn handshake, one `AUTHORIZE`, and the code the
//! player approved in the app. Anything that is not a clear answer from the
//! app means "use the browser".

use std::{ffi::OsString, io, time::Duration};

use serde_json::{json, Value};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::Instant,
};
use zeroize::Zeroizing;

use crate::{error::SignInError, pkce::random_base64url};

/// The `redirect_uri` brokered codes are bound to (01 §6.4).
pub(crate) const APP_BROKER_URI: &str = "urn:gridspawn:app-broker";
/// Looking for the app takes at most this long overall …
const PROBE_BUDGET: Duration = Duration::from_millis(1500);
/// … and this long per pipe that exists.
const PROBE_PER_PIPE: Duration = Duration::from_millis(500);
/// How long the player has to answer in the app. The app dismisses its
/// sheet after 5 minutes without telling anyone; the SDK stops waiting well
/// before that and closes the connection, which dismisses the sheet at once.
pub(crate) const APP_CONSENT_WAIT: Duration = Duration::from_secs(120);
const MAX_FRAME: usize = 64 * 1024;

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
// 2 is a close; like any opcode other than a frame or a ping, it ends the exchange.
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;

pub(crate) trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

/// What the sign-in asks the app for.
pub(crate) struct AppRequest<'a> {
    pub app_id: &'a str,
    pub environment: &'a str,
    pub scopes: &'a [String],
    pub challenge: &'a str,
    pub state: &'a str,
    pub wait: Duration,
}

/// The app's answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AppOutcome {
    /// The player approved: exchange the code with [`APP_BROKER_URI`].
    Approved(Zeroizing<String>),
    /// A final refusal (`access_denied`, `invalid_request`, …): no browser.
    Refused(String),
    /// No app, an app that cannot help now, or no clear answer: use the
    /// browser, without telling the player.
    Unavailable,
}

/// The app's pipes, in the order they are tried.
pub(crate) fn default_candidates() -> Vec<OsString> {
    #[cfg(windows)]
    {
        (0..10)
            .map(|n| OsString::from(format!(r"\\.\pipe\discord-ipc-{n}")))
            .collect()
    }
    #[cfg(unix)]
    {
        let dir = ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"]
            .iter()
            .filter_map(std::env::var_os)
            .filter(|dir| !dir.is_empty())
            .map(std::path::PathBuf::from)
            .chain([std::path::PathBuf::from("/tmp")])
            .find(|dir| dir.is_dir());
        dir.map(|dir| {
            (0..10)
                .map(|n| dir.join(format!("discord-ipc-{n}")).into_os_string())
                .collect()
        })
        .unwrap_or_default()
    }
    #[cfg(not(any(windows, unix)))]
    {
        Vec::new()
    }
}

async fn connect(candidate: &OsString) -> io::Result<Box<dyn Duplex>> {
    #[cfg(windows)]
    {
        let pipe = tokio::net::windows::named_pipe::ClientOptions::new().open(candidate)?;
        Ok(Box::new(pipe))
    }
    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(candidate).await?;
        Ok(Box::new(stream))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = candidate;
        Err(io::Error::new(io::ErrorKind::Unsupported, "no local pipes"))
    }
}

/// One frame: opcode and length (u32 little-endian), then a JSON object.
async fn write_frame(stream: &mut dyn Duplex, opcode: u32, body: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(body).map_err(io::Error::other)?;
    let mut frame = Vec::with_capacity(8 + bytes.len());
    frame.extend_from_slice(&opcode.to_le_bytes());
    frame.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    frame.extend_from_slice(&bytes);
    stream.write_all(&frame).await?;
    stream.flush().await
}

async fn read_frame(stream: &mut dyn Duplex) -> io::Result<(u32, Value)> {
    let mut header = [0u8; 8];
    stream.read_exact(&mut header).await?;
    let opcode = u32::from_le_bytes(header[..4].try_into().expect("4 bytes"));
    let length = u32::from_le_bytes(header[4..].try_into().expect("4 bytes")) as usize;
    if length > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too long"));
    }
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).await?;
    let value: Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not JSON"))?
    };
    if !value.is_object() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not an object"));
    }
    Ok((opcode, value))
}

/// The next frame that is not a ping (pings are answered).
async fn next_frame(stream: &mut dyn Duplex) -> io::Result<(u32, Value)> {
    loop {
        let (opcode, value) = read_frame(stream).await?;
        match opcode {
            OP_PING => write_frame(stream, OP_PONG, &value).await?,
            OP_PONG => {}
            _ => return Ok((opcode, value)),
        }
    }
}

/// The Gridspawn handshake: only a READY carrying `gridspawn.protocol == 1`
/// is the app. Anything else (Discord's READY, a close, an error) is not.
async fn handshake(stream: &mut dyn Duplex, app_id: &str) -> io::Result<bool> {
    write_frame(
        stream,
        OP_HANDSHAKE,
        &json!({ "v": 1, "client_id": app_id, "gridspawn": { "protocol": 1 } }),
    )
    .await?;
    loop {
        let (opcode, frame) = next_frame(stream).await?;
        if opcode != OP_FRAME {
            return Ok(false);
        }
        if frame["cmd"] == "DISPATCH" && frame["evt"] == "READY" {
            return Ok(frame["data"]["gridspawn"]["protocol"] == 1);
        }
        if frame["evt"] == "ERROR" {
            return Ok(false);
        }
    }
}

/// Finds the app within the probe budget: the first pipe that answers the
/// Gridspawn handshake. A missing pipe fails at once.
pub(crate) async fn find_app(candidates: &[OsString], app_id: &str) -> Option<Box<dyn Duplex>> {
    let deadline = Instant::now() + PROBE_BUDGET;
    for candidate in candidates {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let attempt = async {
            let mut stream = connect(candidate).await.ok()?;
            match handshake(stream.as_mut(), app_id).await {
                Ok(true) => Some(stream),
                _ => None,
            }
        };
        if let Ok(Some(stream)) = tokio::time::timeout(left.min(PROBE_PER_PIPE), attempt).await {
            return Some(stream);
        }
    }
    None
}

/// Asks the app once. Waits `request.wait` for the player's answer; when that
/// ends the connection is closed (dropped), which dismisses the app's sheet.
pub(crate) async fn authorize(
    stream: Box<dyn Duplex>,
    request: &AppRequest<'_>,
) -> Result<AppOutcome, SignInError> {
    let mut stream = stream;
    // Without randomness the browser flow cannot run either; it reports it.
    let Ok(nonce) = random_base64url(16) else {
        return Ok(AppOutcome::Unavailable);
    };
    let ask = json!({
        "cmd": "AUTHORIZE",
        "nonce": nonce.as_str(),
        "args": {
            "client_id": request.app_id,
            "environment": request.environment,
            "scopes": request.scopes,
            "code_challenge": request.challenge,
            "code_challenge_method": "S256",
            "state": request.state,
        },
    });
    if write_frame(stream.as_mut(), OP_FRAME, &ask).await.is_err() {
        return Ok(AppOutcome::Unavailable);
    }
    let answer = async {
        loop {
            let (opcode, frame) = next_frame(stream.as_mut()).await?;
            if opcode != OP_FRAME {
                // A close, or anything else: the app is gone.
                return Ok::<Option<Value>, io::Error>(None);
            }
            if frame["cmd"] == "DISPATCH" {
                continue;
            }
            if frame["cmd"] == "AUTHORIZE" && frame["nonce"] == nonce.as_str() {
                return Ok(Some(frame));
            }
            // An answer to something we did not ask.
            return Ok(None);
        }
    };
    let frame = match tokio::time::timeout(request.wait, answer).await {
        Err(_) => {
            drop(stream);
            return Err(SignInError::TimedOut);
        }
        Ok(Err(_)) | Ok(Ok(None)) => return Ok(AppOutcome::Unavailable),
        Ok(Ok(Some(frame))) => frame,
    };
    drop(stream);
    read_answer(&frame, request.state)
}

/// 01 §6.3–§6.4: `{code, state}`, or `evt: "ERROR"` with `data.error`.
fn read_answer(frame: &Value, state: &str) -> Result<AppOutcome, SignInError> {
    let data = &frame["data"];
    if frame["evt"] == "ERROR" {
        if data["code"] == 4009 {
            return Ok(AppOutcome::Unavailable);
        }
        return Ok(match data["error"].as_str() {
            Some("login_required" | "temporarily_unavailable") | None => AppOutcome::Unavailable,
            Some(error) => AppOutcome::Refused(error.to_owned()),
        });
    }
    match (data["code"].as_str(), data["state"].as_str()) {
        (Some(code), Some(answered)) if !code.is_empty() => {
            if answered != state {
                return Err(SignInError::StateMismatch);
            }
            Ok(AppOutcome::Approved(Zeroizing::new(code.to_owned())))
        }
        _ => Ok(AppOutcome::Unavailable),
    }
}

/// Finds the app and asks it; `Unavailable` when there is none.
pub(crate) async fn sign_in_via_app(
    candidates: &[OsString],
    request: &AppRequest<'_>,
) -> Result<AppOutcome, SignInError> {
    match find_app(candidates, request.app_id).await {
        Some(stream) => authorize(stream, request).await,
        None => Ok(AppOutcome::Unavailable),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! A stand-in app: a Unix socket (Linux, macOS) or a uniquely named pipe
    //! (Windows), never the real `discord-ipc-N` names.

    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use super::*;

    /// How the stand-in behaves.
    #[derive(Clone)]
    pub(crate) enum Script {
        /// Gridspawn READY, then this answer to AUTHORIZE (`None`: never answers).
        App(Option<fn(&Value) -> Value>),
        /// A Discord-shaped READY without the marker.
        Discord,
        /// Accepts, then says nothing.
        Silent,
        /// The pre-P2 app: Gridspawn READY is not sent; READY like Discord,
        /// AUTHORIZE answered with 4009.
        OldApp,
    }

    pub(crate) struct StandIn {
        pub candidate: OsString,
        pub closed: Arc<AtomicUsize>,
        pub authorize: Arc<std::sync::Mutex<Option<Value>>>,
        _dir: Option<TempDir>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for StandIn {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    pub(crate) struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn unique() -> String {
        format!(
            "gs-test-{}-{}",
            std::process::id(),
            random_base64url(6).unwrap().replace(['-', '_'], "x")
        )
    }

    async fn serve(
        mut stream: Box<dyn Duplex>,
        script: Script,
        stand_in: (Arc<AtomicUsize>, Arc<std::sync::Mutex<Option<Value>>>),
    ) {
        let (closed, authorize) = stand_in;
        let Ok((op, _hello)) = read_frame(stream.as_mut()).await else {
            return;
        };
        assert_eq!(op, OP_HANDSHAKE);
        let ready = |gridspawn: bool| {
            let mut data = json!({ "v": 1 });
            if gridspawn {
                data["gridspawn"] = json!({ "protocol": 1 });
            } else {
                data["config"] = json!({ "cdn_host": "cdn.discordapp.com" });
                data["user"] = json!({ "id": "0", "username": "Someone" });
            }
            json!({ "cmd": "DISPATCH", "evt": "READY", "nonce": null, "data": data })
        };
        let answer = match &script {
            Script::Silent => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                return;
            }
            Script::Discord => {
                let _ = write_frame(stream.as_mut(), OP_FRAME, &ready(false)).await;
                None
            }
            Script::OldApp => {
                let _ = write_frame(stream.as_mut(), OP_FRAME, &ready(false)).await;
                Some(
                    (|ask: &Value| {
                        json!({
                            "cmd": "AUTHORIZE", "evt": "ERROR", "nonce": ask["nonce"],
                            "data": { "code": 4009, "message": "unsupported" }
                        })
                    }) as fn(&Value) -> Value,
                )
            }
            Script::App(answer) => {
                let _ = write_frame(stream.as_mut(), OP_FRAME, &ready(true)).await;
                *answer
            }
        };
        // Wait for AUTHORIZE (or the client going away).
        match read_frame(stream.as_mut()).await {
            Ok((OP_FRAME, ask)) => {
                *authorize.lock().unwrap() = Some(ask.clone());
                if let Some(answer) = answer {
                    let _ = write_frame(stream.as_mut(), OP_FRAME, &answer(&ask)).await;
                }
                // Until the client closes.
                let mut rest = [0u8; 1];
                if matches!(stream.read(&mut rest).await, Ok(0) | Err(_)) {
                    closed.fetch_add(1, Ordering::SeqCst);
                }
            }
            _ => {
                closed.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// Starts a stand-in that serves every connection with `script`.
    pub(crate) async fn stand_in(script: Script) -> StandIn {
        let closed = Arc::new(AtomicUsize::new(0));
        let authorize = Arc::new(std::sync::Mutex::new(None));
        let shared = (closed.clone(), authorize.clone());
        #[cfg(unix)]
        {
            let dir = std::env::temp_dir().join(unique());
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("discord-ipc-0");
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let task = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(serve(Box::new(stream), script.clone(), shared.clone()));
                }
            });
            StandIn {
                candidate: path.into_os_string(),
                closed,
                authorize,
                _dir: Some(TempDir(dir)),
                task,
            }
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            let name = format!(r"\\.\pipe\{}", unique());
            let mut server = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&name)
                .unwrap();
            let pipe_name = name.clone();
            let task = tokio::spawn(async move {
                loop {
                    if server.connect().await.is_err() {
                        return;
                    }
                    let connected = server;
                    server = ServerOptions::new().create(&pipe_name).unwrap();
                    tokio::spawn(serve(Box::new(connected), script.clone(), shared.clone()));
                }
            });
            StandIn {
                candidate: OsString::from(name),
                closed,
                authorize,
                _dir: None,
                task,
            }
        }
    }

    /// A candidate nobody listens on.
    pub(crate) fn missing() -> OsString {
        #[cfg(unix)]
        {
            std::env::temp_dir()
                .join(unique())
                .join("discord-ipc-0")
                .into_os_string()
        }
        #[cfg(windows)]
        {
            OsString::from(format!(r"\\.\pipe\{}", unique()))
        }
    }

    pub(crate) fn approve(ask: &Value) -> Value {
        json!({ "cmd": "AUTHORIZE", "evt": null, "nonce": ask["nonce"],
                "data": { "code": "brokered-code", "state": ask["args"]["state"] } })
    }

    fn refuse(error: &'static str) -> fn(&Value) -> Value {
        match error {
            "access_denied" => |ask| {
                json!({ "cmd": "AUTHORIZE", "evt": "ERROR", "nonce": ask["nonce"],
                "data": { "code": 4000, "message": "m", "error": "access_denied" } })
            },
            "login_required" => |ask| {
                json!({ "cmd": "AUTHORIZE", "evt": "ERROR", "nonce": ask["nonce"],
                "data": { "code": 4000, "message": "m", "error": "login_required" } })
            },
            "temporarily_unavailable" => |ask| {
                json!({ "cmd": "AUTHORIZE", "evt": "ERROR", "nonce": ask["nonce"],
                "data": { "code": 4000, "message": "m", "error": "temporarily_unavailable" } })
            },
            _ => |ask| {
                json!({ "cmd": "AUTHORIZE", "evt": "ERROR", "nonce": ask["nonce"],
                "data": { "code": 4000, "message": "m", "error": "invalid_request" } })
            },
        }
    }

    fn request(wait: Duration) -> (Vec<String>, AppRequest<'static>) {
        let scopes = vec!["identity.basic".to_owned()];
        let leaked: &'static [String] = Box::leak(scopes.clone().into_boxed_slice());
        (
            scopes,
            AppRequest {
                app_id: "game-test",
                environment: "development",
                scopes: leaked,
                challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
                state: "state-123",
                wait,
            },
        )
    }

    #[tokio::test]
    async fn the_app_approves_and_the_request_has_the_agreed_shape() {
        let app = stand_in(Script::App(Some(approve))).await;
        let (_, req) = request(Duration::from_secs(5));
        let outcome = sign_in_via_app(&[missing(), app.candidate.clone()], &req)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            AppOutcome::Approved(Zeroizing::new("brokered-code".into()))
        );
        let ask = app.authorize.lock().unwrap().clone().unwrap();
        assert_eq!(ask["cmd"], "AUTHORIZE");
        assert_eq!(
            ask["args"],
            json!({
                "client_id": "game-test", "environment": "development",
                "scopes": ["identity.basic"],
                "code_challenge": "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
                "code_challenge_method": "S256", "state": "state-123"
            })
        );
        assert!(ask.get("nonce").and_then(Value::as_str).is_some());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            app.closed.load(Ordering::SeqCst),
            1,
            "closed after the answer"
        );
    }

    #[tokio::test]
    async fn refusals_and_fallbacks() {
        for (error, expected) in [
            ("access_denied", AppOutcome::Refused("access_denied".into())),
            (
                "invalid_request",
                AppOutcome::Refused("invalid_request".into()),
            ),
            ("login_required", AppOutcome::Unavailable),
            ("temporarily_unavailable", AppOutcome::Unavailable),
        ] {
            let app = stand_in(Script::App(Some(refuse(error)))).await;
            let (_, req) = request(Duration::from_secs(5));
            let outcome = sign_in_via_app(std::slice::from_ref(&app.candidate), &req)
                .await
                .unwrap();
            assert_eq!(outcome, expected, "{error}");
        }
        // The app as shipped before P2: Discord-shaped READY, so not used at all.
        let old = stand_in(Script::OldApp).await;
        let (_, req) = request(Duration::from_secs(5));
        assert_eq!(
            sign_in_via_app(std::slice::from_ref(&old.candidate), &req)
                .await
                .unwrap(),
            AppOutcome::Unavailable
        );
        assert!(
            old.authorize.lock().unwrap().is_none(),
            "no AUTHORIZE to a non-Gridspawn READY"
        );
        // A 4009 answer from a Gridspawn READY (defensive).
        let answered_4009 = stand_in(Script::App(Some(|ask| {
            json!({
                "cmd": "AUTHORIZE", "evt": "ERROR", "nonce": ask["nonce"],
                "data": { "code": 4009, "message": "unsupported" }
            })
        })))
        .await;
        assert_eq!(
            sign_in_via_app(std::slice::from_ref(&answered_4009.candidate), &req)
                .await
                .unwrap(),
            AppOutcome::Unavailable
        );
        // A mismatched state ends the sign-in.
        let wrong_state = stand_in(Script::App(Some(|ask| {
            json!({
                "cmd": "AUTHORIZE", "evt": null, "nonce": ask["nonce"],
                "data": { "code": "c", "state": "another" }
            })
        })))
        .await;
        assert_eq!(
            sign_in_via_app(std::slice::from_ref(&wrong_state.candidate), &req).await,
            Err(SignInError::StateMismatch)
        );
        // An answer to another request falls back.
        let other_nonce = stand_in(Script::App(Some(|_| {
            json!({
                "cmd": "AUTHORIZE", "evt": null, "nonce": "not-ours",
                "data": { "code": "c", "state": "state-123" }
            })
        })))
        .await;
        assert_eq!(
            sign_in_via_app(std::slice::from_ref(&other_nonce.candidate), &req)
                .await
                .unwrap(),
            AppOutcome::Unavailable
        );
    }

    #[tokio::test]
    async fn discord_and_silent_pipes_are_skipped_within_the_budget() {
        let discord = stand_in(Script::Discord).await;
        let silent = stand_in(Script::Silent).await;
        let app = stand_in(Script::App(Some(approve))).await;
        let (_, req) = request(Duration::from_secs(5));
        let started = Instant::now();
        let outcome = sign_in_via_app(
            &[
                discord.candidate.clone(),
                silent.candidate.clone(),
                app.candidate.clone(),
            ],
            &req,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, AppOutcome::Approved(_)));
        assert!(discord.authorize.lock().unwrap().is_none());
        // A silent pipe costs at most 500 ms; the whole search at most 1.5 s.
        let silents = [
            silent.candidate.clone(),
            silent.candidate.clone(),
            silent.candidate.clone(),
            silent.candidate.clone(),
        ];
        let started_silent = Instant::now();
        assert_eq!(
            sign_in_via_app(&silents, &req).await.unwrap(),
            AppOutcome::Unavailable
        );
        let took = started_silent.elapsed();
        assert!(took <= Duration::from_millis(1700), "{took:?}");
        assert!(took >= Duration::from_millis(1400), "{took:?}");
        let _ = started;
        // Nothing listening: immediate.
        let started = Instant::now();
        assert_eq!(
            sign_in_via_app(&[missing(), missing()], &req)
                .await
                .unwrap(),
            AppOutcome::Unavailable
        );
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[tokio::test]
    async fn the_consent_wait_ends_with_timed_out_and_closes_the_connection() {
        let app = stand_in(Script::App(None)).await;
        let (_, req) = request(Duration::from_millis(300));
        let outcome = sign_in_via_app(std::slice::from_ref(&app.candidate), &req).await;
        assert_eq!(outcome, Err(SignInError::TimedOut));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            app.closed.load(Ordering::SeqCst),
            1,
            "the app sees the close"
        );
    }

    #[tokio::test]
    async fn the_unix_candidates_live_in_one_directory() {
        let candidates = default_candidates();
        #[cfg(any(unix, windows))]
        assert_eq!(candidates.len(), 10);
        #[cfg(windows)]
        assert_eq!(candidates[0], OsString::from(r"\\.\pipe\discord-ipc-0"));
        #[cfg(unix)]
        assert!(candidates[9].to_string_lossy().ends_with("/discord-ipc-9"));
    }
}
