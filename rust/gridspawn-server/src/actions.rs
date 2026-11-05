//! Game actions: the platform asks this game server to do something for a
//! signed-in player, such as a club shop purchase
//! (`docs/protocol/07-game-actions.md`).
//!
//! [`Client::spawn_action_receiver`] long-polls the SDK edge, confirms each
//! action (the edge's ack is the only path that lets a handler start), runs
//! your handler and posts its [`ActionReply`].
//!
//! **Your handler must deduplicate on [`Action::id`] itself, durably.** The
//! receiver never runs one id twice in this process, but after a restart, or
//! across several processes, the same id can reach a handler again. Insert
//! the id into a uniquely keyed table in the same database transaction as
//! the charge, before anything else, in storage every instance shares.
//!
//! Clocks: every timer here is measured on this machine's monotonic clock
//! from the moment the poll answer arrived. The envelope's absolute
//! `issuedAt` and `deadline` are only used for their difference, never
//! compared with this machine's wall clock, so a skewed clock changes
//! nothing.
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore},
    time::Instant,
};

use crate::{client::segment, error::Error, http::Http, Client};

/// How long the edge waits at most per poll (it caps it lower if configured).
pub const DEFAULT_POLL_WAIT: Duration = Duration::from_secs(20);
const MAX_POLL_WAIT: Duration = Duration::from_secs(25);
/// The edge accepts results until 60 s after the deadline.
const RESULT_WINDOW: Duration = Duration::from_secs(60);
/// An ack this long after the deadline can only answer false: stop retrying.
const ACK_CUTOFF: Duration = Duration::from_secs(5);
/// The deadline span when the envelope's timestamps cannot be read, or when
/// `deadline - issuedAt` is negative or more than 600 s.
const DEFAULT_DEADLINE_SPAN: Duration = Duration::from_secs(10);
/// What `last_error` holds after a handler panicked. Nothing is posted for
/// that action: the edge ends the press as `actions/unknown`.
const HANDLER_PANICKED: &str =
    "game/handler_panicked: the action handler panicked; nothing was posted";
/// Finished results kept per receiver, to answer a repeated id again.
const FINISHED_KEPT: usize = 1_000;
const MAX_NAMES: usize = 16;
const MAX_CONCURRENCY: usize = 64;
/// The edge's limit of actions per poll answer.
const MAX_PER_POLL: usize = 8;

const MAX_USER_MESSAGE_CHARS: usize = 160;
const MAX_DETAIL_CHARS: usize = 500;
const MAX_DATA_BYTES: usize = 1024;

// ------------------------------------------------------------------ action

/// The player who pressed: their pairwise subject for this application and
/// environment (the same `gss_…` your game got at sign-in). Never a platform
/// user id, an email or a name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionActor {
    pub subject: String,
}

/// Where the press came from: public place ids.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionSource {
    pub club_id: Option<String>,
    pub channel_id: Option<String>,
    pub block_id: Option<String>,
}

/// One action, as delivered.
#[derive(Clone)]
pub struct Action {
    /// `gsa1.…`: the same on every delivery attempt of one press. Your
    /// durable dedupe key.
    pub id: String,
    pub name: String,
    pub version: u32,
    /// A JSON object of at most 4 KiB. Never trusted: check everything.
    pub payload: Value,
    pub actor: ActionActor,
    pub source: ActionSource,
    /// 1, or 2 on the single redelivery.
    pub attempt: u8,
    /// As sent, for logs only (the edge's clock).
    pub issued_at: String,
    /// As sent, for logs only (the edge's clock). Use [`Action::remaining`].
    pub deadline: String,
    deadline_at: Instant,
}

impl Action {
    /// Time left until the deadline hint, measured on this machine's
    /// monotonic clock from when the action arrived; zero once past. A hint
    /// of how long the person is likely to wait: running past it is allowed,
    /// and the result is still taken for 60 s more.
    pub fn remaining(&self) -> Duration {
        self.deadline_at.saturating_duration_since(Instant::now())
    }

    /// The deadline hint on this machine's monotonic clock.
    pub fn deadline_instant(&self) -> std::time::Instant {
        self.deadline_at.into_std()
    }

