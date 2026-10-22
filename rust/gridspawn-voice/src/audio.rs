//! Opus encoding, WebRTC's audio processing (echo cancellation, noise
//! suppression, gain) and voice-activity detection. All audio is mono 48 kHz.

use webrtc_audio_processing::Processor;
use webrtc_audio_processing_config::{
    AdaptiveDigital, Config, EchoCanceller, GainController, GainController2, HighPassFilter,
    NoiseSuppression, NoiseSuppressionLevel,
};

use crate::error::VoiceError;

pub(crate) const SAMPLE_RATE: u32 = 48_000;
/// 20 ms: one Opus packet.
pub(crate) const FRAME: usize = 960;
/// 10 ms: one audio-processing frame.
pub(crate) const APM_FRAME: usize = 480;

/// The local microphone pipeline's switches.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct AudioOptions {
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    pub auto_gain: bool,
    /// Opus target bitrate (default 24 kbit/s, capped by the SFU's limit).
    pub bitrate_bps: u32,
}

impl Default for AudioOptions {
    fn default() -> Self {
        Self {
            echo_cancellation: true,
            noise_suppression: true,
            auto_gain: true,
            bitrate_bps: 24_000,
        }
    }
}

impl AudioOptions {
    pub fn echo_cancellation(mut self, on: bool) -> Self {
        self.echo_cancellation = on;
        self
    }

    pub fn noise_suppression(mut self, on: bool) -> Self {
        self.noise_suppression = on;
        self
    }

    pub fn auto_gain(mut self, on: bool) -> Self {
        self.auto_gain = on;
        self
    }

    pub fn bitrate_bps(mut self, bps: u32) -> Self {
        self.bitrate_bps = bps;
        self
    }
}

/// Opus for voice: mono, in-band FEC, DTX. A DTX frame (≤ 2 bytes) is not sent.
pub(crate) struct Encoder(opus::Encoder);

impl Encoder {
    pub fn new(bitrate_bps: u32) -> Result<Self, VoiceError> {
        let err = |e: opus::Error| VoiceError::Audio(e.to_string());
        let mut encoder =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)
                .map_err(err)?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(bitrate_bps.clamp(8_000, 128_000) as i32))
            .map_err(err)?;
        encoder.set_inband_fec(true).map_err(err)?;
        encoder.set_packet_loss_perc(10).map_err(err)?;
        encoder.set_dtx(true).map_err(err)?;
        encoder.set_signal(opus::Signal::Voice).map_err(err)?;
        Ok(Self(encoder))
    }

    /// One 20 ms frame. `None` for a DTX frame (nothing to send).
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Option<Vec<u8>>, VoiceError> {
        debug_assert_eq!(pcm.len(), FRAME);
        let packet = self
            .0
            .encode_vec_float(pcm, 1500)
            .map_err(|e| VoiceError::Audio(e.to_string()))?;
        Ok((packet.len() > 2).then_some(packet))
    }
}

/// WebRTC's audio processing module, 10 ms at a time.
pub(crate) struct Apm {
    processor: Processor,
}

impl Apm {
    pub fn new(options: &AudioOptions) -> Result<Self, VoiceError> {
        let processor =
            Processor::new(SAMPLE_RATE).map_err(|e| VoiceError::Audio(e.to_string()))?;
        processor.set_config(Config {
            echo_canceller: options.echo_cancellation.then(EchoCanceller::default),
            noise_suppression: options.noise_suppression.then(|| NoiseSuppression {
                level: NoiseSuppressionLevel::Moderate,
                ..Default::default()
            }),
            high_pass_filter: Some(HighPassFilter::default()),
            gain_controller: options.auto_gain.then(|| {
                GainController::GainController2(GainController2 {
                    adaptive_digital: Some(AdaptiveDigital::default()),
                    ..Default::default()
                })
            }),
            ..Default::default()
        });
        Ok(Self { processor })
    }

    /// What goes to the speakers (the echo reference). Not modified.
    pub fn render(&self, frame: &mut [f32]) -> Result<(), VoiceError> {
        self.processor
            .process_render_frame(&mut [frame])
            .map_err(|e| VoiceError::Audio(e.to_string()))
    }

    /// The microphone, processed in place.
    pub fn capture(&self, frame: &mut [f32]) -> Result<(), VoiceError> {
        self.processor
            .process_capture_frame(&mut [frame])
            .map_err(|e| VoiceError::Audio(e.to_string()))
    }
}

