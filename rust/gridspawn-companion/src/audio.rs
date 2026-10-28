//! The audio thread: microphone and speakers through cpal (`DeviceIo` must
//! stay on one thread), or, in builds with the `testing` feature, a tone
//! instead of the microphone.

use std::{
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::Duration,
};

use gridspawn_voice::{devices::DeviceIo, VoiceSession};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

pub enum AudioCmd {
    Start {
        voice: VoiceSession,
        input: Option<String>,
        output: Option<String>,
        /// Always false without the `testing` feature (see `Launch::parse`).
        tone: bool,
    },
    Stop,
}

/// Testing only: a test signal pushed as the microphone (a 440 Hz tone, or
/// the WAV file named by `GRIDSPAWN_TEST_SIGNAL`, looped), the mix pulled and
/// dropped, and the RMS of what was sent and received logged each second.
#[cfg(feature = "testing")]
struct Tone {
    voice: VoiceSession,
    /// `None` = the tone.
    wav: Option<Vec<f32>>,
    position: usize,
    phase: f32,
    sent: (f64, usize),
    received: (f64, usize),
    last_report: std::time::Instant,
}

#[cfg(feature = "testing")]
impl Tone {
    /// 20 ms at 48 kHz, mono.
    const FRAME: usize = 960;

    fn new(voice: VoiceSession) -> Self {
        let wav = std::env::var("GRIDSPAWN_TEST_SIGNAL")
            .ok()
            .and_then(|path| match read_wav(&path) {
                Ok(samples) => Some(samples),
                Err(error) => {
                    tracing::warn!(%error, "test signal not read; using the tone");
                    None
                }
            });
        tracing::info!(
            signal = if wav.is_some() { "wav" } else { "tone" },
            "test tone instead of the microphone"
        );
        Self {
            voice,
            wav,
            position: 0,
            phase: 0.0,
            sent: (0.0, 0),
            received: (0.0, 0),
            last_report: std::time::Instant::now(),
        }
    }

    fn tick(&mut self) {
        let mut samples = [0f32; Self::FRAME];
        match &self.wav {
            Some(wav) => {
                for s in samples.iter_mut() {
                    *s = wav[self.position];
                    self.position = (self.position + 1) % wav.len();
                }
            }
            None => {
                let step = 440.0 * std::f32::consts::TAU / 48_000.0;
                for s in samples.iter_mut() {
                    *s = 0.2 * self.phase.sin();
                    self.phase = (self.phase + step) % std::f32::consts::TAU;
                }
            }
        }
        self.sent.0 += samples.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>();
        self.sent.1 += Self::FRAME;
        self.voice.push_capture(&samples);
        let mut mix = [0f32; Self::FRAME];
        self.voice.pull_mix(&mut mix);
        self.received.0 += mix.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>();
        self.received.1 += Self::FRAME;
        if self.last_report.elapsed() >= Duration::from_secs(1) {
            let rms = |(sum, n): (f64, usize)| (sum / n.max(1) as f64).sqrt();
            tracing::info!(
                sent_rms = rms(self.sent),
                received_rms = rms(self.received),
                "test tone: loudest received in the last second"
            );
            self.sent = (0.0, 0);
            self.received = (0.0, 0);
            self.last_report = std::time::Instant::now();
        }
    }
}

/// Testing only: a 16-bit PCM, mono, 48 kHz WAV file as samples.
#[cfg(feature = "testing")]
fn read_wav(path: &str) -> Result<Vec<f32>, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let (mut format, mut data) = (None, None);
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]])
            as usize;
        let body = &bytes[at + 8..(at + 8 + len).min(bytes.len())];
        match id {
            b"fmt " => format = Some(body),
            b"data" => data = Some(body),
            _ => {}
        }
        at += 8 + len + (len & 1);
    }
    let format = format.ok_or("no fmt chunk")?;
    let data = data.ok_or("no data chunk")?;
    if format.len() < 16 {
        return Err("short fmt chunk".into());
    }
    let field = |o: usize| u16::from_le_bytes([format[o], format[o + 1]]);
    let rate = u32::from_le_bytes([format[4], format[5], format[6], format[7]]);
    if field(0) != 1 || field(2) != 1 || rate != 48_000 || field(14) != 16 {
        return Err("needs 16-bit PCM, mono, 48 kHz".into());
    }
    let samples: Vec<f32> = data
        .chunks_exact(2)
        .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
        .collect();
    if samples.is_empty() {
        return Err("empty".into());
    }
    Ok(samples)
}

/// Testing only: `GRIDSPAWN_TEST_PROCESSING=off` joins voice with echo
/// cancellation, noise suppression and automatic gain off (the high-pass
/// filter of gridspawn-voice stays on), to measure what they do.
#[cfg(feature = "testing")]
pub fn test_audio_options(
    mut base: gridspawn_voice::AudioOptions,
    tone: bool,
) -> gridspawn_voice::AudioOptions {
    if tone && std::env::var("GRIDSPAWN_TEST_PROCESSING").as_deref() == Ok("off") {
        tracing::info!("test signal: capture processing off");
        base.echo_cancellation = false;
        base.noise_suppression = false;
        base.auto_gain = false;
    }
    base
}

#[cfg(not(feature = "testing"))]
pub fn test_audio_options(
    base: gridspawn_voice::AudioOptions,
    _tone: bool,
) -> gridspawn_voice::AudioOptions {
    base
}

/// Without the `testing` feature there is no tone: never constructed.
#[cfg(not(feature = "testing"))]
struct Tone;

#[cfg(not(feature = "testing"))]
impl Tone {
    fn new(_voice: VoiceSession) -> Self {
        Self
    }

    fn tick(&mut self) {}
}

pub fn spawn(events: UnboundedSender<Value>) -> mpsc::Sender<AudioCmd> {
    let (tx, rx) = mpsc::channel::<AudioCmd>();
    let _ = thread::Builder::new()
        .name("gridspawn-companion-audio".into())
        .spawn(move || {
            // Held only to keep the streams running; dropping it stops them.
            let mut _devices: Option<DeviceIo> = None;
            let mut tone: Option<Tone> = None;
            loop {
                let wait = if tone.is_some() {
                    Duration::from_millis(20)
                } else {
                    Duration::from_secs(3600)
                };
                match rx.recv_timeout(wait) {
                    Ok(AudioCmd::Start {
                        voice,
                        input,
                        output,
                        tone: test_tone,
                    }) => {
                        _devices = None;
                        tone = None;
                        if test_tone {
                            tone = Some(Tone::new(voice));
                        } else {
                            match DeviceIo::start(&voice, input.as_deref(), output.as_deref()) {
                                Ok(io) => _devices = Some(io),
                                Err(error) => {
                                    tracing::warn!(%error, "audio devices failed");
                                    let _ = events.send(json!({
                                        "event": "error", "code": "voice/audio_device",
                                        "message": error.to_string(),
                                    }));
                                }
                            }
                        }
                    }
                    Ok(AudioCmd::Stop) => {
                        _devices = None;
                        tone = None;
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if let Some(tone) = tone.as_mut() {
                            tone.tick();
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        });
    tx
}
