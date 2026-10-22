//! The C ABI called from Rust, against a mock server: pointers, ownership,
//! error accessors, the async callback and the refusal of blocking calls.

use std::{
    ffi::{c_char, c_void, CStr, CString},
    ptr,
    sync::mpsc::{sync_channel, SyncSender},
    time::Duration,
};

use gridspawn_server_capi::*;
use serde_json::json;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

const CRED: &str = "gsc1.gsc_test.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const POSTS: &str = "/core/v1/game-servers/posts";

fn c(text: &str) -> CString {
    CString::new(text).unwrap()
}

/// SAFETY: `pointer` is NULL or a NUL-terminated string.
unsafe fn read(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(pointer) }
                .to_str()
                .unwrap()
                .to_owned(),
        )
    }
}

/// A mock server on its own runtime, so the test thread stays outside any runtime.
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
            .and(path("/v1/server/grants/club"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "accessToken": "grant-token", "tokenType": "Bearer", "expiresIn": 300, "scope": "club.post"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(POSTS))
            .respond_with(|request: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                if body["feature"] == "smoke" {
                    ResponseTemplate::new(200).set_body_json(json!({"ok": true, "id": "gs-1"}))
                } else {
                    ResponseTemplate::new(404).set_body_json(json!({"error": "feature_not_found"}))
                }
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/server/sessions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "sessionId": "gssn1.abc", "leaseExpiresAt": "2026-10-22T12:00:45Z", "renewAfterSeconds": 15
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/server/sessions/gssn1.abc/renew"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "sessionId": "gssn1.abc", "leaseExpiresAt": "2026-10-22T12:01:00Z", "renewAfterSeconds": 15
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/server/sessions/gssn1.abc/guests"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "participantId": "gsp1.p", "identityKind": "server_verified_guest",
                "subject": "gsg1.s", "displayName": "Wanderer"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/server/sessions/gssn1.abc/participants/gsp1.p/tickets"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "participantTicket": format!("gpt1.{}", "t".repeat(43)),
                "tokenType": "urn:gridspawn:participant-ticket", "expiresIn": 120
            })))
            .mount(&server)
            .await;
        server
    });
    MockHost { rt, server }
}

fn connect(mock: &MockHost) -> *mut GsClient {
    let app = c("test-app");
    let base = c(&mock.server.uri());
    let posts = c(&format!("{}{POSTS}", mock.server.uri()));
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
        assert_eq!(options.environment, GsEnvironment::Production as u32);
        options.app_id = app.as_ptr();
        options.environment = GsEnvironment::Development as u32;
        options.sdk_base_url = base.as_ptr();
        options.club_posts_url = posts.as_ptr();
        let mut client = ptr::null_mut();
        let mut error = ptr::null_mut();
        let status = gs_client_connect(&options, credential, &mut client, &mut error);
        assert_eq!(status, GsStatus::Ok, "{:?}", read(gs_error_message(error)));
        assert!(error.is_null());
        client
    }
}

fn assert_no_origin(mock: &MockHost) {
    for request in mock.rt.block_on(mock.server.received_requests()).unwrap() {
        assert!(
            !request.headers.contains_key("origin"),
            "Origin sent to {}",
            request.url
        );
    }
}

fn post(text: &str, feature: &str) -> (CString, CString, CString) {
    (c(feature), c(text), c("post-key-1"))
}

#[test]
fn version_and_abi() {
    unsafe {
        assert_eq!(read(gs_version()).unwrap(), env!("CARGO_PKG_VERSION"));
    }
    assert_eq!(gs_abi_version(), GS_ABI_VERSION);
    assert_eq!(GS_ABI_VERSION, 1);
}

