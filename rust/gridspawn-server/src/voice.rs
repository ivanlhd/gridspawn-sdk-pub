//! Who hears whom in a session's voice rooms, decided by the game server:
//! the room configuration, each player's state, their
//! positions, and the authoritative mute. The voice service enforces it by not
//! forwarding audio: an inaudible speaker sends that listener no packets.
//!
//! Order of calls for a match:
//! 1. [`VoiceControl::configure`] once (a configured room is "managed");
//! 2. for each player, [`VoiceControl::set_player_state`] **before** issuing
//!    their voice ticket: in a managed room a player without a state hears no
//!    one and is heard by no one;
//! 3. positions through [`VoiceControl::spawn_position_publisher`] for
//!    proximity rooms;
//! 4. [`Session::spawn_renewer`] all along: when the lease lapses the rules go
//!    stale, and everyone goes silent 3 s later.
//!
//! **Sessions longer than 24 hours:** today the renewals keep the rules alive
//! only for 24 h after the last [`VoiceControl::configure`]. A game that sends
//! no other voice updates for that long must call `configure` again before
//! then. (The service will lift this limit; this note goes then.)

use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    client::segment,
    error::{Error, ErrorCode},
    session::{ParticipantId, Session},
};

const MAX_ROOMS: usize = 4;
const MAX_CHANNELS: usize = 8;
const MAX_PARTICIPANTS: u8 = 64;
const DEFAULT_AUDIBLE_SPEAKERS: u8 = 16;
const MAX_COORDINATE: f64 = 1e6;
const MAX_DISTANCE: f64 = 1e4;
const MAX_POSITIONS: usize = 256;
/// The voice service keeps sequence numbers exact up to 2⁵³ − 1.
const MAX_SEQ: u64 = (1 << 53) - 1;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn invalid(field: &'static str, reason: &'static str) -> Error {
    Error::InvalidInput { field, reason }
}

fn one() -> f64 {
    1.0
}

fn yes() -> bool {
    true
}

fn default_participants() -> u8 {
    MAX_PARTICIPANTS
}

fn default_speakers() -> u8 {
    DEFAULT_AUDIBLE_SPEAKERS
}

/// Who a room lets hear whom.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceMode {
    /// Everyone in the room hears everyone.
    Open,
    /// You hear the players who share one of your channels.
    Channels,
    /// You hear the players within range.
    Proximity,
    /// A shared channel **or** range (teammates always, nearby enemies too).
    Hybrid,
}

/// Who places voices in space.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Spatial {
    /// The service sends each listener a gain (and a pan with `yaw`): secrecy-safe.
    #[default]
    Server,
    /// The game client places voices itself; no range culling, so a modified
    /// client hears everyone its channels allow. Not secrecy-safe (co-op,
    /// social hubs).
    Client,
}

/// How range gain falls from `min_distance` (1.0) to `max_distance`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rolloff {
    #[default]
    Linear,
    Inverse,
    Exponential,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Proximity {
    min_distance: f64,
    max_distance: f64,
    #[serde(default)]
    rolloff: Rolloff,
    #[serde(default = "one")]
    rolloff_factor: f64,
    #[serde(default)]
    hysteresis: f64,
}

/// Dead and spectating players. Defaults: they hear the living, speak only to
/// each other, and spectators hear the dead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct Ghosts {
    /// Dead players and spectators hear the living (default true).
    #[serde(default = "yes")]
    pub hear_alive: bool,
    /// The living hear dead players and spectators (default false).
    #[serde(default)]
    pub alive_hear_ghosts: bool,
    /// Spectators hear dead players (default true).
    #[serde(default = "yes")]
    pub spectators_hear_dead: bool,
}

impl Default for Ghosts {
    fn default() -> Self {
        Self {
            hear_alive: true,
            alive_hear_ghosts: false,
            spectators_hear_dead: true,
        }
    }
}

impl Ghosts {
    pub fn hear_alive(mut self, value: bool) -> Self {
        self.hear_alive = value;
        self
    }

    pub fn alive_hear_ghosts(mut self, value: bool) -> Self {
        self.alive_hear_ghosts = value;
        self
    }

    pub fn spectators_hear_dead(mut self, value: bool) -> Self {
        self.spectators_hear_dead = value;
        self
    }
}

