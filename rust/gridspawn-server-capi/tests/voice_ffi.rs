//! The voice functions of the C ABI, called from Rust against a mock edge:
//! bodies, answers, error accessors, NULL handling and the publisher.

use std::{
    ffi::{c_char, CStr, CString},
    ptr,
    time::Duration,
};

use gridspawn_server_capi::*;
use serde_json::{json, Value};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

const CRED: &str = "gsc1.gsc_test.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const SESSION: &str = "/v1/server/sessions/gssn1.abc";

fn c(text: &str) -> CString {
    CString::new(text).unwrap()
}

/// SAFETY: `pointer` is NULL or a NUL-terminated string.
unsafe fn read(pointer: *const c_char) -> Option<String> {
    (!pointer.is_null()).then(|| {
        unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .unwrap()
            .to_owned()
    })
}

struct MockHost {
    rt: tokio::runtime::Runtime,
    server: MockServer,
}

fn mock() -> MockHost {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/bootstrap"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "application": {"appId": "test-app"},
                "environment": "development", "protocol": {"selected": 1}, "modules": ["voice"]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/server/sessions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "sessionId": "gssn1.abc", "leaseExpiresAt": "2026-10-23T12:00:45Z", "renewAfterSeconds": 15
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!("{SESSION}/voice")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "enforced": false})),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!("{SESSION}/participants/gsp1.a/voice")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "enforced": true})),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!("{SESSION}/participants/gsp1.gone/voice")))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "ok": false,
                "error": {"code": "voice/invalid_request", "message": "m", "field": "channels", "reason": "count"},
                "requestId": "req"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{SESSION}/participants/gsp1.a/voice/mute")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "enforced": true})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{SESSION}/voice/positions")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "enforced": true})),
            )
            .mount(&server)
            .await;
        server
    });
    MockHost { rt, server }
}

fn bodies(mock: &MockHost, p: &str) -> Vec<Value> {
    mock.rt
        .block_on(mock.server.received_requests())
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == p)
        .map(|r| {
            assert!(!r.headers.contains_key("origin"));
            serde_json::from_slice(&r.body).unwrap()
        })
        .collect()
}

unsafe fn connect_and_create(mock: &MockHost) -> (*mut GsClient, *mut GsSession) {
    let app = c("test-app");
    let base = c(&mock.server.uri());
    let cred = c(CRED);
    unsafe {
        let mut credential = ptr::null_mut();
        assert_eq!(
            gs_credential_from_string(cred.as_ptr(), &mut credential, ptr::null_mut()),
            GsStatus::Ok
        );
        let mut options = std::mem::MaybeUninit::<GsClientOptions>::uninit();
        gs_client_options_init(options.as_mut_ptr());
        let mut options = options.assume_init();
        options.app_id = app.as_ptr();
        options.environment = GsEnvironment::Development as u32;
        options.sdk_base_url = base.as_ptr();
        let mut client = ptr::null_mut();
        assert_eq!(
            gs_client_connect(&options, credential, &mut client, ptr::null_mut()),
            GsStatus::Ok
        );
        let (external, key) = (c("match-1"), c("match-1-key"));
        let request = GsCreateSession {
            external_session_id: external.as_ptr(),
            idempotency_key: key.as_ptr(),
            region: ptr::null(),
            max_participants: 0,
            metadata_keys: ptr::null(),
            metadata_values: ptr::null(),
            metadata_len: 0,
        };
        let mut session = ptr::null_mut();
        assert_eq!(
            gs_session_create(client, &request, &mut session, ptr::null_mut()),
            GsStatus::Ok
        );
        (client, session)
    }
}

#[test]
fn voice_calls_send_their_bodies_and_answer_acks() {
    let mock = mock();
    unsafe {
        let (client, session) = connect_and_create(&mock);

        let config = c(
            r#"{"rooms": {"match": {"mode": "proximity", "proximity": {"minDistance": 2, "maxDistance": 20}}}}"#,
        );
        let mut ack = GsVoiceAck {
            enforced: true,
            overtaken: true,
        };
        assert_eq!(
            gs_voice_configure(session, config.as_ptr(), &mut ack, ptr::null_mut()),
            GsStatus::Ok
        );
        assert_eq!(
            ack,
            GsVoiceAck {
                enforced: false,
                overtaken: false
            }
        );
        let sent = &bodies(&mock, &format!("{SESSION}/voice"))[0];
        assert_eq!(sent["rooms"]["match"]["proximity"]["maxDistance"], 20.0);
        assert_eq!(
            sent["rooms"]["match"]["maxAudibleSpeakers"], 16,
            "defaults filled in"
        );

        // State: defaults, then channels and a scene.
        let mut state = std::mem::MaybeUninit::<GsVoicePlayerState>::uninit();
        gs_voice_player_state_init(state.as_mut_ptr());
        let mut state = state.assume_init();
        assert!(state.alive && state.can_speak && !state.spectator && state.rooms.is_null());
        let (red, scene) = (c("team:red"), c("cave"));
        let channels = [red.as_ptr()];
        state.channels = channels.as_ptr();
        state.channels_len = 1;
        state.scene = scene.as_ptr();
        let a = c("gsp1.a");
        assert_eq!(
            gs_voice_set_player_state(session, a.as_ptr(), &state, &mut ack, ptr::null_mut()),
            GsStatus::Ok
        );
        assert!(ack.enforced);
        let sent = &bodies(&mock, &format!("{SESSION}/participants/gsp1.a/voice"))[0];
        assert_eq!(sent["channels"], json!(["team:red"]));
        assert_eq!(sent["scene"], "cave");
        assert!(sent.get("rooms").is_none(), "NULL rooms = every room");
        assert!(sent["seq"].as_u64().unwrap() > 0);

        assert_eq!(
            gs_voice_mute(session, a.as_ptr(), true, ptr::null_mut(), ptr::null_mut()),
            GsStatus::Ok
        );
        assert_eq!(
            bodies(&mock, &format!("{SESSION}/participants/gsp1.a/voice/mute")),
            [json!({"muted": true})]
        );

        let positions = [GsVoicePosition {
            participant_id: a.as_ptr(),
            position: [1.0, 0.0, 2.0],
            yaw: 0.5,
            has_yaw: true,
        }];
        assert_eq!(
            gs_voice_publish_positions(session, positions.as_ptr(), 1, &mut ack, ptr::null_mut()),
            GsStatus::Ok
        );
        assert_eq!(
            bodies(&mock, &format!("{SESSION}/voice/positions"))[0]["positions"],
            json!([{"participantId": "gsp1.a", "p": [1.0, 0.0, 2.0], "yaw": 0.5}])
        );

        gs_session_free(session);
        gs_client_free(client);
    }
}

