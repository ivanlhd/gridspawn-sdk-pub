//! The game-data functions of the C ABI, called from Rust against a stand-in
//! edge: bodies, acks, the listing JSON, events, typed errors, NULL handling
//! and the publisher.

use std::{
    ffi::{c_char, CStr, CString},
    ptr,
    time::Duration,
};

use gridspawn_server_capi::*;
use serde_json::{json, Value};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

const CRED: &str = "gsc1.gsc_test.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const DATA: &str = "/v1/server/game-data";
const EVENTS: &str = "/v1/server/game-events";

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

fn quota() -> Value {
    json!({"keys": 2, "maxKeys": 256, "bytes": 40, "maxBytes": 262144})
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
                "environment": "development", "protocol": {"selected": 1}, "modules": ["game_data"]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(DATA))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "keys": [
                    {"key": "match.score", "seq": 9, "bytes": 21, "ttlRemaining": 30,
                     "updatedAt": "2026-10-23T07:32:00Z"},
                    {"key": "lobby.open", "seq": 12, "bytes": 0, "ttlRemaining": null,
                     "updatedAt": "2026-10-23T07:31:00Z", "removed": true}
                ],
                "quota": quota()
            })))
            .mount(&server)
            .await;
        // "owned" stays ahead of every resend; everything else is accepted.
        Mock::given(method("PUT"))
            .and(path(DATA))
            .respond_with(|request: &Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                let mut accepted = Vec::new();
                let mut skipped = Vec::new();
                for key in body["set"].as_object().into_iter().flat_map(|s| s.keys()) {
                    if key == "owned" {
                        skipped.push(json!({"key": "owned", "seq": 1_000_000}));
                    } else if key == "private" {
                        return ResponseTemplate::new(400).set_body_json(json!({
                            "ok": false,
                            "error": {"code": "data/invalid_value", "message": "m",
                                      "field": "set.private.value", "reason": "personal_data"},
                            "requestId": "req"
                        }));
                    } else {
                        accepted.push(json!(key));
                    }
                }
                for entry in body["remove"].as_array().into_iter().flatten() {
                    accepted.push(entry["key"].clone());
                }
                ResponseTemplate::new(200).set_body_json(json!({
                    "ok": true, "accepted": accepted, "skipped": skipped, "quota": quota()
                }))
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(EVENTS))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "ok": true, "id": "118", "duplicate": true
            })))
            .mount(&server)
            .await;
        server
    });
    MockHost { rt, server }
}

fn requests(mock: &MockHost, verb: &str, p: &str) -> Vec<Request> {
    mock.rt
        .block_on(mock.server.received_requests())
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == verb && r.url.path() == p)
        .inspect(|r| assert!(!r.headers.contains_key("origin")))
        .collect()
}

fn bodies(mock: &MockHost, verb: &str, p: &str) -> Vec<Value> {
    requests(mock, verb, p)
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

unsafe fn connect(mock: &MockHost) -> *mut GsClient {
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
        client
    }
}