/// One voice room's rules. Distances are in metres (your world units).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceRoom {
    mode: VoiceMode,
    #[serde(default)]
    spatial: Spatial,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proximity: Option<Proximity>,
    #[serde(default)]
    ghosts: Ghosts,
    #[serde(default = "default_participants")]
    max_participants: u8,
    #[serde(default = "default_speakers")]
    max_audible_speakers: u8,
}

impl VoiceRoom {
    fn new(mode: VoiceMode, range: Option<(f64, f64)>) -> Self {
        Self {
            mode,
            spatial: Spatial::Server,
            proximity: range.map(|(min_distance, max_distance)| Proximity {
                min_distance,
                max_distance,
                rolloff: Rolloff::Linear,
                rolloff_factor: 1.0,
                hysteresis: 0.0,
            }),
            ghosts: Ghosts::default(),
            max_participants: MAX_PARTICIPANTS,
            max_audible_speakers: DEFAULT_AUDIBLE_SPEAKERS,
        }
    }

    /// Everyone in the room hears everyone.
    pub fn open() -> Self {
        Self::new(VoiceMode::Open, None)
    }

    /// You hear the players who share one of your channels.
    pub fn channels() -> Self {
        Self::new(VoiceMode::Channels, None)
    }

    /// You hear players within `max_distance`; full volume up to `min_distance`.
    pub fn proximity(min_distance: f64, max_distance: f64) -> Self {
        Self::new(VoiceMode::Proximity, Some((min_distance, max_distance)))
    }

    /// A shared channel at full volume, or range as in [`Self::proximity`].
    pub fn hybrid(min_distance: f64, max_distance: f64) -> Self {
        Self::new(VoiceMode::Hybrid, Some((min_distance, max_distance)))
    }

    /// The game client places voices itself (not secrecy-safe; see [`Spatial::Client`]).
    pub fn spatial_client(mut self) -> Self {
        self.spatial = Spatial::Client;
        self
    }

    /// Proximity and hybrid rooms only; other rooms have no range.
    pub fn rolloff(mut self, rolloff: Rolloff) -> Self {
        if let Some(p) = &mut self.proximity {
            p.rolloff = rolloff;
        }
        self
    }

    /// Proximity and hybrid rooms only: (0, 100], default 1.
    pub fn rolloff_factor(mut self, factor: f64) -> Self {
        if let Some(p) = &mut self.proximity {
            p.rolloff_factor = factor;
        }
        self
    }

    /// Proximity and hybrid rooms only: a player leaves range only beyond
    /// `max_distance + hysteresis` (0 to half of `max_distance`, default 0).
    pub fn hysteresis(mut self, distance: f64) -> Self {
        if let Some(p) = &mut self.proximity {
            p.hysteresis = distance;
        }
        self
    }

    pub fn ghosts(mut self, ghosts: Ghosts) -> Self {
        self.ghosts = ghosts;
        self
    }

    /// 1–64, default 64.
    pub fn max_participants(mut self, max: u8) -> Self {
        self.max_participants = max;
        self
    }

    /// How many speakers one listener hears at most, the closest first (a
    /// shared channel counts as distance 0). 1–64, default 16.
    pub fn max_audible_speakers(mut self, max: u8) -> Self {
        self.max_audible_speakers = max;
        self
    }

    pub fn mode(&self) -> VoiceMode {
        self.mode
    }

    pub fn spatial(&self) -> Spatial {
        self.spatial
    }

    /// False for client-spatialized rooms.
    pub fn secrecy_safe(&self) -> bool {
        self.spatial == Spatial::Server
    }