#[test]
fn voice_errors_carry_field_and_reason_and_bad_input_is_refused_locally() {
    let mock = mock();
    unsafe {
        let (client, session) = connect_and_create(&mock);
        let mut state = std::mem::MaybeUninit::<GsVoicePlayerState>::uninit();
        gs_voice_player_state_init(state.as_mut_ptr());
        let state = state.assume_init();

        let gone = c("gsp1.gone");
        let mut error = ptr::null_mut();
        assert_eq!(
            gs_voice_set_player_state(session, gone.as_ptr(), &state, ptr::null_mut(), &mut error),
            GsStatus::Api
        );
        assert_eq!(gs_error_code(error), GsErrorCode::VoiceInvalidRequest);
        assert_eq!(read(gs_error_field(error)).as_deref(), Some("channels"));
        assert_eq!(read(gs_error_reason(error)).as_deref(), Some("count"));
        gs_error_free(error);

        // Refused before sending: bad JSON, a bad channel key, NULL arguments.
        let bad = c(r#"{"rooms": {"Match": {"mode": "open"}}}"#);
        assert_eq!(
            gs_voice_configure(session, bad.as_ptr(), ptr::null_mut(), ptr::null_mut()),
            GsStatus::InvalidArgument
        );
        assert_eq!(
            gs_voice_configure(session, ptr::null(), ptr::null_mut(), ptr::null_mut()),
            GsStatus::InvalidArgument
        );
        let key = c("Bad Key");
        let keys = [key.as_ptr()];
        let mut bad_state = std::mem::MaybeUninit::<GsVoicePlayerState>::uninit();
        gs_voice_player_state_init(bad_state.as_mut_ptr());
        let mut bad_state = bad_state.assume_init();
        bad_state.channels = keys.as_ptr();
        bad_state.channels_len = 1;
        let a = c("gsp1.a");
        assert_eq!(
            gs_voice_set_player_state(
                session,
                a.as_ptr(),
                &bad_state,
                ptr::null_mut(),
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument
        );
        assert_eq!(
            gs_voice_publish_positions(session, ptr::null(), 0, ptr::null_mut(), ptr::null_mut()),
            GsStatus::InvalidArgument
        );
        assert_eq!(
            gs_voice_mute(
                ptr::null_mut(),
                a.as_ptr(),
                true,
                ptr::null_mut(),
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument
        );
        assert!(
            bodies(&mock, &format!("{SESSION}/participants/gsp1.a/voice")).is_empty(),
            "nothing sent"
        );
        // NULL-safe no-ops.
        gs_voice_player_state_init(ptr::null_mut());
        gs_voice_publisher_free(ptr::null_mut());
        gs_voice_publisher_remove(ptr::null_mut(), a.as_ptr());
        assert!(gs_voice_publisher_last_error(ptr::null()).is_null());
        assert!(gs_error_field(ptr::null()).is_null());
        gs_session_free(session);
        gs_client_free(client);
    }
}

#[test]
fn the_publisher_sends_and_stops_when_freed() {
    let mock = mock();
    let positions = format!("{SESSION}/voice/positions");
    unsafe {
        let (client, session) = connect_and_create(&mock);
        let mut publisher = ptr::null_mut();
        assert_eq!(
            gs_voice_publisher_spawn(session, 10, &mut publisher, ptr::null_mut()),
            GsStatus::Ok
        );
        let (a, b) = (c("gsp1.a"), c("gsp1.b"));
        assert_eq!(
            gs_voice_publisher_update(
                publisher,
                a.as_ptr(),
                1.0,
                0.0,
                2.0,
                0.0,
                false,
                ptr::null_mut()
            ),
            GsStatus::Ok
        );
        assert_eq!(
            gs_voice_publisher_update(
                publisher,
                b.as_ptr(),
                f64::NAN,
                0.0,
                0.0,
                0.0,
                false,
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument
        );
        std::thread::sleep(Duration::from_millis(350));
        assert!(gs_voice_publisher_last_error(publisher).is_null());
        gs_voice_publisher_remove(publisher, a.as_ptr());
        gs_voice_publisher_free(publisher);
        let sent = bodies(&mock, &positions);
        assert!(!sent.is_empty());
        assert_eq!(
            sent[0]["positions"],
            json!([{"participantId": "gsp1.a", "p": [1.0, 0.0, 2.0]}])
        );
        let count = sent.len();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(bodies(&mock, &positions).len(), count, "stopped");
        gs_session_free(session);
        gs_client_free(client);
    }
}