#[test]
fn null_arguments_and_frees_are_safe() {
    unsafe {
        gs_string_free(ptr::null_mut());
        gs_credential_free(ptr::null_mut());
        gs_client_free(ptr::null_mut());
        gs_session_free(ptr::null_mut());
        gs_participant_free(ptr::null_mut());
        gs_renewer_free(ptr::null_mut());
        gs_error_free(ptr::null_mut());
        gs_client_options_init(ptr::null_mut());
        assert!(gs_session_id(ptr::null()).is_null());
        assert!(gs_error_message(ptr::null()).is_null());
        assert_eq!(gs_error_status(ptr::null()), GsStatus::Ok);

        let mut error = ptr::null_mut();
        let mut id = ptr::null_mut();
        let mut duplicate = false;
        let feature = c("smoke");
        let status = gs_post(
            ptr::null_mut(),
            feature.as_ptr(),
            ptr::null(),
            ptr::null(),
            &mut id,
            &mut duplicate,
            &mut error,
        );
        assert_eq!(status, GsStatus::InvalidArgument);
        assert_eq!(read(gs_error_message(error)).unwrap(), "client is NULL");
        assert_eq!(gs_error_status(error), GsStatus::InvalidArgument);
        gs_error_free(error);

        // A NULL error pointer is allowed.
        let mut credential = ptr::null_mut();
        assert_eq!(
            gs_credential_from_string(ptr::null(), &mut credential, ptr::null_mut()),
            GsStatus::InvalidArgument
        );
        // Non-UTF-8 input.
        let bad = b"\xff\xfe\0";
        assert_eq!(
            gs_credential_from_string(bad.as_ptr().cast(), &mut credential, ptr::null_mut()),
            GsStatus::InvalidArgument
        );
        assert!(credential.is_null());
        // A NULL credential passed to connect.
        let mut options = std::mem::MaybeUninit::<GsClientOptions>::uninit();
        gs_client_options_init(options.as_mut_ptr());
        let mut client = ptr::null_mut();
        assert_eq!(
            gs_client_connect(
                options.as_ptr(),
                ptr::null_mut(),
                &mut client,
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument
        );
    }
}

#[test]
fn credentials_and_environment() {
    unsafe {
        let mut credential = ptr::null_mut();
        let mut error = ptr::null_mut();
        let bad = c("gsc1.x.y");
        assert_eq!(
            gs_credential_from_string(bad.as_ptr(), &mut credential, &mut error),
            GsStatus::Credential
        );
        assert_eq!(gs_error_code(error), GsErrorCode::None);
        assert!(gs_error_wire_code(error).is_null());
        assert_eq!(gs_error_http_status(error), 0);
        assert_eq!(gs_error_retry_after_ms(error), -1);
        assert!(!gs_error_is_retryable(error));
        gs_error_free(error);

        let mut environment = GsEnvironment::Production;
        let name = c("staging");
        assert_eq!(
            gs_environment_parse(name.as_ptr(), &mut environment, ptr::null_mut()),
            GsStatus::Ok
        );
        assert_eq!(environment, GsEnvironment::Staging);
        let wrong = c("prod");
        assert_eq!(
            gs_environment_parse(wrong.as_ptr(), &mut environment, ptr::null_mut()),
            GsStatus::InvalidArgument
        );

        // The connect consumes the credential even when it fails.
        let good = c(CRED);
        assert_eq!(
            gs_credential_from_string(good.as_ptr(), &mut credential, ptr::null_mut()),
            GsStatus::Ok
        );
        let mut client = ptr::null_mut();
        assert_eq!(
            gs_client_connect(ptr::null(), credential, &mut client, ptr::null_mut()),
            GsStatus::InvalidArgument
        );
        assert!(client.is_null());
    }
}

#[test]
fn posts_errors_sessions_and_tickets() {
    let mock = mock();
    let client = connect(&mock);
    unsafe {
        let (feature, content, key) = post("hello", "smoke");
        let body = GsPost {
            title: ptr::null(),
            content: content.as_ptr(),
            summary: ptr::null(),
            link_url: ptr::null(),
            link_label: ptr::null(),
        };
        let mut id = ptr::null_mut();
        let mut duplicate = true;
        let mut error = ptr::null_mut();
        assert_eq!(
            gs_post(
                client,
                feature.as_ptr(),
                &body,
                key.as_ptr(),
                &mut id,
                &mut duplicate,
                &mut error
            ),
            GsStatus::Ok
        );
        assert_eq!(read(id).unwrap(), "gs-1");
        assert!(!duplicate);
        gs_string_free(id);

        let unknown = c("no-such-feature");
        assert_eq!(
            gs_post(
                client,
                unknown.as_ptr(),
                &body,
                key.as_ptr(),
                &mut id,
                &mut duplicate,
                &mut error
            ),
            GsStatus::Api
        );
        assert_eq!(gs_error_code(error), GsErrorCode::FeatureNotFound);
        assert_eq!(
            read(gs_error_wire_code(error)).unwrap(),
            "feature_not_found"
        );
        assert_eq!(gs_error_http_status(error), 404);
        assert!(!gs_error_is_retryable(error));
        assert_eq!(
            read(gs_error_message(error)).unwrap(),
            "core 404 feature_not_found"
        );
        gs_error_free(error);

        // A link needs both halves.
        let url = c("https://example.com/p");
        let half = GsPost {
            link_url: url.as_ptr(),
            ..body
        };
        assert_eq!(
            gs_post(
                client,
                feature.as_ptr(),
                &half,
                key.as_ptr(),
                &mut id,
                &mut duplicate,
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument
        );

        // Sessions.
        let external = c("match-1");
        let create_key = c("match-1-key");
        let region = c("us-east");
        let keys = [c("map")];
        let values = [c("arena")];
        let key_ptrs = [keys[0].as_ptr()];
        let value_ptrs = [values[0].as_ptr()];
        let request = GsCreateSession {
            external_session_id: external.as_ptr(),
            idempotency_key: create_key.as_ptr(),
            region: region.as_ptr(),
            max_participants: 16,
            metadata_keys: key_ptrs.as_ptr(),
            metadata_values: value_ptrs.as_ptr(),
            metadata_len: 1,
        };
        let mut session = ptr::null_mut();
        assert_eq!(
            gs_session_create(client, &request, &mut session, ptr::null_mut()),
            GsStatus::Ok
        );
        assert_eq!(read(gs_session_id(session)).unwrap(), "gssn1.abc");
        let mut renew_after = 0;
        assert_eq!(
            gs_session_renew(session, &mut renew_after, ptr::null_mut()),
            GsStatus::Ok
        );
        assert_eq!(renew_after, 15_000);
        let mut renewer = ptr::null_mut();
        assert_eq!(
            gs_session_spawn_renewer(session, &mut renewer, ptr::null_mut()),
            GsStatus::Ok
        );
        gs_renewer_free(renewer);

        let mut opened = ptr::null_mut();
        let id_text = c("gssn1.abc");
        assert_eq!(
            gs_session_open(client, id_text.as_ptr(), &mut opened, ptr::null_mut()),
            GsStatus::Ok
        );

        let player = c("player-7f3a");
        let name = c("Wanderer");
        let guest_key = c("guest-key-1");
        let admit = GsAdmitGuest {
            external_player_id: player.as_ptr(),
            display_name: name.as_ptr(),
            idempotency_key: guest_key.as_ptr(),
            role: ptr::null(),
            team: ptr::null(),
        };
        let mut participant = ptr::null_mut();
        assert_eq!(
            gs_admit_guest(opened, &admit, &mut participant, ptr::null_mut()),
            GsStatus::Ok
        );
        assert_eq!(read(gs_participant_id(participant)).unwrap(), "gsp1.p");
        assert_eq!(read(gs_participant_subject(participant)).unwrap(), "gsg1.s");
        assert_eq!(
            read(gs_participant_display_name(participant)).unwrap(),
            "Wanderer"
        );
        assert_eq!(
            gs_participant_identity_kind(participant),
            GsIdentityKind::ServerVerifiedGuest
        );

        let mut ticket = ptr::null_mut();
        let mut expires = 0;
        assert_eq!(
            gs_issue_ticket(participant, 0, &mut ticket, &mut expires, ptr::null_mut()),
            GsStatus::InvalidArgument
        );
        assert_eq!(
            gs_issue_ticket(participant, 64, &mut ticket, &mut expires, ptr::null_mut()),
            GsStatus::InvalidArgument
        );
        assert_eq!(
            gs_issue_ticket(
                participant,
                GS_MODULE_VOICE,
                &mut ticket,
                &mut expires,
                ptr::null_mut()
            ),
            GsStatus::Ok
        );
        assert_eq!(read(ticket).unwrap(), format!("gpt1.{}", "t".repeat(43)));
        assert_eq!(expires, 120_000);
        gs_string_free(ticket);

        // Handles outlive the client: free the client first.
        gs_client_free(client);
        assert_eq!(
            gs_session_renew(session, ptr::null_mut(), ptr::null_mut()),
            GsStatus::Ok
        );
        gs_participant_free(participant);
        gs_session_free(opened);
        gs_session_free(session);
    }
    assert_no_origin(&mock);
}

type Outcome = (
    GsStatus,
    Option<String>,
    bool,
    Option<String>,
    Option<GsStatus>,
);

unsafe extern "C" fn on_post(
    user_data: *mut c_void,
    status: GsStatus,
    id: *const c_char,
    duplicate: bool,
    error: *const GsError,
) {
    let sender = unsafe { &*(user_data as *const SyncSender<Outcome>) };
    let wire = unsafe { read(gs_error_wire_code(error)) };
    sender
        .send((status, unsafe { read(id) }, duplicate, wire, None))
        .unwrap();
}

/// Tries a blocking call from inside the callback; reports its status.
unsafe extern "C" fn on_post_then_block(
    user_data: *mut c_void,
    status: GsStatus,
    _id: *const c_char,
    _duplicate: bool,
    _error: *const GsError,
) {
    let (sender, client) = unsafe { &*(user_data as *const (SyncSender<Outcome>, usize)) };
    let feature = c("smoke");
    let content = c("again");
    let key = c("post-key-2");
    let body = GsPost {
        title: ptr::null(),
        content: content.as_ptr(),
        summary: ptr::null(),
        link_url: ptr::null(),
        link_label: ptr::null(),
    };
    let mut id = ptr::null_mut();
    let mut duplicate = false;
    let mut error = ptr::null_mut();
    let inner = unsafe {
        gs_post(
            *client as *mut GsClient,
            feature.as_ptr(),
            &body,
            key.as_ptr(),
            &mut id,
            &mut duplicate,
            &mut error,
        )
    };
    let message = unsafe { read(gs_error_message(error)) };
    unsafe { gs_error_free(error) };
    sender
        .send((status, None, false, message, Some(inner)))
        .unwrap();
}

#[test]
fn async_posts_call_back_once_and_refuse_blocking_inside() {
    let mock = mock();
    let client = connect(&mock);
    let (feature, content, key) = post("hello", "smoke");
    let body = GsPost {
        title: ptr::null(),
        content: content.as_ptr(),
        summary: ptr::null(),
        link_url: ptr::null(),
        link_label: ptr::null(),
    };
    let (sender, receiver) = sync_channel::<Outcome>(4);
    unsafe {
        assert_eq!(
            gs_post_async(
                client,
                feature.as_ptr(),
                &body,
                key.as_ptr(),
                Some(on_post),
                (&sender as *const SyncSender<Outcome>).cast_mut().cast(),
                ptr::null_mut()
            ),
            GsStatus::Ok
        );
    }
    let outcome = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(outcome.0, GsStatus::Ok);
    assert_eq!(outcome.1.as_deref(), Some("gs-1"));
    assert!(
        receiver.recv_timeout(Duration::from_millis(300)).is_err(),
        "called once"
    );

    let unknown = c("no-such-feature");
    unsafe {
        assert_eq!(
            gs_post_async(
                client,
                unknown.as_ptr(),
                &body,
                key.as_ptr(),
                Some(on_post),
                (&sender as *const SyncSender<Outcome>).cast_mut().cast(),
                ptr::null_mut()
            ),
            GsStatus::Ok
        );
    }
    let outcome = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(outcome.0, GsStatus::Api);
    assert_eq!(outcome.3.as_deref(), Some("feature_not_found"));

    // Arguments are checked before queuing: a NULL callback is refused.
    unsafe {
        assert_eq!(
            gs_post_async(
                client,
                feature.as_ptr(),
                &body,
                key.as_ptr(),
                None,
                ptr::null_mut(),
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument
        );
    }

    let context = (sender.clone(), client as usize);
    unsafe {
        assert_eq!(
            gs_post_async(
                client,
                feature.as_ptr(),
                &body,
                key.as_ptr(),
                Some(on_post_then_block),
                (&context as *const (SyncSender<Outcome>, usize))
                    .cast_mut()
                    .cast(),
                ptr::null_mut()
            ),
            GsStatus::Ok
        );
    }
    let outcome = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(outcome.4, Some(GsStatus::InvalidArgument));
    assert!(outcome
        .3
        .unwrap()
        .contains("blocking call from a Gridspawn callback"));
    unsafe { gs_client_free(client) };
    assert_no_origin(&mock);
}

#[test]
fn blocking_calls_inside_an_async_runtime_are_refused() {
    let mock = mock();
    let client = connect(&mock);
    let (feature, content, key) = post("hello", "smoke");
    let body = GsPost {
        title: ptr::null(),
        content: content.as_ptr(),
        summary: ptr::null(),
        link_url: ptr::null(),
        link_label: ptr::null(),
    };
    let client_address = client as usize;
    let status = mock.rt.block_on(async move {
        let mut id = ptr::null_mut();
        let mut duplicate = false;
        unsafe {
            gs_post(
                client_address as *mut GsClient,
                feature.as_ptr(),
                &body,
                key.as_ptr(),
                &mut id,
                &mut duplicate,
                ptr::null_mut(),
            )
        }
    });
    assert_eq!(status, GsStatus::InvalidArgument);
    unsafe { gs_client_free(client) };
}
