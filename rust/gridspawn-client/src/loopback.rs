//! The RFC 8252 loopback listener that receives the browser callback (protocol 01 §1).
//!
//! It listens on `127.0.0.1:<port chosen by the OS>`, answers 404 to anything
//! that is not `GET <redirect path>` (favicon, probes) and keeps waiting. The
//! first request to the redirect path decides the outcome. The browser gets a
//! small self-contained page: no scripts and no external resources.

use std::{net::Ipv4Addr, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
    time::{timeout, Instant},
};
use url::Url;
use zeroize::Zeroizing;

use crate::error::SignInError;

const MAX_HEAD: usize = 8 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Connections read at the same time; more are closed unanswered.
const MAX_PENDING: usize = 16;

pub(crate) struct Listener {
    inner: TcpListener,
    port: u16,
}

impl Listener {
    pub async fn bind() -> Result<Self, SignInError> {
        let inner = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| SignInError::ListenerFailed)?;
        let port = inner
            .local_addr()
            .map_err(|_| SignInError::ListenerFailed)?
            .port();
        Ok(Self { inner, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Waits for the callback until `deadline`. On success the browser's
    /// connection is kept open so the page can say how the exchange went.
    pub async fn wait(
        &self,
        redirect_path: &str,
        state: &str,
        deadline: Instant,
    ) -> Result<Callback, SignInError> {
        let mut reading = JoinSet::new();
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return Err(SignInError::TimedOut),
                accepted = self.inner.accept() => {
                    if let Ok((stream, _)) = accepted {
                        if reading.len() < MAX_PENDING {
                            reading.spawn(read_head(stream));
                        }
                    }
                }
                Some(joined) = reading.join_next(), if !reading.is_empty() => {
                    let Ok(Some((stream, head))) = joined else { continue };
                    match classify(&head, redirect_path, state, self.port) {
                        None => not_found(stream).await,
                        Some(Ok(code)) => return Ok(Callback { code, responder: Responder { stream } }),
                        Some(Err(error)) => {
                            let page = if matches!(error, SignInError::Denied(_)) {
                                Page::Declined
                            } else {
                                Page::Failed
                            };
                            Responder { stream }.send(page).await;
                            return Err(error);
                        }
                    }
                }
            }
        }
    }
}

pub(crate) struct Callback {
    pub code: Zeroizing<String>,
    pub responder: Responder,
}

/// The browser's open connection.
pub(crate) struct Responder {
    stream: TcpStream,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    SignedIn,
    Declined,
    Failed,
}

impl Responder {
    pub async fn send(mut self, page: Page) {
        let (title, text) = match page {
            Page::SignedIn => (
                "You're signed in",
                "You can close this tab and return to the game.",
            ),
            Page::Declined => (
                "Sign-in was not approved",
                "You can close this tab and return to the game.",
            ),
            Page::Failed => ("Sign-in didn't finish", "Return to the game and try again."),
        };
        let html = format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
             <title>Gridspawn</title><style>\
             body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
             background:#111318;color:#e8e8ea;font-family:system-ui,-apple-system,Segoe UI,sans-serif}}\
             main{{max-width:28rem;padding:2rem;text-align:center}}\
             h1{{font-size:1.4rem;margin:0 0 .75rem}}p{{margin:0;color:#b4b6bd;line-height:1.5}}\
             </style></head><body><main><h1>{title}</h1><p>{text}</p></main></body></html>"
        );
        let status = if page == Page::Failed {
            "400 Bad Request"
        } else {
            "200 OK"
        };
        let response = format!(
            "HTTP/1.1 {status}\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\n\
             Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\n\
             Cache-Control: no-store\r\n\
             Referrer-Policy: no-referrer\r\n\
             X-Content-Type-Options: nosniff\r\n\
             Connection: close\r\n\r\n{html}",
            html.len()
        );
        let _ = timeout(WRITE_TIMEOUT, async {
            self.stream.write_all(response.as_bytes()).await?;
            self.stream.shutdown().await
        })
        .await;
    }
}

async fn not_found(mut stream: TcpStream) {
    let _ = timeout(WRITE_TIMEOUT, async {
        stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await?;
        stream.shutdown().await
    })
    .await;
}

/// Reads the request head (up to the blank line, at most 8 KiB).
async fn read_head(mut stream: TcpStream) -> Option<(TcpStream, Zeroizing<String>)> {
    let mut buffer = Zeroizing::new(Vec::with_capacity(1024));
    let complete = timeout(READ_TIMEOUT, async {
        let mut chunk = [0u8; 1024];
        loop {
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                return Some(());
            }
            if buffer.len() > MAX_HEAD {
                return None;
            }
        }
    })
    .await
    .ok()
    .flatten();
    complete?;
    let head = Zeroizing::new(String::from_utf8_lossy(&buffer).into_owned());
    Some((stream, head))
}