    fn validate(&self) -> Result<(), Error> {
        if !(1..=MAX_PARTICIPANTS).contains(&self.max_participants) {
            return Err(invalid("voice max participants", "1-64"));
        }
        if !(1..=MAX_PARTICIPANTS).contains(&self.max_audible_speakers) {
            return Err(invalid("voice max audible speakers", "1-64"));
        }
        let ranged = matches!(self.mode, VoiceMode::Proximity | VoiceMode::Hybrid);
        match &self.proximity {
            None if ranged && self.spatial == Spatial::Server => Err(invalid(
                "voice proximity",
                "required for proximity and hybrid rooms",
            )),
            None => Ok(()),
            Some(p) => {
                let distance = |v: f64| v.is_finite() && (0.0..=MAX_DISTANCE).contains(&v);
                if !distance(p.min_distance)
                    || !distance(p.max_distance)
                    || p.min_distance >= p.max_distance
                {
                    return Err(invalid(
                        "voice proximity distances",
                        "0 <= min < max <= 10000",
                    ));
                }
                if !(p.rolloff_factor.is_finite()
                    && p.rolloff_factor > 0.0
                    && p.rolloff_factor <= 100.0)
                {
                    return Err(invalid(
                        "voice rolloff factor",
                        "greater than 0, at most 100",
                    ));
                }
                if !(p.hysteresis.is_finite()
                    && p.hysteresis >= 0.0
                    && p.hysteresis <= p.max_distance * 0.5)
                {
                    return Err(invalid("voice hysteresis", "0 to half of max distance"));
                }
                Ok(())
            }
        }
    }
}

/// `^[a-z][a-z0-9_-]{0,31}$`
fn valid_room_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// `^[a-z0-9:_-]{1,48}$`
fn valid_channel(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 48
        && key.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b':' | b'_' | b'-')
        })
}

/// A session's voice rooms (1–4). Until named rooms are enabled on the voice
/// service, players can join only the room `match`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceConfig {
    rooms: BTreeMap<String, VoiceRoom>,
}

