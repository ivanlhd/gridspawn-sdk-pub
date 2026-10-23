//! Live game data for the game's club pages (protocol 06): documents the game
//! server keeps current, and events. Data belongs to the application and
//! environment, so no game session is needed.
//!
//! **Club pages are public.** Publish display names only, never emails,
//! account or participant ids, or anything private. The service refuses
//! email shapes and Gridspawn id prefixes ([`ApiError::is_personal_data`]), but
//! that check is a safety net, not a filter to rely on.
//!
//! **One writer per key.** Each `set` and `remove` entry carries a sequence
//! number from one in-process counter per [`Client`]; the service skips an
//! entry that is not newer than the key's stored one. Game-server processes
//! that run side by side write disjoint keys (for example under a `shard-eu.`
//! prefix). No clock is read anywhere: on the first write the counter is
//! seeded above every stored sequence number (the writer's listing), and an
//! entry that is still skipped is sent once more above the stored number; if
//! that is skipped too, another writer owns the key ([`DataAck::skipped`]).

use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::sync::OnceCell;

use crate::{
    client::Client,
    error::{ApiError, Error, ErrorCode},
    types::IdempotencyKey,
};

/// The service keeps sequence numbers exact up to 2⁵³ − 1.
const MAX_SEQ: u64 = (1 << 53) - 1;
const MAX_ENTRIES: usize = 40;
const MAX_BODY: usize = 32 * 1024;
const MAX_VALUE: usize = 4 * 1024;
const MAX_PAYLOAD: usize = 4 * 1024;
const MAX_DEPTH: usize = 8;
const MIN_TTL: u64 = 10;
const MAX_TTL: u64 = 86_400;
/// Entries a second the service allows per application and environment.
const ENTRIES_PER_SECOND: u32 = 20;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn invalid(field: &'static str, reason: &'static str) -> Error {
    Error::InvalidInput { field, reason }
}

/// `^[a-z][a-z0-9_.-]{0,63}$`
fn valid_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        })
}

/// `^[a-z0-9][a-z0-9_.-]{0,47}$`
fn valid_event_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 48
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        })
}

