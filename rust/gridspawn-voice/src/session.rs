//! A joined voice room: signaling, both transports, the microphone pipeline
//! and one jitter buffer per remote speaker, behind a PCM API.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

use gridspawn_client::{AttachedSession, Client, Module};
use serde_json::{json, Value};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
};
use url::Url;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};

use crate::{
    admission::{self, Admission},
    audio::{Apm, AudioOptions, Encoder, Vad, APM_FRAME, FRAME},
    error::VoiceError,
    jitter::JitterBuffer,
    media::{Added, MediaEvent, RecvLeg, SendLeg},
    roster::{pan_gains, Announced, Audibility, Peer, RoomConfig},
    sdp::{OpusCodec, TransportInfo},
    signaling::Signaling,
};

/// Renew the SFU token this long before it expires (04 §2).
const REFRESH_BEFORE: Duration = Duration::from_secs(20);
/// The shortest wait between renewal attempts.
const MIN_RENEW_WAIT: Duration = Duration::from_secs(5);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn clamp(samples: &mut [f32]) {
    for sample in samples {
        *sample = sample.clamp(-1.0, 1.0);
    }
}

/// How long to wait before renewing a token that lasts until `deadline`.
fn renew_wait(deadline: Instant, now: Instant) -> Duration {
    deadline
        .saturating_duration_since(now)
        .saturating_sub(REFRESH_BEFORE)
        .max(MIN_RENEW_WAIT)
}

/// How the microphone decides when to send.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TransmitMode {
    /// Open microphone, sent while voice is detected (the default).
    #[default]
    VoiceActivation,
    /// Sent while the push-to-talk key is held ([`VoiceSession::set_transmitting`]).
    PushToTalk,
    /// Always sent (except DTX silence).
    Continuous,
}

/// Join options.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct VoiceOptions {
    pub audio: AudioOptions,
    pub transmit: TransmitMode,
    /// The voice room to join (default `match`, which every session has;
    /// other rooms exist when the game server configured them).
    pub room: String,
}

impl Default for VoiceOptions {
    fn default() -> Self {
        Self {
            audio: AudioOptions::default(),
            transmit: TransmitMode::default(),
            room: admission::DEFAULT_ROOM.to_owned(),
        }
    }
}

impl VoiceOptions {
    pub fn room(mut self, room: impl Into<String>) -> Self {
        self.room = room.into();
        self
    }

    pub fn audio(mut self, audio: AudioOptions) -> Self {
        self.audio = audio;
        self
    }

    pub fn transmit(mut self, mode: TransmitMode) -> Self {
        self.transmit = mode;
        self
    }
}

/// Voice room events. Participant ids are the game session's participant ids
/// (`gsp1.…`, the ids your game server holds), never for display: show
/// [`ParticipantInfo::display_name`]. Against a voice service without a roster,
/// they are SFU endpoint ids instead.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum VoiceEvent {
    /// The server's decision for one speaker changed: whether this client
    /// hears them, and with which gain (multiplied with the local volume) and
    /// pan (−1 left … 1 right, `None` without one).
    Audibility {
        participant: String,
        audible: bool,
        gain: f32,
        pan: Option<f32>,
    },
    /// The room's rules arrived or changed.
    RoomConfig {
        config: RoomConfig,
    },
    /// Whether the voice service's rules are reachable. While false nothing is
    /// forwarded (it fails closed); the session stays joined.
    Authority {
        available: bool,
    },
    /// The game server muted or unmuted another participant.
    ServerMuted {
        participant: String,
        muted: bool,
    },
    /// The game server muted or unmuted this client: nothing it sends is
    /// forwarded, and unmuting locally is refused (`voice/server_muted`).
    LocalServerMuted {
        muted: bool,
    },
    /// A speaker could not be received (the receive negotiation failed twice).
    /// The session goes on; this participant is not heard until they speak
    /// again from a new producer (for example after rejoining).
    ReceiveFailed {
        participant: String,
        code: String,
        message: String,
    },
    ParticipantJoined {
        participant: String,
    },
    ParticipantLeft {
        participant: String,
    },
    SpeakingStarted {
        participant: String,
    },
    SpeakingStopped {
        participant: String,
    },
    /// The local microphone's voice activity (also with push-to-talk).
    LocalSpeaking {
        active: bool,
    },
    Error {
        code: String,
        message: String,
    },
    /// Something failed but is being retried (for example an authorization
    /// renewal); the session continues.
    Warning {
        code: String,
        message: String,
    },
    /// The session ended (leave, disconnect or a fatal error).
    Left {
        reason: String,
    },
}

/// One remote participant, as known to this client.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ParticipantInfo {
    /// The game session's participant id (see [`VoiceEvent`]). Not for display.
    pub id: String,
    /// The name to show. `None` against a voice service without a roster.
    pub display_name: Option<String>,
    pub speaking: bool,
    /// The local volume (0–2), set with [`VoiceSession::set_participant_volume`].
    pub volume: f32,
    /// The server lets this client hear them now (false while they send nothing).
    pub audible: bool,
    /// The server's gain (0–1), multiplied with `volume`.
    pub gain: f32,
    /// The server's pan (−1 left … 1 right), if any.
    pub pan: Option<f32>,
    /// Muted by the game server.
    pub server_muted: bool,
    /// Muted by this client ([`VoiceSession::set_participant_muted`]).
    pub locally_muted: bool,
}

/// Counters for diagnostics and tests.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct VoiceStats {
    /// Opus packets handed to the send transport.
    pub packets_sent: u64,
    /// Successful SFU authorization renewals (`refreshAuth`).
    pub auth_renewals: u64,
    /// Per participant: RTP packets received.
    pub packets_received: HashMap<String, u64>,
    /// Per participant: played frames that came from data (normal + FEC) and concealed ones.
    pub frames_from_data: HashMap<String, u64>,
    pub frames_concealed: HashMap<String, u64>,
    /// Per participant: packets dropped to keep the buffer delay bounded.
    pub frames_dropped: HashMap<String, u64>,
    /// Per participant: the jitter buffer's current target delay (ms).
    pub target_delay_ms: HashMap<String, u64>,
}

