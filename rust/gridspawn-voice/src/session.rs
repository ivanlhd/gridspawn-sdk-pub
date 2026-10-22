//! A joined voice room: signaling, both transports, the microphone pipeline
//! and one jitter buffer per remote speaker, behind a PCM API.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
    media::{MediaEvent, RecvLeg, SendLeg},
    sdp::{OpusCodec, TransportInfo},
    signaling::Signaling,
};

/// Renew the SFU token this long before it expires (04 §2).
const REFRESH_BEFORE: i64 = 20;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
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
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct VoiceOptions {
    pub audio: AudioOptions,
    pub transmit: TransmitMode,
}

impl VoiceOptions {
    pub fn audio(mut self, audio: AudioOptions) -> Self {
        self.audio = audio;
        self
    }

    pub fn transmit(mut self, mode: TransmitMode) -> Self {
        self.transmit = mode;
        self
    }
}

/// Voice room events. Participant ids are opaque (today the SFU endpoint id),
/// never for display; display names are not available yet.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum VoiceEvent {
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
    /// Opaque id (the SFU endpoint). Not for display.
    pub id: String,
    /// Always `None` until the SFU sends a roster with display names.
    pub display_name: Option<String>,
    pub speaking: bool,
    pub volume: f32,
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

struct Remote {
    id: String,
    consumer_id: String,
    ssrc: u32,
    jitter: Mutex<JitterBuffer>,
    pending: Mutex<VecDeque<f32>>,
    volume: Mutex<f32>,
    vad: Mutex<Vad>,
    speaking: AtomicBool,
    received: AtomicU64,
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
    room_id: String,
    endpoint_id: String,
    node_id: String,
    sfu_expires_at: AtomicI64,
    send: SendLeg,
    recv: tokio::sync::Mutex<RecvLeg>,
    router_capabilities: Value,
    capture: Mutex<Capture>,
    frames_tx: mpsc::Sender<Vec<u8>>,
    packets_sent: AtomicU64,
    renewals: AtomicU64,
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
    /// Joins the session's voice room (`match`): admission, signaling, both
    /// transports, our producer, and consumers for everyone already speaking.
    /// Must run on a tokio runtime.
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
        let admission = admission::admit(&http, &voice_url, grant.as_str(), None).await?;
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
            room_id,
            node_id,
            sfu_expires_at,
            ..
        } = admission;
        let inner = Arc::new(Inner {
            attached: session.clone(),
            http,
            voice_url,
            signaling,
            room_id,
            endpoint_id,
            node_id,
            sfu_expires_at: AtomicI64::new(sfu_expires_at),
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

    /// This client's own participant id (the SFU endpoint id).
    pub fn local_id(&self) -> &str {
        &self.inner.endpoint_id
    }

    pub fn participants(&self) -> Vec<ParticipantInfo> {
        let mut list: Vec<ParticipantInfo> = lock(&self.inner.remotes)
            .values()
            .map(|r| ParticipantInfo {
                id: r.id.clone(),
                display_name: None,
                speaking: r.speaking.load(Ordering::SeqCst),
                volume: *lock(&r.volume),
            })
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    pub fn stats(&self) -> VoiceStats {
        let mut stats = VoiceStats {
            packets_sent: self.inner.packets_sent.load(Ordering::SeqCst),
            auth_renewals: self.inner.renewals.load(Ordering::SeqCst),
            ..Default::default()
        };
        for remote in lock(&self.inner.remotes).values() {
            let jitter = lock(&remote.jitter);
            stats
                .packets_received
                .insert(remote.id.clone(), remote.received.load(Ordering::SeqCst));
            stats
                .frames_from_data
                .insert(remote.id.clone(), jitter.stats.normal + jitter.stats.fec);
            stats
                .frames_concealed
                .insert(remote.id.clone(), jitter.stats.concealed);
            stats
                .frames_dropped
                .insert(remote.id.clone(), jitter.stats.dropped_for_latency);
            stats
                .target_delay_ms
                .insert(remote.id.clone(), jitter.target_ms());
        }
        stats
    }

    /// Mutes our microphone: nothing is sent, and the SFU producer is paused.
    pub async fn set_muted(&self, muted: bool) -> Result<(), VoiceError> {
        lock(&self.inner.flags).muted = muted;
        let event = if muted {
            "pauseProducer"
        } else {
            "resumeProducer"
        };
        self.inner
            .signaling
            .request(
                event,
                json!({ "roomId": self.inner.room_id, "producerId": self.inner.send.producer_id }),
            )
            .await
            .map(|_| ())
    }

    pub fn is_muted(&self) -> bool {
        lock(&self.inner.flags).muted
    }

    /// Silences playback locally. Remote audio keeps arriving: the SFU has no
    /// per-listener pause yet.
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

    /// Local volume for one participant (0.0–2.0).
    pub fn set_participant_volume(&self, participant: &str, volume: f32) {
        if let Some(remote) = lock(&self.inner.remotes).get(participant) {
            *lock(&remote.volume) = volume.clamp(0.0, 2.0);
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
    /// volume applied. Use either this per participant or [`Self::pull_mix`].
    pub fn pull_participant(&self, participant: &str, out: &mut [f32]) {
        let remote = lock(&self.inner.remotes).get(participant).cloned();
        match remote {
            Some(remote) => self.fill(&remote, out),
            None => out.fill(0.0),
        }
    }

    /// Every participant mixed into `out` (mono, 48 kHz).
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
        for sample in out.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }
    }

    fn fill(&self, remote: &Remote, out: &mut [f32]) {
        let deafened = lock(&self.inner.flags).deafened;
        let volume = *lock(&remote.volume);
        let mut pending = lock(&remote.pending);
        while pending.len() < out.len() {
            let mut frame = vec![0f32; FRAME];
            let now = self.inner.epoch.elapsed().as_millis() as u64;
            lock(&remote.jitter).pull(now, &mut frame);
            if let Some(active) = lock(&remote.vad).update(&frame) {
                remote.speaking.store(active, Ordering::SeqCst);
                let participant = remote.id.clone();
                let _ = self.inner.events.send(if active {
                    VoiceEvent::SpeakingStarted { participant }
                } else {
                    VoiceEvent::SpeakingStopped { participant }
                });
            }
            pending.extend(frame.iter().map(|s| s * volume));
        }
        let wanted = out.len();
        for (o, s) in out.iter_mut().zip(pending.drain(..wanted)) {
            *o = if deafened { 0.0 } else { s };
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
        let is_voice =
            producer["kind"] == "audio" && producer["type"].as_str().is_none_or(|t| t == "voice");
        let Some(producer_id) = producer["producerId"].as_str() else {
            return;
        };
        let owner = producer["peerId"]
            .as_str()
            .or(producer["ownerPeerId"].as_str())
            .unwrap_or("")
            .to_owned();
        if !is_voice || owner == self.inner.endpoint_id || owner.is_empty() {
            return;
        }
        let mut recv = self.inner.recv.lock().await;
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
        let remote = Arc::new(Remote {
            id: owner.clone(),
            consumer_id: consumer_id.to_owned(),
            ssrc: ssrc as u32,
            jitter: Mutex::new(jitter),
            pending: Mutex::new(VecDeque::new()),
            volume: Mutex::new(1.0),
            vad: Mutex::new(Vad::default()),
            speaking: AtomicBool::new(false),
            received: AtomicU64::new(0),
        });
        let replaced = lock(&self.inner.remotes).insert(owner.clone(), remote);
        if let Err(error) = recv
            .add(
                &self.inner.signaling,
                &self.inner.room_id,
                codec,
                ssrc as u32,
                cname,
            )
            .await
        {
            lock(&self.inner.remotes).remove(&owner);
            self.fail(error);
            return;
        }
        if replaced.is_none() {
            let _ = self
                .inner
                .events
                .send(VoiceEvent::ParticipantJoined { participant: owner });
        }
    }

    /// Removes the participant whose consumer the SFU closed.
    async fn drop_consumer(&self, consumer_id: &str) {
        let removed: Vec<Arc<Remote>> = {
            let mut remotes = lock(&self.inner.remotes);
            let ids: Vec<String> = remotes
                .values()
                .filter(|r| r.consumer_id == consumer_id)
                .map(|r| r.id.clone())
                .collect();
            ids.iter().filter_map(|id| remotes.remove(id)).collect()
        };
        for remote in removed {
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
                let _ = self.inner.events.send(VoiceEvent::Warning {
                    code: error.code(),
                    message: error.to_string(),
                });
            }
            let _ = self.inner.events.send(VoiceEvent::ParticipantLeft {
                participant: remote.id.clone(),
            });
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
            (inner.sfu_expires_at.load(Ordering::SeqCst) - REFRESH_BEFORE - unix_now()).max(5)
        };
        tokio::time::sleep(Duration::from_secs(wait as u64)).await;
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
                let expired = session.inner.sfu_expires_at.load(Ordering::SeqCst) <= unix_now();
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
        inner
            .sfu_expires_at
            .store(admission.sfu_expires_at, Ordering::SeqCst);
        inner.renewals.fetch_add(1, Ordering::SeqCst);
        tracing::debug!("voice authorization renewed");
        Ok(())
    }
}