/// `None`: not the callback (404, keep waiting). `Some`: the outcome.
fn classify(
    head: &str,
    redirect_path: &str,
    state: &str,
    port: u16,
) -> Option<Result<Zeroizing<String>, SignInError>> {
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let (method, target) = (request_line.next()?, request_line.next()?);
    if method != "GET" || !target.starts_with('/') {
        return None;
    }
    let url = Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    if url.path() != redirect_path {
        return None;
    }
    let host = lines.take_while(|line| !line.is_empty()).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("host")
            .then(|| value.trim().to_owned())
    });
    // A page on another host name resolving to 127.0.0.1 (DNS rebinding) is not our callback.
    if host.is_some_and(|host| host != format!("127.0.0.1:{port}")) {
        return Some(Err(SignInError::RedirectMismatch));
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| Zeroizing::new(value.into_owned()))
    };
    if param("state").as_deref().map(String::as_str) != Some(state) {
        return Some(Err(SignInError::StateMismatch));
    }
    if let Some(error) = param("error") {
        let known = !error.is_empty()
            && error.len() <= 64
            && error.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
        let error = if known {
            error.to_string()
        } else {
            "unknown".into()
        };
        return Some(Err(SignInError::Denied(error)));
    }
    match param("code") {
        Some(code) if !code.is_empty() => Some(Ok(code)),
        _ => Some(Err(SignInError::MissingCode)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(head: &str) -> Option<Result<String, SignInError>> {
        classify(head, "/callback", "s1", 4000).map(|result| result.map(|code| code.to_string()))
    }

    #[test]
    fn classification() {
        let ok = "GET /callback?code=c1&state=s1 HTTP/1.1\r\nHost: 127.0.0.1:4000\r\n\r\n";
        assert_eq!(run(ok), Some(Ok("c1".into())));
        assert_eq!(run("GET /favicon.ico HTTP/1.1\r\n\r\n"), None);
        assert_eq!(run("POST /callback?code=c&state=s1 HTTP/1.1\r\n\r\n"), None);
        assert_eq!(
            run("GET /callback?code=c&state=s2 HTTP/1.1\r\n\r\n"),
            Some(Err(SignInError::StateMismatch))
        );
        assert_eq!(
            run("GET /callback?code=c HTTP/1.1\r\n\r\n"),
            Some(Err(SignInError::StateMismatch))
        );
        assert_eq!(
            run("GET /callback?error=access_denied&state=s1 HTTP/1.1\r\n\r\n"),
            Some(Err(SignInError::Denied("access_denied".into())))
        );
        assert_eq!(
            run("GET /callback?error=%3Cb%3E&state=s1 HTTP/1.1\r\n\r\n"),
            Some(Err(SignInError::Denied("unknown".into())))
        );
        assert_eq!(
            run("GET /callback?state=s1 HTTP/1.1\r\n\r\n"),
            Some(Err(SignInError::MissingCode))
        );
        assert_eq!(
            run("GET /callback?code=c&state=s1 HTTP/1.1\r\nhost: evil.example:4000\r\n\r\n"),
            Some(Err(SignInError::RedirectMismatch))
        );
    }
}