/// Energy-based voice activity with a short attack and a hangover.
#[derive(Debug)]
pub(crate) struct Vad {
    threshold_dbfs: f32,
    attack_frames: u32,
    hangover_frames: u32,
    above: u32,
    below: u32,
    active: bool,
}

impl Default for Vad {
    fn default() -> Self {
        Self {
            threshold_dbfs: -45.0,
            attack_frames: 2,
            // 300 ms at 20 ms per frame.
            hangover_frames: 15,
            above: 0,
            below: 0,
            active: false,
        }
    }
}

pub(crate) fn level_dbfs(pcm: &[f32]) -> f32 {
    if pcm.is_empty() {
        return -120.0;
    }
    let power = pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32;
    10.0 * power.max(1e-12).log10()
}

impl Vad {
    /// Feeds one 20 ms frame; returns `Some(active)` when the state changed.
    pub fn update(&mut self, pcm: &[f32]) -> Option<bool> {
        if level_dbfs(pcm) > self.threshold_dbfs {
            self.above += 1;
            self.below = 0;
        } else {
            self.below += 1;
            self.above = 0;
        }
        let next = if self.active {
            self.below < self.hangover_frames
        } else {
            self.above >= self.attack_frames
        };
        (next != self.active).then(|| {
            self.active = next;
            next
        })
    }

    pub fn active(&self) -> bool {
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, amplitude: f32, n: usize, phase: &mut f32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                *phase += 2.0 * std::f32::consts::PI * hz / SAMPLE_RATE as f32;
                amplitude * phase.sin()
            })
            .collect()
    }

    #[test]
    fn encoder_sends_voice_and_skips_silence() {
        let mut encoder = Encoder::new(24_000).unwrap();
        let mut phase = 0.0;
        let packet = encoder
            .encode(&tone(440.0, 0.3, FRAME, &mut phase))
            .unwrap();
        assert!(packet.is_some_and(|p| p.len() > 10));
        let mut dtx = 0;
        for _ in 0..50 {
            if encoder.encode(&[0.0; FRAME]).unwrap().is_none() {
                dtx += 1;
            }
        }
        assert!(dtx > 30, "DTX frames in 1 s of silence: {dtx}");
    }

    #[test]
    fn vad_attack_and_hangover() {
        let mut vad = Vad::default();
        let mut phase = 0.0;
        let speech = tone(300.0, 0.2, FRAME, &mut phase);
        let silence = vec![0.0; FRAME];
        assert_eq!(vad.update(&speech), None);
        assert_eq!(vad.update(&speech), Some(true));
        for _ in 0..14 {
            assert_eq!(vad.update(&silence), None);
        }
        assert_eq!(vad.update(&silence), Some(false));
    }

    /// The acceptance check for echo cancellation through our pipeline: a
    /// far-end speech-like signal, echoed into the microphone 40 ms later.
    #[test]
    fn echo_cancellation_erle_through_the_pipeline() {
        let apm = Apm::new(&AudioOptions::default().auto_gain(false)).unwrap();
        let mut seed = 7u64;
        let mut noise = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
        };
        let delay = 48 * 40;
        let mut history = vec![0f32; delay + APM_FRAME];
        let (mut lp, mut envelope) = (0f32, 0f32);
        let (mut mic_energy, mut out_energy) = (0f64, 0f64);
        for frame in 0..1000 {
            let mut render = vec![0f32; APM_FRAME];
            for s in render.iter_mut() {
                lp = 0.7 * lp + 0.3 * noise();
                envelope += 2.0 * std::f32::consts::PI * 3.0 / SAMPLE_RATE as f32;
                *s = 0.5 * lp * (0.6 + 0.4 * envelope.sin());
            }
            history.drain(..APM_FRAME);
            history.extend_from_slice(&render);
            let mut mic: Vec<f32> = (0..APM_FRAME)
                .map(|i| 0.4 * history[i] + 0.15 * history[i.saturating_sub(96)] + 0.001 * noise())
                .collect();
            let before = mic.clone();
            apm.render(&mut render).unwrap();
            apm.capture(&mut mic).unwrap();
            if frame >= 700 {
                mic_energy += before.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
                out_energy += mic.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
            }
        }
        let erle = 10.0 * (mic_energy / out_energy.max(1e-15)).log10();
        eprintln!("ERLE through the voice pipeline (last 3 s of 10): {erle:.1} dB");
        assert!(erle >= 20.0, "ERLE {erle:.1} dB");
    }
}
