//! Game actions against a stand-in edge (docs/protocol/07-game-actions.md):
//! poll, ack, handler and result; refused and retried acks; deadlines on the
//! monotonic clock (an envelope hours off this machine's clock); no
//! cancellation; coalescing and the finished cache; the concurrency limit;
//! poll backoff; result retries; invalid replies; stop. No network.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use gridspawn_server::{
    Action, ActionReceiverOptions, ActionReply, Client, Credential, Environment, Error, ErrorCode,
    RetryPolicy,
};
use serde_json::{json, Value};
use url::Url;
use wiremock::{
    matchers::{method, path, path_regex},
    Mock, MockServer, Request, Respond, ResponseTemplate,
};

const CRED: &str = "gsc1.gsc_test.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const POLL: &str = "/v1/server/actions/poll";
const ACK: &str = "/v1/server/actions/ack";
const STOP: &str = "/v1/server/actions/stop";
const RESULT: &str = r"^/v1/server/actions/gsa1\.[a-z2-7]{26}/result$";

fn id(n: u8) -> String {
    format!("gsa1.{}", ((b'a' + n) as char).to_string().repeat(26))
}

/// An envelope whose clock is far from this machine's: only `deadline -
/// issuedAt` may count.
fn envelope(n: u8, attempt: u8, span_ms: u64) -> Value {
    let deadline = format!(
        "1999-12-31T23:{:02}:{:02}.{:03}Z",
        span_ms / 60_000,
        (span_ms / 1_000) % 60,
        span_ms % 1_000
    );
    json!({
        "actionId": id(n),
        "name": "shop.buy",
        "version": 1,
        "payload": {"itemId": "red_potion", "quantity": 3},
        "actor": {"subject": "gss_PPPPPPPPPPPPPPPPPPPPPP"},
        "source": {"kind": "club_block", "clubId": "club1", "blockId": "block1"},
        "issuedAt": "1999-12-31T23:00:00.000Z",
        "deadline": deadline,
        "attempt": attempt,
    })
}

/// Poll answers in order; then empty answers after a short wait.
struct PollScript {
    answers: Mutex<VecDeque<ResponseTemplate>>,
    bodies: Mutex<Vec<Value>>,
}

impl Respond for PollScript {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.bodies
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&request.body).unwrap_or(Value::Null));
        self.answers.lock().unwrap().pop_front().unwrap_or_else(|| {
            ResponseTemplate::new(200)
                .set_body_json(json!({"actions": []}))
                .set_delay(Duration::from_millis(100))
        })
    }
}

/// Answers in order, then the last one forever; records the bodies.
struct Script {
    answers: Mutex<VecDeque<ResponseTemplate>>,
    last: ResponseTemplate,
    bodies: Mutex<Vec<(String, Value)>>,
}

impl Respond for Script {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.bodies.lock().unwrap().push((
            request.url.path().to_string(),
            serde_json::from_slice(&request.body).unwrap_or(Value::Null),
        ));
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.last.clone())
    }
}

fn script(answers: Vec<ResponseTemplate>, last: ResponseTemplate) -> Arc<Script> {
    Arc::new(Script {
        answers: Mutex::new(answers.into()),
        last,
        bodies: Mutex::default(),
    })
}

struct Shared<T>(Arc<T>);

impl<T: Respond> Respond for Shared<T> {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.0.respond(request)
    }
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn refusal(status: u16, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({
        "ok": false, "error": {"code": code, "message": "x"}, "requestId": "r"
    }))
}

struct Edge {
    server: MockServer,
    polls: Arc<PollScript>,
    acks: Arc<Script>,
    results: Arc<Script>,
    stops: Arc<Script>,
}