#[test]
fn documents_acks_and_the_listing() {
    let mock = mock();
    unsafe {
        let client = connect(&mock);
        let (key, value) = (c("match.score"), c(r#"{"red": 3, "blue": 1}"#));
        let mut ack = ptr::null_mut();
        assert_eq!(
            gs_game_data_set(
                client,
                key.as_ptr(),
                value.as_ptr(),
                0,
                &mut ack,
                ptr::null_mut()
            ),
            GsStatus::Ok
        );
        assert_eq!(gs_data_ack_accepted_len(ack), 1);
        assert_eq!(
            read(gs_data_ack_accepted_key(ack, 0)).as_deref(),
            Some("match.score")
        );
        assert!(gs_data_ack_accepted_key(ack, 1).is_null());
        assert_eq!(gs_data_ack_skipped_len(ack), 0);
        let mut q = GsDataQuota::default();
        gs_data_ack_quota(ack, &mut q);
        assert_eq!((q.keys, q.max_keys, q.max_bytes), (2, 256, 262_144));
        gs_data_ack_free(ack);

        // With a TTL, and a NULL ack_out.
        let (banner, text) = (c("lobby.banner"), c(r#""Overtime""#));
        assert_eq!(
            gs_game_data_set(
                client,
                banner.as_ptr(),
                text.as_ptr(),
                60,
                ptr::null_mut(),
                ptr::null_mut()
            ),
            GsStatus::Ok
        );
        assert_eq!(
            gs_game_data_remove(client, banner.as_ptr(), ptr::null_mut(), ptr::null_mut()),
            GsStatus::Ok
        );

        // A batch where one key is owned elsewhere.
        let (round, four, owned, one) = (c("match.round"), c("4"), c("owned"), c("1"));
        let entries = [
            GsDataEntry {
                key: round.as_ptr(),
                value_json: four.as_ptr(),
                ttl_seconds: 0,
            },
            GsDataEntry {
                key: owned.as_ptr(),
                value_json: one.as_ptr(),
                ttl_seconds: 0,
            },
        ];
        let removes = [key.as_ptr()];
        let mut ack = ptr::null_mut();
        assert_eq!(
            gs_game_data_set_many(
                client,
                entries.as_ptr(),
                2,
                removes.as_ptr(),
                1,
                &mut ack,
                ptr::null_mut()
            ),
            GsStatus::Ok
        );
        assert_eq!(gs_data_ack_accepted_len(ack), 2);
        assert_eq!(gs_data_ack_skipped_len(ack), 1);
        assert_eq!(
            read(gs_data_ack_skipped_key(ack, 0)).as_deref(),
            Some("owned")
        );
        assert_eq!(gs_data_ack_skipped_seq(ack, 0), 1_000_000);
        gs_data_ack_free(ack);

        let sent = bodies(&mock, "PUT", DATA);
        assert_eq!(
            sent[0],
            json!({"set": {"match.score": {"value": {"red": 3, "blue": 1}, "seq": 13}}}),
            "seeded above the tombstone's 12"
        );
        assert_eq!(sent[1]["set"]["lobby.banner"]["ttlSeconds"], 60);
        assert_eq!(
            sent[2],
            json!({"remove": [{"key": "lobby.banner", "seq": 15}]})
        );
        assert_eq!(sent[3]["remove"][0]["key"], "match.score");
        assert_eq!(sent.len(), 5, "the batch and its one resend");
        assert_eq!(requests(&mock, "GET", DATA).len(), 1, "one listing");

        let mut json_out = ptr::null_mut();
        assert_eq!(
            gs_game_data_list(client, &mut json_out, ptr::null_mut()),
            GsStatus::Ok
        );
        let listing: Value = serde_json::from_str(&read(json_out).unwrap()).unwrap();
        gs_string_free(json_out);
        assert_eq!(listing["keys"][0]["ttlRemaining"], 30.0);
        assert_eq!(listing["keys"][1]["removed"], true);
        assert_eq!(listing["keys"][1]["ttlRemaining"], Value::Null);
        assert_eq!(listing["quota"]["maxKeys"], 256);

        gs_client_free(client);
    }
}

#[test]
fn events_errors_and_null_handling() {
    let mock = mock();
    unsafe {
        let client = connect(&mock);
        let (name, payload) = (c("match.ended"), c(r#"{"winner": "Red team"}"#));
        let mut event_id = ptr::null_mut();
        let mut duplicate = false;
        assert_eq!(
            gs_game_event_emit(
                client,
                name.as_ptr(),
                payload.as_ptr(),
                ptr::null(),
                &mut event_id,
                &mut duplicate,
                ptr::null_mut()
            ),
            GsStatus::Ok
        );
        assert_eq!(read(event_id).as_deref(), Some("118"));
        assert!(duplicate);
        gs_string_free(event_id);
        let own = c("round-4-of-match-81");
        assert_eq!(
            gs_game_event_emit(
                client,
                name.as_ptr(),
                payload.as_ptr(),
                own.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut()
            ),
            GsStatus::Ok
        );
        let sent = requests(&mock, "POST", EVENTS);
        let generated = sent[0]
            .headers
            .get("idempotency-key")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(generated.len(), 22);
        assert_eq!(
            sent[1].headers.get("idempotency-key").unwrap(),
            "round-4-of-match-81"
        );

        // A personal-data refusal, typed, with field and reason.
        let (private, mail) = (c("private"), c(r#""someone@example.com""#));
        let mut error = ptr::null_mut();
        assert_eq!(
            gs_game_data_set(
                client,
                private.as_ptr(),
                mail.as_ptr(),
                0,
                ptr::null_mut(),
                &mut error
            ),
            GsStatus::Api
        );
        assert_eq!(gs_error_code(error), GsErrorCode::GameDataInvalidValue);
        assert_eq!(
            read(gs_error_reason(error)).as_deref(),
            Some("personal_data")
        );
        assert_eq!(
            read(gs_error_field(error)).as_deref(),
            Some("set.private.value")
        );
        gs_error_free(error);

        // Refused before sending: bad JSON, a bad TTL, a bad event id, NULLs.
        let (bad_json, key) = (c("{not json"), c("k"));
        let mut error = ptr::null_mut();
        assert_eq!(
            gs_game_data_set(
                client,
                key.as_ptr(),
                bad_json.as_ptr(),
                0,
                ptr::null_mut(),
                &mut error
            ),
            GsStatus::InvalidArgument
        );
        gs_error_free(error);
        let one = c("1");
        assert_eq!(
            gs_game_data_set(
                client,
                key.as_ptr(),
                one.as_ptr(),
                5,
                ptr::null_mut(),
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument
        );
        let bad_id = c("has.dots.x");
        assert_eq!(
            gs_game_event_emit(
                client,
                name.as_ptr(),
                payload.as_ptr(),
                bad_id.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument
        );
        assert_eq!(
            gs_game_data_set(
                ptr::null_mut(),
                key.as_ptr(),
                one.as_ptr(),
                0,
                ptr::null_mut(),
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument
        );
        assert_eq!(
            gs_game_data_set_many(
                client,
                ptr::null(),
                1,
                ptr::null(),
                0,
                ptr::null_mut(),
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument
        );
        assert_eq!(
            gs_game_data_set_many(
                client,
                ptr::null(),
                0,
                ptr::null(),
                0,
                ptr::null_mut(),
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument,
            "an empty batch"
        );
        assert_eq!(gs_data_ack_accepted_len(ptr::null()), 0);
        gs_data_ack_free(ptr::null_mut());
        let mut q = GsDataQuota {
            keys: 9,
            ..GsDataQuota::default()
        };
        gs_data_ack_quota(ptr::null(), &mut q);
        assert_eq!(q, GsDataQuota::default());

        gs_client_free(client);
    }
}

#[test]
fn the_publisher() {
    let mock = mock();
    unsafe {
        let client = connect(&mock);
        let mut publisher = ptr::null_mut();
        assert_eq!(
            gs_game_data_publisher_spawn(client, 5, &mut publisher, ptr::null_mut()),
            GsStatus::Ok
        );
        let clock = c("match.clock");
        for value in 0..30 {
            let value = c(&value.to_string());
            assert_eq!(
                gs_game_data_publisher_set(
                    publisher,
                    clock.as_ptr(),
                    value.as_ptr(),
                    0,
                    ptr::null_mut()
                ),
                GsStatus::Ok
            );
        }
        let (owned, one) = (c("owned"), c("1"));
        gs_game_data_publisher_set(publisher, owned.as_ptr(), one.as_ptr(), 0, ptr::null_mut());
        let bad = c("Bad Key");
        assert_eq!(
            gs_game_data_publisher_set(publisher, bad.as_ptr(), one.as_ptr(), 0, ptr::null_mut()),
            GsStatus::InvalidArgument
        );
        std::thread::sleep(Duration::from_millis(600));
        let skipped = gs_game_data_publisher_skipped_json(publisher);
        assert_eq!(read(skipped).as_deref(), Some(r#"["owned"]"#));
        gs_string_free(skipped);
        assert!(gs_game_data_publisher_last_error(publisher).is_null());
        gs_game_data_publisher_remove(publisher, clock.as_ptr());
        gs_game_data_publisher_remove(ptr::null_mut(), clock.as_ptr());
        std::thread::sleep(Duration::from_millis(400));
        gs_game_data_publisher_free(publisher);
        gs_game_data_publisher_free(ptr::null_mut());
        assert!(gs_game_data_publisher_skipped_json(ptr::null()).is_null());

        let sent = bodies(&mock, "PUT", DATA);
        assert_eq!(
            sent[0]["set"]["match.clock"]["value"], 29,
            "the latest value"
        );
        assert!(sent.iter().any(|b| b["remove"][0]["key"] == "match.clock"));
        gs_client_free(client);
    }
}
