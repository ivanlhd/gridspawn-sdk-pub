//! The game-action functions of the C ABI, called from Rust against a
//! stand-in edge: a reply from another thread, a reply inside the callback,
//! free without a reply, a reply after stop, argument checks, the
//! accessors (relative time on the monotonic clock), stats and last_error.

use std::{
    collections::VecDeque,
    ffi::{c_char, c_void, CStr, CString},
    ptr,
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

use gridspawn_server_capi::*;
use serde_json::{json, Value};
use wiremock::{
    matchers::{method, path, path_regex},
    Mock, MockServer, Request, Respond, ResponseTemplate,
};

const CRED: &str = "gsc1.gsc_test.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const RESULT: &str = r"^/v1/server/actions/gsa1\.[a-z2-7]{26}/result$";

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

fn id(n: u8) -> String {
    format!("gsa1.{}", ((b'a' + n) as char).to_string().repeat(26))
}

/// Dated 1999: only the 10 s between issuedAt and deadline may count.
fn envelope(n: u8) -> Value {
    json!({
        "actionId": id(n), "name": "shop.buy", "version": 2,
        "payload": {"itemId": "red_potion"},
        "actor": {"subject": "gss_PPPPPPPPPPPPPPPPPPPPPP"},
        "source": {"kind": "club_block", "clubId": "club1", "blockId": "block1"},
        "issuedAt": "1999-12-31T23:00:00.000Z", "deadline": "1999-12-31T23:00:10.000Z",
        "attempt": 1
    })
}

/// The first poll answers `first`; later ones answer nothing after a wait.
struct Polls(Mutex<VecDeque<Value>>);

impl Respond for Polls {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        match self.0.lock().unwrap().pop_front() {
            Some(answer) => ResponseTemplate::new(200).set_body_json(answer),
            None => ResponseTemplate::new(200)
                .set_body_json(json!({"actions": []}))
                .set_delay(Duration::from_millis(100)),
        }
    }
}

struct MockHost {
    rt: tokio::runtime::Runtime,
    server: MockServer,
}

fn mock(actions: Vec<Value>) -> MockHost {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/bootstrap"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "application": {"appId": "test-app"},
                "environment": "development", "protocol": {"selected": 1}, "modules": ["actions"]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/server/actions/poll"))
            .respond_with(Polls(Mutex::new(VecDeque::from([
                json!({"actions": actions}),
            ]))))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/server/actions/ack"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"accepted": true})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(RESULT))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/server/actions/stop"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&server)
            .await;
        server
    });
    MockHost { rt, server }
}

fn posted(mock: &MockHost, matches: impl Fn(&str) -> bool) -> Vec<Value> {
    mock.rt
        .block_on(mock.server.received_requests())
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "POST" && matches(r.url.path()))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn results(mock: &MockHost) -> Vec<Value> {
    posted(mock, |p| p.ends_with("/result"))
}

fn wait_for(what: &str, check: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
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

/// What the callback does with each action.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Hand the pointer to the test thread.
    Keep,
    /// Reply "declined" inside the callback.
    DeclineInside,
}

struct Sink {
    mode: Mode,
    actions: Mutex<mpsc::Sender<usize>>,
}

unsafe extern "C" fn on_action(user_data: *mut c_void, action: *mut GsAction) {
    let sink = unsafe { &*(user_data as *const Sink) };
    if sink.mode == Mode::DeclineInside {
        let (code, message) = (c("game/out_of_stock"), c("Sold out."));
        let status = unsafe {
            gs_action_reply_declined(
                action,
                code.as_ptr(),
                message.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
            )
        };
        assert_eq!(status, GsStatus::Ok);
        let _ = sink.actions.lock().unwrap().send(0);
    } else {
        let _ = sink.actions.lock().unwrap().send(action as usize);
    }
}

struct Running {
    mock: MockHost,
    client: *mut GsClient,
    receiver: *mut GsActionReceiver,
    actions: mpsc::Receiver<usize>,
    _sink: Arc<Sink>,
}