/// One consumed producer: a participant's voice on one of their endpoints.
struct Remote {
    participant: String,
    consumer_id: String,
    producer_id: String,
    ssrc: u32,
    jitter: Mutex<JitterBuffer>,
    pending: Mutex<VecDeque<f32>>,
    vad: Mutex<Vad>,
    speaking: AtomicBool,
    received: AtomicU64,
    audibility: Mutex<Audibility>,
    server_muted: AtomicBool,
}

/// A participant in the room (from the roster, or from their first voice
/// without one).
struct Participant {
    display_name: Option<String>,
    endpoints: HashSet<String>,
    volume: f32,
    locally_muted: bool,
}

impl Participant {
    fn new(display_name: Option<String>) -> Self {
        Self {
            display_name,
            endpoints: HashSet::new(),
            volume: 1.0,
            locally_muted: false,
        }
    }
}

struct Capture {
    apm: Apm,
    encoder: Encoder,
    vad: Vad,
    mic: VecDeque<f32>,
    render: VecDeque<f32>,
    frame: Vec<f32>,
}

#[derive(Default)]
struct Flags {
    muted: bool,
    deafened: bool,
    transmit: TransmitMode,
    ptt_pressed: bool,
}

#[cfg(feature = "impairment")]
#[derive(Clone, Copy, Default)]
struct Impairment {
    loss: f32,
    jitter_ms: u32,
}

struct Inner {
    attached: AttachedSession,
    http: reqwest::Client,
    voice_url: Url,
    signaling: Signaling,
    /// The room's name (for renewals) and the SFU's id for it.
    room: String,
    room_id: String,
    endpoint_id: String,
    /// Our participant id, or our endpoint id without a roster.
    local_participant: String,
    /// The SFU sends a roster (`peers`, `peerJoined`, `peerLeft`).
    roster_mode: bool,
    /// endpoint → participant, from the roster.
    roster: Mutex<HashMap<String, Peer>>,
    /// participant id → what we know of them.
    participants: Mutex<HashMap<String, Participant>>,
    /// Decisions that arrived before their consumer (by producer id).
    pending_audibility: Mutex<HashMap<String, Audibility>>,
    room_config: Mutex<Option<RoomConfig>>,
    authority_available: AtomicBool,
    local_server_muted: AtomicBool,
    node_id: String,
    /// When the SFU token stops working (monotonic; see `Admission::sfu_deadline`).
    sfu_deadline: Mutex<Instant>,
    send: SendLeg,
    recv: tokio::sync::Mutex<RecvLeg>,
    router_capabilities: Value,
    capture: Mutex<Capture>,
    frames_tx: mpsc::Sender<Vec<u8>>,
    packets_sent: AtomicU64,
    renewals: AtomicU64,
    /// consumer id → remote.
    remotes: Mutex<HashMap<String, Arc<Remote>>>,
    flags: Mutex<Flags>,
    epoch: Instant,
    events: broadcast::Sender<VoiceEvent>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    closed: AtomicBool,
    join_timings: Mutex<Vec<(String, Duration)>>,
    #[cfg(feature = "impairment")]
    impairment: Mutex<Impairment>,
}

/// Wall-clock time of each step of [`VoiceSession::join`].
pub(crate) struct JoinTimings {
    last: Instant,
    steps: Vec<(String, Duration)>,
}

impl JoinTimings {
    fn start() -> Self {
        Self {
            last: Instant::now(),
            steps: Vec::new(),
        }
    }

    /// Ends the current step.
    pub(crate) fn step(&mut self, name: &str) {
        let now = Instant::now();
        self.steps.push((name.to_owned(), now - self.last));
        self.last = now;
    }
}

/// A joined voice room. Cheap to clone.
#[derive(Clone)]
pub struct VoiceSession {
    inner: Arc<Inner>,
}

impl VoiceSession {
    /// Joins a voice room of the session (`options.room`, default `match`):
    /// admission, signaling, both transports, our producer, and consumers for
    /// everyone already speaking. Must run on a tokio runtime.
    pub async fn join(
        client: &Client,
        session: &AttachedSession,
        options: VoiceOptions,
    ) -> Result<Self, VoiceError> {
        if !session.capabilities().contains(&Module::Voice) {
            return Err(VoiceError::ModuleUnavailable);
        }
        let mut timings = JoinTimings::start();
        let voice_url = client
            .voice_url()
            .cloned()
            .ok_or(VoiceError::ModuleUnavailable)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| VoiceError::Network(Arc::new(e)))?;
        let grant = session.grant(Module::Voice).await?;
        timings.step("voice grant");
        let admission =
            admission::admit(&http, &voice_url, grant.as_str(), &options.room, None).await?;
        timings.step("admission");
        let (signaling, mut sfu_events) = Signaling::connect(&admission.sfu_url).await?;
        timings.step("signaling connect");
        let joined = signaling
            .request(
                "joinRoom",
                json!({ "roomId": admission.room_id, "peerId": admission.endpoint_id, "token": admission.sfu_token.as_str() }),
            )
            .await?;
        timings.step("joinRoom");
        let router_capabilities = joined["routerRtpCapabilities"].clone();
        let receive_pt = router_capabilities["codecs"]
            .as_array()
            .and_then(|codecs| {
                codecs.iter().find(|c| {
                    c["mimeType"]
                        .as_str()
                        .is_some_and(|m| m.eq_ignore_ascii_case("audio/opus"))
                })
            })
            .and_then(|c| c["preferredPayloadType"].as_u64())
            .filter(|pt| *pt < 128)
            .ok_or(VoiceError::InvalidResponse("router has no Opus"))?
            as u8;