    /// The deadline as sent, in milliseconds since 1970 on the edge's clock,
    /// for display and logs only: the receiver never compares it with this
    /// machine's clock. `None` when unreadable.
    pub fn deadline_unix_ms(&self) -> Option<i64> {
        unix_ms(&self.deadline)
    }

    fn window_end(&self) -> Instant {
        self.deadline_at + RESULT_WINDOW
    }
}

impl fmt::Debug for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Action")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("version", &self.version)
            .field("attempt", &self.attempt)
            .field("remaining", &self.remaining())
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    action_id: String,
    name: String,
    version: u32,
    payload: Value,
    actor: EnvelopeActor,
    #[serde(default)]
    source: EnvelopeSource,
    issued_at: String,
    deadline: String,
    attempt: u8,
}

#[derive(Deserialize)]
struct EnvelopeActor {
    subject: String,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnvelopeSource {
    club_id: Option<String>,
    channel_id: Option<String>,
    block_id: Option<String>,
}

impl Envelope {
    /// The action, its deadline placed on the monotonic clock: arrival plus
    /// the envelope's own `deadline - issuedAt`.
    fn into_action(self, received_at: Instant) -> Action {
        let span = match (unix_ms(&self.issued_at), unix_ms(&self.deadline)) {
            (Some(issued), Some(deadline)) if (0..=600_000).contains(&(deadline - issued)) => {
                Duration::from_millis((deadline - issued) as u64)
            }
            _ => DEFAULT_DEADLINE_SPAN,
        };
        Action {
            id: self.action_id,
            name: self.name,
            version: self.version,
            payload: self.payload,
            actor: ActionActor {
                subject: self.actor.subject,
            },
            source: ActionSource {
                club_id: self.source.club_id,
                channel_id: self.source.channel_id,
                block_id: self.source.block_id,
            },
            attempt: self.attempt,
            issued_at: self.issued_at,
            deadline: self.deadline,
            deadline_at: received_at + span,
        }
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.fff]Z` as milliseconds since 1970; `None` otherwise.
/// Used only for differences between two edge timestamps.
pub fn unix_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || *bytes.last()? != b'Z' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let millis = match bytes[19] {
        b'Z' if bytes.len() == 20 => 0,
        b'.' => {
            let digits = text.get(20..text.len() - 1)?;
            if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            let padded = format!("{digits:0<3}");
            padded[..3].parse::<i64>().ok()?
        }
        _ => return None,
    };
    // Days from the civil date (proleptic Gregorian).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1_000 + millis)
}

// ------------------------------------------------------------------ reply

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplyStatus {
    Ok,
    Declined,
    Error,
}

/// What the handler answers. Builders never fail: a reply that breaks the
/// protocol's bounds (code shape, lengths, data size) is posted as `error`
/// with `game/invalid_reply`, and the receiver's `last_error` says why.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionReply {
    status: ReplyStatus,
    code: Option<String>,
    user_message: Option<String>,
    detail: Option<String>,
    data: Option<Value>,
}

impl ActionReply {
    /// Done. `data` is an object of at most 1 KiB the block may show, such as
    /// a new balance.
    pub fn ok(data: Value) -> Self {
        Self {
            status: ReplyStatus::Ok,
            code: None,
            user_message: None,
            detail: None,
            data: Some(data),
        }
    }

    /// Done, nothing to show.
    pub fn ok_empty() -> Self {
        Self {
            status: ReplyStatus::Ok,
            code: None,
            user_message: None,
            detail: None,
            data: None,
        }
    }

    /// Refused for a game reason. `code`: `game/snake_case`, 3–64 characters;
    /// `user_message`: plain text the person sees, at most 160 characters.
    pub fn declined(code: impl Into<String>, user_message: impl Into<String>) -> Self {
        Self {
            status: ReplyStatus::Declined,
            code: Some(code.into()),
            user_message: Some(user_message.into()),
            detail: None,
            data: None,
        }
    }

    /// Could not process it. `detail` (at most 500 characters) is shown to
    /// developers in the Dev Hub, never to the person.
    pub fn error(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: ReplyStatus::Error,
            code: Some(code.into()),
            user_message: None,
            detail: Some(detail.into()),
            data: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_user_message(mut self, message: impl Into<String>) -> Self {
        self.user_message = Some(message.into());
        self
    }

    pub fn is_ok(&self) -> bool {
        self.status == ReplyStatus::Ok
    }

    /// The result body, or the rule it breaks.
    fn body(&self) -> Result<Value, &'static str> {
        let mut body = json!({
            "status": match self.status {
                ReplyStatus::Ok => "ok",
                ReplyStatus::Declined => "declined",
                ReplyStatus::Error => "error",
            }
        });
        match &self.code {
            Some(code) if !valid_game_code(code) => {
                return Err("the code must be game/snake_case, 3-64 characters")
            }
            Some(code) => body["code"] = json!(code),
            None if self.status != ReplyStatus::Ok => {
                return Err("a declined or error reply needs a code")
            }
            None => {}
        }
        if let Some(message) = &self.user_message {
            if message.chars().count() > MAX_USER_MESSAGE_CHARS {
                return Err("the user message is longer than 160 characters");
            }
            body["userMessage"] = json!(message);
        }
        if let Some(detail) = &self.detail {
            if detail.chars().count() > MAX_DETAIL_CHARS {
                return Err("the detail is longer than 500 characters");
            }
            body["detail"] = json!(detail);
        }
        if let Some(data) = &self.data {
            let size = serde_json::to_vec(data).map_or(usize::MAX, |b| b.len());
            if !data.is_object() || size > MAX_DATA_BYTES {
                return Err("data must be a JSON object of at most 1 KiB");
            }
            body["data"] = data.clone();
        }
        Ok(body)
    }
}

/// `game/` and snake case, 3–64 characters in all.
pub fn valid_game_code(value: &str) -> bool {
    (3..=64).contains(&value.len())
        && value.strip_prefix("game/").is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

/// Letters, digits, `.` and `_`, 3–64 characters.
pub fn valid_action_name(value: &str) -> bool {
    (3..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
}

// ------------------------------------------------------------------ options

/// What to receive and how many handlers may run at once.
#[derive(Clone, Debug)]
pub struct ActionReceiverOptions {
    names: Vec<String>,
    concurrency: usize,
    poll_wait: Duration,
}

impl ActionReceiverOptions {
    /// The action names this server handles (enabled for the application in
    /// the Dev Hub). At most 16.
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            names: names.into_iter().map(Into::into).collect(),
            concurrency: 4,
            poll_wait: DEFAULT_POLL_WAIT,
        }
    }

    /// Handlers running at once, 1–64 (default 4).
    pub fn concurrency(mut self, handlers: usize) -> Self {
        self.concurrency = handlers;
        self
    }

    /// How long one poll waits at most, 1–25 s (default 20 s). The edge may
    /// cap it lower.
    pub fn poll_wait(mut self, wait: Duration) -> Self {
        self.poll_wait = wait;
        self
    }

    fn validated(mut self) -> Result<Self, Error> {
        self.names.sort();
        self.names.dedup();
        if self.names.is_empty()
            || self.names.len() > MAX_NAMES
            || !self.names.iter().all(|name| valid_action_name(name))
        {
            return Err(Error::InvalidInput {
                field: "names",
                reason: "1-16 action names: letters, digits, . and _, 3-64 characters",
            });
        }
        if !(1..=MAX_CONCURRENCY).contains(&self.concurrency) {
            return Err(Error::InvalidInput {
                field: "concurrency",
                reason: "must be 1-64",
            });
        }
        if self.poll_wait < Duration::from_secs(1) || self.poll_wait > MAX_POLL_WAIT {
            return Err(Error::InvalidInput {
                field: "poll_wait",
                reason: "must be 1-25 seconds",
            });
        }
        Ok(self)
    }
}

// ------------------------------------------------------------------ receiver

/// Counters since the receiver started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActionStats {
    /// Poll answers received.
    pub polls: u64,
    /// Actions delivered by the edge.
    pub received: u64,
    /// Acks the edge accepted: handlers that may start.
    pub accepted: u64,
    /// Acks the edge refused (too late, or another instance won).
    pub refused: u64,
    /// Past the deadline hint once accepted and given a handler slot:
    /// answered `game/expired`, not run.
    pub expired: u64,
    /// A repeated id while it was running, or already finished: not run again.
    pub coalesced: u64,
    pub results_posted: u64,
    /// Results the edge already had.
    pub results_duplicate: u64,
    /// Results the edge refused for good, or that ran out of time.
    pub results_failed: u64,
    /// Replies that broke the bounds and were posted as `game/invalid_reply`.
    pub invalid_replies: u64,
    /// Handlers running now (and their results not yet posted).
    pub running: u64,
}

type HandlerFuture = Pin<Box<dyn Future<Output = ActionReply> + Send>>;
type Handler = Arc<dyn Fn(Action) -> HandlerFuture + Send + Sync>;

struct Shared {
    client: Client,
    /// The receiver's own connection pool.
    http: Http,
    names: Vec<String>,
    poll_wait: Duration,
    instance_id: String,
    handler: Handler,
    handlers: Arc<Semaphore>,
    stopping: AtomicBool,
    stop: Notify,
    idle: Notify,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    stats: ActionStats,
    last_error: Option<Error>,
    /// Ids accepted and not yet finished.
    running: HashSet<String>,
    /// The last finished results, oldest first.
    finished: VecDeque<String>,
    results: HashMap<String, Value>,
}

impl Shared {
    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()))
    }

    fn record(&self, error: Error) {
        tracing::warn!(%error, "gridspawn action receiver");
        self.with(|state| state.last_error = Some(error));
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }
}

/// The background receiver. Dropping it stops polling at once (without
/// telling the edge); [`ActionReceiver::stop`] stops cleanly. Running handlers
/// are never cancelled either way.
pub struct ActionReceiver {
    shared: Arc<Shared>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Client {
    /// Starts receiving actions in the background (call it inside a tokio
    /// runtime). Each accepted action runs `handler` as its own task, with no
    /// time limit; its reply is posted once. A handler that panics posts
    /// nothing (the platform ends the press as `actions/unknown`), and
    /// [`ActionReceiver::last_error`] then names `game/handler_panicked`. See
    /// the module documentation for the dedupe your handler must do.
    pub fn spawn_action_receiver<F, Fut>(
        &self,
        options: ActionReceiverOptions,
        handler: F,
    ) -> Result<ActionReceiver, Error>
    where
        F: Fn(Action) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ActionReply> + Send + 'static,
    {
        let options = options.validated()?;
        let handler: Handler = Arc::new(move |action| Box::pin(handler(action)));
        let shared = Arc::new(Shared {
            client: self.clone(),
            http: self.fresh_http()?,
            names: options.names,
            poll_wait: options.poll_wait,
            instance_id: format!("inst-{}", uuid::Uuid::new_v4().simple()),
            handler,
            handlers: Arc::new(Semaphore::new(options.concurrency)),
            stopping: AtomicBool::new(false),
            stop: Notify::new(),
            idle: Notify::new(),
            state: Mutex::new(State::default()),
        });
        let task = tokio::spawn(poll_loop(shared.clone()));
        Ok(ActionReceiver {
            shared,
            task: Mutex::new(Some(task)),
        })
    }
}

impl ActionReceiver {
    /// The instance id this receiver polls with (random per receiver).
    pub fn instance_id(&self) -> &str {
        &self.shared.instance_id
    }

    pub fn stats(&self) -> ActionStats {
        self.shared.with(|state| {
            let mut stats = state.stats;
            stats.running = state.running.len() as u64;
            stats
        })
    }

    /// The last failure (poll, ack, result, an invalid reply or a panicked
    /// handler), or `None`.
    pub fn last_error(&self) -> Option<Error> {
        self.shared.with(|state| state.last_error.clone())
    }

    /// Stops polling, tells the edge this instance stopped, then waits up to
    /// `timeout` for running handlers and their results. Returns how many were
    /// still running; they are not cancelled and still post when they finish.
    pub async fn stop(&self, timeout: Duration) -> usize {
        self.shared.stopping.store(true, Ordering::SeqCst);
        self.shared.stop.notify_waiters();
        let task = self
            .task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
        let body = json!({ "instanceId": self.shared.instance_id });
        if let Err(error) = self
            .shared
            .client
            .edge_post_on::<Value>(
                &self.shared.http,
                "POST /v1/server/actions/stop",
                "/v1/server/actions/stop",
                &body,
                None,
            )
            .await
        {
            self.shared.record(error);
        }
        let until = Instant::now() + timeout;
        loop {
            let idle = self.shared.idle.notified();
            let running = self.shared.with(|state| state.running.len());
            if running == 0 {
                return 0;
            }
            if tokio::time::timeout_at(until, idle).await.is_err() {
                return self.shared.with(|state| state.running.len());
            }
        }
    }
}

impl Drop for ActionReceiver {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        self.shared.stop.notify_waiters();
    }
}

impl fmt::Debug for ActionReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActionReceiver")
            .field("instance_id", &self.shared.instance_id)
            .field("names", &self.shared.names)
            .field("stats", &self.stats())
            .finish()
    }
}

#[derive(Deserialize)]
struct PollAnswer {
    actions: Vec<Value>,
}

#[derive(Deserialize)]
struct AckAnswer {
    accepted: bool,
}

#[derive(Deserialize)]
struct ResultAnswer {
    #[serde(default)]
    duplicate: bool,
}

/// How long to wait after a failed poll.
fn poll_backoff(error: &Error, failures: u32) -> Duration {
    if let Some(api) = error.api() {
        if api.status == 429 {
            return api.retry_after.unwrap_or(Duration::from_secs(5));
        }
        if (400..500).contains(&api.status) {
            // Disabled, scope missing, credential refused: not soon.
            return Duration::from_secs(30);
        }
    }
    Duration::from_secs(1u64 << failures.min(5)).min(Duration::from_secs(30))
}

async fn poll_loop(shared: Arc<Shared>) {
    let mut failures = 0u32;
    loop {
        // Armed before the flag is read, so a stop between the two still wakes us.
        let stopped = shared.stop.notified();
        tokio::pin!(stopped);
        if shared.stopping() {
            break;
        }
        // Poll only with a free handler slot: wait for one, then take the
        // others free now. Each action of the answer gets one of them, so it
        // never waits for a slot after its ack.
        let first = tokio::select! {
            permit = shared.handlers.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                // Never closed.
                Err(_) => break,
            },
            () = &mut stopped => break,
        };
        let mut permits = vec![first];
        while permits.len() < MAX_PER_POLL {
            match shared.handlers.clone().try_acquire_owned() {
                Ok(permit) => permits.push(permit),
                Err(_) => break,
            }
        }
        let body = json!({
            "instanceId": shared.instance_id,
            "names": shared.names,
            "max": permits.len(),
            "waitSeconds": shared.poll_wait.as_secs(),
        });
        let poll = shared.client.edge_post_on::<PollAnswer>(
            &shared.http,
            "POST /v1/server/actions/poll",
            "/v1/server/actions/poll",
            &body,
            Some(shared.poll_wait + Duration::from_secs(10)),
        );
        let answer = tokio::select! {
            answer = poll => answer,
            () = &mut stopped => break,
        };
        let received_at = Instant::now();
        match answer {
            Ok(answer) => {
                failures = 0;
                shared.with(|state| state.stats.polls += 1);
                // More actions than asked for get no slot here and wait in `take`.
                let mut permits = permits.into_iter();
                for raw in answer.actions {
                    match serde_json::from_value::<Envelope>(raw) {
                        Ok(envelope) => {
                            let action = envelope.into_action(received_at);
                            shared.with(|state| state.stats.received += 1);
                            tokio::spawn(take(shared.clone(), action, permits.next()));
                        }
                        Err(_) => shared.record(Error::InvalidResponse(
                            "an action in the poll answer is not the expected JSON",
                        )),
                    }
                }
            }
            Err(error) => {
                drop(permits);
                failures = failures.saturating_add(1);
                let wait = poll_backoff(&error, failures);
                shared.record(error);
                let stopped = shared.stop.notified();
                if shared.stopping() {
                    break;
                }
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    () = stopped => break,
                }
            }
        }
    }
}

/// Confirms one action and, when accepted, runs or coalesces it. `permit` is
/// the handler slot the poll reserved for it (`None` when the edge sent more
/// than asked: it then waits for a slot).
async fn take(shared: Arc<Shared>, action: Action, permit: Option<OwnedSemaphorePermit>) {
    if !ack(&shared, &action).await {
        shared.with(|state| state.stats.refused += 1);
        return;
    }
    enum Next {
        Run,
        Coalesced,
        Repost(Value),
    }
    let next = shared.with(|state| {
        state.stats.accepted += 1;
        if let Some(body) = state.results.get(&action.id) {
            state.stats.coalesced += 1;
            Next::Repost(body.clone())
        } else if state.running.contains(&action.id) {
            state.stats.coalesced += 1;
            Next::Coalesced
        } else {
            state.running.insert(action.id.clone());
            Next::Run
        }
    });
    match next {
        Next::Coalesced => return,
        Next::Repost(body) => {
            drop(permit);
            // Null: the handler panicked and nothing was posted the first time.
            if !body.is_null() {
                post_result(&shared, &action, &body).await;
            }
            return;
        }
        Next::Run => {}
    }
    let permit = match permit {
        Some(permit) => permit,
        None => match shared.handlers.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => unreachable!("the handler semaphore is never closed"),
        },
    };
    // Checked with the slot in hand: an action that waited past its deadline
    // for a slot is not run either.
    let body = if action.remaining().is_zero() {
        drop(permit);
        shared.with(|state| state.stats.expired += 1);
        Some(json!({"status": "error", "code": "game/expired",
                    "detail": "accepted after the deadline hint; not run"}))
    } else {
        let handler = shared.handler.clone();
        let input = action.clone();
        // Called inside the task, so a panic while the handler builds its
        // future is caught as well.
        let outcome = tokio::spawn(async move { handler(input).await }).await;
        drop(permit);
        match outcome {
            Ok(reply) => Some(match reply.body() {
                Ok(body) => body,
                Err(rule) => {
                    shared.with(|state| state.stats.invalid_replies += 1);
                    shared.record(Error::InvalidInput {
                        field: "action reply",
                        reason: rule,
                    });
                    json!({"status": "error", "code": "game/invalid_reply", "detail": rule})
                }
            }),
            // A panic posts nothing: the edge ends the press as
            // `actions/unknown` after the window. Only last_error says why.
            Err(_) => {
                shared.record(Error::InvalidInput {
                    field: "action handler",
                    reason: HANDLER_PANICKED,
                });
                None
            }
        }
    };
    shared.with(|state| {
        let kept = body.clone().unwrap_or(Value::Null);
        state.results.insert(action.id.clone(), kept);
        state.finished.push_back(action.id.clone());
        while state.finished.len() > FINISHED_KEPT {
            if let Some(old) = state.finished.pop_front() {
                state.results.remove(&old);
            }
        }
    });
    if let Some(body) = &body {
        post_result(&shared, &action, body).await;
    }
    shared.with(|state| state.running.remove(&action.id));
    shared.idle.notify_waiters();
}

/// The ack, retried on network errors, timeouts, 5xx and 429 until the edge
/// answers, or until 5 s past the deadline (a later ack can only be refused).
async fn ack(shared: &Shared, action: &Action) -> bool {
    let body = json!({
        "actionId": action.id,
        "attempt": action.attempt,
        "instanceId": shared.instance_id,
    });
    let cutoff = action.deadline_at + ACK_CUTOFF;
    let mut attempt = 1u32;
    loop {
        match shared
            .client
            .edge_post_on::<AckAnswer>(
                &shared.http,
                "POST /v1/server/actions/ack",
                "/v1/server/actions/ack",
                &body,
                None,
            )
            .await
        {
            Ok(answer) => return answer.accepted,
            Err(error) if error.is_retryable() && Instant::now() < cutoff => {
                let wait = error
                    .api()
                    .and_then(|api| api.retry_after)
                    .unwrap_or_else(|| Duration::from_millis(100 * u64::from(attempt.min(10))));
                shared.record(error);
                tokio::time::sleep(wait.min(cutoff.saturating_duration_since(Instant::now())))
                    .await;
                attempt += 1;
            }
            Err(error) => {
                shared.record(error);
                return false;
            }
        }
    }
}

/// Posts the result, retried until the edge answers or the window closes.
async fn post_result(shared: &Shared, action: &Action, body: &Value) {
    let path = format!("/v1/server/actions/{}/result", segment(&action.id));
    let window_end = action.window_end();
    let mut attempt = 1u32;
    loop {
        match shared
            .client
            .edge_post_on::<ResultAnswer>(
                &shared.http,
                "POST /v1/server/actions/{actionId}/result",
                &path,
                body,
                None,
            )
            .await
        {
            Ok(answer) => {
                shared.with(|state| {
                    if answer.duplicate {
                        state.stats.results_duplicate += 1;
                    } else {
                        state.stats.results_posted += 1;
                    }
                });
                return;
            }
            Err(error) if error.is_retryable() && Instant::now() < window_end => {
                let wait = error
                    .api()
                    .and_then(|api| api.retry_after)
                    .unwrap_or_else(|| Duration::from_millis(250 * u64::from(attempt.min(20))));
                shared.record(error);
                tokio::time::sleep(wait.min(window_end.saturating_duration_since(Instant::now())))
                    .await;
                attempt += 1;
            }
            Err(error) => {
                shared.with(|state| state.stats.results_failed += 1);
                shared.record(error);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn edge_timestamps_read_as_unix_milliseconds() {
        assert_eq!(unix_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(unix_ms("1970-01-01T00:00:01Z"), Some(1_000));
        assert_eq!(unix_ms("2000-01-01T00:00:00.000Z"), Some(946_684_800_000));
        assert_eq!(unix_ms("2000-03-01T00:00:00.5Z"), Some(951_868_800_500));
        assert_eq!(
            unix_ms("2026-11-04T10:00:10.000Z").unwrap()
                - unix_ms("2026-11-04T10:00:00.000Z").unwrap(),
            10_000
        );
        for bad in [
            "",
            "2026-11-04 10:00:00Z",
            "2026-13-04T10:00:00Z",
            "2026-11-04T10:00:00+01:00",
            "x",
        ] {
            assert_eq!(unix_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_deadline_is_placed_on_the_monotonic_clock() {
        let envelope = |issued: &str, deadline: &str| Envelope {
            action_id: "gsa1.aaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            name: "shop.buy".into(),
            version: 1,
            payload: json!({}),
            actor: EnvelopeActor {
                subject: "gss_x".into(),
            },
            source: EnvelopeSource::default(),
            issued_at: issued.into(),
            deadline: deadline.into(),
            attempt: 1,
        };
        let now = Instant::now();
        // Hours off any real clock: only the difference counts.
        let action =
            envelope("1999-01-01T00:00:00.000Z", "1999-01-01T00:00:07.000Z").into_action(now);
        assert_eq!(action.deadline_at, now + Duration::from_secs(7));
        let unreadable = envelope("yesterday", "today").into_action(now);
        assert_eq!(unreadable.deadline_at, now + DEFAULT_DEADLINE_SPAN);
        let backwards =
            envelope("2026-11-04T10:00:10.000Z", "2026-11-04T10:00:00.000Z").into_action(now);
        assert_eq!(backwards.deadline_at, now + DEFAULT_DEADLINE_SPAN);
    }

    #[test]
    fn replies_are_checked_before_posting() {
        assert_eq!(
            ActionReply::ok(json!({"balance": 1})).body().unwrap()["data"]["balance"],
            1
        );
        assert_eq!(
            ActionReply::ok_empty().body().unwrap(),
            json!({"status": "ok"})
        );
        let declined = ActionReply::declined("game/not_enough_gold", "Not enough gold.")
            .with_detail("needs 30");
        assert_eq!(
            declined.body().unwrap(),
            json!({"status": "declined", "code": "game/not_enough_gold",
                   "userMessage": "Not enough gold.", "detail": "needs 30"})
        );
        assert!(ActionReply::declined("not_enough_gold", "x")
            .body()
            .is_err());
        assert!(ActionReply::declined("game/x", "y".repeat(161))
            .body()
            .is_err());
        assert!(ActionReply::error("game/db", "z".repeat(501))
            .body()
            .is_err());
        assert!(ActionReply::ok(json!([1])).body().is_err());
        assert!(ActionReply::ok(json!({"x": "a".repeat(1024)}))
            .body()
            .is_err());
    }

    #[test]
    fn options_are_bounded() {
        assert!(ActionReceiverOptions::new(["shop.buy", "shop.buy"])
            .validated()
            .is_ok());
        assert!(ActionReceiverOptions::new(Vec::<String>::new())
            .validated()
            .is_err());
        assert!(ActionReceiverOptions::new(["shop buy"])
            .validated()
            .is_err());
        assert!(ActionReceiverOptions::new(["shop.buy"])
            .concurrency(0)
            .validated()
            .is_err());
        assert!(ActionReceiverOptions::new(["shop.buy"])
            .concurrency(65)
            .validated()
            .is_err());
        assert!(ActionReceiverOptions::new(["shop.buy"])
            .poll_wait(Duration::from_secs(26))
            .validated()
            .is_err());
    }
}