impl VoiceConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces a room. Names: `^[a-z][a-z0-9_-]{0,31}$`, for example `match`.
    pub fn room(mut self, name: impl Into<String>, room: VoiceRoom) -> Self {
        self.rooms.insert(name.into(), room);
        self
    }

    /// The configuration in its wire shape (`{"rooms": {"match": {"mode": …}}}`),
    /// for example from a file. Checked like the builder.
    pub fn from_json(text: &str) -> Result<Self, Error> {
        let config: Self = serde_json::from_str(text).map_err(|_| {
            invalid(
                "voice configuration",
                "not the voice configuration's JSON shape",
            )
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn rooms(&self) -> impl Iterator<Item = (&str, &VoiceRoom)> {
        self.rooms.iter().map(|(name, room)| (name.as_str(), room))
    }

    fn validate(&self) -> Result<(), Error> {
        if self.rooms.is_empty() || self.rooms.len() > MAX_ROOMS {
            return Err(invalid("voice rooms", "1-4 rooms"));
        }
        for (name, room) in &self.rooms {
            if !valid_room_name(name) {
                return Err(invalid(
                    "voice room name",
                    "a-z first, then a-z 0-9 _ -, at most 32",
                ));
            }
            room.validate()?;
        }
        Ok(())
    }
}

/// One player's voice state, set by the game server. A new state replaces the
/// previous one; the mute is separate ([`VoiceControl::mute`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoicePlayerState {
    rooms: Option<Vec<String>>,
    channels: Vec<String>,
    alive: bool,
    spectator: bool,
    scene: Option<String>,
    can_speak: bool,
}

impl Default for VoicePlayerState {
    fn default() -> Self {
        Self {
            rooms: None,
            channels: Vec::new(),
            alive: true,
            spectator: false,
            scene: None,
            can_speak: true,
        }
    }
}

impl VoicePlayerState {
    /// Alive, not a spectator, may speak, no channels, every room.
    pub fn new() -> Self {
        Self::default()
    }

    /// The rooms this player may be in (default: every configured room).
    pub fn rooms<I, S>(mut self, rooms: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.rooms = Some(rooms.into_iter().map(Into::into).collect());
        self
    }

    /// Up to 8 keys `^[a-z0-9:_-]{1,48}$`, for example `team:red`, `squad:2`.
    pub fn channels<I, S>(mut self, channels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.channels = channels.into_iter().map(Into::into).collect();
        self
    }

    pub fn alive(mut self, alive: bool) -> Self {
        self.alive = alive;
        self
    }

    pub fn spectator(mut self, spectator: bool) -> Self {
        self.spectator = spectator;
        self
    }

    /// Players in different scenes never hear each other (1–48 characters).
    pub fn scene(mut self, scene: impl Into<String>) -> Self {
        self.scene = Some(scene.into());
        self
    }

    pub fn can_speak(mut self, can_speak: bool) -> Self {
        self.can_speak = can_speak;
        self
    }

    fn validate(&self) -> Result<(), Error> {
        if let Some(rooms) = &self.rooms {
            if rooms.len() > MAX_ROOMS || rooms.iter().any(|room| !valid_room_name(room)) {
                return Err(invalid("voice player rooms", "at most 4 room names"));
            }
        }
        if self.channels.len() > MAX_CHANNELS {
            return Err(invalid("voice channels", "at most 8"));
        }
        if self.channels.iter().any(|key| !valid_channel(key)) {
            return Err(invalid("voice channel", "1-48 of a-z 0-9 : _ -"));
        }
        if let Some(scene) = &self.scene {
            let length = scene.chars().count();
            if length == 0 || length > 48 || scene.chars().any(char::is_control) {
                return Err(invalid(
                    "voice scene",
                    "1-48 characters, no control characters",
                ));
            }
        }
        Ok(())
    }

    fn to_body(&self, seq: u64) -> Value {
        let mut body = json!({
            "seq": seq,
            "channels": self.channels,
            "alive": self.alive,
            "spectator": self.spectator,
            "canSpeak": self.can_speak,
        });
        if let Some(rooms) = &self.rooms {
            body["rooms"] = json!(rooms);
        }
        if let Some(scene) = &self.scene {
            body["scene"] = json!(scene);
        }
        body
    }
}

/// Where a player is: right-handed, y up, in metres. `yaw` (radians) is where
/// they face: 0 faces −z and it grows turning left (Godot's `rotation.y`). The
/// pan is computed in the x/z plane.
#[derive(Clone, Debug, PartialEq)]
pub struct VoicePosition {
    participant: ParticipantId,
    position: [f64; 3],
    yaw: Option<f64>,
}

impl VoicePosition {
    pub fn new(participant: &ParticipantId, position: [f64; 3]) -> Self {
        Self {
            participant: participant.clone(),
            position,
            yaw: None,
        }
    }

    /// A 2D game's (x, y), sent as `[x, 0, y]`: the voice service measures
    /// pan in the x/z plane.
    pub fn planar(participant: &ParticipantId, x: f64, y: f64) -> Self {
        Self::new(participant, [x, 0.0, y])
    }

    /// Radians; without it the listener gets no pan.
    pub fn yaw(mut self, yaw: f64) -> Self {
        self.yaw = Some(yaw);
        self
    }

    fn validate(&self) -> Result<(), Error> {
        if !valid_participant_id(self.participant.as_str()) {
            return Err(invalid(
                "voice position participant",
                "a participant id (gsp1.…)",
            ));
        }
        if self
            .position
            .iter()
            .any(|v| !v.is_finite() || v.abs() > MAX_COORDINATE)
        {
            return Err(invalid("voice position", "finite, |x| <= 1000000"));
        }
        if self
            .yaw
            .is_some_and(|yaw| !yaw.is_finite() || yaw.abs() > 1e3)
        {
            return Err(invalid("voice yaw", "finite radians, |yaw| <= 1000"));
        }
        Ok(())
    }

    fn to_body(&self) -> Value {
        let mut entry = json!({ "participantId": self.participant.as_str(), "p": self.position });
        if let Some(yaw) = self.yaw {
            entry["yaw"] = json!(yaw);
        }
        entry
    }
}

/// Participant ids as the edge issues them, checked by shape only.
fn valid_participant_id(id: &str) -> bool {
    id.len() > 5
        && id.len() <= 128
        && id.starts_with("gsp1.")
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The service's answer to a voice update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct VoiceAck {
    /// The rules are enforced now. False until the first player of the session
    /// joins voice (the session has no voice server before that), or while the
    /// voice server's link to the rules is down. Not an error.
    pub enforced: bool,
    /// A retried state or position update met "a newer update was already
    /// accepted": either an earlier attempt of this call landed, or a newer
    /// update replaced it. The service did not say whether the rules are
    /// enforced, so `enforced` is false.
    pub overtaken: bool,
}

#[derive(Deserialize)]
struct AckResponse {
    ok: bool,
    enforced: bool,
}

/// Per session handle: the next sequence number, and which players' states
/// were sent, for the admission rule's warning.
#[derive(Debug, Default)]
pub(crate) struct VoiceLocal {
    last_seq: u64,
    managed: bool,
    states_sent: HashSet<String>,
}

pub(crate) type VoiceShared = Arc<Mutex<VoiceLocal>>;

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl VoiceLocal {
    /// Increasing, and above anything an earlier process sent: the wall clock
    /// in ms, or one more than the last.
    fn next_seq(&mut self, now_ms: u64) -> Result<u64, Error> {
        let seq = now_ms.max(self.last_seq.saturating_add(1));
        if seq > MAX_SEQ {
            return Err(invalid("voice sequence", "the clock is beyond 2^53 ms"));
        }
        self.last_seq = seq;
        Ok(seq)
    }
}

/// Logs the admission rule's warning (a voice ticket for a player whose state
/// this session handle never sent, after it configured managed rooms).
pub(crate) fn warn_if_stateless(shared: &VoiceShared, participant: &ParticipantId) {
    let local = lock(shared);
    if local.managed && !local.states_sent.contains(participant.as_str()) {
        tracing::warn!(
            participant_id = %participant,
            "voice ticket issued before set_player_state: in a managed voice room this player hears no one and is heard by no one"
        );
    }
}

impl Session {
    /// This session's voice rules.
    pub fn voice(&self) -> VoiceControl {
        VoiceControl {
            session: self.clone(),
        }
    }
}

/// Voice rules for one session. Cheap to clone. Needs the `session.manage`
/// scope, a live lease, and voice enabled for the application and environment.
#[derive(Clone, Debug)]
pub struct VoiceControl {
    session: Session,
}

impl VoiceControl {
    pub fn session(&self) -> &Session {
        &self.session
    }

    fn path(&self, suffix: &str) -> String {
        format!(
            "/v1/server/sessions/{}{suffix}",
            segment(self.session.id().as_str())
        )
    }

    fn participant_path(&self, participant: &ParticipantId, suffix: &str) -> String {
        self.path(&format!(
            "/participants/{}/voice{suffix}",
            segment(participant.as_str())
        ))
    }

    fn next_seq(&self) -> Result<u64, Error> {
        lock(self.session.voice_shared()).next_seq(unix_ms())
    }

    async fn send(
        &self,
        method: reqwest::Method,
        route: &'static str,
        path: &str,
        body: &Value,
        sequenced: bool,
    ) -> Result<VoiceAck, Error> {
        // Every body is an absolute value (or ordered by `seq`): retrying is safe.
        let (result, attempts) = self
            .session
            .client()
            .edge_send_counted::<AckResponse>(method, route, path, Some(body), None, true)
            .await;
        match result {
            Ok(AckResponse { ok: true, enforced }) => Ok(VoiceAck {
                enforced,
                overtaken: false,
            }),
            Ok(_) => Err(Error::InvalidResponse("invalid voice answer")),
            Err(Error::Api(api))
                if sequenced && attempts > 1 && api.code == ErrorCode::VoiceStaleSequence =>
            {
                Ok(VoiceAck {
                    enforced: false,
                    overtaken: true,
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Replaces the session's voice rooms. 5 calls per 10 s per session.
    pub async fn configure(&self, config: &VoiceConfig) -> Result<VoiceAck, Error> {
        config.validate()?;
        let body = serde_json::to_value(config)
            .map_err(|_| invalid("voice configuration", "could not be encoded"))?;
        let ack = self
            .send(
                reqwest::Method::PUT,
                "PUT /v1/server/sessions/{id}/voice",
                &self.path("/voice"),
                &body,
                false,
            )
            .await?;
        lock(self.session.voice_shared()).managed = true;
        Ok(ack)
    }

    /// Sets one player's state. **Call it before issuing the player's voice
    /// ticket**: in a managed room a player without a state hears no one and
    /// is heard by no one. 10 calls per second per player, 200 per session.
    ///
    /// Updates carry an increasing sequence number, so of two racing updates
    /// the newer stands; the older answers [`ErrorCode::VoiceStaleSequence`].
    pub async fn set_player_state(
        &self,
        participant: &ParticipantId,
        state: &VoicePlayerState,
    ) -> Result<VoiceAck, Error> {
        state.validate()?;
        let body = state.to_body(self.next_seq()?);
        let result = self
            .send(
                reqwest::Method::PUT,
                "PUT /v1/server/sessions/{id}/participants/{participant}/voice",
                &self.participant_path(participant, ""),
                &body,
                true,
            )
            .await;
        let stale =
            matches!(&result, Err(Error::Api(api)) if api.code == ErrorCode::VoiceStaleSequence);
        if result.is_ok() || stale {
            lock(self.session.voice_shared())
                .states_sent
                .insert(participant.as_str().to_owned());
        }
        result
    }

    /// The authoritative mute: a muted player is forwarded to no one, also
    /// after reconnecting. Separate from the state, so a state update never
    /// unmutes. 20 calls per second per session.
    pub async fn mute(&self, participant: &ParticipantId, muted: bool) -> Result<VoiceAck, Error> {
        self.send(
            reqwest::Method::POST,
            "POST /v1/server/sessions/{id}/participants/{participant}/voice/mute",
            &self.participant_path(participant, "/mute"),
            &json!({ "muted": muted }),
            false,
        )
        .await
    }

    /// One batch of positions (1–256; a player listed twice keeps the last).
    /// Players who are not in a room are ignored by the service. A position
    /// counts fully for 1 s after it arrives, fades out from 1 to 3 s, and
    /// then grants nothing: send every player's position several times a
    /// second ([`Self::spawn_position_publisher`] does). 20 batches per 2 s
    /// per session.
    pub async fn publish_positions(&self, positions: &[VoicePosition]) -> Result<VoiceAck, Error> {
        if positions.is_empty() || positions.len() > MAX_POSITIONS {
            return Err(invalid("voice positions", "1-256 per batch"));
        }
        for position in positions {
            position.validate()?;
        }
        let body = json!({
            "seq": self.next_seq()?,
            "positions": positions.iter().map(VoicePosition::to_body).collect::<Vec<_>>(),
        });
        self.send(
            reqwest::Method::POST,
            "POST /v1/server/sessions/{id}/voice/positions",
            &self.path("/voice/positions"),
            &body,
            true,
        )
        .await
    }

    /// Sends the latest position of every player [`PositionPublisher::update`]
    /// was given, `hz` times a second (clamped to 1–10; 2 or more keeps every
    /// position inside the service's 1 s hold), on the current tokio runtime.
    /// Stops when the handle is dropped.
    pub fn spawn_position_publisher(&self, hz: u32) -> PositionPublisher {
        let period = Duration::from_secs_f64(1.0 / f64::from(hz.clamp(1, 10)));
        let shared = Arc::new(Mutex::new(PublisherState::default()));
        let state = shared.clone();
        let control = self.clone();
        let task = tokio::spawn(async move {
            let mut ticks = tokio::time::interval(period);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                let batch: Vec<VoicePosition> = lock(&state).latest.values().cloned().collect();
                for chunk in batch.chunks(MAX_POSITIONS) {
                    match control.publish_positions(chunk).await {
                        Ok(_) => lock(&state).last_error = None,
                        // Another writer is ahead; the next tick is newer still.
                        Err(Error::Api(api)) if api.code == ErrorCode::VoiceStaleSequence => {}
                        Err(error) => {
                            let wait = error.api().and_then(|api| {
                                (api.code == ErrorCode::RateLimited)
                                    .then(|| api.retry_after.unwrap_or(Duration::from_secs(1)))
                            });
                            tracing::warn!(session_id = %control.session.id(), %error, "gridspawn voice positions not sent");
                            lock(&state).last_error = Some(error);
                            if let Some(wait) = wait {
                                tokio::time::sleep(wait).await;
                            }
                            break;
                        }
                    }
                }
            }
        });
        PositionPublisher {
            state: shared,
            task,
        }
    }
}

#[derive(Default)]
struct PublisherState {
    latest: BTreeMap<String, VoicePosition>,
    last_error: Option<Error>,
}

/// Coalesces positions to the latest per player and sends them at a fixed
/// rate. Stops when dropped.
pub struct PositionPublisher {
    state: Arc<Mutex<PublisherState>>,
    task: tokio::task::JoinHandle<()>,
}

impl PositionPublisher {
    /// The player's latest position (cheap; call every tick). Checked here.
    pub fn update(
        &self,
        participant: &ParticipantId,
        position: [f64; 3],
        yaw: Option<f64>,
    ) -> Result<(), Error> {
        let mut entry = VoicePosition::new(participant, position);
        entry.yaw = yaw;
        self.put(entry)
    }

    /// As [`Self::update`] for a 2D game's (x, y): sent as `[x, 0, y]`.
    pub fn update_planar(
        &self,
        participant: &ParticipantId,
        x: f64,
        y: f64,
        yaw: Option<f64>,
    ) -> Result<(), Error> {
        let mut entry = VoicePosition::planar(participant, x, y);
        entry.yaw = yaw;
        self.put(entry)
    }

    fn put(&self, entry: VoicePosition) -> Result<(), Error> {
        entry.validate()?;
        lock(&self.state)
            .latest
            .insert(entry.participant.as_str().to_owned(), entry);
        Ok(())
    }

    /// Stops sending this player's position; its range audibility fades out
    /// 1–3 s later.
    pub fn remove(&self, participant: &ParticipantId) {
        lock(&self.state).latest.remove(participant.as_str());
    }

    /// The failure of the last batch, if it failed.
    pub fn last_error(&self) -> Option<Error> {
        lock(&self.state).last_error.clone()
    }

    pub fn stop(self) {}
}

impl Drop for PositionPublisher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl std::fmt::Debug for PositionPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PositionPublisher")
            .field("players", &lock(&self.state).latest.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(value: &str) -> ParticipantId {
        ParticipantId::new(value)
    }

    #[test]
    fn config_body_has_every_value_in_the_wire_shape() {
        let config = VoiceConfig::new().room(
            "match",
            VoiceRoom::proximity(2.0, 25.0)
                .rolloff(Rolloff::Inverse)
                .hysteresis(2.0)
                .max_audible_speakers(8),
        );
        config.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            json!({"rooms": {"match": {
                "mode": "proximity", "spatial": "server",
                "proximity": {"minDistance": 2.0, "maxDistance": 25.0, "rolloff": "inverse",
                              "rolloffFactor": 1.0, "hysteresis": 2.0},
                "ghosts": {"hearAlive": true, "aliveHearGhosts": false, "spectatorsHearDead": true},
                "maxParticipants": 64, "maxAudibleSpeakers": 8
            }}})
        );
        let open =
            serde_json::to_value(VoiceConfig::new().room("lobby", VoiceRoom::open())).unwrap();
        assert!(open["rooms"]["lobby"].get("proximity").is_none());
    }

    #[test]
    fn json_round_trip_uses_the_gateway_defaults() {
        let config =
            VoiceConfig::from_json(r#"{"rooms": {"match": {"mode": "channels"}}}"#).unwrap();
        assert_eq!(
            config,
            VoiceConfig::new().room("match", VoiceRoom::channels())
        );
        assert!(VoiceConfig::from_json(r#"{"rooms": {"match": {"mode": "x"}}}"#).is_err());
        assert!(
            VoiceConfig::from_json(r#"{"rooms": {"match": {"mode": "open", "extra": 1}}}"#)
                .is_err()
        );
        // Proximity with server spatial needs its distances.
        assert!(VoiceConfig::from_json(r#"{"rooms": {"match": {"mode": "proximity"}}}"#).is_err());
        assert!(VoiceConfig::from_json(
            r#"{"rooms": {"match": {"mode": "proximity", "spatial": "client"}}}"#
        )
        .is_ok());
    }

    #[test]
    fn config_bounds_are_checked_locally() {
        let room = |r: VoiceRoom| VoiceConfig::new().room("match", r).validate();
        assert!(VoiceConfig::new().validate().is_err(), "no rooms");
        let five = (0..5).fold(VoiceConfig::new(), |c, i| {
            c.room(format!("r{i}"), VoiceRoom::open())
        });
        assert!(five.validate().is_err(), "5 rooms");
        for name in ["Match", "1st", "", "a b", &"a".repeat(33)] {
            assert!(
                VoiceConfig::new()
                    .room(name, VoiceRoom::open())
                    .validate()
                    .is_err(),
                "{name:?}"
            );
        }
        assert!(VoiceConfig::new()
            .room("a".repeat(32), VoiceRoom::open())
            .validate()
            .is_ok());
        assert!(room(VoiceRoom::proximity(5.0, 5.0)).is_err());
        assert!(room(VoiceRoom::proximity(-1.0, 5.0)).is_err());
        assert!(room(VoiceRoom::proximity(0.0, 10_001.0)).is_err());
        assert!(room(VoiceRoom::proximity(0.0, f64::NAN)).is_err());
        assert!(room(VoiceRoom::proximity(0.0, 10.0).hysteresis(5.1)).is_err());
        assert!(room(VoiceRoom::proximity(0.0, 10.0).hysteresis(5.0)).is_ok());
        assert!(room(VoiceRoom::proximity(0.0, 10.0).rolloff_factor(0.0)).is_err());
        assert!(room(VoiceRoom::proximity(0.0, 10.0).rolloff_factor(100.5)).is_err());
        assert!(room(VoiceRoom::open().max_participants(0)).is_err());
        assert!(room(VoiceRoom::open().max_participants(65)).is_err());
        assert!(room(VoiceRoom::open().max_audible_speakers(0)).is_err());
        assert!(!VoiceRoom::channels().spatial_client().secrecy_safe());
    }

    #[test]
    fn state_body_and_bounds() {
        let state = VoicePlayerState::new()
            .channels(["team:red", "squad:2"])
            .alive(false)
            .scene("cave");
        assert_eq!(
            state.to_body(7),
            json!({"seq": 7, "channels": ["team:red", "squad:2"], "alive": false,
                   "spectator": false, "canSpeak": true, "scene": "cave"})
        );
        assert_eq!(
            VoicePlayerState::new().rooms(["match"]).to_body(1)["rooms"],
            json!(["match"])
        );
        assert!(VoicePlayerState::new()
            .channels((0..9).map(|i| format!("c{i}")))
            .validate()
            .is_err());
        assert!(VoicePlayerState::new()
            .channels(["Team:Red"])
            .validate()
            .is_err());
        assert!(VoicePlayerState::new()
            .channels(["x".repeat(49)])
            .validate()
            .is_err());
        assert!(VoicePlayerState::new().scene("").validate().is_err());
        assert!(VoicePlayerState::new()
            .scene("é".repeat(48))
            .validate()
            .is_ok());
        assert!(VoicePlayerState::new().scene("a\nb").validate().is_err());
        assert!(VoicePlayerState::new()
            .rooms(["a", "b", "c", "d", "e"])
            .validate()
            .is_err());
    }

    #[test]
    fn positions_planar_and_bounds() {
        let a = pid("gsp1.a");
        assert_eq!(
            VoicePosition::planar(&a, 3.0, 4.0).yaw(0.5).to_body(),
            json!({"participantId": "gsp1.a", "p": [3.0, 0.0, 4.0], "yaw": 0.5})
        );
        assert_eq!(
            VoicePosition::new(&a, [1.0, 2.0, 3.0]).to_body(),
            json!({"participantId": "gsp1.a", "p": [1.0, 2.0, 3.0]})
        );
        assert!(VoicePosition::new(&a, [1e6, 0.0, 0.0]).validate().is_ok());
        assert!(VoicePosition::new(&a, [1e6 + 1.0, 0.0, 0.0])
            .validate()
            .is_err());
        assert!(VoicePosition::new(&a, [f64::INFINITY, 0.0, 0.0])
            .validate()
            .is_err());
        assert!(VoicePosition::new(&a, [0.0; 3])
            .yaw(f64::NAN)
            .validate()
            .is_err());
        assert!(VoicePosition::new(&pid("player-1"), [0.0; 3])
            .validate()
            .is_err());
    }

    #[test]
    fn sequence_numbers_increase_and_survive_a_restart() {
        let mut local = VoiceLocal::default();
        assert_eq!(local.next_seq(1_000).unwrap(), 1_000);
        assert_eq!(local.next_seq(1_000).unwrap(), 1_001, "same millisecond");
        assert_eq!(local.next_seq(900).unwrap(), 1_002, "clock stepped back");
        assert_eq!(local.next_seq(5_000).unwrap(), 5_000);
        // A new process starts from the clock, above what the old one sent at
        // a realistic rate (far below 1000 updates per second).
        let mut restarted = VoiceLocal::default();
        assert!(restarted.next_seq(5_001).unwrap() > 5_000);
        assert!(VoiceLocal::default().next_seq(MAX_SEQ + 1).is_err());
    }
}