        let (media_tx, mut media_rx) = mpsc::unbounded_channel();
        let send_transport = TransportInfo::parse(
            &signaling
                .request("createTransport", json!({ "roomId": admission.room_id }))
                .await?,
        )?;
        timings.step("createTransport (send)");
        let send = SendLeg::open(
            &signaling,
            &admission.room_id,
            send_transport,
            media_tx.clone(),
            &mut timings,
        )
        .await?;
        let recv_transport = TransportInfo::parse(
            &signaling
                .request("createTransport", json!({ "roomId": admission.room_id }))
                .await?,
        )?;
        timings.step("createTransport (receive)");
        let recv = RecvLeg::open(recv_transport, receive_pt, media_tx).await?;
        timings.step("receive peer connection");

        let bitrate = admission
            .audio_max_bps
            .map_or(options.audio.bitrate_bps, |max| {
                options.audio.bitrate_bps.min(max)
            });
        let capture = Capture {
            apm: Apm::new(&options.audio)?,
            encoder: Encoder::new(bitrate)?,
            vad: Vad::default(),
            mic: VecDeque::new(),
            render: VecDeque::new(),
            frame: Vec::with_capacity(FRAME),
        };
        let (frames_tx, mut frames_rx) = mpsc::channel::<Vec<u8>>(50);
        let (events, _) = broadcast::channel(256);
        let Admission {
            endpoint_id,
            participant_id,
            room_id,
            node_id,
            sfu_deadline,
            ..
        } = admission;
        // The roster (SFUs with server-side voice rules): participant ids and
        // display names instead of endpoint ids.
        let roster_mode = joined["peers"].is_array();
        let local_participant = match (&participant_id, roster_mode) {
            (Some(id), true) => id.clone(),
            _ => endpoint_id.clone(),
        };
        let mut roster = HashMap::new();
        let mut participants = HashMap::new();
        for peer in joined["peers"].as_array().into_iter().flatten() {
            let Some(peer) = Peer::parse(peer) else {
                continue;
            };
            if peer.participant != local_participant {
                participants
                    .entry(peer.participant.clone())
                    .or_insert_with(|| Participant::new(peer.display_name.clone()))
                    .endpoints
                    .insert(peer.endpoint.clone());
            }
            roster.insert(peer.endpoint.clone(), peer);
        }
        let inner = Arc::new(Inner {
            attached: session.clone(),
            http,
            voice_url,
            signaling,
            room: options.room.clone(),
            room_id,
            endpoint_id,
            local_participant,
            roster_mode,
            roster: Mutex::new(roster),
            participants: Mutex::new(participants),
            pending_audibility: Mutex::new(HashMap::new()),
            room_config: Mutex::new(RoomConfig::parse(&joined["voiceRoom"])),
            authority_available: AtomicBool::new(
                joined["voiceAuthority"]["available"]
                    .as_bool()
                    .unwrap_or(true),
            ),
            local_server_muted: AtomicBool::new(false),
            node_id,
            sfu_deadline: Mutex::new(sfu_deadline),
            send,
            recv: tokio::sync::Mutex::new(recv),
            router_capabilities,
            capture: Mutex::new(capture),
            frames_tx,
            packets_sent: AtomicU64::new(0),
            renewals: AtomicU64::new(0),
            remotes: Mutex::new(HashMap::new()),
            flags: Mutex::new(Flags {
                transmit: options.transmit,
                ..Flags::default()
            }),
            epoch: Instant::now(),
            events,
            tasks: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            join_timings: Mutex::new(Vec::new()),
            #[cfg(feature = "impairment")]
            impairment: Mutex::new(Impairment::default()),
        });
        let this = Self { inner };

        // Sender: encoded frames to the send transport.
        let weak = Arc::downgrade(&this.inner);
        let sender = tokio::spawn(async move {
            while let Some(packet) = frames_rx.recv().await {
                let Some(inner) = weak.upgrade() else { break };
                inner.send.send(packet).await;
            }
        });
        // Media events: remote tracks and transport failures.
        let weak = Arc::downgrade(&this.inner);
        let media = tokio::spawn(async move {
            while let Some(event) = media_rx.recv().await {
                let Some(inner) = weak.upgrade() else { break };
                let session = VoiceSession { inner };
                match event {
                    MediaEvent::Track(track) => session.read_track(track).await,
                    MediaEvent::Failed(leg) => {
                        session.fail(VoiceError::TransportFailed(format!(
                            "{leg} transport failed"
                        )));
                    }
                }
            }
        });
        lock(&this.inner.tasks).extend([sender, media]);

