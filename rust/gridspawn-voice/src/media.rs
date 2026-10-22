//! The two mediasoup transports as webrtc-rs peer connections: one sends our
//! microphone, one receives every remote speaker (one SDP section each).

use std::{sync::Arc, time::Duration};

use rtc::{
    media::Sample,
    media_stream::MediaStreamTrack,
    peer_connection::{
        configuration::{
            interceptor_registry::register_default_interceptors,
            media_engine::{MediaEngine, MIME_TYPE_OPUS},
            setting_engine::SettingEngineBuilder,
            RTCConfigurationBuilder,
        },
        sdp::RTCSessionDescription,
        transport::RTCDtlsRole,
    },
    rtp_transceiver::rtp_sender::{
        RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
        RtpCodecKind,
    },
};
use serde_json::json;
use tokio::sync::mpsc;
use webrtc::{
    media_stream::{
        track_local::{static_sample::TrackLocalStaticSample, TrackLocal},
        track_remote::TrackRemote,
    },
    peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCPeerConnectionState,
    },
    runtime::default_runtime,
};

use crate::{
    error::VoiceError,
    sdp::{self, OpusCodec, RecvSection, TransportInfo},
    session::JoinTimings,
    signaling::Signaling,
};

/// Our send payload type (any dynamic value; the SFU maps it).
pub(crate) const SEND_PAYLOAD_TYPE: u8 = 111;
const OPUS_FMTP: &str = "minptime=10;useinbandfec=1";

fn media_error(error: impl std::fmt::Display) -> VoiceError {
    VoiceError::TransportFailed(error.to_string())
}

/// What a peer connection reports back to the session.
pub(crate) enum MediaEvent {
    Track(Arc<dyn TrackRemote>),
    Failed(&'static str),
}

struct Handler {
    label: &'static str,
    events: mpsc::UnboundedSender<MediaEvent>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        tracing::debug!(leg = self.label, %state, "voice transport state");
        if matches!(state, RTCPeerConnectionState::Failed) {
            let _ = self.events.send(MediaEvent::Failed(self.label));
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.events.send(MediaEvent::Track(track));
    }
}

fn opus(payload_type: u8) -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48_000,
            channels: 2,
            sdp_fmtp_line: OPUS_FMTP.to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type,
    }
}

async fn peer_connection(
    payload_type: u8,
    answering: bool,
    handler: Handler,
) -> Result<Arc<dyn PeerConnection>, VoiceError> {
    let mut media = MediaEngine::default();
    media
        .register_codec(opus(payload_type), RtpCodecKind::Audio)
        .map_err(media_error)?;
    let registry = register_default_interceptors(rtc::interceptor::Registry::new(), &mut media)
        .map_err(media_error)?;
    let mut settings = SettingEngineBuilder::new();
    if answering {
        // mediasoup offers actpass; we are always the DTLS client.
        settings = settings.with_answering_dtls_role(RTCDtlsRole::Client);
    }
    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_setting_engine(settings.build())
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(handler))
        .with_runtime(
            default_runtime().ok_or(VoiceError::TransportFailed("no async runtime".into()))?,
        )
        .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
        .build()
        .await
        .map_err(media_error)?;
    Ok(Arc::new(pc))
}

/// Our microphone: one audio track, produced to the SFU.
pub(crate) struct SendLeg {
    pc: Arc<dyn PeerConnection>,
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    pub producer_id: String,
}