impl Edge {
    async fn start(
        polls: Vec<ResponseTemplate>,
        acks: Vec<ResponseTemplate>,
        results: Vec<ResponseTemplate>,
    ) -> Self {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/bootstrap"))
            .respond_with(ok(json!({
                "ok": true, "requestId": "r",
                "application": {"appId": "test-app", "displayName": "Test"},
                "environment": "development",
                "protocol": {"selected": 1, "minimum": 1, "maximum": 1},
                "modules": ["actions"]
            })))
            .mount(&server)
            .await;
        let polls = Arc::new(PollScript {
            answers: Mutex::new(polls.into()),
            bodies: Mutex::default(),
        });
        let acks = script(acks, ok(json!({"accepted": true})));
        let results = script(results, ok(json!({"ok": true})));
        let stops = script(vec![], ok(json!({"ok": true})));
        Mock::given(method("POST"))
            .and(path(POLL))
            .respond_with(Shared(polls.clone()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(ACK))
            .respond_with(Shared(acks.clone()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(RESULT))
            .respond_with(Shared(results.clone()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(STOP))
            .respond_with(Shared(stops.clone()))
            .mount(&server)
            .await;
        Self {
            server,
            polls,
            acks,
            results,
            stops,
        }
    }

    async fn client(&self) -> Client {
        let mut retry = RetryPolicy::default();
        retry.initial_backoff = Duration::from_millis(1);
        retry.max_backoff = Duration::from_millis(5);
        Client::builder(
            "test-app",
            Environment::Development,
            Credential::parse(CRED).unwrap(),
        )
        .sdk_base_url(Url::parse(&self.server.uri()).unwrap())
        .retry_policy(retry)
        .connect()
        .await
        .unwrap()
    }

    fn results(&self) -> Vec<(String, Value)> {
        self.results.bodies.lock().unwrap().clone()
    }

    fn acks(&self) -> Vec<Value> {
        self.acks
            .bodies
            .lock()
            .unwrap()
            .iter()
            .map(|(_, body)| body.clone())
            .collect()
    }
}

fn actions(items: Vec<Value>) -> ResponseTemplate {
    ok(json!({ "actions": items }))
}

fn options() -> ActionReceiverOptions {
    ActionReceiverOptions::new(["shop.buy"]).poll_wait(Duration::from_secs(1))
}

/// Waits until `check` holds, for up to 5 s.
async fn eventually(what: &str, check: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn an_accepted_action_runs_once_and_its_reply_is_posted() {
    let edge = Edge::start(vec![actions(vec![envelope(1, 1, 10_000)])], vec![], vec![]).await;
    let client = edge.client().await;
    let seen: Arc<Mutex<Vec<Action>>> = Arc::default();
    let log = seen.clone();
    let receiver = client
        .spawn_action_receiver(options(), move |action: Action| {
            log.lock().unwrap().push(action);
            async { ActionReply::ok(json!({"balance": 120})).with_user_message("Bought.") }
        })
        .unwrap();
    eventually("the result", || edge.results().len() == 1).await;
    let action = seen.lock().unwrap()[0].clone();
    assert_eq!(action.id, id(1));
    assert_eq!(action.name, "shop.buy");
    assert_eq!(action.actor.subject, "gss_PPPPPPPPPPPPPPPPPPPPPP");
    assert_eq!(action.source.club_id.as_deref(), Some("club1"));
    assert_eq!(action.payload["quantity"], 3);
    let (path, body) = edge.results()[0].clone();
    assert_eq!(path, format!("/v1/server/actions/{}/result", id(1)));
    assert_eq!(
        body,
        json!({"status": "ok", "userMessage": "Bought.", "data": {"balance": 120}})
    );
    let poll = edge.polls.bodies.lock().unwrap()[0].clone();
    assert_eq!(poll["names"], json!(["shop.buy"]));
    assert_eq!(poll["max"], 4);
    assert_eq!(poll["waitSeconds"], 1);
    assert_eq!(poll["instanceId"], receiver.instance_id());
    assert_eq!(
        edge.acks()[0],
        json!({"actionId": id(1), "attempt": 1, "instanceId": receiver.instance_id()})
    );
    let stats = receiver.stats();
    assert_eq!(
        (stats.received, stats.accepted, stats.results_posted),
        (1, 1, 1)
    );
}

#[tokio::test]
async fn a_refused_ack_never_starts_the_handler() {
    let edge = Edge::start(
        vec![actions(vec![envelope(2, 1, 10_000)])],
        vec![ok(json!({"accepted": false}))],
        vec![],
    )
    .await;
    let client = edge.client().await;
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let receiver = client
        .spawn_action_receiver(options(), move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            async { ActionReply::ok_empty() }
        })
        .unwrap();
    eventually("the refusal", || receiver.stats().refused == 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert!(edge.results().is_empty());
}

#[tokio::test]
async fn the_ack_is_retried_until_the_edge_answers() {
    let edge = Edge::start(
        vec![actions(vec![envelope(3, 1, 10_000)])],
        vec![
            refusal(503, "actions/unavailable"),
            refusal(502, "http_502"),
        ],
        vec![],
    )
    .await;
    let client = edge.client().await;
    let receiver = client
        .spawn_action_receiver(options(), |_| async { ActionReply::ok_empty() })
        .unwrap();
    eventually("the result", || edge.results().len() == 1).await;
    assert_eq!(edge.acks().len(), 3, "two failures, then accepted");
    assert_eq!(receiver.stats().accepted, 1);
}

#[tokio::test]
async fn deadlines_use_the_envelope_span_on_this_machines_clock() {
    // issuedAt/deadline are in 1999: compared with the wall clock this action
    // would be decades late. Only the 10 s span counts.
    let edge = Edge::start(vec![actions(vec![envelope(4, 1, 10_000)])], vec![], vec![]).await;
    let client = edge.client().await;
    let remaining: Arc<Mutex<Option<Duration>>> = Arc::default();
    let seen = remaining.clone();
    let receiver = client
        .spawn_action_receiver(options(), move |action: Action| {
            *seen.lock().unwrap() = Some(action.remaining());
            // 1999-12-31T23:00:10Z, for display only.
            assert_eq!(action.deadline_unix_ms(), Some(946_681_210_000));
            async { ActionReply::ok_empty() }
        })
        .unwrap();
    eventually("the result", || edge.results().len() == 1).await;
    let left = remaining.lock().unwrap().unwrap();
    assert!(
        left > Duration::from_secs(8) && left <= Duration::from_secs(10),
        "{left:?}"
    );
    assert_eq!(receiver.stats().expired, 0);
    assert_eq!(edge.results()[0].1["status"], "ok");
}

#[tokio::test]
async fn an_action_accepted_after_its_deadline_is_not_run() {
    // A zero span: the deadline passed when the answer arrived.
    let edge = Edge::start(vec![actions(vec![envelope(5, 2, 0)])], vec![], vec![]).await;
    let client = edge.client().await;
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let receiver = client
        .spawn_action_receiver(options(), move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            async { ActionReply::ok_empty() }
        })
        .unwrap();
    eventually("the result", || edge.results().len() == 1).await;
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(edge.results()[0].1["code"], "game/expired");
    assert_eq!(receiver.stats().expired, 1);
}

#[tokio::test]
async fn a_slow_handler_is_never_cancelled_and_its_late_result_is_posted() {
    let edge = Edge::start(vec![actions(vec![envelope(6, 1, 300)])], vec![], vec![]).await;
    let client = edge.client().await;
    let _receiver = client
        .spawn_action_receiver(options(), |_| async {
            tokio::time::sleep(Duration::from_millis(800)).await;
            ActionReply::declined("game/out_of_stock", "Sold out.")
        })
        .unwrap();
    eventually("the late result", || edge.results().len() == 1).await;
    assert_eq!(edge.results()[0].1["code"], "game/out_of_stock");
}

#[tokio::test]
async fn a_repeated_id_runs_once_and_a_finished_one_reposts_its_result() {
    let edge = Edge::start(
        vec![
            actions(vec![envelope(7, 1, 10_000)]),
            // Again while it runs: coalesced, nothing posted for it.
            actions(vec![envelope(7, 1, 10_000)]),
        ],
        vec![],
        vec![],
    )
    .await;
    let client = edge.client().await;
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let receiver = client
        .spawn_action_receiver(options(), move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            async {
                tokio::time::sleep(Duration::from_millis(400)).await;
                ActionReply::ok_empty()
            }
        })
        .unwrap();
    eventually("the result", || edge.results().len() == 1).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(receiver.stats().coalesced, 1);
    drop(receiver);

    // Finished, then delivered again: the stored result is posted again.
    let edge = Edge::start(
        vec![
            actions(vec![envelope(8, 1, 10_000)]),
            ResponseTemplate::new(200)
                .set_body_json(json!({"actions": [envelope(8, 2, 10_000)]}))
                .set_delay(Duration::from_millis(300)),
        ],
        vec![],
        vec![
            ok(json!({"ok": true})),
            ok(json!({"ok": true, "duplicate": true})),
        ],
    )
    .await;
    let client = edge.client().await;
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let receiver = client
        .spawn_action_receiver(options(), move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            async { ActionReply::ok(json!({"n": 1})) }
        })
        .unwrap();
    eventually("both posts", || edge.results().len() == 2).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(edge.results()[0].1, edge.results()[1].1);
    let stats = receiver.stats();
    assert_eq!((stats.results_posted, stats.results_duplicate), (1, 1));
}

#[tokio::test]
async fn no_more_handlers_run_at_once_than_the_concurrency() {
    let batch = (10..15).map(|n| envelope(n, 1, 10_000)).collect();
    let edge = Edge::start(vec![actions(batch)], vec![], vec![]).await;
    let client = edge.client().await;
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (now, top) = (running.clone(), peak.clone());
    let _receiver = client
        .spawn_action_receiver(options().concurrency(2), move |_| {
            let (now, top) = (now.clone(), top.clone());
            async move {
                let current = now.fetch_add(1, Ordering::SeqCst) + 1;
                top.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(100)).await;
                now.fetch_sub(1, Ordering::SeqCst);
                ActionReply::ok_empty()
            }
        })
        .unwrap();
    eventually("five results", || edge.results().len() == 5).await;
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    assert_eq!(edge.polls.bodies.lock().unwrap()[0]["max"], 2);
}