        // Everyone already speaking.
        if let Some(producers) = joined["producers"].as_array() {
            for producer in producers {
                this.consume(producer).await;
            }
            timings.step(&format!("consume existing speakers ({})", producers.len()));
        }
        *lock(&this.inner.join_timings) = timings.steps;
        // SFU events.
        let weak = Arc::downgrade(&this.inner);
        let signaling_task = tokio::spawn(async move {
            while let Some((name, payload)) = sfu_events.recv().await {
                let Some(inner) = weak.upgrade() else { return };
                let session = VoiceSession { inner };
                match name.as_str() {
                    "newProducer" => session.consume(&payload).await,
                    // A closed producer closes its consumers, which the SFU
                    // reports as consumerClosed: removal happens there.
                    "consumerClosed" => {
                        if let Some(id) = payload["consumerId"].as_str() {
                            session.drop_consumer(id).await;
                        }
                    }
                    "peerJoined" => session.peer_joined(&payload),
                    "peerLeft" => {
                        if let Some(endpoint) = payload["endpointId"].as_str() {
                            session.peer_left(endpoint);
                        }
                    }
                    "audibility" => {
                        if let Some((producer, audibility)) = Audibility::parse(&payload) {
                            session.audibility(&producer, audibility);
                        }
                    }
                    "voiceRoom" => {
                        if let Some(config) = RoomConfig::parse(&payload) {
                            *lock(&session.inner.room_config) = Some(config.clone());
                            session.emit(VoiceEvent::RoomConfig { config });
                        }
                    }
                    "voiceAuthority" => {
                        if let Some(available) = payload["available"].as_bool() {
                            let before = session
                                .inner
                                .authority_available
                                .swap(available, Ordering::SeqCst);
                            if before != available {
                                session.emit(VoiceEvent::Authority { available });
                            }
                        }
                    }
                    "producerPaused" if payload["reason"] == "server_mute" => {
                        session.producer_server_muted(&payload, true);
                    }
                    "producerResumed" => session.producer_server_muted(&payload, false),
                    _ => {}
                }
            }
            if let Some(inner) = weak.upgrade() {
                let session = VoiceSession { inner };
                if !session.inner.closed.load(Ordering::SeqCst) {
                    session.fail(VoiceError::SignalingDisconnected);
                }
            }
        });
        let weak = Arc::downgrade(&this.inner);
        let renewal = tokio::spawn(async move { renew_loop(weak).await });
        lock(&this.inner.tasks).extend([signaling_task, renewal]);
        Ok(this)
    }

    pub fn events(&self) -> broadcast::Receiver<VoiceEvent> {
        self.inner.events.subscribe()
    }

    /// How long each step of joining took (network round trips mostly), in
    /// order. For diagnostics.
    pub fn join_timings(&self) -> Vec<(String, Duration)> {
        lock(&self.inner.join_timings).clone()
    }

    /// This client's own participant id (the game session's; the SFU endpoint
    /// id against a voice service without a roster).
    pub fn local_id(&self) -> &str {
        &self.inner.local_participant
    }

    /// Everyone else in the room (with a roster), or everyone heard so far
    /// (without one), sorted by id.
    pub fn participants(&self) -> Vec<ParticipantInfo> {
        let ids: Vec<String> = lock(&self.inner.participants).keys().cloned().collect();
        let mut list: Vec<ParticipantInfo> =
            ids.iter().filter_map(|id| self.participant(id)).collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// One participant, if they are in the room.
    pub fn participant(&self, id: &str) -> Option<ParticipantInfo> {
        let (display_name, volume, locally_muted) = {
            let participants = lock(&self.inner.participants);
            let p = participants.get(id)?;
            (p.display_name.clone(), p.volume, p.locally_muted)
        };
        let remotes = self.remotes_of(id);
        let heard: Vec<Audibility> = remotes
            .iter()
            .map(|r| *lock(&r.audibility))
            .filter(|a| a.audible)
            .collect();
        Some(ParticipantInfo {
            id: id.to_owned(),
            display_name,
            speaking: remotes.iter().any(|r| r.speaking.load(Ordering::SeqCst)),
            volume,
            audible: !heard.is_empty(),
            gain: heard.iter().map(|a| a.gain).fold(0.0, f32::max),
            pan: heard.first().and_then(|a| a.pan),
            server_muted: remotes
                .iter()
                .any(|r| r.server_muted.load(Ordering::SeqCst)),
            locally_muted,
        })
    }

    /// The room's rules; `None` until the voice service sends them, and always
    /// for rooms without server rules (everyone hears everyone).
    pub fn room_config(&self) -> Option<RoomConfig> {
        lock(&self.inner.room_config).clone()
    }

    /// False while the voice service's rules are unreachable (nothing is
    /// forwarded then).
    pub fn authority_available(&self) -> bool {
        self.inner.authority_available.load(Ordering::SeqCst)
    }

    /// The game server muted this client.
    pub fn is_server_muted(&self) -> bool {
        self.inner.local_server_muted.load(Ordering::SeqCst)
    }

    /// Per participant (a participant on two endpoints is summed).
    pub fn stats(&self) -> VoiceStats {
        let mut stats = VoiceStats {
            packets_sent: self.inner.packets_sent.load(Ordering::SeqCst),
            auth_renewals: self.inner.renewals.load(Ordering::SeqCst),
            ..Default::default()
        };
        let remotes: Vec<Arc<Remote>> = lock(&self.inner.remotes).values().cloned().collect();
        for remote in remotes {
            let jitter = lock(&remote.jitter);
            let id = &remote.participant;
            *stats.packets_received.entry(id.clone()).or_default() +=
                remote.received.load(Ordering::SeqCst);
            *stats.frames_from_data.entry(id.clone()).or_default() +=
                jitter.stats.normal + jitter.stats.fec;
            *stats.frames_concealed.entry(id.clone()).or_default() += jitter.stats.concealed;
            *stats.frames_dropped.entry(id.clone()).or_default() +=
                jitter.stats.dropped_for_latency;
            stats.target_delay_ms.insert(id.clone(), jitter.target_ms());
        }
        stats
    }

    fn remotes_of(&self, participant: &str) -> Vec<Arc<Remote>> {
        lock(&self.inner.remotes)
            .values()
            .filter(|r| r.participant == participant)
            .cloned()
            .collect()
    }

    fn emit(&self, event: VoiceEvent) {
        let _ = self.inner.events.send(event);
    }

    /// Mutes our microphone: nothing is sent, and the SFU producer is paused.
    /// While the game server has muted this client ([`Self::is_server_muted`]),
    /// unmuting answers `voice/server_muted`.
    pub async fn set_muted(&self, muted: bool) -> Result<(), VoiceError> {
        lock(&self.inner.flags).muted = muted;
        let event = if muted {
            "pauseProducer"
        } else {
            "resumeProducer"
        };
        let result = self
            .inner
            .signaling
            .request(
                event,
                json!({ "roomId": self.inner.room_id, "producerId": self.inner.send.producer_id }),
            )
            .await
            .map(|_| ());
        // Refused because the game server muted us (for example before a
        // rejoin, which no event announces): stay muted, and say so.
        if let Err(VoiceError::Sfu { reason, .. }) = &result {
            if !muted && reason == "server_muted" {
                lock(&self.inner.flags).muted = true;
                if !self.inner.local_server_muted.swap(true, Ordering::SeqCst) {
                    self.emit(VoiceEvent::LocalServerMuted { muted: true });
                }
            }
        }
        result
    }

    pub fn is_muted(&self) -> bool {
        lock(&self.inner.flags).muted
    }

    /// Silences playback locally; remote audio keeps arriving. To stop one
    /// participant's audio from being sent at all, use
    /// [`Self::set_participant_muted`].
    pub fn set_deafened(&self, deafened: bool) {
        lock(&self.inner.flags).deafened = deafened;
    }

    pub fn set_transmit_mode(&self, mode: TransmitMode) {
        lock(&self.inner.flags).transmit = mode;
    }

    /// The push-to-talk key state (used with [`TransmitMode::PushToTalk`]).
    pub fn set_transmitting(&self, pressed: bool) {
        lock(&self.inner.flags).ptt_pressed = pressed;
    }

    /// Local volume for one participant (0.0–2.0), multiplied with the
    /// server's gain.
    pub fn set_participant_volume(&self, participant: &str, volume: f32) {
        if let Some(p) = lock(&self.inner.participants).get_mut(participant) {
            p.volume = volume.clamp(0.0, 2.0);
        }
    }

    /// Mutes one participant for this client only: the SFU stops sending
    /// their audio here (`pauseConsumer`), and it is not played. Unmuting
    /// asks the SFU to resume; if the server does not let this client hear
    /// them now, they stay silent until it does (that is not an error).
    pub async fn set_participant_muted(
        &self,
        participant: &str,
        muted: bool,
    ) -> Result<(), VoiceError> {
        match lock(&self.inner.participants).get_mut(participant) {
            Some(p) => p.locally_muted = muted,
            None => return Ok(()),
        }
        for remote in self.remotes_of(participant) {
            self.pause_consumer(&remote.consumer_id, muted).await?;
        }
        Ok(())
    }

    async fn pause_consumer(&self, consumer_id: &str, paused: bool) -> Result<(), VoiceError> {
        let event = if paused {
            "pauseConsumer"
        } else {
            "resumeConsumer"
        };
        match self
            .inner
            .signaling
            .request(
                event,
                json!({ "roomId": self.inner.room_id, "consumerId": consumer_id }),
            )
            .await
        {
            // The SFU stored the client's wish; the server's rules still apply.
            Err(VoiceError::Sfu { reason, .. }) if reason == "not_audible" => Ok(()),
            other => other.map(|_| ()),
        }
    }

    /// Microphone samples (mono, 48 kHz), any length. Processed (echo
    /// cancellation, noise suppression, gain), encoded and sent per 20 ms.
    /// Call it from the audio or game thread; it never waits on the network.
    pub fn push_capture(&self, samples: &[f32]) {
        let (transmit, muted, ptt) = {
            let flags = lock(&self.inner.flags);
            (flags.transmit, flags.muted, flags.ptt_pressed)
        };
        let mut capture = lock(&self.inner.capture);
        capture.mic.extend(samples.iter().copied());
        while capture.mic.len() >= APM_FRAME {
            let mut chunk: Vec<f32> = capture.mic.drain(..APM_FRAME).collect();
            if let Err(error) = capture.apm.capture(&mut chunk) {
                tracing::debug!(%error, "audio processing failed for one frame");
            }
            capture.frame.extend_from_slice(&chunk);
            if capture.frame.len() < FRAME {
                continue;
            }
            let frame = std::mem::replace(&mut capture.frame, Vec::with_capacity(FRAME));
            if let Some(active) = capture.vad.update(&frame) {
                let _ = self.inner.events.send(VoiceEvent::LocalSpeaking { active });
            }
            let send = !muted
                && match transmit {
                    TransmitMode::VoiceActivation => capture.vad.active(),
                    TransmitMode::PushToTalk => ptt,
                    TransmitMode::Continuous => true,
                };
            if !send {
                continue;
            }
            match capture.encoder.encode(&frame) {
                Ok(Some(packet)) => {
                    if self.inner.frames_tx.try_send(packet).is_ok() {
                        self.inner.packets_sent.fetch_add(1, Ordering::SeqCst);
                    }
                }
                Ok(None) => {}
                Err(error) => tracing::debug!(%error, "encoding failed for one frame"),
            }
        }
    }

    /// What is played to the speakers (mono, 48 kHz): the echo canceller's
    /// reference. Push it as it is played, including the game's own sound.
    pub fn push_playback_reference(&self, samples: &[f32]) {
        let mut capture = lock(&self.inner.capture);
        capture.render.extend(samples.iter().copied());
        while capture.render.len() >= APM_FRAME {
            let mut chunk: Vec<f32> = capture.render.drain(..APM_FRAME).collect();
            if let Err(error) = capture.apm.render(&mut chunk) {
                tracing::debug!(%error, "audio processing failed for one reference frame");
            }
        }
    }

    /// One participant's decoded audio (mono, 48 kHz) into `out`, with their
    /// volume and the server's gain applied (no pan). Use either the
    /// per-participant pulls or the mix pulls, not both.
    pub fn pull_participant(&self, participant: &str, out: &mut [f32]) {
        out.fill(0.0);
        let mut one = vec![0f32; out.len()];
        for remote in self.remotes_of(participant) {
            self.fill(&remote, &mut one);
            for (o, s) in out.iter_mut().zip(&one) {
                *o = (*o + *s).clamp(-1.0, 1.0);
            }
        }
    }

    /// As [`Self::pull_participant`], in stereo (`out` interleaved L, R) with
    /// the server's pan (equal-power; the centre keeps both channels at full
    /// level).
    pub fn pull_participant_stereo(&self, participant: &str, out: &mut [f32]) {
        out.fill(0.0);
        for remote in self.remotes_of(participant) {
            self.add_stereo(&remote, out);
        }
        clamp(out);
    }

    /// Every participant mixed into `out` (mono, 48 kHz), without pan.
    pub fn pull_mix(&self, out: &mut [f32]) {
        out.fill(0.0);
        let remotes: Vec<Arc<Remote>> = lock(&self.inner.remotes).values().cloned().collect();
        let mut one = vec![0f32; out.len()];
        for remote in remotes {
            self.fill(&remote, &mut one);
            for (o, s) in out.iter_mut().zip(&one) {
                *o += *s;
            }
        }
        clamp(out);
    }

    /// Every participant mixed in stereo (`out` interleaved L, R), each with
    /// the server's pan.
    pub fn pull_mix_stereo(&self, out: &mut [f32]) {
        out.fill(0.0);
        let remotes: Vec<Arc<Remote>> = lock(&self.inner.remotes).values().cloned().collect();
        for remote in remotes {
            self.add_stereo(&remote, out);
        }
        clamp(out);
    }

    /// Adds one remote's audio, panned, to an interleaved stereo buffer.
    fn add_stereo(&self, remote: &Remote, out: &mut [f32]) {
        let mut mono = vec![0f32; out.len() / 2];
        self.fill(remote, &mut mono);
        let (left, right) = pan_gains(lock(&remote.audibility).pan);
        for (pair, sample) in out.chunks_exact_mut(2).zip(&mono) {
            pair[0] += sample * left;
            pair[1] += sample * right;
        }
    }

    /// The next `out.len()` samples of one remote, with the local volume and
    /// the server's gain (silence while deafened, muted locally, or inaudible).
    fn fill(&self, remote: &Remote, out: &mut [f32]) {
        let deafened = lock(&self.inner.flags).deafened;
        let (volume, locally_muted) = lock(&self.inner.participants)
            .get(&remote.participant)
            .map_or((1.0, false), |p| (p.volume, p.locally_muted));
        let audibility = *lock(&remote.audibility);
        let level = if deafened || locally_muted || !audibility.audible {
            0.0
        } else {
            volume * audibility.gain
        };
        let mut pending = lock(&remote.pending);
        while pending.len() < out.len() {
            let mut frame = vec![0f32; FRAME];
            let now = self.inner.epoch.elapsed().as_millis() as u64;
            lock(&remote.jitter).pull(now, &mut frame);
            if let Some(active) = lock(&remote.vad).update(&frame) {
                remote.speaking.store(active, Ordering::SeqCst);
                let participant = remote.participant.clone();
                self.emit(if active {
                    VoiceEvent::SpeakingStarted { participant }
                } else {
                    VoiceEvent::SpeakingStopped { participant }
                });
            }
            pending.extend(frame);
        }
        let wanted = out.len();
        for (o, s) in out.iter_mut().zip(pending.drain(..wanted)) {
            *o = s * level;
        }
    }

    /// Test-only receive impairment, applied before the jitter buffer.
    #[cfg(feature = "impairment")]
    #[doc(hidden)]
    pub fn set_receive_impairment(&self, loss: f32, jitter_ms: u32) {
        *lock(&self.inner.impairment) = Impairment { loss, jitter_ms };
    }

    /// Leaves the room: closes our producer, both transports and the socket.
    pub async fn leave(&self) {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self
            .inner
            .signaling
            .request(
                "closeProducer",
                json!({ "roomId": self.inner.room_id, "producerId": self.inner.send.producer_id }),
            )
            .await;
        self.shutdown("left").await;
    }

    async fn shutdown(&self, reason: &str) {
        self.inner.send.close().await;
        self.inner.recv.lock().await.close().await;
        self.inner.signaling.close();
        for task in lock(&self.inner.tasks).drain(..) {
            task.abort();
        }
        lock(&self.inner.remotes).clear();
        let _ = self.inner.events.send(VoiceEvent::Left {
            reason: reason.to_owned(),
        });
    }

    fn fail(&self, error: VoiceError) {
        let _ = self.inner.events.send(VoiceEvent::Error {
            code: error.code(),
            message: error.to_string(),
        });
        if !self.inner.closed.swap(true, Ordering::SeqCst) {
            let session = self.clone();
            let code = error.code();
            tokio::spawn(async move { session.shutdown(&code).await });
        }
    }

    /// Consumes one announced producer if it is someone else's voice.
    async fn consume(&self, producer: &Value) {
        let Some(Announced {
            producer_id,
            owner,
            server_muted,
        }) = Announced::parse(producer)
        else {
            return;
        };
        let producer_id = producer_id.as_str();
        if owner == self.inner.endpoint_id {
            return;
        }
        let participant = self.participant_of(&owner);
        // Our own participant on another device: never played back to us,
        // but its descriptor tells that the game server muted us (a mute is
        // per participant).
        if participant == self.inner.local_participant {
            if server_muted && !self.inner.local_server_muted.swap(true, Ordering::SeqCst) {
                self.emit(VoiceEvent::LocalServerMuted { muted: true });
            }
            return;
        }
        let mut recv = self.inner.recv.lock().await;
        // Announced twice (in joinRoom's list and as newProducer): once is enough.
        if lock(&self.inner.remotes)
            .values()
            .any(|r| r.producer_id == producer_id)
        {
            return;
        }
        let consumed = match self
            .inner
            .signaling
            .request(
                "consume",
                json!({
                    "roomId": self.inner.room_id, "producerId": producer_id,
                    "transportId": recv.transport_id(), "rtpCapabilities": self.inner.router_capabilities
                }),
            )
            .await
        {
            Ok(consumed) => consumed,
            Err(error) => {
                tracing::warn!(%error, "voice consume failed");
                return;
            }
        };
        let params = &consumed["rtpParameters"];
        let (Ok(codec), Some(ssrc), Some(consumer_id)) = (
            OpusCodec::from_rtp_codec(&params["codecs"][0]),
            params["encodings"][0]["ssrc"].as_u64(),
            consumed["consumerId"].as_str(),
        ) else {
            tracing::warn!("voice consumer without usable parameters");
            return;
        };
        let cname = params["rtcp"]["cname"]
            .as_str()
            .unwrap_or("gridspawn")
            .to_owned();
        let Ok(jitter) = JitterBuffer::new() else {
            return;
        };
        // A decision that arrived before the consumer applies now.
        let audibility = lock(&self.inner.pending_audibility)
            .remove(producer_id)
            .unwrap_or_default();
        let remote = Arc::new(Remote {
            participant: participant.clone(),
            consumer_id: consumer_id.to_owned(),
            producer_id: producer_id.to_owned(),
            ssrc: ssrc as u32,
            jitter: Mutex::new(jitter),
            pending: Mutex::new(VecDeque::new()),
            vad: Mutex::new(Vad::default()),
            speaking: AtomicBool::new(false),
            received: AtomicU64::new(0),
            audibility: Mutex::new(audibility),
            // Muted before we joined (or before this producer existed):
            // `producerPaused` reports only later changes.
            server_muted: AtomicBool::new(server_muted),
        });
        lock(&self.inner.remotes).insert(consumer_id.to_owned(), remote);
        let added = recv
            .add(
                &self.inner.signaling,
                &self.inner.room_id,
                codec,
                ssrc as u32,
                cname,
            )
            .await;
        drop(recv);
        match added {
            Ok(Added::First) => {}
            Ok(Added::Retried(error)) => self.emit(VoiceEvent::Warning {
                code: "voice/receive_retried".into(),
                message: format!("receiving {participant} needed a second negotiation: {error}"),
            }),
            Err(error) => {
                // Not heard, and said so; the session goes on. The SFU stops
                // forwarding to the closed section.
                lock(&self.inner.remotes).remove(consumer_id);
                let _ = self
                    .inner
                    .signaling
                    .request(
                        "closeConsumer",
                        json!({ "roomId": self.inner.room_id, "consumerId": consumer_id }),
                    )
                    .await;
                tracing::warn!(%participant, %error, "voice: a speaker could not be added");
                self.emit(VoiceEvent::ReceiveFailed {
                    participant,
                    code: error.code(),
                    message: error.to_string(),
                });
                return;
            }
        }
        // Without a roster, a participant appears with their first voice.
        let (joined, locally_muted) = {
            let mut participants = lock(&self.inner.participants);
            let joined = !participants.contains_key(&participant);
            let p = participants
                .entry(participant.clone())
                .or_insert_with(|| Participant::new(None));
            p.endpoints.insert(owner);
            (joined, p.locally_muted)
        };
        if joined && !self.inner.roster_mode {
            self.emit(VoiceEvent::ParticipantJoined {
                participant: participant.clone(),
            });
        }
        if server_muted {
            self.emit(VoiceEvent::ServerMuted {
                participant,
                muted: true,
            });
        }
        if locally_muted {
            if let Err(error) = self.pause_consumer(consumer_id, true).await {
                tracing::warn!(%error, "voice: pausing a locally muted participant failed");
            }
        }
    }

    /// The participant behind an SFU endpoint (the endpoint itself without a roster).
    fn participant_of(&self, endpoint: &str) -> String {
        lock(&self.inner.roster)
            .get(endpoint)
            .map_or_else(|| endpoint.to_owned(), |peer| peer.participant.clone())
    }

    /// Drops the consumer the SFU closed (its producer is gone). Without a
    /// roster, a participant leaves with their last voice; with one, they
    /// leave when the roster says so.
    async fn drop_consumer(&self, consumer_id: &str) {
        let Some(remote) = lock(&self.inner.remotes).remove(consumer_id) else {
            return;
        };
        let removal = self
            .inner
            .recv
            .lock()
            .await
            .remove(&self.inner.signaling, &self.inner.room_id, remote.ssrc)
            .await;
        // The leg is back to stable either way; later speakers still join.
        if let Err(error) = removal {
            tracing::warn!(%error, "voice receive renegotiation after a removal failed");
            self.emit(VoiceEvent::Warning {
                code: error.code(),
                message: error.to_string(),
            });
        }
        if remote.speaking.swap(false, Ordering::SeqCst) {
            self.emit(VoiceEvent::SpeakingStopped {
                participant: remote.participant.clone(),
            });
        }
        if !self.inner.roster_mode && self.remotes_of(&remote.participant).is_empty() {
            lock(&self.inner.participants).remove(&remote.participant);
            self.emit(VoiceEvent::ParticipantLeft {
                participant: remote.participant.clone(),
            });
        }
    }

    /// `peerJoined`: someone entered the room (they may not speak yet).
    fn peer_joined(&self, value: &Value) {
        let Some(peer) = Peer::parse(value) else {
            return;
        };
        let participant = peer.participant.clone();
        lock(&self.inner.roster).insert(peer.endpoint.clone(), peer.clone());
        if participant == self.inner.local_participant {
            return;
        }
        let joined = {
            let mut participants = lock(&self.inner.participants);
            let joined = !participants.contains_key(&participant);
            let p = participants
                .entry(participant.clone())
                .or_insert_with(|| Participant::new(peer.display_name.clone()));
            if p.display_name.is_none() {
                p.display_name = peer.display_name;
            }
            p.endpoints.insert(peer.endpoint);
            joined
        };
        if joined {
            self.emit(VoiceEvent::ParticipantJoined { participant });
        }
    }

    /// `peerLeft`: an endpoint left; its participant leaves with their last one.
    fn peer_left(&self, endpoint: &str) {
        let Some(peer) = lock(&self.inner.roster).remove(endpoint) else {
            return;
        };
        let left = {
            let mut participants = lock(&self.inner.participants);
            match participants.get_mut(&peer.participant) {
                Some(p) => {
                    p.endpoints.remove(endpoint);
                    let gone = p.endpoints.is_empty();
                    if gone {
                        participants.remove(&peer.participant);
                    }
                    gone
                }
                None => false,
            }
        };
        if left {
            self.emit(VoiceEvent::ParticipantLeft {
                participant: peer.participant,
            });
        }
    }

    /// `audibility`: the server's decision for one of our consumers. On
    /// becoming audible again the jitter buffer starts over, so the first
    /// packets after a pause are not played as a long concealment run.
    fn audibility(&self, producer: &str, next: Audibility) {
        let remote = lock(&self.inner.remotes)
            .values()
            .find(|r| r.producer_id == producer)
            .cloned();
        let Some(remote) = remote else {
            lock(&self.inner.pending_audibility).insert(producer.to_owned(), next);
            return;
        };
        let before = std::mem::replace(&mut *lock(&remote.audibility), next);
        if before == next {
            return;
        }
        if before.audible && !next.audible && remote.speaking.swap(false, Ordering::SeqCst) {
            self.emit(VoiceEvent::SpeakingStopped {
                participant: remote.participant.clone(),
            });
        }
        if !before.audible && next.audible {
            if let Ok(fresh) = JitterBuffer::new() {
                *lock(&remote.jitter) = fresh;
            }
            lock(&remote.pending).clear();
            *lock(&remote.vad) = Vad::default();
        }
        self.emit(VoiceEvent::Audibility {
            participant: remote.participant.clone(),
            audible: next.audible,
            gain: next.gain,
            pan: next.pan,
        });
    }

    /// `producerPaused` (`server_mute`) / `producerResumed`: ours or another's.
    fn producer_server_muted(&self, value: &Value, muted: bool) {
        let Some(producer) = value["producerId"].as_str() else {
            return;
        };
        if producer == self.inner.send.producer_id {
            if self.inner.local_server_muted.swap(muted, Ordering::SeqCst) != muted {
                self.emit(VoiceEvent::LocalServerMuted { muted });
            }
            return;
        }
        let remote = lock(&self.inner.remotes)
            .values()
            .find(|r| r.producer_id == producer)
            .cloned();
        if let Some(remote) = remote {
            if remote.server_muted.swap(muted, Ordering::SeqCst) != muted {
                self.emit(VoiceEvent::ServerMuted {
                    participant: remote.participant.clone(),
                    muted,
                });
            }
        }
    }

    /// Feeds one remote track's RTP packets into its participant's jitter buffer.
    async fn read_track(&self, track: Arc<dyn TrackRemote>) {
        let ssrcs = track.ssrcs().await;
        let remote = {
            let remotes = lock(&self.inner.remotes);
            remotes.values().find(|r| ssrcs.contains(&r.ssrc)).cloned()
        };
        let Some(remote) = remote else {
            tracing::debug!("voice track without a known consumer");
            return;
        };
        let epoch = self.inner.epoch;
        #[cfg(feature = "impairment")]
        let weak = Arc::downgrade(&self.inner);
        let task = tokio::spawn(async move {
            while let Some(event) = track.poll().await {
                let TrackRemoteEvent::OnRtpPacket(packet) = event else {
                    continue;
                };
                remote.received.fetch_add(1, Ordering::SeqCst);
                let (seq, ts) = (packet.header.sequence_number, packet.header.timestamp);
                let payload = packet.payload.to_vec();
                #[cfg(feature = "impairment")]
                {
                    let impairment = weak
                        .upgrade()
                        .map(|inner| *lock(&inner.impairment))
                        .unwrap_or_default();
                    if impairment.loss > 0.0 || impairment.jitter_ms > 0 {
                        let mut byte = [0u8; 4];
                        let _ = getrandom::fill(&mut byte);
                        let roll = u32::from_le_bytes(byte) as f32 / u32::MAX as f32;
                        if roll < impairment.loss {
                            continue;
                        }
                        let delay = (u32::from_le_bytes(byte).rotate_left(13)
                            % (2 * impairment.jitter_ms + 1))
                            as u64;
                        let remote = remote.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(delay)).await;
                            let now = epoch.elapsed().as_millis() as u64;
                            lock(&remote.jitter).push(seq, ts, payload, now);
                        });
                        continue;
                    }
                }
                let now = epoch.elapsed().as_millis() as u64;
                lock(&remote.jitter).push(seq, ts, payload, now);
            }
        });
        lock(&self.inner.tasks).push(task);
    }
}

