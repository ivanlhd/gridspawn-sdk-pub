//! The pipe protocol shared with `gridspawn-companion` (version 1): pipe
//! names, and the companion's events and replies as seen by the DLL.

use serde_json::Value;

pub const VERSION: u64 = 1;

/// (DLL → companion, companion → DLL) pipe names for a game process.
pub fn pipe_names(game_pid: u32) -> (String, String) {
    (
        format!(r"\\.\pipe\gridspawn-rathena-{game_pid}-in"),
        format!(r"\\.\pipe\gridspawn-rathena-{game_pid}-out"),
    )
}

/// What the callback thread delivers.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Ready,
    CompanionLost(String),
    SignedIn(String),
    SignedOut,
    Assertion(u32, String),
    Attached(String, Vec<(String, String)>),
    Detached(String),
    /// 0 left, 1 joining, 2 joined.
    VoiceState(i32),
    Participant(bool, String, String),
    Speaking(String, bool),
    LocalSpeaking(bool),
    /// `None` = this player.
    ServerMuted(Option<String>, bool),
    /// 0 input, 1 output.
    Devices(i32, Vec<String>, Option<String>),
    Error(String, String),
}

fn s(frame: &Value, key: &str) -> String {
    frame
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn b(frame: &Value, key: &str) -> bool {
    frame.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// An event frame or a failed reply; `None` for successful replies and
/// anything unknown (a newer companion may send more).
pub fn parse(frame: &Value) -> Option<Event> {
    // Events carry "event" (some also a participant "id"); replies carry "ok".
    let Some(event) = frame.get("event") else {
        if frame.get("ok").and_then(Value::as_bool) == Some(false) {
            let error = frame.get("error").cloned().unwrap_or(Value::Null);
            return Some(Event::Error(s(&error, "code"), s(&error, "message")));
        }
        return None;
    };
    let event = event.as_str()?;
    Some(match event {
        "ready" => Event::Ready,
        "signed_in" => Event::SignedIn(s(frame, "displayName")),
        "signed_out" => Event::SignedOut,
        "assertion" => Event::Assertion(
            frame.get("requestId").and_then(Value::as_u64).unwrap_or(0) as u32,
            s(frame, "assertion"),
        ),
        "attached" => Event::Attached(
            s(frame, "displayName"),
            frame
                .get("roster")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .map(|p| (s(p, "id"), s(p, "displayName")))
                        .collect()
                })
                .unwrap_or_default(),
        ),
        "detached" => Event::Detached(s(frame, "reason")),
        "voice_state" => {
            Event::VoiceState(frame.get("state").and_then(Value::as_i64).unwrap_or(0) as i32)
        }
        "participant" => {
            Event::Participant(b(frame, "joined"), s(frame, "id"), s(frame, "displayName"))
        }
        "speaking" => Event::Speaking(s(frame, "id"), b(frame, "active")),
        "local_speaking" => Event::LocalSpeaking(b(frame, "active")),
        "server_muted" => Event::ServerMuted(
            frame.get("id").and_then(Value::as_str).map(str::to_owned),
            b(frame, "muted"),
        ),
        "devices" => Event::Devices(
            if b(frame, "input") { 0 } else { 1 },
            frame
                .get("names")
                .and_then(Value::as_array)
                .map(|l| {
                    l.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            frame
                .get("selected")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ),
        "error" => Event::Error(s(frame, "code"), s(frame, "message")),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn events_and_failed_replies_parse() {
        assert_eq!(
            parse(&json!({"event": "ready", "version": "0.1.0"})),
            Some(Event::Ready)
        );
        assert_eq!(
            parse(&json!({"event": "attached", "displayName": "Aria",
                          "roster": [{"id": "gsp1.a", "displayName": "Bram"}]})),
            Some(Event::Attached(
                "Aria".into(),
                vec![("gsp1.a".into(), "Bram".into())]
            ))
        );
        assert_eq!(
            parse(&json!({"event": "server_muted", "id": null, "muted": true})),
            Some(Event::ServerMuted(None, true))
        );
        assert_eq!(
            parse(&json!({"event": "speaking", "id": "gsp1.a", "active": true})),
            Some(Event::Speaking("gsp1.a".into(), true))
        );
        assert_eq!(
            parse(&json!({"event": "devices", "input": true, "names": ["Mic"], "selected": null})),
            Some(Event::Devices(0, vec!["Mic".into()], None))
        );
        assert_eq!(
            parse(
                &json!({"id": 3, "ok": false, "error": {"code": "voice/not_attached", "message": "m"}})
            ),
            Some(Event::Error("voice/not_attached".into(), "m".into()))
        );
        assert_eq!(parse(&json!({"id": 3, "ok": true, "result": {}})), None);
        assert_eq!(parse(&json!({"event": "from_the_future"})), None);
    }

    #[test]
    fn pipe_names_carry_the_game_pid() {
        let (a, b) = pipe_names(4242);
        assert_eq!(a, r"\\.\pipe\gridspawn-rathena-4242-in");
        assert_eq!(b, r"\\.\pipe\gridspawn-rathena-4242-out");
    }
}