impl SendLeg {
    pub async fn open(
        signaling: &Signaling,
        room_id: &str,
        transport: TransportInfo,
        events: mpsc::UnboundedSender<MediaEvent>,
        timings: &mut JoinTimings,
    ) -> Result<Self, VoiceError> {
        let pc = peer_connection(
            SEND_PAYLOAD_TYPE,
            false,
            Handler {
                label: "send",
                events,
            },
        )
        .await?;
        let ssrc = {
            let mut bytes = [0u8; 4];
            getrandom::fill(&mut bytes).map_err(|e| VoiceError::Audio(e.to_string()))?;
            u32::from_le_bytes(bytes) | 1
        };
        let track = Arc::new(
            TrackLocalStaticSample::new(
                std::time::Instant::now(),
                MediaStreamTrack::new(
                    "gridspawn-voice".into(),
                    "microphone".into(),
                    "microphone".into(),
                    RtpCodecKind::Audio,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters {
                            ssrc: Some(ssrc),
                            ..Default::default()
                        },
                        codec: opus(SEND_PAYLOAD_TYPE).rtp_codec,
                        ..Default::default()
                    }],
                ),
            )
            .map_err(media_error)?,
        );
        pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await
            .map_err(media_error)?;
        let offer = pc.create_offer(None).await.map_err(media_error)?;
        pc.set_local_description(offer).await.map_err(media_error)?;
        let local = pc
            .local_description()
            .await
            .ok_or(VoiceError::TransportFailed("no local description".into()))?
            .sdp;
        let mid = sdp::attribute(&local, "a=mid:").unwrap_or("0").to_owned();
        let cname = sdp::local_cname(&local).unwrap_or_else(|| "gridspawn".into());
        timings.step("send peer connection + offer");
        signaling
            .request(
                "connectTransport",
                json!({ "roomId": room_id, "transportId": transport.id, "dtlsParameters": sdp::local_dtls(&local)? }),
            )
            .await?;
        timings.step("connectTransport (send)");
        let answer = sdp::send_answer(
            &transport,
            &mid,
            &OpusCodec {
                payload_type: SEND_PAYLOAD_TYPE,
                fmtp: OPUS_FMTP.into(),
            },
        );
        pc.set_remote_description(RTCSessionDescription::answer(answer).map_err(media_error)?)
            .await
            .map_err(media_error)?;
        let produced = signaling
            .request(
                "produce",
                json!({
                    "roomId": room_id,
                    "transportId": transport.id,
                    "kind": "audio",
                    "rtpParameters": {
                        "mid": mid,
                        "codecs": [{
                            "mimeType": "audio/opus", "payloadType": SEND_PAYLOAD_TYPE,
                            "clockRate": 48000, "channels": 2,
                            "parameters": { "minptime": 10, "useinbandfec": 1, "usedtx": 1 },
                            "rtcpFeedback": []
                        }],
                        "headerExtensions": [],
                        "encodings": [{ "ssrc": ssrc, "dtx": true }],
                        "rtcp": { "cname": cname, "reducedSize": true }
                    },
                    "appData": { "type": "voice", "routeClass": "room" }
                }),
            )
            .await?;
        let producer_id = produced["id"]
            .as_str()
            .ok_or(VoiceError::InvalidResponse("produce id"))?
            .to_owned();
        timings.step("answer + produce");
        Ok(Self {
            pc,
            track,
            ssrc,
            producer_id,
        })
    }

    /// Sends one encoded 20 ms Opus frame.
    pub async fn send(&self, packet: Vec<u8>) {
        let sample = Sample {
            data: packet.into(),
            duration: Duration::from_millis(20),
            ..Sample::new(std::time::Instant::now())
        };
        if let Err(error) = self
            .track
            .write_sample(self.ssrc, SEND_PAYLOAD_TYPE, &sample, &[])
            .await
        {
            tracing::debug!(%error, "voice frame not sent");
        }
    }

    pub async fn close(&self) {
        let _ = self.pc.close().await;
    }
}

/// Every remote speaker on one transport.
pub(crate) struct RecvLeg {
    pc: Arc<dyn PeerConnection>,
    transport: TransportInfo,
    sections: Vec<RecvSection>,
    connected: bool,
    next_mid: u32,
}

impl RecvLeg {
    pub async fn open(
        transport: TransportInfo,
        payload_type: u8,
        events: mpsc::UnboundedSender<MediaEvent>,
    ) -> Result<Self, VoiceError> {
        let pc = peer_connection(
            payload_type,
            true,
            Handler {
                label: "receive",
                events,
            },
        )
        .await?;
        Ok(Self {
            pc,
            transport,
            sections: Vec::new(),
            connected: false,
            next_mid: 0,
        })
    }

    pub fn transport_id(&self) -> &str {
        &self.transport.id
    }

    /// Adds one consumer's section and renegotiates.
    pub async fn add(
        &mut self,
        signaling: &Signaling,
        room_id: &str,
        codec: OpusCodec,
        ssrc: u32,
        cname: String,
    ) -> Result<(), VoiceError> {
        self.sections.push(RecvSection {
            mid: self.next_mid.to_string(),
            codec,
            ssrc,
            cname,
            active: true,
        });
        self.next_mid += 1;
        self.renegotiate(signaling, room_id).await
    }

    /// Turns a consumer's section inactive (its mid is not reused).
    pub async fn remove(&mut self, signaling: &Signaling, room_id: &str, ssrc: u32) {
        let mut changed = false;
        for section in &mut self.sections {
            if section.ssrc == ssrc && section.active {
                section.active = false;
                changed = true;
            }
        }
        if changed {
            if let Err(error) = self.renegotiate(signaling, room_id).await {
                tracing::debug!(%error, "voice receive renegotiation after a removal failed");
            }
        }
    }

    async fn renegotiate(
        &mut self,
        signaling: &Signaling,
        room_id: &str,
    ) -> Result<(), VoiceError> {
        let offer = sdp::recv_offer(&self.transport, &self.sections);
        self.pc
            .set_remote_description(RTCSessionDescription::offer(offer).map_err(media_error)?)
            .await
            .map_err(media_error)?;
        let answer = self.pc.create_answer(None).await.map_err(media_error)?;
        self.pc
            .set_local_description(answer)
            .await
            .map_err(media_error)?;
        if !self.connected {
            let local = self
                .pc
                .local_description()
                .await
                .ok_or(VoiceError::TransportFailed("no local description".into()))?
                .sdp;
            signaling
                .request(
                    "connectTransport",
                    json!({ "roomId": room_id, "transportId": self.transport.id, "dtlsParameters": sdp::local_dtls(&local)? }),
                )
                .await?;
            self.connected = true;
        }
        Ok(())
    }

    pub async fn close(&self) {
        let _ = self.pc.close().await;
    }
}