/// Renews the SFU token about 20 s before it expires: a new admission for the
/// same endpoint (with a fresh voice grant), then `refreshAuth`.
async fn renew_loop(weak: std::sync::Weak<Inner>) {
    loop {
        let wait = {
            let Some(inner) = weak.upgrade() else { return };
            let deadline = *lock(&inner.sfu_deadline);
            renew_wait(deadline, Instant::now())
        };
        tokio::time::sleep(wait).await;
        let Some(inner) = weak.upgrade() else { return };
        if inner.closed.load(Ordering::SeqCst) {
            return;
        }
        let session = VoiceSession { inner };
        match session.refresh().await {
            Ok(()) => {}
            Err(VoiceError::ReconnectRequired) => {
                session.fail(VoiceError::ReconnectRequired);
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "voice authorization renewal failed; retrying");
                let _ = session.inner.events.send(VoiceEvent::Warning {
                    code: error.code(),
                    message: error.to_string(),
                });
                let expired = *lock(&session.inner.sfu_deadline) <= Instant::now();
                if expired {
                    session.fail(VoiceError::AuthRefreshFailed);
                    return;
                }
            }
        }
    }
}

impl VoiceSession {
    async fn refresh(&self) -> Result<(), VoiceError> {
        let inner = &self.inner;
        let grant = inner.attached.grant(Module::Voice).await?;
        let admission = admission::admit(
            &inner.http,
            &inner.voice_url,
            grant.as_str(),
            &inner.room,
            Some(&inner.endpoint_id),
        )
        .await?;
        if admission.endpoint_id != inner.endpoint_id
            || admission.room_id != inner.room_id
            || admission.node_id != inner.node_id
        {
            return Err(VoiceError::ReconnectRequired);
        }
        inner
            .signaling
            .request(
                "refreshAuth",
                json!({ "roomId": inner.room_id, "token": admission.sfu_token.as_str() }),
            )
            .await?;
        *lock(&inner.sfu_deadline) = admission.sfu_deadline;
        inner.renewals.fetch_add(1, Ordering::SeqCst);
        tracing::debug!("voice authorization renewed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renewal is scheduled on the monotonic clock from the token's own
    /// lifetime: 20 s before the deadline, at least 5 s apart.
    #[test]
    fn renewal_waits_are_monotonic_and_bounded() {
        let now = Instant::now();
        let at = |secs| now + Duration::from_secs(secs);
        assert_eq!(renew_wait(at(60), now), Duration::from_secs(40));
        assert_eq!(renew_wait(at(22), now), MIN_RENEW_WAIT);
        assert_eq!(renew_wait(now, now), MIN_RENEW_WAIT, "already due");
        assert_eq!(renew_wait(now, at(30)), MIN_RENEW_WAIT, "past the deadline");
    }
}