#[tokio::test]
async fn a_rate_limited_poll_waits_for_retry_after() {
    let edge = Edge::start(
        vec![
            refusal(429, "actions/rate_limited").insert_header("retry-after", "1"),
            actions(vec![envelope(16, 1, 10_000)]),
        ],
        vec![],
        vec![],
    )
    .await;
    let client = edge.client().await;
    let started = Instant::now();
    let receiver = client
        .spawn_action_receiver(options(), |_| async { ActionReply::ok_empty() })
        .unwrap();
    eventually("the result", || edge.results().len() == 1).await;
    assert!(started.elapsed() >= Duration::from_millis(950));
    let error = receiver.last_error().unwrap();
    assert_eq!(error.api().unwrap().code, ErrorCode::RateLimited);
}

#[tokio::test]
async fn a_disabled_poll_backs_off_and_says_why() {
    let edge = Edge::start(vec![refusal(403, "actions/disabled")], vec![], vec![]).await;
    let client = edge.client().await;
    let receiver = client
        .spawn_action_receiver(options(), |_| async { ActionReply::ok_empty() })
        .unwrap();
    eventually("the error", || receiver.last_error().is_some()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(edge.polls.bodies.lock().unwrap().len(), 1, "backing off");
    match receiver.last_error() {
        Some(Error::Api(api)) => assert_eq!(api.code, ErrorCode::ActionsDisabled),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn results_are_retried_and_refusals_are_recorded() {
    let edge = Edge::start(
        vec![actions(vec![
            envelope(17, 1, 10_000),
            envelope(18, 1, 10_000),
        ])],
        vec![],
        vec![
            refusal(503, "actions/unavailable"),
            ok(json!({"ok": true})),
            refusal(409, "actions/result_too_late"),
        ],
    )
    .await;
    let client = edge.client().await;
    let receiver = client
        .spawn_action_receiver(options().concurrency(1), |_| async {
            ActionReply::ok_empty()
        })
        .unwrap();
    eventually("both outcomes", || {
        let stats = receiver.stats();
        stats.results_posted == 1 && stats.results_failed == 1
    })
    .await;
    assert_eq!(edge.results().len(), 3, "one retry, then one refusal");
    match receiver.last_error() {
        Some(Error::Api(api)) => assert_eq!(api.code, ErrorCode::ActionsResultTooLate),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_invalid_reply_is_posted_as_invalid_reply() {
    let edge = Edge::start(vec![actions(vec![envelope(19, 1, 10_000)])], vec![], vec![]).await;
    let client = edge.client().await;
    let receiver = client
        .spawn_action_receiver(options(), |_| async {
            ActionReply::declined("not_enough_gold", "No.")
        })
        .unwrap();
    eventually("the result", || edge.results().len() == 1).await;
    assert_eq!(edge.results()[0].1["code"], "game/invalid_reply");
    assert_eq!(receiver.stats().invalid_replies, 1);
    match receiver.last_error() {
        Some(Error::InvalidInput { field, .. }) => assert_eq!(field, "action reply"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_panicking_handler_posts_nothing_and_says_so_in_last_error() {
    let edge = Edge::start(
        vec![
            actions(vec![envelope(20, 1, 10_000)]),
            // The same id again after the panic: not run again, nothing posted.
            ResponseTemplate::new(200)
                .set_body_json(json!({"actions": [envelope(20, 2, 10_000)]}))
                .set_delay(Duration::from_millis(300)),
        ],
        vec![],
        vec![],
    )
    .await;
    let client = edge.client().await;
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let receiver = client
        .spawn_action_receiver(options(), move |action: Action| {
            counter.fetch_add(1, Ordering::SeqCst);
            async move {
                if action.id == id(20) {
                    panic!("a handler bug");
                }
                ActionReply::ok_empty()
            }
        })
        .unwrap();
    eventually("the repeat", || receiver.stats().coalesced == 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(edge.results().is_empty(), "nothing posted");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    match receiver.last_error() {
        Some(Error::InvalidInput { field, reason }) => {
            assert_eq!(field, "action handler");
            assert!(reason.starts_with("game/handler_panicked"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    let stats = receiver.stats();
    assert_eq!((stats.running, stats.results_posted), (0, 0));
    assert_eq!(receiver.stop(Duration::from_secs(1)).await, 0);
}

#[tokio::test]
async fn an_action_waiting_for_a_busy_slot_past_its_deadline_is_not_run() {
    // One slot. The edge sends two actions although it was asked for one:
    // the second waits for the slot, and its 300 ms are gone by then.
    let edge = Edge::start(
        vec![actions(vec![envelope(24, 1, 10_000), envelope(25, 1, 300)])],
        vec![],
        vec![],
    )
    .await;
    let client = edge.client().await;
    let ran: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = ran.clone();
    let receiver = client
        .spawn_action_receiver(options().concurrency(1), move |action: Action| {
            log.lock().unwrap().push(action.id.clone());
            async {
                tokio::time::sleep(Duration::from_millis(800)).await;
                ActionReply::ok_empty()
            }
        })
        .unwrap();
    eventually("both results", || edge.results().len() == 2).await;
    assert_eq!(*ran.lock().unwrap(), vec![id(24)], "the late one never ran");
    let expired: Vec<_> = edge
        .results()
        .into_iter()
        .filter(|(_, body)| body["code"] == "game/expired")
        .map(|(path, _)| path)
        .collect();
    assert_eq!(
        expired,
        vec![format!("/v1/server/actions/{}/result", id(25))]
    );
    assert_eq!(receiver.stats().expired, 1);
    assert_eq!(edge.polls.bodies.lock().unwrap()[0]["max"], 1);
}

#[tokio::test]
async fn no_poll_while_every_slot_is_busy_and_polls_ask_for_free_slots_only() {
    let edge = Edge::start(vec![actions(vec![envelope(9, 1, 10_000)])], vec![], vec![]).await;
    let client = edge.client().await;
    let receiver = client
        .spawn_action_receiver(options().concurrency(1), |_| async {
            tokio::time::sleep(Duration::from_millis(800)).await;
            ActionReply::ok_empty()
        })
        .unwrap();
    eventually("the handler", || receiver.stats().accepted == 1).await;
    let polls = edge.polls.bodies.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        edge.polls.bodies.lock().unwrap().len(),
        polls,
        "no poll while the only slot is busy"
    );
    assert_eq!(polls, 1);
    eventually("the result", || edge.results().len() == 1).await;
    eventually("the next poll", || {
        edge.polls.bodies.lock().unwrap().len() >= 2
    })
    .await;
    for poll in edge.polls.bodies.lock().unwrap().iter() {
        assert_eq!(poll["max"], 1);
    }
    drop(receiver);

    // Two slots, one busy: the next poll asks for one.
    let edge = Edge::start(vec![actions(vec![envelope(15, 1, 10_000)])], vec![], vec![]).await;
    let client = edge.client().await;
    let receiver = client
        .spawn_action_receiver(options().concurrency(2), |_| async {
            tokio::time::sleep(Duration::from_millis(800)).await;
            ActionReply::ok_empty()
        })
        .unwrap();
    eventually("the second poll", || {
        edge.polls.bodies.lock().unwrap().len() >= 2
    })
    .await;
    let bodies = edge.polls.bodies.lock().unwrap().clone();
    assert_eq!(bodies[0]["max"], 2);
    assert_eq!(bodies[1]["max"], 1);
    drop(receiver);
}

#[tokio::test]
async fn stop_tells_the_edge_and_waits_for_running_handlers() {
    let edge = Edge::start(vec![actions(vec![envelope(21, 1, 10_000)])], vec![], vec![]).await;
    let client = edge.client().await;
    let receiver = client
        .spawn_action_receiver(options(), |_| async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            ActionReply::ok_empty()
        })
        .unwrap();
    eventually("the handler", || receiver.stats().accepted == 1).await;
    assert_eq!(receiver.stop(Duration::from_secs(3)).await, 0);
    assert_eq!(edge.results().len(), 1, "finished before stop returned");
    let stops = edge.stops.bodies.lock().unwrap().clone();
    assert_eq!(
        stops,
        vec![(
            STOP.to_string(),
            json!({"instanceId": receiver.instance_id()})
        )]
    );
    let polls = edge.polls.bodies.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        edge.polls.bodies.lock().unwrap().len(),
        polls,
        "no polls after stop"
    );
}

#[tokio::test]
async fn stop_reports_handlers_still_running_and_they_still_post() {
    let edge = Edge::start(vec![actions(vec![envelope(22, 1, 10_000)])], vec![], vec![]).await;
    let client = edge.client().await;
    let receiver = client
        .spawn_action_receiver(options(), |_| async {
            tokio::time::sleep(Duration::from_millis(600)).await;
            ActionReply::ok_empty()
        })
        .unwrap();
    eventually("the handler", || receiver.stats().accepted == 1).await;
    assert_eq!(receiver.stop(Duration::from_millis(100)).await, 1);
    eventually("the late result", || edge.results().len() == 1).await;
}

#[test]
fn options_and_names_are_checked_before_starting() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let edge = Edge::start(vec![], vec![], vec![]).await;
        let client = edge.client().await;
        for options in [
            ActionReceiverOptions::new(["bad name"]),
            ActionReceiverOptions::new(["shop.buy"]).concurrency(0),
            ActionReceiverOptions::new(["shop.buy"]).poll_wait(Duration::from_secs(30)),
        ] {
            let refused =
                client.spawn_action_receiver(options, |_| async { ActionReply::ok_empty() });
            assert!(matches!(refused, Err(Error::InvalidInput { .. })));
        }
    });
}

#[cfg(feature = "blocking")]
#[test]
fn the_blocking_receiver_runs_plain_functions() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let edge = runtime.block_on(Edge::start(
        vec![actions(vec![envelope(23, 1, 10_000)])],
        vec![],
        vec![],
    ));
    let builder = Client::builder(
        "test-app",
        Environment::Development,
        Credential::parse(CRED).unwrap(),
    )
    .sdk_base_url(Url::parse(&edge.server.uri()).unwrap());
    let client = gridspawn_server::blocking::Client::connect(builder).unwrap();
    let receiver = client
        .spawn_action_receiver_blocking(options(), |action: Action| {
            std::thread::sleep(Duration::from_millis(50));
            ActionReply::ok(json!({"id": action.id}))
        })
        .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while edge.results().is_empty() {
        assert!(Instant::now() < until, "no result");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(edge.results()[0].1["data"]["id"], id(23));
    assert_eq!(receiver.stats().results_posted, 1);
    assert_eq!(receiver.stop(Duration::from_secs(1)), 0);
}

#[cfg(feature = "blocking")]
#[test]
fn a_panicking_blocking_handler_posts_nothing() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let edge = runtime.block_on(Edge::start(
        vec![actions(vec![envelope(0, 1, 10_000)])],
        vec![],
        vec![],
    ));
    let builder = Client::builder(
        "test-app",
        Environment::Development,
        Credential::parse(CRED).unwrap(),
    )
    .sdk_base_url(Url::parse(&edge.server.uri()).unwrap());
    let client = gridspawn_server::blocking::Client::connect(builder).unwrap();
    let receiver = client
        .spawn_action_receiver_blocking(options(), |action: Action| {
            if action.id == id(0) {
                panic!("a handler bug");
            }
            ActionReply::ok_empty()
        })
        .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while receiver.last_error().is_none() {
        assert!(Instant::now() < until, "no error recorded");
        std::thread::sleep(Duration::from_millis(20));
    }
    match receiver.last_error() {
        Some(Error::InvalidInput { field, reason }) => {
            assert_eq!(field, "action handler");
            assert!(reason.starts_with("game/handler_panicked"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(edge.results().is_empty(), "nothing posted");
    assert_eq!(receiver.stats().running, 0);
    assert_eq!(receiver.stop(Duration::from_secs(1)), 0);
}