fn start(actions: Vec<Value>, mode: Mode) -> Running {
    let mock = mock(actions);
    let (tx, rx) = mpsc::channel();
    let sink = Arc::new(Sink {
        mode,
        actions: Mutex::new(tx),
    });
    unsafe {
        let client = connect(&mock);
        let names = [c("shop.buy")];
        let pointers: Vec<*const c_char> = names.iter().map(|n| n.as_ptr()).collect();
        let mut receiver = ptr::null_mut();
        assert_eq!(
            gs_actions_start(
                client,
                pointers.as_ptr(),
                pointers.len(),
                0,
                Some(on_action),
                Arc::as_ptr(&sink) as *mut c_void,
                &mut receiver,
                ptr::null_mut(),
            ),
            GsStatus::Ok
        );
        Running {
            mock,
            client,
            receiver,
            actions: rx,
            _sink: sink,
        }
    }
}

impl Running {
    fn next_action(&self) -> *mut GsAction {
        self.actions.recv_timeout(Duration::from_secs(5)).unwrap() as *mut GsAction
    }

    fn finish(self) {
        unsafe {
            gs_action_receiver_free(self.receiver);
            gs_client_free(self.client);
        }
    }
}

#[test]
fn a_held_action_is_read_and_replied_to_from_another_thread() {
    let running = start(vec![envelope(1)], Mode::Keep);
    let action = running.next_action();
    unsafe {
        assert_eq!(read(gs_action_id(action)).unwrap(), id(1));
        assert_eq!(read(gs_action_name(action)).unwrap(), "shop.buy");
        assert_eq!(gs_action_version(action), 2);
        assert_eq!(gs_action_attempt(action), 1);
        assert_eq!(
            read(gs_action_subject(action)).unwrap(),
            "gss_PPPPPPPPPPPPPPPPPPPPPP"
        );
        let payload: Value =
            serde_json::from_str(&read(gs_action_payload_json(action)).unwrap()).unwrap();
        assert_eq!(payload, json!({"itemId": "red_potion"}));
        let source: Value =
            serde_json::from_str(&read(gs_action_source_json(action)).unwrap()).unwrap();
        assert_eq!(
            source,
            json!({"kind": "club_block", "clubId": "club1", "blockId": "block1"})
        );
        // 1999 on the edge's clock, decades off this machine's: only the span counts.
        let remaining = gs_action_remaining_ms(action);
        assert!((8_000..=10_000).contains(&remaining), "{remaining}");
        assert_eq!(gs_action_deadline_unix_ms(action), 946_681_210_000);
    }
    let pointer = action as usize;
    let replier = std::thread::spawn(move || unsafe {
        let data = c(r#"{"balance": 120}"#);
        gs_action_reply_ok(pointer as *mut GsAction, data.as_ptr(), ptr::null_mut())
    });
    assert_eq!(replier.join().unwrap(), GsStatus::Ok);
    wait_for("the result", || results(&running.mock).len() == 1);
    assert_eq!(
        results(&running.mock)[0],
        json!({"status": "ok", "data": {"balance": 120}})
    );
    unsafe {
        let stats: Value =
            serde_json::from_str(&read(gs_action_receiver_stats_json(running.receiver)).unwrap())
                .unwrap();
        assert_eq!(stats["accepted"], 1);
        assert_eq!(stats["resultsPosted"], 1);
        assert!(gs_action_receiver_last_error(running.receiver).is_null());
        assert!(gs_action_receiver_stats_json(ptr::null()).is_null());
    }
    running.finish();
}

#[test]
fn a_reply_inside_the_callback_does_not_block() {
    let running = start(vec![envelope(2)], Mode::DeclineInside);
    assert_eq!(
        running
            .actions
            .recv_timeout(Duration::from_secs(5))
            .unwrap(),
        0
    );
    wait_for("the result", || results(&running.mock).len() == 1);
    assert_eq!(
        results(&running.mock)[0],
        json!({"status": "declined", "code": "game/out_of_stock", "userMessage": "Sold out."})
    );
    running.finish();
}

#[test]
fn freeing_an_action_without_a_reply_posts_no_reply() {
    let running = start(vec![envelope(3)], Mode::Keep);
    let action = running.next_action();
    unsafe { gs_action_free(action) };
    wait_for("the result", || results(&running.mock).len() == 1);
    assert_eq!(results(&running.mock)[0]["code"], "game/no_reply");
    running.finish();
}

#[test]
fn after_stop_a_reply_fails_and_nothing_is_posted() {
    let running = start(vec![envelope(4)], Mode::Keep);
    let action = running.next_action();
    unsafe {
        let mut error = ptr::null_mut();
        assert_eq!(
            gs_actions_stop(running.receiver, 100, &mut error),
            GsStatus::Timeout,
            "one action was still held"
        );
        gs_error_free(error);
        assert_eq!(posted(&running.mock, |p| p.ends_with("/stop")).len(), 1);
        let mut error = ptr::null_mut();
        assert_eq!(
            gs_action_reply_ok(action, ptr::null(), &mut error),
            GsStatus::Runtime
        );
        assert!(!error.is_null());
        gs_error_free(error);
        assert_eq!(
            gs_actions_stop(running.receiver, 100, ptr::null_mut()),
            GsStatus::InvalidArgument,
            "already stopped"
        );
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(results(&running.mock).is_empty(), "nothing was posted");
    running.finish();
}

#[test]
fn arguments_are_checked_and_a_refused_reply_keeps_the_action() {
    let mock = mock(vec![]);
    unsafe {
        let client = connect(&mock);
        let (good, bad) = (c("shop.buy"), c("shop buy"));
        let mut receiver = ptr::null_mut();
        let one = [good.as_ptr()];
        assert_eq!(
            gs_actions_start(
                client,
                one.as_ptr(),
                1,
                0,
                None,
                ptr::null_mut(),
                &mut receiver,
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument,
            "no callback"
        );
        assert_eq!(
            gs_actions_start(
                client,
                one.as_ptr(),
                0,
                0,
                Some(on_action),
                ptr::null_mut(),
                &mut receiver,
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument,
            "no names"
        );
        let wrong = [bad.as_ptr()];
        assert_eq!(
            gs_actions_start(
                client,
                wrong.as_ptr(),
                1,
                0,
                Some(on_action),
                ptr::null_mut(),
                &mut receiver,
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument,
            "a bad name"
        );
        assert_eq!(
            gs_actions_start(
                client,
                one.as_ptr(),
                1,
                65,
                Some(on_action),
                ptr::null_mut(),
                &mut receiver,
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument,
            "concurrency 65"
        );
        assert_eq!(
            gs_action_reply_ok(ptr::null_mut(), ptr::null(), ptr::null_mut()),
            GsStatus::InvalidArgument
        );
        assert!(gs_action_id(ptr::null()).is_null());
        assert_eq!(gs_action_remaining_ms(ptr::null()), 0);
        gs_action_free(ptr::null_mut());
        gs_action_receiver_free(ptr::null_mut());
        gs_client_free(client);
    }

    let running = start(vec![envelope(5)], Mode::Keep);
    let action = running.next_action();
    unsafe {
        let not_json = c("{balance");
        assert_eq!(
            gs_action_reply_ok(action, not_json.as_ptr(), ptr::null_mut()),
            GsStatus::InvalidArgument
        );
        assert_eq!(
            gs_action_reply_declined(
                action,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null_mut()
            ),
            GsStatus::InvalidArgument
        );
        // Still the game's: read it, then reply properly.
        assert_eq!(read(gs_action_id(action)).unwrap(), id(5));
        let code = c("game/database_unavailable");
        assert_eq!(
            gs_action_reply_error(action, code.as_ptr(), ptr::null(), ptr::null_mut()),
            GsStatus::Ok
        );
    }
    wait_for("the result", || results(&running.mock).len() == 1);
    assert_eq!(
        results(&running.mock)[0]["code"],
        "game/database_unavailable"
    );
    running.finish();
}

#[test]
fn an_invalid_reply_is_posted_as_invalid_reply_and_reported() {
    let running = start(vec![envelope(6)], Mode::Keep);
    let action = running.next_action();
    unsafe {
        let (code, message) = (c("not_a_game_code"), c("No."));
        assert_eq!(
            gs_action_reply_declined(
                action,
                code.as_ptr(),
                message.as_ptr(),
                ptr::null(),
                ptr::null_mut()
            ),
            GsStatus::Ok
        );
    }
    wait_for("the result", || results(&running.mock).len() == 1);
    assert_eq!(results(&running.mock)[0]["code"], "game/invalid_reply");
    unsafe {
        let error = gs_action_receiver_last_error(running.receiver);
        assert!(!error.is_null());
        assert_eq!(gs_error_status(error), GsStatus::InvalidArgument);
        gs_error_free(error);
    }
    running.finish();
}
