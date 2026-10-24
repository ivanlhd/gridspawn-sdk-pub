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
    collections::{BTreeSet, HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::{
    sync::{watch, Notify, OnceCell},
    time::Instant,
};

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
/// Asks for per-entry results (protocol 06 §3). An edge without partial mode
/// ignores it and answers all or nothing.
const PARTIAL: (&str, &str) = ("Gridspawn-Partial", "1");
/// Results a publisher keeps for [`DataPublisher::results`] and
/// [`EventPublisher::results`]; older ones are dropped.
const RESULTS_KEPT: usize = 256;
/// Keep-alive needs a TTL of at least this; it refreshes at TTL/2.
const KEEP_ALIVE_MIN_TTL: u64 = 10;
/// Never refresh a key more often than this (a tenth of its rate limit).
const KEEP_ALIVE_FLOOR: u64 = 5;
const EVENT_QUEUE: usize = 1000;
/// Events a second the service allows per application and environment.
const EVENTS_PER_SECOND: u64 = 10;
const EVENT_MAX_BACKOFF: Duration = Duration::from_secs(30);
/// The wait after a failed listing at a publisher's start.
const SEED_RETRY: Duration = Duration::from_secs(5);

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

fn check_event(name: &str, payload: &Value) -> Result<(), Error> {
    if !valid_event_name(name) {
        return Err(invalid(
            "event name",
            "1-48 characters of a-z 0-9 _ . - starting with a letter or digit",
        ));
    }
    if !payload.is_object() {
        return Err(invalid("event payload", "a JSON object"));
    }
    let size = serde_json::to_vec(payload)
        .map_err(|_| invalid("event payload", "could not be encoded as JSON"))?
        .len();
    if size > MAX_PAYLOAD {
        return Err(invalid("event payload", "at most 4 KiB as JSON"));
    }
    Ok(())
}

fn check_event_id(id: &str) -> Result<(), Error> {
    if valid_event_id(id) {
        Ok(())
    } else {
        Err(invalid("event id", "8-64 characters of A-Z a-z 0-9 _ -"))
    }
}

/// How long to wait before retrying after a 429 (its `Retry-After`, or 1 s).
fn rate_limit_wait(error: &Error) -> Option<Duration> {
    error.api().and_then(|api| {
        (api.code == ErrorCode::RateLimited)
            .then(|| api.retry_after.unwrap_or(Duration::from_secs(1)))
    })
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

/// Several documents set or removed in one request (1–40 entries; see
/// [`GameData::set_many`]). A key listed twice keeps its last entry.
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

/// An entry the service refused while the rest of the write went on.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RejectedKey {
    pub key: String,
    /// The JSON path, for example `set.scoreboard.value.players[3].name`.
    pub field: Option<String>,
    /// For example `personal_data`, `size` or `depth` (protocol 06 §7).
    pub reason: Option<String>,
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
    /// Entries refused one by one (an invalid or personal-data value) while
    /// the others were stored. When every entry is refused, the write fails
    /// with the first refusal instead.
    pub rejected: Vec<RejectedKey>,
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

/// `accepted`: the keys (all-or-nothing answer) or their number (partial
/// answer).
#[derive(Deserialize)]
#[serde(untagged)]
enum WireAccepted {
    Keys(Vec<String>),
    Count(u64),
}

/// `{key, seq}` (all-or-nothing answer) or `{key, storedSeq}` (partial).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireSkipped {
    key: String,
    #[serde(default)]
    seq: Option<u64>,
    #[serde(default)]
    stored_seq: Option<u64>,
}

#[derive(Deserialize)]
struct WireRejected {
    key: String,
    #[serde(default)]
    field: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// Both answer shapes of a write: today's all-or-nothing one and the partial
/// one (`Gridspawn-Partial: 1`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteResponse {
    ok: bool,
    accepted: WireAccepted,
    #[serde(default)]
    skipped: Vec<WireSkipped>,
    #[serde(default)]
    rejected: Vec<WireRejected>,
    quota: DataQuota,
    #[serde(default)]
    request_id: Option<String>,
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
/// sequence counter, seeded once from the listing (the highest listed `seq`).
#[derive(Debug, Default)]
pub(crate) struct GameShared {
    seeded: OnceCell<u64>,
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
    /// It reads the listing once at start to seed the sequence counter
    /// ([`DataPublisher::seq_base`]). Use [`DataPublisher::shutdown`] to send
    /// what is pending before stopping; dropping it stops at once.
    pub fn spawn_data_publisher(&self, hz: u32) -> DataPublisher {
        DataPublisher::spawn(self.data(), hz)
    }

    /// Queues events and sends them in order on the current tokio runtime,
    /// at most 10 a second, retrying network errors, 5xx and 429 with the
    /// same `Idempotency-Key`.
    pub fn spawn_event_publisher(&self) -> EventPublisher {
        EventPublisher::spawn(self.events())
    }

    fn shared(&self) -> &GameShared {
        &self.client.inner.game
    }

    /// Seeds the counter from the listing once (single-flight; a failure
    /// leaves it unseeded, and the next write tries again). The base is the
    /// highest listed `seq`, tombstones included (0 when none).
    async fn seed(&self) -> Result<u64, Error> {
        self.shared()
            .seeded
            .get_or_try_init(|| async {
                let listing = self.data().list().await?;
                let base = listing.keys.iter().map(|k| k.seq).max().unwrap_or(0);
                self.shared().raise_to(base);
                Ok::<u64, Error>(base)
            })
            .await
            .copied()
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

    /// One request of 1–40 entries. Each entry gets the next sequence number;
    /// entries the service skips are sent once more above their stored
    /// number (see [`DataAck::skipped`]).
    ///
    /// An entry the service refuses (an invalid or personal-data value) does
    /// not hold back the others: they are stored and the refused one is in
    /// [`DataAck::rejected`]. The crate asks for per-entry results
    /// (`Gridspawn-Partial: 1`); from an edge that answers all or nothing it
    /// drops the entry the refusal names and sends the rest again, at most
    /// once per entry. When every entry is refused, the first refusal is the
    /// error. Rate limits, the quota and other failures refuse the whole
    /// write.
    pub async fn set_many(&self, batch: &DataBatch) -> Result<DataAck, Error> {
        batch.validate()?;
        let outcome = self.send_entries(batch.entries.clone()).await;
        if let Some((error, _)) = outcome.failed {
            return Err(error);
        }
        if outcome.stored.is_empty() && outcome.owned.is_empty() {
            if let Some((_, error)) = outcome.rejected.into_iter().next() {
                return Err(error);
            }
            return Err(Error::InvalidResponse("invalid game data answer"));
        }
        Ok(DataAck {
            accepted: outcome.stored.into_iter().map(|s| s.key).collect(),
            skipped: outcome.owned,
            rejected: outcome.rejected.into_iter().map(|(key, _)| key).collect(),
            quota: outcome.quota.unwrap_or_default(),
        })
    }

    /// The highest `seq` the listing showed when the writer started (0 when
    /// it listed none): every entry is numbered above it. `None` until the
    /// first write or the first [`DataPublisher`] tick reads the listing.
    pub fn seq_base(&self) -> Option<u64> {
        self.game.shared().seeded.get().copied()
    }

    /// Numbers the entries, writes them (dropping refused ones) and resends
    /// the skipped ones once. Never fails as a whole: see [`Outcome`].
    async fn send_entries(&self, entries: Vec<(String, Entry)>) -> Outcome {
        let mut outcome = Outcome::default();
        if let Err(error) = self.game.seed().await {
            outcome.failed = Some((error, entries.into_iter().map(|(k, _)| k).collect()));
            return outcome;
        }
        let shared = self.game.shared();
        let mut sent = Vec::with_capacity(entries.len());
        for (key, entry) in entries {
            match shared.next_seq() {
                Ok(seq) => sent.push(Sent { key, entry, seq }),
                Err(error) => {
                    let mut keys: Vec<String> = sent.into_iter().map(|s| s.key).collect();
                    keys.push(key);
                    outcome.failed = Some((error, keys));
                    return outcome;
                }
            }
        }
        // Skipped with a higher stored number: resend once above it.
        let resend = self.write_rounds(sent, false, &mut outcome).await;
        if resend.is_empty() {
            return outcome;
        }
        for (_, stored) in &resend {
            shared.raise_to(*stored);
        }
        let mut again = Vec::with_capacity(resend.len());
        for (sent, _) in resend {
            match shared.next_seq() {
                Ok(seq) => again.push(Sent { seq, ..sent }),
                Err(error) => {
                    outcome.failed = Some((error, vec![sent.key]));
                    return outcome;
                }
            }
        }
        self.write_rounds(again, true, &mut outcome).await;
        outcome
    }

    /// Writes `sent` until the service answers 200 or nothing is left. A
    /// refusal from an all-or-nothing edge names its first offending entry:
    /// that entry is dropped and the rest go again (one round per entry at
    /// most; rounds refused by the edge cost no rate-limit tokens). Returns
    /// the entries skipped with a higher stored number (`last`: none; they
    /// are owned by another writer).
    async fn write_rounds(
        &self,
        mut sent: Vec<Sent>,
        last: bool,
        outcome: &mut Outcome,
    ) -> Vec<(Sent, u64)> {
        let mut resend = Vec::new();
        while !sent.is_empty() {
            let error = match self.write(&sent).await {
                Ok(response) => {
                    if let Err(error) = self.read_answer(response, sent, last, outcome, &mut resend)
                    {
                        outcome.failed = Some(error);
                    }
                    return resend;
                }
                Err(error) => error,
            };
            let Some(api) = error.api() else {
                outcome.failed = Some((error, sent.into_iter().map(|s| s.key).collect()));
                return resend;
            };
            // Partial mode, every entry refused: 400 with `rejected`.
            if let Some(list) = api.rejected.as_deref().and_then(Value::as_array) {
                let refused = parse_rejected(list);
                if !refused.is_empty() {
                    for rejected in refused {
                        if let Some(at) = sent.iter().position(|s| s.key == rejected.key) {
                            sent.remove(at);
                            let error = entry_error(api, &rejected);
                            outcome.rejected.push((rejected, error));
                        }
                    }
                    if !sent.is_empty() {
                        // Not named, so not refused: not stored either.
                        outcome.failed = Some((error, sent.into_iter().map(|s| s.key).collect()));
                    }
                    return resend;
                }
            }
            // All or nothing: the first offending entry, by its path.
            let offender = (api.status == 400 && api.code == ErrorCode::GameDataInvalidValue)
                .then(|| {
                    api.field
                        .as_deref()
                        .and_then(|field| offender(&sent, field))
                })
                .flatten();
            let Some(at) = offender else {
                outcome.failed = Some((error, sent.into_iter().map(|s| s.key).collect()));
                return resend;
            };
            let dropped = sent.remove(at);
            tracing::warn!(
                key = %dropped.key,
                reason = api.reason.as_deref().unwrap_or(""),
                "gridspawn game data entry refused; sending the others again"
            );
            outcome.rejected.push((
                RejectedKey {
                    key: dropped.key,
                    field: api.field.clone(),
                    reason: api.reason.clone(),
                },
                error.clone(),
            ));
        }
        resend
    }

    /// Reads a 200 answer of either shape into `outcome`.
    fn read_answer(
        &self,
        response: WriteResponse,
        mut sent: Vec<Sent>,
        last: bool,
        outcome: &mut Outcome,
        resend: &mut Vec<(Sent, u64)>,
    ) -> Result<(), (Error, Vec<String>)> {
        let keys = |sent: &[Sent]| sent.iter().map(|s| s.key.clone()).collect::<Vec<_>>();
        if !response.ok {
            let keys = keys(&sent);
            return Err((Error::InvalidResponse("invalid game data answer"), keys));
        }
        outcome.quota = Some(response.quota);
        let shared = self.game.shared();
        let mut skipped = Vec::with_capacity(response.skipped.len());
        for wire in response.skipped {
            let Some(stored) = wire.stored_seq.or(wire.seq) else {
                let keys = keys(&sent);
                return Err((Error::InvalidResponse("invalid game data answer"), keys));
            };
            skipped.push((wire.key, stored));
        }
        for wire in response.rejected {
            if let Some(at) = sent.iter().position(|s| s.key == wire.key) {
                let dropped = sent.remove(at);
                let rejected = RejectedKey {
                    key: dropped.key,
                    field: wire.field,
                    reason: wire.reason,
                };
                let error = Error::Api(Box::new(ApiError {
                    service: crate::error::Service::Edge,
                    status: 400,
                    code: ErrorCode::GameDataInvalidValue,
                    wire_code: "data/invalid_value".to_owned(),
                    message: Some("the entry was refused".to_owned()),
                    request_id: response.request_id.clone(),
                    retry_after: None,
                    field: rejected.field.clone(),
                    reason: rejected.reason.clone(),
                    rejected: None,
                }));
                outcome.rejected.push((rejected, error));
            }
        }
        for (key, stored) in skipped {
            let Some(at) = sent.iter().position(|s| s.key == key) else {
                continue;
            };
            let entry = sent.remove(at);
            if stored == entry.seq {
                // An earlier attempt of this request landed (a retry).
                outcome.stored.push(entry);
            } else if last {
                shared.raise_to(stored);
                tracing::warn!(
                    key = %key,
                    "gridspawn game data key is owned by another writer (one writer per key)"
                );
                outcome.owned.push(SkippedKey { key, seq: stored });
            } else {
                resend.push((entry, stored));
            }
        }
        match response.accepted {
            WireAccepted::Keys(accepted) => {
                for key in accepted {
                    if let Some(at) = sent.iter().position(|s| s.key == key) {
                        outcome.stored.push(sent.remove(at));
                    }
                }
            }
            WireAccepted::Count(count) => {
                // The rest of the batch.
                if count != sent.len() as u64 {
                    tracing::warn!(
                        count,
                        left = sent.len(),
                        "gridspawn game data answer counts differ"
                    );
                }
                outcome.stored.append(&mut sent);
            }
        }
        Ok(())
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

    async fn write(&self, entries: &[Sent]) -> Result<WriteResponse, Error> {
        let mut set = Map::new();
        let mut remove = Vec::new();
        for Sent { key, entry, seq } in entries {
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
        self.game
            .client
            .edge_call(
                Method::PUT,
                "PUT /v1/server/game-data",
                "/v1/server/game-data",
                Some(&body),
                None,
                &[PARTIAL],
                true,
            )
            .await
            .0
    }
}

/// One numbered entry of a write.
#[derive(Clone, Debug)]
struct Sent {
    key: String,
    entry: Entry,
    seq: u64,
}

/// What a write did, entry by entry.
#[derive(Default)]
struct Outcome {
    /// Stored or removed, with the sequence number that landed.
    stored: Vec<Sent>,
    /// Still skipped after the one resend: another writer owns them.
    owned: Vec<SkippedKey>,
    /// Refused one by one.
    rejected: Vec<(RejectedKey, Error)>,
    quota: Option<DataQuota>,
    /// The request failed as a whole: none of these keys was stored.
    failed: Option<(Error, Vec<String>)>,
}

fn parse_rejected(list: &[Value]) -> Vec<RejectedKey> {
    let text =
        |value: &Value, name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    list.iter()
        .filter_map(|item| {
            Some(RejectedKey {
                key: text(item, "key")?,
                field: text(item, "field"),
                reason: text(item, "reason"),
            })
        })
        .collect()
}

/// The batch's refusal, narrowed to one entry.
fn entry_error(api: &ApiError, rejected: &RejectedKey) -> Error {
    let mut api = api.clone();
    api.field = rejected.field.clone();
    api.reason = rejected.reason.clone();
    api.rejected = None;
    Error::Api(Box::new(api))
}

/// The entry an all-or-nothing refusal names: `set.<key>…` (the longest sent
/// key that fits, since keys may contain dots) or `remove[<i>]…` (the i-th
/// removal as sent).
fn offender(sent: &[Sent], field: &str) -> Option<usize> {
    if let Some(path) = field.strip_prefix("set.") {
        return sent
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s.entry, Entry::Set { .. }))
            .filter(|(_, s)| {
                path == s.key
                    || path
                        .strip_prefix(s.key.as_str())
                        .is_some_and(|rest| rest.starts_with('.'))
            })
            .max_by_key(|(_, s)| s.key.len())
            .map(|(at, _)| at);
    }
    let index: usize = field
        .strip_prefix("remove[")?
        .split(']')
        .next()?
        .parse()
        .ok()?;
    sent.iter()
        .enumerate()
        .filter(|(_, s)| s.entry == Entry::Remove)
        .nth(index)
        .map(|(at, _)| at)
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
        check_event_id(id)?;
        self.send(name, payload, id.to_owned()).await
    }

    async fn send(&self, name: &str, payload: Value, id: String) -> Result<EventReceipt, Error> {
        check_event(name, &payload)?;
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

/// A bounded log of results: the oldest is dropped when full.
struct Ring<T> {
    items: VecDeque<T>,
    dropped: u64,
}

impl<T> Default for Ring<T> {
    fn default() -> Self {
        Self {
            items: VecDeque::new(),
            dropped: 0,
        }
    }
}

impl<T> Ring<T> {
    fn push(&mut self, item: T) {
        if self.items.len() == RESULTS_KEPT {
            self.items.pop_front();
            self.dropped += 1;
        }
        self.items.push_back(item);
    }

    fn drain(&mut self) -> Vec<T> {
        self.items.drain(..).collect()
    }
}

/// What became of a key's value in a [`DataPublisher`] (final outcomes
/// only: what can succeed later is retried, not reported).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum DataOutcome {
    /// Stored with this sequence number.
    Stored { seq: u64 },
    /// Removed; the tombstone has this sequence number.
    Removed { seq: u64 },
    /// Still skipped after one resend: another writer owns the key.
    OwnedElsewhere { stored_seq: u64 },
    /// Refused, and retrying cannot fix it: an invalid or personal-data value
    /// ([`ApiError::field`] names it), the quota, a missing scope, game data
    /// switched off. The value is dropped.
    Refused(Error),
}

/// One key's outcome, from [`DataPublisher::results`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DataResult {
    pub key: String,
    pub outcome: DataOutcome,
}

/// What a publisher had not sent when [`DataPublisher::flush`] (or
/// [`EventPublisher::flush`]) reached its timeout.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{} items were not sent in time", pending.len())]
#[non_exhaustive]
pub struct Unsent {
    /// Document keys, or the `Idempotency-Key`s of events, still waiting or
    /// in flight.
    pub pending: Vec<String>,
}

#[derive(Default)]
struct PublisherState {
    pending: HashMap<String, Pending>,
    next_order: u64,
    /// The keys of the batch being sent.
    in_flight: Vec<String>,
    skipped: BTreeSet<String>,
    last_error: Option<Error>,
    results: Ring<DataResult>,
    /// The latest value given with a TTL, per key (what keep-alive resends).
    ttl_values: HashMap<String, (Value, u64)>,
    /// When a value with a TTL was last stored, and its TTL.
    stored_at: HashMap<String, (Instant, u64)>,
    /// Keys kept alive, with their next refresh (`None` until the latest
    /// value is stored).
    keep: HashMap<String, Option<Instant>>,
    seq_base: Option<u64>,
}

/// Keep-alive resends at TTL/2: a refresh can be late by half the TTL and
/// the document still lives. Never more often than every 5 s.
fn refresh_interval(ttl: u64) -> Duration {
    Duration::from_secs((ttl / 2).max(KEEP_ALIVE_FLOOR))
}

impl PublisherState {
    /// A new value from the game.
    fn put(&mut self, key: String, entry: Entry) {
        match &entry {
            Entry::Set {
                value,
                ttl: Some(ttl),
            } => {
                if *ttl < KEEP_ALIVE_MIN_TTL {
                    self.keep.remove(&key);
                } else if let Some(due) = self.keep.get_mut(&key) {
                    // Restarts from this value's store.
                    *due = None;
                }
                self.ttl_values.insert(key.clone(), (value.clone(), *ttl));
            }
            Entry::Set { ttl: None, .. } | Entry::Remove => {
                self.keep.remove(&key);
                self.ttl_values.remove(&key);
            }
        }
        self.queue(key, entry);
    }

    fn queue(&mut self, key: String, entry: Entry) {
        self.next_order += 1;
        let order = self.next_order;
        self.pending.insert(key, Pending { entry, order });
    }

    fn keep_alive(&mut self, key: &str) -> Result<(), Error> {
        let Some((_, ttl)) = self.ttl_values.get(key) else {
            return Err(invalid(
                "ttl",
                "keep_alive needs a TTL: give the key's value one with set_with_ttl first",
            ));
        };
        if *ttl < KEEP_ALIVE_MIN_TTL {
            return Err(invalid("ttl", "keep_alive needs a TTL of at least 10 s"));
        }
        if self.keep.contains_key(key) {
            return Ok(());
        }
        let waiting = self.pending.contains_key(key) || self.in_flight.iter().any(|k| k == key);
        let due = if waiting {
            None
        } else {
            self.stored_at
                .get(key)
                .map(|(at, ttl)| *at + refresh_interval(*ttl))
        };
        self.keep.insert(key.to_owned(), due);
        Ok(())
    }

    /// A key's entry was stored at `now`.
    fn on_stored(&mut self, key: &str, entry: &Entry, now: Instant) {
        match entry {
            Entry::Set { ttl: Some(ttl), .. } => {
                self.stored_at.insert(key.to_owned(), (now, *ttl));
                if !self.pending.contains_key(key) {
                    if let Some(due) = self.keep.get_mut(key) {
                        *due = Some(now + refresh_interval(*ttl));
                    }
                }
            }
            _ => {
                self.stored_at.remove(key);
            }
        }
    }

    /// A key's value was dropped for good.
    fn on_refused(&mut self, key: &str) {
        if !self.pending.contains_key(key) {
            self.keep.remove(key);
            self.ttl_values.remove(key);
        }
    }

    /// Queues the latest value again for every kept key whose refresh is
    /// due. A refresh is an ordinary entry with a new sequence number (a
    /// resend of the stored number would be skipped and refresh nothing).
    fn queue_due_refreshes(&mut self, now: Instant) {
        let due: Vec<String> = self
            .keep
            .iter()
            .filter(|(key, due)| {
                due.is_some_and(|at| at <= now) && !self.pending.contains_key(*key)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in due {
            match self.ttl_values.get(&key).cloned() {
                Some((value, ttl)) => {
                    self.keep.insert(key.clone(), None);
                    self.queue(
                        key,
                        Entry::Set {
                            value,
                            ttl: Some(ttl),
                        },
                    );
                }
                None => {
                    self.keep.remove(&key);
                }
            }
        }
    }

    fn unsent(&self) -> Unsent {
        let mut pending: Vec<String> = self.pending.keys().cloned().collect();
        pending.extend(self.in_flight.iter().cloned());
        pending.sort();
        pending.dedup();
        Unsent { pending }
    }

    fn idle(&self) -> bool {
        self.pending.is_empty() && self.in_flight.is_empty()
    }

    /// Records a write's outcome. Returns the wait a 429 asked for.
    fn apply(
        &mut self,
        taken: &[(String, Pending)],
        outcome: Outcome,
        now: Instant,
    ) -> Option<Duration> {
        let mut clean = true;
        for sent in outcome.stored {
            self.skipped.remove(&sent.key);
            self.on_stored(&sent.key, &sent.entry, now);
            let outcome = match sent.entry {
                Entry::Remove => DataOutcome::Removed { seq: sent.seq },
                Entry::Set { .. } => DataOutcome::Stored { seq: sent.seq },
            };
            self.results.push(DataResult {
                key: sent.key,
                outcome,
            });
        }
        for skipped in outcome.owned {
            self.skipped.insert(skipped.key.clone());
            self.results.push(DataResult {
                key: skipped.key,
                outcome: DataOutcome::OwnedElsewhere {
                    stored_seq: skipped.seq,
                },
            });
        }
        for (rejected, error) in outcome.rejected {
            clean = false;
            self.on_refused(&rejected.key);
            self.results.push(DataResult {
                key: rejected.key,
                outcome: DataOutcome::Refused(error.clone()),
            });
            self.last_error = Some(error);
        }
        let mut wait = None;
        if let Some((error, unsent)) = outcome.failed {
            clean = false;
            tracing::warn!(%error, "gridspawn game data not sent");
            if error.is_retryable() {
                // Put back what no newer value replaced.
                for key in unsent {
                    if let Some((_, pending)) = taken.iter().find(|(k, _)| *k == key) {
                        self.pending.entry(key).or_insert_with(|| pending.clone());
                    }
                }
                wait = rate_limit_wait(&error);
            } else {
                for key in unsent {
                    self.on_refused(&key);
                    self.results.push(DataResult {
                        key,
                        outcome: DataOutcome::Refused(error.clone()),
                    });
                }
            }
            self.last_error = Some(error);
        }
        if clean {
            self.last_error = None;
        }
        wait
    }
}

/// Coalesces values to the latest per key and sends the changed keys, oldest
/// change first, in one batch per tick.
///
/// - [`Self::results`]: what became of each key, entry by entry.
/// - [`Self::flush`] and [`Self::shutdown`]: send what is pending before
///   going on or stopping. Dropping the publisher (or [`Self::stop`]) stops
///   at once, and entries not sent yet are lost: removals included.
/// - [`Self::keep_alive`]: refreshes a document's TTL while the game runs.
pub struct DataPublisher {
    state: Arc<Mutex<PublisherState>>,
    progress: watch::Receiver<u64>,
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
        let (progress_tx, progress) = watch::channel(0u64);
        let task = tokio::spawn(async move {
            let mut ticks = tokio::time::interval(period);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut seeded = false;
            loop {
                ticks.tick().await;
                if !seeded {
                    // The listing once, so the numbering (and its base) is
                    // known before the first batch.
                    match data.game.seed().await {
                        Ok(base) => {
                            seeded = true;
                            lock(&state).seq_base = Some(base);
                            tracing::info!(
                                seq_base = base,
                                "gridspawn game data publisher started; sequence numbers continue above the stored ones"
                            );
                        }
                        Err(error) => {
                            tracing::warn!(%error, "gridspawn game data listing failed; retrying");
                            let wait = rate_limit_wait(&error).unwrap_or(SEED_RETRY);
                            lock(&state).last_error = Some(error);
                            progress_tx.send_modify(|n| *n += 1);
                            tokio::time::sleep(wait).await;
                            continue;
                        }
                    }
                }
                let taken = {
                    let mut s = lock(&state);
                    s.queue_due_refreshes(Instant::now());
                    let taken = take_batch(&mut s, per_tick);
                    s.in_flight = taken.iter().map(|(key, _)| key.clone()).collect();
                    taken
                };
                if taken.is_empty() {
                    continue;
                }
                let wait = send_taken(&data, &state, &taken).await;
                progress_tx.send_modify(|n| *n += 1);
                if let Some(wait) = wait {
                    tokio::time::sleep(wait).await;
                }
            }
        });
        Self {
            state: shared,
            progress,
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

    /// Keeps a document with a TTL alive: its latest value is sent again
    /// (with a new sequence number) every TTL/2 after it was stored, never
    /// more often than every 5 s, until [`Self::stop_keep_alive`], a
    /// [`Self::set`] without TTL or a [`Self::remove`]. A short TTL hides the
    /// value soon after the server stops or crashes.
    ///
    /// The key's latest value must have a TTL (at least 10 s). Each refresh
    /// costs one entry of the key's and the app's rate limits, sends an
    /// update to every page that shows the key, and moves its `updatedAt`.
    /// When the value rarely changes, prefer a TTL of 60 s or more, or no
    /// TTL and a [`Self::remove`] before [`Self::shutdown`].
    pub fn keep_alive(&self, key: &str) -> Result<(), Error> {
        check_key(key)?;
        lock(&self.state).keep_alive(key)
    }

    /// Stops refreshing the key; it expires at the end of its TTL.
    pub fn stop_keep_alive(&self, key: &str) {
        lock(&self.state).keep.remove(key);
    }

    /// What became of each key since the last call, oldest first (at most
    /// the last 256; see [`Self::dropped_results`]). Values that will be
    /// retried are not reported; [`Self::last_error`] shows why.
    pub fn results(&self) -> Vec<DataResult> {
        lock(&self.state).results.drain()
    }

    /// Results dropped because [`Self::results`] was not called often enough.
    pub fn dropped_results(&self) -> u64 {
        lock(&self.state).results.dropped
    }

    /// Keys another writer owns (still skipped after one resend).
    pub fn skipped(&self) -> Vec<String> {
        lock(&self.state).skipped.iter().cloned().collect()
    }

    /// The failure of the last batch (or of one of its entries), if any.
    /// Cleared by the next batch that goes through without a refusal.
    pub fn last_error(&self) -> Option<Error> {
        lock(&self.state).last_error.clone()
    }

    /// The highest `seq` the listing showed at start (0 when none): every
    /// entry is numbered above it. `None` until the listing was read.
    pub fn seq_base(&self) -> Option<u64> {
        lock(&self.state).seq_base
    }

    /// Waits until everything given so far was sent (stored, refused or
    /// owned elsewhere), within the usual rate. On timeout, the keys still
    /// waiting; they are still sent later.
    pub async fn flush(&self, timeout: Duration) -> Result<(), Unsent> {
        let deadline = Instant::now() + timeout;
        let mut progress = self.progress.clone();
        loop {
            progress.borrow_and_update();
            {
                let s = lock(&self.state);
                if s.idle() {
                    return Ok(());
                }
            }
            match tokio::time::timeout_at(deadline, progress.changed()).await {
                Ok(Ok(())) => {}
                // Timed out, or the task is gone.
                _ => return Err(lock(&self.state).unsent()),
            }
        }
    }

    /// [`Self::flush`], then stops. Use it at shutdown so removals and last
    /// values are not lost. Keep-alive stops; kept documents expire at the
    /// end of their TTL.
    pub async fn shutdown(self, timeout: Duration) -> Result<(), Unsent> {
        let result = self.flush(timeout).await;
        drop(self);
        result
    }

    /// Stops at once, like dropping it: entries not sent yet are lost. Use
    /// [`Self::shutdown`] to send them first.
    pub fn stop(self) {}
}

/// Sends one tick's batch and records the outcome. A refused batch (400) has
/// already lost only its refused entries ([`GameData::set_many`]). On 409
/// (quota) the removals go again alone, since they free space, and the sets
/// are refused: the service cannot say which one passed the quota.
async fn send_taken(
    data: &GameData,
    state: &Mutex<PublisherState>,
    taken: &[(String, Pending)],
) -> Option<Duration> {
    let entries: Vec<(String, Entry)> = taken
        .iter()
        .map(|(key, pending)| (key.clone(), pending.entry.clone()))
        .collect();
    let mut outcome = data.send_entries(entries).await;
    let mut second = None;
    let quota = outcome.failed.as_ref().is_some_and(|(error, _)| {
        error
            .api()
            .is_some_and(|api| api.code == ErrorCode::GameDataQuotaExceeded)
    });
    if quota {
        let (error, unsent) = outcome.failed.take().expect("checked above");
        let mut removals = Vec::new();
        for key in unsent {
            let is_removal = taken
                .iter()
                .any(|(k, pending)| *k == key && pending.entry == Entry::Remove);
            if is_removal {
                removals.push((key, Entry::Remove));
            } else {
                let field = error.api().and_then(|api| api.field.clone());
                outcome.rejected.push((
                    RejectedKey {
                        key,
                        field,
                        reason: None,
                    },
                    error.clone(),
                ));
            }
        }
        if !removals.is_empty() {
            second = Some(data.send_entries(removals).await);
        }
    }
    let now = Instant::now();
    let mut s = lock(state);
    s.in_flight.clear();
    let mut wait = s.apply(taken, outcome, now);
    if let Some(second) = second {
        wait = s.apply(taken, second, now).or(wait);
    }
    wait
}

#[derive(Clone, Debug)]
struct QueuedEvent {
    name: String,
    payload: Value,
    id: String,
}

/// One event's outcome, from [`EventPublisher::results`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct EventResult {
    /// The event's `Idempotency-Key` (what `emit` returned).
    pub id: String,
    /// The receipt, or a refusal that retrying cannot fix (the event is
    /// dropped).
    pub outcome: Result<EventReceipt, Error>,
}

#[derive(Default)]
struct EventState {
    queue: VecDeque<QueuedEvent>,
    in_flight: Option<String>,
    results: Ring<EventResult>,
    last_error: Option<Error>,
}

impl EventState {
    fn idle(&self) -> bool {
        self.queue.is_empty() && self.in_flight.is_none()
    }

    fn unsent(&self) -> Unsent {
        Unsent {
            pending: self.queue.iter().map(|event| event.id.clone()).collect(),
        }
    }
}

/// Sends events in order, one at a time and at most 10 a second. Network
/// errors, 5xx and 429 (after its `Retry-After`) are retried with the same
/// `Idempotency-Key`, backing off up to 30 s, so a retried event is never
/// counted twice. Dropping it (or [`Self::stop`]) stops at once; use
/// [`Self::shutdown`] to send the queue first.
pub struct EventPublisher {
    state: Arc<Mutex<EventState>>,
    wake: Arc<Notify>,
    progress: watch::Receiver<u64>,
    task: tokio::task::JoinHandle<()>,
}

impl EventPublisher {
    fn spawn(events: GameEvents) -> Self {
        let shared = Arc::new(Mutex::new(EventState::default()));
        let state = shared.clone();
        let wake = Arc::new(Notify::new());
        let woken = wake.clone();
        let (progress_tx, progress) = watch::channel(0u64);
        let pace = Duration::from_millis(1000 / EVENTS_PER_SECOND);
        let task = tokio::spawn(async move {
            let mut failures: u32 = 0;
            loop {
                let next = {
                    let mut s = lock(&state);
                    let next = s.queue.front().cloned();
                    s.in_flight = next.as_ref().map(|event| event.id.clone());
                    next
                };
                let Some(event) = next else {
                    woken.notified().await;
                    continue;
                };
                let result = events
                    .send(&event.name, event.payload.clone(), event.id.clone())
                    .await;
                let wait = {
                    let mut s = lock(&state);
                    s.in_flight = None;
                    match result {
                        Ok(receipt) => {
                            failures = 0;
                            s.queue.pop_front();
                            s.last_error = None;
                            s.results.push(EventResult {
                                id: event.id,
                                outcome: Ok(receipt),
                            });
                            pace
                        }
                        Err(error) if error.is_retryable() => {
                            failures = failures.saturating_add(1);
                            tracing::warn!(%error, "gridspawn game event not sent; retrying");
                            let backoff = Duration::from_secs(1)
                                .saturating_mul(1u32 << (failures - 1).min(5))
                                .min(EVENT_MAX_BACKOFF);
                            let wait = rate_limit_wait(&error).unwrap_or(backoff);
                            s.last_error = Some(error);
                            wait
                        }
                        Err(error) => {
                            failures = 0;
                            tracing::warn!(%error, "gridspawn game event refused");
                            s.queue.pop_front();
                            s.results.push(EventResult {
                                id: event.id,
                                outcome: Err(error.clone()),
                            });
                            s.last_error = Some(error);
                            pace
                        }
                    }
                };
                progress_tx.send_modify(|n| *n += 1);
                tokio::time::sleep(wait).await;
            }
        });
        Self {
            state: shared,
            wake,
            progress,
            task,
        }
    }

    /// Queues an event with a generated `Idempotency-Key`, returned. Checked
    /// here; never blocks.
    pub fn emit(&self, name: &str, payload: Value) -> Result<String, Error> {
        self.push(name, payload, generated_event_id())
    }

    /// As [`Self::emit`] with your own id (`[A-Za-z0-9_-]{8,64}`), for an
    /// event that may be sent again after a restart: a repeat within 24 h
    /// returns the first answer with `duplicate: true`.
    pub fn emit_with_id(&self, name: &str, payload: Value, id: &str) -> Result<String, Error> {
        check_event_id(id)?;
        self.push(name, payload, id.to_owned())
    }

    fn push(&self, name: &str, payload: Value, id: String) -> Result<String, Error> {
        check_event(name, &payload)?;
        {
            let mut s = lock(&self.state);
            if s.queue.len() >= EVENT_QUEUE {
                return Err(invalid(
                    "game event queue",
                    "full: 1000 events wait to be sent (see last_error)",
                ));
            }
            s.queue.push_back(QueuedEvent {
                name: name.to_owned(),
                payload,
                id: id.clone(),
            });
        }
        self.wake.notify_one();
        Ok(id)
    }

    /// Events sent or refused since the last call, oldest first (at most the
    /// last 256; see [`Self::dropped_results`]).
    pub fn results(&self) -> Vec<EventResult> {
        lock(&self.state).results.drain()
    }

    /// Results dropped because [`Self::results`] was not called often enough.
    pub fn dropped_results(&self) -> u64 {
        lock(&self.state).results.dropped
    }

    /// Events waiting, the one in flight included.
    pub fn queued(&self) -> usize {
        lock(&self.state).queue.len()
    }

    /// The last failure, retried or not; cleared by the next event sent.
    pub fn last_error(&self) -> Option<Error> {
        lock(&self.state).last_error.clone()
    }

    /// Waits until the queue is empty. On timeout, the ids still queued;
    /// they are still sent later.
    pub async fn flush(&self, timeout: Duration) -> Result<(), Unsent> {
        let deadline = Instant::now() + timeout;
        let mut progress = self.progress.clone();
        loop {
            progress.borrow_and_update();
            if lock(&self.state).idle() {
                return Ok(());
            }
            match tokio::time::timeout_at(deadline, progress.changed()).await {
                Ok(Ok(())) => {}
                _ => return Err(lock(&self.state).unsent()),
            }
        }
    }

    /// [`Self::flush`], then stops.
    pub async fn shutdown(self, timeout: Duration) -> Result<(), Unsent> {
        let result = self.flush(timeout).await;
        drop(self);
        result
    }

    /// Stops at once, like dropping it: queued events are lost.
    pub fn stop(self) {}
}

impl Drop for EventPublisher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl std::fmt::Debug for EventPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = lock(&self.state);
        f.debug_struct("EventPublisher")
            .field("queued", &s.queue.len())
            .finish()
    }
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
fn take_batch(s: &mut PublisherState, limit: usize) -> Vec<(String, Pending)> {
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

/// Stops at once: what was not sent is lost (best effort; see
/// [`DataPublisher::shutdown`]).
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
        let mut s = PublisherState::default();
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
        let taken: Vec<String> = take_batch(&mut s, 3).into_iter().map(|(k, _)| k).collect();
        assert_eq!(taken, ["a", "b", "d"]);
        let rest: Vec<String> = take_batch(&mut s, 3).into_iter().map(|(k, _)| k).collect();
        assert_eq!(rest, ["c"]);
        // A batch never passes 32 KiB.
        for i in 0..12 {
            s.put(
                format!("big{i:02}"),
                Entry::Set {
                    value: json!("x".repeat(4000)),
                    ttl: None,
                },
            );
        }
        let first = take_batch(&mut s, 40);
        assert!(first.len() < 12 && first.len() >= 7, "{}", first.len());
    }

    #[test]
    fn an_all_or_nothing_refusal_names_its_entry() {
        let set = || Entry::Set {
            value: json!(1),
            ttl: None,
        };
        let sent = |key: &str, entry: Entry| Sent {
            key: key.into(),
            entry,
            seq: 1,
        };
        let batch = vec![
            sent("match", set()),
            sent("match.score", set()),
            sent("lobby", Entry::Remove),
            sent("old", Entry::Remove),
        ];
        // Keys contain dots: the longest key that fits the path.
        assert_eq!(
            offender(&batch, "set.match.score.value.players[3].name"),
            Some(1)
        );
        assert_eq!(offender(&batch, "set.match.score"), Some(1));
        assert_eq!(offender(&batch, "set.match.value"), Some(0));
        assert_eq!(offender(&batch, "set.match"), Some(0));
        assert_eq!(offender(&batch, "set.matchx.value"), None);
        // remove[i]: the i-th removal as sent.
        assert_eq!(offender(&batch, "remove[0].seq"), Some(2));
        assert_eq!(offender(&batch, "remove[1].key"), Some(3));
        assert_eq!(offender(&batch, "remove[2].key"), None);
        // Not one entry: the whole batch.
        assert_eq!(offender(&batch, "entries"), None);
        assert_eq!(offender(&batch, "body"), None);
    }

    #[test]
    fn results_keep_the_last_256() {
        let mut ring = Ring::default();
        for i in 0..300u32 {
            ring.push(i);
        }
        assert_eq!(ring.dropped, 44);
        let kept = ring.drain();
        assert_eq!(kept.len(), 256);
        assert_eq!((kept[0], kept[255]), (44, 299), "oldest first");
        assert!(ring.drain().is_empty(), "drained");
    }

    /// Keep-alive timing with explicit instants: no clock is read here.
    #[test]
    fn keep_alive_refreshes_the_latest_value_at_half_the_ttl() {
        let t0 = Instant::now();
        let s = &mut PublisherState::default();
        let with_ttl = |value: Value, ttl: u64| Entry::Set {
            value,
            ttl: Some(ttl),
        };
        let sec = Duration::from_secs;
        // A TTL is needed (at least 10 s; set_with_ttl allows no less).
        s.put(
            "plain".into(),
            Entry::Set {
                value: json!(1),
                ttl: None,
            },
        );
        assert!(s.keep_alive("plain").is_err());
        assert!(s.keep_alive("never-set").is_err());
        s.put("short".into(), with_ttl(json!(1), 9));
        assert!(s.keep_alive("short").is_err());
        s.put("ten".into(), with_ttl(json!(1), 10));
        s.keep_alive("ten").unwrap();
        s.keep.remove("ten");
        take_batch(s, 40);

        s.put("lobby".into(), with_ttl(json!("open"), 120));
        s.keep_alive("lobby").unwrap();
        // Waiting to be sent: the timer starts when it is stored.
        assert_eq!(s.keep["lobby"], None);
        let taken = take_batch(s, 40);
        s.on_stored("lobby", &taken[0].1.entry, t0);
        assert_eq!(s.keep["lobby"], Some(t0 + sec(60)));
        s.queue_due_refreshes(t0 + sec(59));
        assert!(s.pending.is_empty());
        s.queue_due_refreshes(t0 + sec(60));
        assert_eq!(s.pending["lobby"].entry, with_ttl(json!("open"), 120));
        // Not queued twice while it waits.
        s.queue_due_refreshes(t0 + sec(90));
        assert_eq!(s.pending.len(), 1);
        let taken = take_batch(s, 40);
        s.on_stored("lobby", &taken[0].1.entry, t0 + sec(61));
        assert_eq!(s.keep["lobby"], Some(t0 + sec(121)));

        // A newer value is what gets refreshed, and restarts the timer.
        s.put("lobby".into(), with_ttl(json!("closing"), 300));
        assert_eq!(s.keep["lobby"], None);
        let taken = take_batch(s, 40);
        s.on_stored("lobby", &taken[0].1.entry, t0 + sec(70));
        assert_eq!(s.keep["lobby"], Some(t0 + sec(220)));
        s.queue_due_refreshes(t0 + sec(220));
        assert_eq!(s.pending["lobby"].entry, with_ttl(json!("closing"), 300));
        take_batch(s, 40);

        // Kept alive after its store: counted from that store.
        s.put("round".into(), with_ttl(json!(4), 100));
        let taken = take_batch(s, 40);
        s.on_stored("round", &taken[0].1.entry, t0);
        s.keep_alive("round").unwrap();
        assert_eq!(s.keep["round"], Some(t0 + sec(50)));
        s.queue_due_refreshes(t0 + sec(80));
        assert!(s.pending.contains_key("round"), "already due");
        take_batch(s, 40);

        // A value without TTL, a removal or stop_keep_alive end it.
        s.put(
            "round".into(),
            Entry::Set {
                value: json!(5),
                ttl: None,
            },
        );
        assert!(!s.keep.contains_key("round"));
        // A new TTL value restarts it from that value's store.
        s.put("lobby".into(), with_ttl(json!("x"), 30));
        assert_eq!(s.keep["lobby"], None);
        s.put("a".into(), with_ttl(json!(1), 60));
        s.keep_alive("a").unwrap();
        s.put("a".into(), Entry::Remove);
        assert!(!s.keep.contains_key("a"));
        // A refused value ends it too (when nothing newer waits).
        s.put("b".into(), with_ttl(json!(1), 60));
        s.keep_alive("b").unwrap();
        take_batch(s, 40);
        s.on_refused("b");
        assert!(!s.keep.contains_key("b"));
        assert_eq!(refresh_interval(60), sec(30));
        assert_eq!(refresh_interval(10), sec(5));
        assert_eq!(refresh_interval(11), sec(5), "never under 5 s");
        assert_eq!(refresh_interval(86_400), sec(43_200));
    }
}
