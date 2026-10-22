//! A small Socket.IO v4 client over WebSocket for the SFU (protocol 04 §2):
//! the default namespace, events with acknowledgements, server events. No
//! `Origin` header (native clients), no automatic reconnection.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use url::Url;

use crate::error::VoiceError;

const TIMEOUT: Duration = Duration::from_secs(10);

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

pub(crate) struct Signaling {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
}

/// A server event: name and payload. The receiver ends when the socket closes.
pub(crate) type Events = mpsc::UnboundedReceiver<(String, Value)>;

/// Encodes one Socket.IO event packet, with an acknowledgement id.
pub(crate) fn encode_event(id: u64, event: &str, payload: &Value) -> String {
    format!("42{id}{}", json!([event, payload]))
}

/// Decodes a text frame from the server.
#[derive(Debug, PartialEq)]
pub(crate) enum Frame {
    Open,
    Ping,
    Connected,
    Disconnected,
    Ack(u64, Value),
    Event(String, Value),
    Other,
}

pub(crate) fn decode(text: &str) -> Frame {
    if text.starts_with('0') {
        Frame::Open
    } else if text == "2" {
        Frame::Ping
    } else if text.starts_with("40") {
        Frame::Connected
    } else if text.starts_with("41") {
        Frame::Disconnected
    } else if let Some(rest) = text.strip_prefix("43") {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        match (
            digits.parse::<u64>(),
            serde_json::from_str::<Value>(&rest[digits.len()..]),
        ) {
            (Ok(id), Ok(body)) => Frame::Ack(id, body.get(0).cloned().unwrap_or(Value::Null)),
            _ => Frame::Other,
        }
    } else if let Some(rest) = text.strip_prefix("42") {
        match serde_json::from_str::<Value>(rest) {
            Ok(Value::Array(items)) => match items.first() {
                Some(Value::String(name)) => {
                    Frame::Event(name.clone(), items.get(1).cloned().unwrap_or(Value::Null))
                }
                _ => Frame::Other,
            },
            _ => Frame::Other,
        }
    } else {
        Frame::Other
    }
}

/// The WebSocket URL for a Socket.IO server at `base`.
pub(crate) fn socket_url(base: &Url) -> Result<Url, VoiceError> {
    let mut url = base.clone();
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|_| VoiceError::InvalidResponse("sfu.url"))?;
    let path = format!("{}/socket.io/", url.path().trim_end_matches('/'));
    url.set_path(&path);
    url.set_query(Some("EIO=4&transport=websocket"));
    Ok(url)
}

impl Signaling {
    pub async fn connect(base: &Url) -> Result<(Self, Events), VoiceError> {
        let url = socket_url(base)?;
        let (socket, _) =
            tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(url.as_str()))
                .await
                .map_err(|_| VoiceError::SignalingTimeout)?
                .map_err(|_| VoiceError::SignalingFailed)?;
        let (mut sink, mut stream) = socket.split();
        let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (event_tx, events) = mpsc::unbounded_channel();
        let pending: Pending = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let (connected_tx, connected_rx) = oneshot::channel();
        tokio::spawn(async move {
            while let Some(text) = out_rx.recv().await {
                if sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let reader_out = out.clone();
        let reader_pending = pending.clone();
        let reader_closed = closed.clone();
        tokio::spawn(async move {
            let mut connected_tx = Some(connected_tx);
            while let Some(Ok(message)) = stream.next().await {
                let text = match message {
                    Message::Text(text) => text,
                    Message::Close(_) => break,
                    _ => continue,
                };
                match decode(text.as_str()) {
                    Frame::Open => {
                        let _ = reader_out.send("40".into());
                    }
                    Frame::Ping => {
                        let _ = reader_out.send("3".into());
                    }
                    Frame::Connected => {
                        if let Some(tx) = connected_tx.take() {
                            let _ = tx.send(());
                        }
                    }
                    Frame::Disconnected => break,
                    Frame::Ack(id, value) => {
                        let waiter = reader_pending.lock().unwrap().remove(&id);
                        if let Some(waiter) = waiter {
                            let _ = waiter.send(value);
                        }
                    }
                    Frame::Event(name, payload) => {
                        let _ = event_tx.send((name, payload));
                    }
                    Frame::Other => {}
                }
            }
            reader_closed.store(true, Ordering::SeqCst);
            // Waiters see their sender dropped; the event receiver ends.
            reader_pending.lock().unwrap().clear();
        });
        tokio::time::timeout(TIMEOUT, connected_rx)
            .await
            .map_err(|_| VoiceError::SignalingTimeout)?
            .map_err(|_| VoiceError::SignalingFailed)?;
        Ok((
            Self {
                out,
                pending,
                next_id: AtomicU64::new(1),
                closed,
            },
            events,
        ))
    }

    /// Emits `event` and waits for its acknowledgement; `{"error": …}` is a refusal.
    pub async fn request(&self, event: &str, payload: Value) -> Result<Value, VoiceError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(VoiceError::SignalingDisconnected);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.out
            .send(encode_event(id, event, &payload))
            .map_err(|_| VoiceError::SignalingDisconnected)?;
        let value = match tokio::time::timeout(TIMEOUT, rx).await {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => return Err(VoiceError::SignalingDisconnected),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                return Err(VoiceError::SignalingTimeout);
            }
        };
        if let Some(reason) = value.get("error") {
            return Err(VoiceError::Sfu {
                operation: event.to_owned(),
                reason: reason.as_str().unwrap_or("unknown").to_owned(),
            });
        }
        Ok(value)
    }

    /// Socket.IO disconnect, then the socket closes.
    pub fn close(&self) {
        let _ = self.out.send("41".into());
    }
}

impl Drop for Signaling {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames() {
        assert_eq!(decode("0{\"sid\":\"x\"}"), Frame::Open);
        assert_eq!(decode("2"), Frame::Ping);
        assert_eq!(decode("40{\"sid\":\"y\"}"), Frame::Connected);
        assert_eq!(decode("41"), Frame::Disconnected);
        assert_eq!(
            decode("4317[{\"id\":\"t\"}]"),
            Frame::Ack(17, json!({"id": "t"}))
        );
        assert_eq!(
            decode("42[\"newProducer\",{\"producerId\":\"p\"}]"),
            Frame::Event("newProducer".into(), json!({"producerId": "p"}))
        );
        assert_eq!(decode("42[1]"), Frame::Other);
        assert_eq!(
            encode_event(5, "joinRoom", &json!({"roomId": "r"})),
            "425[\"joinRoom\",{\"roomId\":\"r\"}]"
        );
    }

    #[test]
    fn urls() {
        let url = socket_url(&Url::parse("https://sfu.example/base/").unwrap()).unwrap();
        assert_eq!(
            url.as_str(),
            "wss://sfu.example/base/socket.io/?EIO=4&transport=websocket"
        );
        let url = socket_url(&Url::parse("http://127.0.0.1:3000").unwrap()).unwrap();
        assert_eq!(
            url.as_str(),
            "ws://127.0.0.1:3000/socket.io/?EIO=4&transport=websocket"
        );
    }
}