/// `[A-Za-z0-9_-]{8,64}`
fn valid_event_id(id: &str) -> bool {
    (8..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// Container nesting (an object or array counts one level, scalars none).
/// Counting containers only keeps the SDK from refusing anything the service
/// would accept; the service stays the authority.
fn depth(value: &Value) -> usize {
    match value {
        Value::Array(items) => 1 + items.iter().map(depth).max().unwrap_or(0),
        Value::Object(map) => 1 + map.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

fn check_key(key: &str) -> Result<(), Error> {
    if valid_key(key) {
        Ok(())
    } else {
        Err(invalid(
            "game data key",
            "1-64 characters of a-z 0-9 _ . - starting with a letter",
        ))
    }
}

fn check_value(value: &Value) -> Result<(), Error> {
    let size = serde_json::to_vec(value)
        .map_err(|_| invalid("game data value", "could not be encoded as JSON"))?
        .len();
    if size > MAX_VALUE {
        return Err(invalid("game data value", "at most 4 KiB as JSON"));
    }
    if depth(value) > MAX_DEPTH {
        return Err(invalid("game data value", "nested at most 8 levels deep"));
    }
    Ok(())
}

fn ttl_seconds(ttl: Duration) -> Result<u64, Error> {
    let seconds = ttl.as_secs();
    if ttl.subsec_nanos() != 0 || !(MIN_TTL..=MAX_TTL).contains(&seconds) {
        return Err(invalid(
            "game data ttl",
            "whole seconds from 10 to 86400 (24 hours)",
        ));
    }
    Ok(seconds)
}

/// 22 characters of `[A-Za-z0-9_-]`: a random UUID, base64url without padding.
fn generated_event_id() -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let mut out = String::with_capacity(22);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

impl ApiError {
    /// The service refused a value, key or event name that looked like
    /// personal data (an email, or a Gridspawn id or credential); `field` is
    /// the JSON path, for example `set.scoreboard.value.players[3].name`.
    pub fn is_personal_data(&self) -> bool {
        self.code == ErrorCode::GameDataInvalidValue
            && self.reason.as_deref() == Some("personal_data")
    }
}

/// One `set` or `remove` in a [`DataBatch`].
#[derive(Clone, Debug, PartialEq)]
enum Entry {
    Set { value: Value, ttl: Option<u64> },
    Remove,
}

/// Several documents set or removed in one all-or-nothing request (1–40
/// entries). A key listed twice keeps its last entry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DataBatch {
    entries: Vec<(String, Entry)>,
}

impl DataBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(mut self, key: impl Into<String>, value: Value) -> Self {
        self.put(key.into(), Entry::Set { value, ttl: None });
        self
    }

    /// The document disappears for readers `ttl` after this write (whole
    /// seconds, 10 s to 24 h).
    pub fn set_with_ttl(mut self, key: impl Into<String>, value: Value, ttl: Duration) -> Self {
        // Checked in `set_many`, so the builder stays infallible.
        let ttl = Some(if ttl.subsec_nanos() == 0 {
            ttl.as_secs()
        } else {
            0
        });
        self.put(key.into(), Entry::Set { value, ttl });
        self
    }

    pub fn remove(mut self, key: impl Into<String>) -> Self {
        self.put(key.into(), Entry::Remove);
        self
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn put(&mut self, key: String, entry: Entry) {
        self.entries.retain(|(k, _)| *k != key);
        self.entries.push((key, entry));
    }

    fn validate(&self) -> Result<(), Error> {
        if self.entries.is_empty() || self.entries.len() > MAX_ENTRIES {
            return Err(invalid("game data batch", "1-40 entries"));
        }
        for (key, entry) in &self.entries {
            check_key(key)?;
            if let Entry::Set { value, ttl } = entry {
                check_value(value)?;
                if let Some(ttl) = ttl {
                    if !(MIN_TTL..=MAX_TTL).contains(ttl) {
                        return Err(invalid(
                            "game data ttl",
                            "whole seconds from 10 to 86400 (24 hours)",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// The service's quota use for the application and environment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DataQuota {
    pub keys: u64,
    pub max_keys: u64,
    pub bytes: u64,
    pub max_bytes: u64,
}

/// A key the service did not change, with its stored sequence number.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct SkippedKey {
    pub key: String,
    pub seq: u64,
}

/// The answer to a write.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DataAck {
    /// The keys that were stored (or removed).
    pub accepted: Vec<String>,
    /// Keys still skipped after one resend above their stored sequence
    /// number: another writer owns them. Empty when every key is yours.
    pub skipped: Vec<SkippedKey>,
    pub quota: DataQuota,
}

/// One key of the writer's listing. No values.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DataKey {
    pub key: String,
    pub seq: u64,
    pub bytes: u64,
    /// Time left before the document expires, as the service reported it.
    pub ttl_remaining: Option<Duration>,
    /// The service's time of the last change (RFC 3339), for information only.
    pub updated_at: String,
    /// A removal kept for 24 h (a tombstone).
    pub removed: bool,
}

/// The writer's listing (`GET /v1/server/game-data`). Limited to 10 per 10 s:
/// for start-up and checks, not for polling.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DataListing {
    pub keys: Vec<DataKey>,
    pub quota: DataQuota,
}

/// The answer to an event.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EventReceipt {
    /// The service's sequence number for the event, as a string.
    pub id: String,
    /// The same `Idempotency-Key` was already used within 24 h; this is the
    /// first answer again.
    pub duplicate: bool,
}

#[derive(Deserialize)]
struct WriteResponse {
    ok: bool,
    accepted: Vec<String>,
    #[serde(default)]
    skipped: Vec<SkippedKey>,
    quota: DataQuota,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListedKey {
    key: String,
    seq: u64,
    bytes: u64,
    ttl_remaining: Option<f64>,
    updated_at: String,
    #[serde(default)]
    removed: bool,
}

#[derive(Deserialize)]
struct ListResponse {
    keys: Vec<ListedKey>,
    quota: DataQuota,
}

#[derive(Deserialize)]
struct EventResponse {
    ok: bool,
    id: String,
    duplicate: bool,
}

/// The writer's state, shared by every [`Game`] of a [`Client`]: the
/// sequence counter, seeded once from the listing.
#[derive(Debug, Default)]
pub(crate) struct GameShared {
    seeded: OnceCell<()>,
    last_seq: Mutex<u64>,
}

impl GameShared {
    /// The next sequence number (above everything sent or seen).
    fn next_seq(&self) -> Result<u64, Error> {
        let mut last = lock(&self.last_seq);
        if *last >= MAX_SEQ {
            return Err(invalid(
                "game data sequence",
                "the stored sequence numbers reached 2^53",
            ));
        }
        *last += 1;
        Ok(*last)
    }

    /// Never below `seq` from now on.
    fn raise_to(&self, seq: u64) {
        let mut last = lock(&self.last_seq);
        *last = (*last).max(seq);
    }
}

impl Client {
    /// The game's live data and events for this application and environment.
    /// Every `Game` of one `Client` is the same writer.
    pub fn game(&self) -> Game {
        Game {
            client: self.clone(),
        }
    }
}

/// The game's live data and events. Cheap to clone; clones (and every
/// [`Client::game`] of one client) share one writer. Needs the
/// `game_data.write` scope and game data switched on for the application and
/// environment in the Dev Hub.
#[derive(Clone, Debug)]
pub struct Game {
    client: Client,
}

impl Game {
    pub fn data(&self) -> GameData {
        GameData { game: self.clone() }
    }

    pub fn events(&self) -> GameEvents {
        GameEvents { game: self.clone() }
    }

    /// Sends the latest value of every key given to [`DataPublisher::set`],
    /// `hz` times a second (clamped to 1–5), on the current tokio runtime.
    /// Stops when the handle is dropped.
    pub fn spawn_data_publisher(&self, hz: u32) -> DataPublisher {
        DataPublisher::spawn(self.data(), hz)
    }

    fn shared(&self) -> &GameShared {
        &self.client.inner.game
    }

    /// Seeds the counter from the listing once (single-flight; a failure
    /// leaves it unseeded, and the next write tries again).
    async fn seed(&self) -> Result<(), Error> {
        self.shared()
            .seeded
            .get_or_try_init(|| async {
                let listing = self.data().list().await?;
                if let Some(max) = listing.keys.iter().map(|k| k.seq).max() {
                    self.shared().raise_to(max);
                }
                Ok::<(), Error>(())
            })
            .await
            .map(|_| ())
    }
}

/// Documents: named JSON values the game keeps current.
#[derive(Clone, Debug)]
pub struct GameData {
    game: Game,
}

impl GameData {
    pub async fn set(&self, key: &str, value: Value) -> Result<DataAck, Error> {
        self.set_many(&DataBatch::new().set(key, value)).await
    }

    /// As [`Self::set`]; the document disappears for readers `ttl` after this
    /// write (whole seconds, 10 s to 24 h).
    pub async fn set_with_ttl(
        &self,
        key: &str,
        value: Value,
        ttl: Duration,
    ) -> Result<DataAck, Error> {
        ttl_seconds(ttl)?;
        self.set_many(&DataBatch::new().set_with_ttl(key, value, ttl))
            .await
    }

    /// Removes a document. The service keeps the removal for 24 h, so an
    /// older delayed write cannot bring the key back.
    pub async fn remove(&self, key: &str) -> Result<DataAck, Error> {
        self.set_many(&DataBatch::new().remove(key)).await
    }

    /// One all-or-nothing request of 1–40 entries. Each entry gets the next
    /// sequence number; entries the service skips are sent once more above
    /// their stored number (see [`DataAck::skipped`]).
    pub async fn set_many(&self, batch: &DataBatch) -> Result<DataAck, Error> {
        batch.validate()?;
        self.game.seed().await?;
        let shared = self.game.shared();
        let mut sent = Vec::with_capacity(batch.entries.len());
        for (key, entry) in &batch.entries {
            sent.push((key.clone(), entry.clone(), shared.next_seq()?));
        }
        let first = self.write(&sent).await?;
        let mut ack = DataAck {
            accepted: first.accepted,
            skipped: Vec::new(),
            quota: first.quota,
        };
        // Skipped with our own number: an earlier attempt of this request
        // landed (a retry). Otherwise the stored number is higher: resend
        // once above it.
        let mut resend = Vec::new();
        for skipped in first.skipped {
            let Some((key, entry, seq)) = sent.iter().find(|(k, ..)| *k == skipped.key) else {
                continue;
            };
            if skipped.seq == *seq {
                ack.accepted.push(skipped.key);
            } else {
                shared.raise_to(skipped.seq);
                resend.push((key.clone(), entry.clone()));
            }
        }
        if !resend.is_empty() {
            let mut again = Vec::with_capacity(resend.len());
            for (key, entry) in resend {
                again.push((key, entry, shared.next_seq()?));
            }
            let second = self.write(&again).await?;
            ack.accepted.extend(second.accepted);
            ack.quota = second.quota;
            for skipped in second.skipped {
                let own = again
                    .iter()
                    .any(|(k, _, seq)| *k == skipped.key && *seq == skipped.seq);
                if own {
                    ack.accepted.push(skipped.key);
                } else {
                    shared.raise_to(skipped.seq);
                    tracing::warn!(
                        key = %skipped.key,
                        "gridspawn game data key is owned by another writer (one writer per key)"
                    );
                    ack.skipped.push(skipped);
                }
            }
        }
        Ok(ack)
    }

    /// The writer's listing: every key with its sequence number, size, time
    /// left and removals of the last 24 h, and the quota. No values. Limited
    /// to 10 per 10 s: for start-up and checks, not for polling.
    pub async fn list(&self) -> Result<DataListing, Error> {
        let response: ListResponse = self
            .game
            .client
            .edge_send(
                Method::GET,
                "GET /v1/server/game-data",
                "/v1/server/game-data",
                None,
                None,
                true,
            )
            .await?;
        let mut keys = Vec::with_capacity(response.keys.len());
        for listed in response.keys {
            let ttl_remaining = match listed.ttl_remaining {
                Some(seconds) if seconds.is_finite() && seconds >= 0.0 => {
                    Some(Duration::from_secs_f64(seconds))
                }
                Some(_) => return Err(Error::InvalidResponse("invalid game data listing")),
                None => None,
            };
            keys.push(DataKey {
                key: listed.key,
                seq: listed.seq,
                bytes: listed.bytes,
                ttl_remaining,
                updated_at: listed.updated_at,
                removed: listed.removed,
            });
        }
        Ok(DataListing {
            keys,
            quota: response.quota,
        })
    }

    async fn write(&self, entries: &[(String, Entry, u64)]) -> Result<WriteResponse, Error> {
        let mut set = Map::new();
        let mut remove = Vec::new();
        for (key, entry, seq) in entries {
            match entry {
                Entry::Set { value, ttl } => {
                    let mut item = json!({ "value": value, "seq": seq });
                    if let Some(ttl) = ttl {
                        item["ttlSeconds"] = json!(ttl);
                    }
                    set.insert(key.clone(), item);
                }
                Entry::Remove => remove.push(json!({ "key": key, "seq": seq })),
            }
        }
        let mut body = Map::new();
        if !set.is_empty() {
            body.insert("set".into(), Value::Object(set));
        }
        if !remove.is_empty() {
            body.insert("remove".into(), Value::Array(remove));
        }
        let body = Value::Object(body);
        let size = serde_json::to_vec(&body)
            .map_err(|_| invalid("game data batch", "could not be encoded as JSON"))?
            .len();
        if size > MAX_BODY {
            return Err(invalid("game data batch", "at most 32 KiB as JSON"));
        }
        // Each entry carries its sequence number: retrying is safe.
        let response: WriteResponse = self
            .game
            .client
            .edge_send(
                Method::PUT,
                "PUT /v1/server/game-data",
                "/v1/server/game-data",
                Some(&body),
                None,
                true,
            )
            .await?;
        if !response.ok {
            return Err(Error::InvalidResponse("invalid game data answer"));
        }
        Ok(response)
    }
}

/// Events: an append-only stream of `{name, payload}`.
#[derive(Clone, Debug)]
pub struct GameEvents {
    game: Game,
}

impl GameEvents {
    /// Sends an event with a generated `Idempotency-Key`, so the crate's own
    /// retries are not counted twice.
    pub async fn emit(&self, name: &str, payload: Value) -> Result<EventReceipt, Error> {
        self.send(name, payload, generated_event_id()).await
    }

    /// As [`Self::emit`] with your own id (`[A-Za-z0-9_-]{8,64}`), for an
    /// event that may be sent again after a restart: a repeat within 24 h
    /// returns the first answer with `duplicate: true`.
    pub async fn emit_with_id(
        &self,
        name: &str,
        payload: Value,
        id: &str,
    ) -> Result<EventReceipt, Error> {
        if !valid_event_id(id) {
            return Err(invalid("event id", "8-64 characters of A-Z a-z 0-9 _ -"));
        }
        self.send(name, payload, id.to_owned()).await
    }

    async fn send(&self, name: &str, payload: Value, id: String) -> Result<EventReceipt, Error> {
        if !valid_event_name(name) {
            return Err(invalid(
                "event name",
                "1-48 characters of a-z 0-9 _ . - starting with a letter or digit",
            ));
        }
        if !payload.is_object() {
            return Err(invalid("event payload", "a JSON object"));
        }
        let size = serde_json::to_vec(&payload)
            .map_err(|_| invalid("event payload", "could not be encoded as JSON"))?
            .len();
        if size > MAX_PAYLOAD {
            return Err(invalid("event payload", "at most 4 KiB as JSON"));
        }
        let key = IdempotencyKey::new(id)?;
        let response: EventResponse = self
            .game
            .client
            .edge_post(
                "POST /v1/server/game-events",
                "/v1/server/game-events",
                &json!({ "name": name, "payload": payload }),
                Some(&key),
                true,
            )
            .await?;
        if !response.ok || response.id.is_empty() {
            return Err(Error::InvalidResponse("invalid game event answer"));
        }
        Ok(EventReceipt {
            id: response.id,
            duplicate: response.duplicate,
        })
    }
}

#[derive(Clone, Debug)]
struct Pending {
    entry: Entry,
    /// Local change order (not a clock): oldest change goes first.
    order: u64,
}

#[derive(Default)]
struct PublisherState {
    pending: HashMap<String, Pending>,
    next_order: u64,
    skipped: BTreeSet<String>,
    last_error: Option<Error>,
}

impl PublisherState {
    fn put(&mut self, key: String, entry: Entry) {
        self.next_order += 1;
        let order = self.next_order;
        self.pending.insert(key, Pending { entry, order });
    }
}

/// Coalesces values to the latest per key and sends the changed keys, oldest
/// change first, in one batch per tick. Stops when dropped.
pub struct DataPublisher {
    state: Arc<Mutex<PublisherState>>,
    task: tokio::task::JoinHandle<()>,
}

impl DataPublisher {
    fn spawn(data: GameData, hz: u32) -> Self {
        let hz = hz.clamp(1, 5);
        let period = Duration::from_secs_f64(1.0 / f64::from(hz));
        // At most 20 entries a second overall, and never more than a batch.
        let per_tick = ((ENTRIES_PER_SECOND / hz) as usize).clamp(4, MAX_ENTRIES);
        let shared = Arc::new(Mutex::new(PublisherState::default()));
        let state = shared.clone();
        let task = tokio::spawn(async move {
            let mut ticks = tokio::time::interval(period);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                let taken = take_batch(&state, per_tick);
                if taken.is_empty() {
                    continue;
                }
                let mut batch = DataBatch::new();
                for (key, pending) in &taken {
                    batch.entries.push((key.clone(), pending.entry.clone()));
                }
                match data.set_many(&batch).await {
                    Ok(ack) => {
                        let mut s = lock(&state);
                        s.last_error = None;
                        for key in &ack.accepted {
                            s.skipped.remove(key);
                        }
                        for skipped in ack.skipped {
                            s.skipped.insert(skipped.key);
                        }
                    }
                    Err(error) => {
                        let wait = error.api().and_then(|api| {
                            (api.code == ErrorCode::RateLimited)
                                .then(|| api.retry_after.unwrap_or(Duration::from_secs(1)))
                        });
                        tracing::warn!(%error, "gridspawn game data not sent");
                        {
                            let mut s = lock(&state);
                            if error.is_retryable() {
                                // Put back what no newer value replaced.
                                for (key, pending) in taken {
                                    s.pending.entry(key).or_insert(pending);
                                }
                            }
                            s.last_error = Some(error);
                        }
                        if let Some(wait) = wait {
                            tokio::time::sleep(wait).await;
                        }
                    }
                }
            }
        });
        Self {
            state: shared,
            task,
        }
    }

    /// The key's latest value (cheap; call every frame). Checked here.
    pub fn set(&self, key: &str, value: Value) -> Result<(), Error> {
        check_key(key)?;
        check_value(&value)?;
        lock(&self.state).put(key.to_owned(), Entry::Set { value, ttl: None });
        Ok(())
    }

    /// As [`Self::set`], with a TTL (whole seconds, 10 s to 24 h).
    pub fn set_with_ttl(&self, key: &str, value: Value, ttl: Duration) -> Result<(), Error> {
        check_key(key)?;
        check_value(&value)?;
        let ttl = ttl_seconds(ttl)?;
        lock(&self.state).put(
            key.to_owned(),
            Entry::Set {
                value,
                ttl: Some(ttl),
            },
        );
        Ok(())
    }

    /// Sends a removal on the next tick, and stops sending the key. An
    /// invalid key is ignored (it was never sent).
    pub fn remove(&self, key: &str) {
        if valid_key(key) {
            lock(&self.state).put(key.to_owned(), Entry::Remove);
        }
    }

    /// Keys another writer owns (still skipped after one resend).
    pub fn skipped(&self) -> Vec<String> {
        lock(&self.state).skipped.iter().cloned().collect()
    }

    /// The failure of the last batch, if it failed.
    pub fn last_error(&self) -> Option<Error> {
        lock(&self.state).last_error.clone()
    }

    pub fn stop(self) {}
}

const MAX_MARKERS: usize = 64;
const MAX_MARKER_KIND: usize = 32;

/// One marker of a [`Markers`] document (protocol 06 §2.3).
#[derive(Clone, Debug, PartialEq)]
pub struct Marker {
    id: String,
    label: String,
    x: f64,
    y: f64,
    kind: Option<String>,
}

impl Marker {
    /// `id`: stable for this marker across updates. `label`: public text
    /// (display names only). `x`, `y`: the game's own world units, in one
    /// stable coordinate space.
    pub fn new(id: impl Into<String>, label: impl Into<String>, x: f64, y: f64) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            x,
            y,
            kind: None,
        }
    }

    /// Free text (at most 32 characters), for example `objective`.
    pub fn kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }
}

/// A set of markers that club pages can draw on a "Game markers" layer
/// (protocol 06 §2.3). Send the whole set with every update; an empty set
/// shows nothing.
///
/// ```
/// use gridspawn_server::{Marker, Markers};
/// let markers = Markers::new()
///     .push(Marker::new("flag-red", "Red flag", 120.5, -40.0).kind("objective"))
///     .push(Marker::new("p-17", "Ana", 98.0, 12.25));
/// let value = markers.to_value().unwrap();   // game.data().set("map.live", value)
/// assert_eq!(value["markers"][0]["kind"], "objective");
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Markers {
    markers: Vec<Marker>,
}

impl Markers {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(mut self, marker: Marker) -> Self {
        self.markers.push(marker);
        self
    }

    pub fn len(&self) -> usize {
        self.markers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.markers.is_empty()
    }

    /// The document's value, checked here: at most 64 markers, a non-empty
    /// id and label, finite coordinates, a kind of at most 32 characters, and
    /// at most 4 KiB as JSON (about 60 bytes per marker when full).
    pub fn to_value(&self) -> Result<Value, Error> {
        if self.markers.len() > MAX_MARKERS {
            return Err(invalid("markers", "at most 64 markers"));
        }
        let mut list = Vec::with_capacity(self.markers.len());
        for marker in &self.markers {
            if marker.id.is_empty() {
                return Err(invalid("marker id", "not empty"));
            }
            if marker.label.is_empty() {
                return Err(invalid("marker label", "not empty"));
            }
            if !marker.x.is_finite() || !marker.y.is_finite() {
                return Err(invalid("marker position", "finite numbers"));
            }
            let mut entry = json!({
                "id": marker.id, "label": marker.label, "x": marker.x, "y": marker.y,
            });
            if let Some(kind) = &marker.kind {
                if kind.chars().count() > MAX_MARKER_KIND {
                    return Err(invalid("marker kind", "at most 32 characters"));
                }
                entry["kind"] = json!(kind);
            }
            list.push(entry);
        }
        let value = json!({ "markers": list });
        check_value(&value).map_err(|_| {
            invalid(
                "markers",
                "at most 4 KiB as JSON: keep ids and labels short",
            )
        })?;
        Ok(value)
    }
}

impl DataPublisher {
    /// As [`Self::set`] with a [`Markers`] set.
    pub fn set_markers(&self, key: &str, markers: &Markers) -> Result<(), Error> {
        self.set(key, markers.to_value()?)
    }
}

/// Takes up to `limit` pending entries, oldest change first, within 32 KiB.
fn take_batch(state: &Mutex<PublisherState>, limit: usize) -> Vec<(String, Pending)> {
    let mut s = lock(state);
    let mut order: Vec<(u64, String)> = s
        .pending
        .iter()
        .map(|(key, pending)| (pending.order, key.clone()))
        .collect();
    order.sort();
    let mut taken = Vec::new();
    // Room for {"set":{},"remove":[]} and per-entry overhead.
    let mut size = 32;
    for (_, key) in order {
        if taken.len() == limit {
            break;
        }
        let pending = &s.pending[&key];
        // A generous estimate of the entry's JSON (quotes, "value", "seq",
        // "ttlSeconds" and their numbers), so a batch never reaches the
        // service over 32 KiB.
        let entry_size = key.len()
            + 96
            + match &pending.entry {
                Entry::Set { value, .. } => serde_json::to_vec(value).map_or(0, |v| v.len()),
                Entry::Remove => 0,
            };
        if size + entry_size > MAX_BODY && !taken.is_empty() {
            break;
        }
        size += entry_size;
        let pending = s.pending.remove(&key).expect("listed key");
        taken.push((key, pending));
    }
    taken
}

impl Drop for DataPublisher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl std::fmt::Debug for DataPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = lock(&self.state);
        f.debug_struct("DataPublisher")
            .field("pending", &s.pending.len())
            .field("skipped", &s.skipped.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        for key in ["a", "match.score", "shard-eu.players_2", &"a".repeat(64)] {
            assert!(valid_key(key), "{key}");
        }
        for key in ["", "Match", "1abc", "a b", "a/b", &"a".repeat(65)] {
            assert!(!valid_key(key), "{key}");
        }
        for name in ["round.won", "1st-blood", &"x".repeat(48)] {
            assert!(valid_event_name(name), "{name}");
        }
        for name in ["", "Round", ".x", &"x".repeat(49)] {
            assert!(!valid_event_name(name), "{name}");
        }
        for id in ["round-4-of-match-81", "AbC_1234", &"z".repeat(64)] {
            assert!(valid_event_id(id), "{id}");
        }
        for id in ["short", "has.dot.inside", "has space x", &"z".repeat(65)] {
            assert!(!valid_event_id(id), "{id}");
        }
    }

    #[test]
    fn generated_ids_fit_the_event_id_rule() {
        let mut seen = BTreeSet::new();
        for _ in 0..200 {
            let id = generated_event_id();
            assert_eq!(id.len(), 22, "{id}");
            assert!(valid_event_id(&id), "{id}");
            assert!(IdempotencyKey::new(id.clone()).is_ok());
            seen.insert(id);
        }
        assert_eq!(seen.len(), 200);
    }

    #[test]
    fn value_checks() {
        assert!(check_value(&json!({"red": 3, "blue": 1})).is_ok());
        assert!(check_value(&json!("x".repeat(4000))).is_ok());
        assert!(check_value(&json!("x".repeat(4100))).is_err());
        let mut deep = json!(1);
        for _ in 0..8 {
            deep = json!([deep]);
        }
        assert_eq!(depth(&deep), 8);
        assert!(check_value(&deep).is_ok());
        assert!(check_value(&json!([deep])).is_err());
        assert_eq!(depth(&json!(5)), 0);
    }

    #[test]
    fn ttls() {
        assert_eq!(ttl_seconds(Duration::from_secs(10)).unwrap(), 10);
        assert_eq!(ttl_seconds(Duration::from_secs(86_400)).unwrap(), 86_400);
        for bad in [
            Duration::from_secs(9),
            Duration::from_secs(86_401),
            Duration::from_millis(10_500),
        ] {
            assert!(ttl_seconds(bad).is_err(), "{bad:?}");
        }
        assert!(DataBatch::new()
            .set_with_ttl("a", json!(1), Duration::from_millis(10_500))
            .validate()
            .is_err());
    }

    #[test]
    fn batches() {
        assert!(DataBatch::new().validate().is_err(), "empty");
        let mut batch = DataBatch::new();
        for i in 0..40 {
            batch = batch.set(format!("k{i}"), json!(i));
        }
        assert!(batch.validate().is_ok());
        assert!(batch.clone().set("k40", json!(0)).validate().is_err(), "41");
        let batch = DataBatch::new().set("a", json!(1)).remove("a");
        assert_eq!(
            batch.entries,
            vec![("a".into(), Entry::Remove)],
            "the last entry per key"
        );
    }

    #[test]
    fn markers_serialise_to_the_reserved_shape_and_are_checked() {
        let value = Markers::new()
            .push(Marker::new("flag-red", "Red flag", 120.5, -40.0).kind("objective"))
            .push(Marker::new("p-17", "Ana", 98.0, 12.25))
            .to_value()
            .unwrap();
        assert_eq!(
            value,
            json!({"markers": [
                {"id": "flag-red", "label": "Red flag", "x": 120.5, "y": -40.0, "kind": "objective"},
                {"id": "p-17", "label": "Ana", "x": 98.0, "y": 12.25}
            ]})
        );
        assert_eq!(
            Markers::new().to_value().unwrap(),
            json!({"markers": []}),
            "an empty set shows nothing"
        );
        // 64 short markers fit; 65 do not.
        let mut full = Markers::new();
        for i in 0..64 {
            full = full.push(Marker::new(
                format!("m{i}"),
                format!("P{i}"),
                f64::from(i),
                0.5,
            ));
        }
        assert!(full.to_value().is_ok());
        assert!(full
            .clone()
            .push(Marker::new("x", "x", 0.0, 0.0))
            .to_value()
            .is_err());
        // Long labels pass 4 KiB before 64 markers.
        let mut long = Markers::new();
        for i in 0..40 {
            long = long.push(Marker::new(format!("m{i}"), "x".repeat(100), 0.0, 0.0));
        }
        assert!(matches!(
            long.to_value(),
            Err(Error::InvalidInput {
                field: "markers",
                ..
            })
        ));
        for (marker, field) in [
            (Marker::new("", "a", 0.0, 0.0), "marker id"),
            (Marker::new("a", "", 0.0, 0.0), "marker label"),
            (Marker::new("a", "a", f64::NAN, 0.0), "marker position"),
            (Marker::new("a", "a", 0.0, f64::INFINITY), "marker position"),
            (
                Marker::new("a", "a", 0.0, 0.0).kind("k".repeat(33)),
                "marker kind",
            ),
        ] {
            match Markers::new().push(marker).to_value() {
                Err(Error::InvalidInput { field: got, .. }) => assert_eq!(got, field),
                other => panic!("{field}: {other:?}"),
            }
        }
        // 32 characters, counted as characters (not bytes).
        assert!(Markers::new()
            .push(Marker::new("a", "a", 0.0, 0.0).kind("é".repeat(32)))
            .to_value()
            .is_ok());
    }

    #[test]
    fn the_counter_never_goes_back() {
        let shared = GameShared::default();
        assert_eq!(shared.next_seq().unwrap(), 1);
        shared.raise_to(41);
        assert_eq!(shared.next_seq().unwrap(), 42);
        shared.raise_to(7);
        assert_eq!(shared.next_seq().unwrap(), 43);
        shared.raise_to(MAX_SEQ);
        assert!(shared.next_seq().is_err());
    }

    #[test]
    fn the_publisher_takes_oldest_changes_first_within_the_limit() {
        let state = Mutex::new(PublisherState::default());
        {
            let mut s = lock(&state);
            for key in ["c", "a", "b", "d"] {
                s.put(
                    key.into(),
                    Entry::Set {
                        value: json!(1),
                        ttl: None,
                    },
                );
            }
            // A newer value for "c" moves it to the back.
            s.put(
                "c".into(),
                Entry::Set {
                    value: json!(2),
                    ttl: None,
                },
            );
        }
        let taken: Vec<String> = take_batch(&state, 3).into_iter().map(|(k, _)| k).collect();
        assert_eq!(taken, ["a", "b", "d"]);
        let rest: Vec<String> = take_batch(&state, 3).into_iter().map(|(k, _)| k).collect();
        assert_eq!(rest, ["c"]);
        // A batch never passes 32 KiB.
        {
            let mut s = lock(&state);
            for i in 0..12 {
                s.put(
                    format!("big{i:02}"),
                    Entry::Set {
                        value: json!("x".repeat(4000)),
                        ttl: None,
                    },
                );
            }
        }
        let first = take_batch(&state, 40);
        assert!(first.len() < 12 && first.len() >= 7, "{}", first.len());
    }
}
