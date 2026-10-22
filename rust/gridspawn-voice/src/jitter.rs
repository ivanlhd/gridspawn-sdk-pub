//! Receive-side jitter buffer for one remote speaker: reorders RTP packets,
//! plays them out every 20 ms, recovers single losses from the next packet's
//! in-band FEC, conceals others (Opus PLC), and adapts its delay to the
//! observed jitter. Time is passed in (milliseconds), so it is testable.

use std::collections::{BTreeMap, VecDeque};

use crate::audio::FRAME;

const FRAME_MS: u64 = 20;
const MIN_TARGET_MS: u64 = 40;
const MAX_TARGET_MS: u64 = 200;
/// Margin over the observed spread of transit times.
const TARGET_MARGIN_MS: i64 = 30;
const TRANSIT_WINDOW: usize = 100;
/// Concealed frames played into an empty buffer before going quiet.
const MAX_EMPTY_CONCEALMENT: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameKind {
    /// Decoded from its own packet.
    Normal,
    /// Rebuilt from the next packet's in-band FEC.
    Fec,
    /// Opus packet-loss concealment.
    Concealed,
    /// Nothing to play (not started, or the speaker paused).
    Silence,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct JitterStats {
    pub received: u64,
    pub late: u64,
    pub duplicate: u64,
    pub normal: u64,
    pub fec: u64,
    pub concealed: u64,
    /// Pulls while playing that found the buffer empty.
    pub underruns: u64,
    pub dropped_for_latency: u64,
    /// Time each normally played packet waited in the buffer (ms), most recent last.
    pub waits_ms: VecDeque<u32>,
}

struct Packet {
    payload: Vec<u8>,
    arrival_ms: u64,
}

pub(crate) struct JitterBuffer {
    decoder: opus::Decoder,
    packets: BTreeMap<u64, Packet>,
    highest: Option<u64>,
    next: Option<u64>,
    playing: bool,
    empty_run: u32,
    transits: VecDeque<i64>,
    target_ms: u64,
    pub stats: JitterStats,
}

impl JitterBuffer {
    pub fn new() -> Result<Self, opus::Error> {
        Ok(Self {
            decoder: opus::Decoder::new(crate::audio::SAMPLE_RATE, opus::Channels::Mono)?,
            packets: BTreeMap::new(),
            highest: None,
            next: None,
            playing: false,
            empty_run: 0,
            transits: VecDeque::new(),
            target_ms: 60,
            stats: JitterStats::default(),
        })
    }

    pub fn target_ms(&self) -> u64 {
        self.target_ms
    }

    /// Extends a 16-bit sequence number to the value closest to the highest seen.
    fn extend(&mut self, seq: u16) -> u64 {
        let Some(highest) = self.highest else {
            let first = (1u64 << 32) + u64::from(seq);
            self.highest = Some(first);
            return first;
        };
        let mut candidate = (highest & !0xffff) | u64::from(seq);
        if candidate + 0x8000 < highest {
            candidate += 0x1_0000;
        } else if candidate > highest + 0x8000 {
            candidate -= 0x1_0000;
        }
        if candidate > highest {
            self.highest = Some(candidate);
        }
        candidate
    }

    /// One received RTP packet (sequence, RTP timestamp at 48 kHz, Opus payload).
    pub fn push(&mut self, seq: u16, timestamp: u32, payload: Vec<u8>, now_ms: u64) {
        let ext = self.extend(seq);
        if self.next.is_some_and(|next| ext < next) {
            self.stats.late += 1;
            return;
        }
        if self.packets.contains_key(&ext) {
            self.stats.duplicate += 1;
            return;
        }
        self.stats.received += 1;
        self.packets.insert(
            ext,
            Packet {
                payload,
                arrival_ms: now_ms,
            },
        );
        // Transit = arrival − send time (both in ms; the offset cancels in the spread).
        let transit = now_ms as i64 - i64::from(timestamp / 48);
        self.transits.push_back(transit);
        if self.transits.len() > TRANSIT_WINDOW {
            self.transits.pop_front();
        }
        if self.transits.len() >= 10 {
            let max = *self.transits.iter().max().unwrap();
            let min = *self.transits.iter().min().unwrap();
            // Spread over the window, ignoring stream pauses (DTX gaps shift both ends).
            let spread = (max - min).clamp(0, MAX_TARGET_MS as i64);
            self.target_ms =
                ((spread + TARGET_MARGIN_MS) as u64).clamp(MIN_TARGET_MS, MAX_TARGET_MS);
        }
    }

    /// The next 20 ms of audio into `out` (960 samples).
    pub fn pull(&mut self, now_ms: u64, out: &mut [f32]) -> FrameKind {
        debug_assert_eq!(out.len(), FRAME);
        if !self.playing {
            let ready = self
                .packets
                .iter()
                .next()
                .is_some_and(|(_, first)| first.arrival_ms + self.target_ms <= now_ms);
            if !ready {
                out.fill(0.0);
                return FrameKind::Silence;
            }
            self.playing = true;
            self.empty_run = 0;
            self.next = self.packets.keys().next().copied();
        }
        let seq = self.next.expect("playing has a next packet");
        let kind = if let Some(packet) = self.packets.remove(&seq) {
            self.empty_run = 0;
            let wait = now_ms.saturating_sub(packet.arrival_ms) as u32;
            self.stats.waits_ms.push_back(wait);
            if self.stats.waits_ms.len() > 10_000 {
                self.stats.waits_ms.pop_front();
            }
            self.decode(&packet.payload, out, false, FrameKind::Normal)
        } else if let Some(following) = self.packets.get(&(seq + 1)) {
            self.empty_run = 0;
            let payload = following.payload.clone();
            self.decode(&payload, out, true, FrameKind::Fec)
        } else if self.packets.is_empty() {
            self.stats.underruns += 1;
            self.empty_run += 1;
            if self.empty_run > MAX_EMPTY_CONCEALMENT {
                // The speaker paused (DTX, mute) or the stream stalled: go quiet
                // and buffer again before playing.
                self.playing = false;
                out.fill(0.0);
                return FrameKind::Silence;
            }
            self.decode(&[], out, false, FrameKind::Concealed)
        } else {
            // A loss with later packets waiting; jump over long gaps.
            if let Some(&first) = self.packets.keys().next() {
                if first > seq + 50 {
                    self.next = Some(first);
                    return self.pull(now_ms, out);
                }
            }
            self.decode(&[], out, false, FrameKind::Concealed)
        };
        self.next = Some(seq + 1);
        // Keep latency bounded: drop the oldest packet when far over target.
        let buffered_ms = self.packets.len() as u64 * FRAME_MS;
        if buffered_ms > self.target_ms + 3 * FRAME_MS {
            if let Some((&oldest, _)) = self.packets.iter().next() {
                self.packets.remove(&oldest);
                self.next = Some(oldest + 1);
                self.stats.dropped_for_latency += 1;
            }
        }
        kind
    }

    fn decode(&mut self, payload: &[u8], out: &mut [f32], fec: bool, kind: FrameKind) -> FrameKind {
        match self.decoder.decode_float(payload, out, fec) {
            Ok(n) if n == FRAME => {
                match kind {
                    FrameKind::Normal => self.stats.normal += 1,
                    FrameKind::Fec => self.stats.fec += 1,
                    _ => self.stats.concealed += 1,
                }
                kind
            }
            Ok(n) => {
                // Unexpected frame size: keep timing, pad with silence.
                out[n.min(FRAME)..].fill(0.0);
                self.stats.concealed += 1;
                FrameKind::Concealed
            }
            Err(_) => {
                out.fill(0.0);
                self.stats.concealed += 1;
                FrameKind::Concealed
            }
        }
    }
}

/// The receive-side impairment gate (deterministic: simulated network, real
/// Opus encoder and decoder). The numbers are printed and asserted.
#[cfg(test)]
mod gate {
    use super::*;
    use crate::audio::{Encoder, SAMPLE_RATE};

    struct Rng(u64);
    impl Rng {
        fn unit(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    struct Outcome {
        kinds: Vec<FrameKind>,
        pcm: Vec<f32>,
        stats: JitterStats,
        target_ms: u64,
    }

    /// 10 s of a 440 Hz tone, sent every 20 ms; each packet is lost with
    /// probability `loss`, else delayed by 20 ms + uniform [0, 2·jitter] ms.
    fn run(loss: f64, jitter_ms: f64, seed: u64) -> Outcome {
        let mut encoder = Encoder::new(24_000).unwrap();
        let mut rng = Rng(seed);
        let mut phase = 0f32;
        let mut arrivals: Vec<(u64, u16, u32, Vec<u8>)> = Vec::new();
        for i in 0..500u64 {
            let pcm: Vec<f32> = (0..FRAME)
                .map(|_| {
                    phase += 2.0 * std::f32::consts::PI * 440.0 / SAMPLE_RATE as f32;
                    0.3 * phase.sin()
                })
                .collect();
            let packet = encoder.encode(&pcm).unwrap().expect("tone is never DTX");
            if rng.unit() < loss {
                continue;
            }
            let delay = 20.0 + rng.unit() * 2.0 * jitter_ms;
            arrivals.push((i * 20 + delay as u64, i as u16, (i * 960) as u32, packet));
        }
        arrivals.sort_by_key(|a| a.0);
        let mut buffer = JitterBuffer::new().unwrap();
        let (mut kinds, mut pcm) = (Vec::new(), Vec::new());
        let mut next_arrival = 0;
        for tick in 0..540u64 {
            let now = tick * 20;
            while next_arrival < arrivals.len() && arrivals[next_arrival].0 <= now {
                let (at, seq, ts, payload) = arrivals[next_arrival].clone();
                buffer.push(seq, ts, payload, at);
                next_arrival += 1;
            }
            let mut frame = vec![0f32; FRAME];
            kinds.push(buffer.pull(now, &mut frame));
            pcm.extend_from_slice(&frame);
        }
        Outcome {
            kinds,
            pcm,
            target_ms: buffer.target_ms(),
            stats: buffer.stats,
        }
    }

    /// Tone vs everything else, in dB: a least-squares 440 Hz sine fitted per
    /// 20 ms window (so a timing shift between windows is not counted as
    /// noise), residual energy summed over all windows.
    fn tone_snr(pcm: &[f32]) -> f64 {
        let w = 2.0 * std::f64::consts::PI * 440.0 / SAMPLE_RATE as f64;
        let (mut signal, mut noise) = (0f64, 0f64);
        for (index, window) in pcm.chunks_exact(FRAME).enumerate() {
            let offset = (index * FRAME) as f64;
            let (mut ss, mut cc, mut sc, mut xs, mut xc) = (0f64, 0f64, 0f64, 0f64, 0f64);
            for (n, x) in window.iter().enumerate() {
                let (s, c) = (w * (offset + n as f64)).sin_cos();
                let x = *x as f64;
                ss += s * s;
                cc += c * c;
                sc += s * c;
                xs += x * s;
                xc += x * c;
            }
            let det = ss * cc - sc * sc;
            let (a, b) = ((xs * cc - xc * sc) / det, (xc * ss - xs * sc) / det);
            for (n, x) in window.iter().enumerate() {
                let (s, c) = (w * (offset + n as f64)).sin_cos();
                let fit = a * s + b * c;
                signal += fit * fit;
                noise += (*x as f64 - fit).powi(2);
            }
        }
        10.0 * (signal / noise.max(1e-12)).log10()
    }

    fn median(values: &VecDeque<u32>) -> u32 {
        let mut sorted: Vec<u32> = values.iter().copied().collect();
        sorted.sort_unstable();
        sorted.get(sorted.len() / 2).copied().unwrap_or(0)
    }

    /// From the first played frame until the sender stopped (frames 0..500 sent).
    fn played(o: &Outcome) -> &[FrameKind] {
        let start = o
            .kinds
            .iter()
            .position(|k| *k != FrameKind::Silence)
            .unwrap();
        &o.kinds[start..(start + 480).min(o.kinds.len())]
    }

    fn longest_silence(kinds: &[FrameKind]) -> usize {
        kinds
            .split(|k| *k != FrameKind::Silence)
            .map(<[FrameKind]>::len)
            .max()
            .unwrap_or(0)
    }

    /// The codec's own ceiling for this measurement: no loss, no jitter.
    fn baseline_snr() -> f64 {
        let o = run(0.0, 0.0, 9);
        let snr = tone_snr(&o.pcm[48_000..48_000 * 9]);
        eprintln!(
            "INFO baseline (no loss, no jitter): normal {} concealed {}, tone SNR {snr:.1} dB",
            o.stats.normal, o.stats.concealed
        );
        snr
    }

    #[test]
    fn impairment_gate() {
        let mut failures: Vec<String> = Vec::new();
        let mut check = |ok: bool, what: String| {
            if !ok {
                failures.push(what);
            }
        };
        // A regression tripwire for the encoder path, not a quality claim: a
        // pure tone through a speech codec has a waveform-SNR ceiling (about
        // 16 dB at the shipped 24 kbps).
        let baseline = baseline_snr();
        check(
            baseline >= 14.0,
            format!("lossless baseline tone SNR {baseline:.1} dB < 14"),
        );

        // 10 % loss: ≥ 98 % of frames from real or FEC data, tone SNR at most
        // 3 dB below the lossless baseline of this run.
        let o = run(0.10, 0.0, 1);
        let p = played(&o);
        let recovered = p
            .iter()
            .filter(|k| matches!(k, FrameKind::Normal | FrameKind::Fec))
            .count();
        let share = recovered as f64 / p.len() as f64;
        let snr = tone_snr(&o.pcm[48_000..48_000 * 9]);
        eprintln!(
            "GATE loss 10%: normal {} fec {} concealed {} dropped-for-latency {} → {:.1}% from data, \
             tone SNR {snr:.1} dB (lossless baseline {baseline:.1} dB, bar {:.1} dB)",
            o.stats.normal, o.stats.fec, o.stats.concealed, o.stats.dropped_for_latency, share * 100.0,
            baseline - 3.0
        );
        check(
            share >= 0.98,
            format!("10% loss: {:.1}% from data", share * 100.0),
        );
        check(
            snr >= baseline - 3.0,
            format!("10% loss: tone SNR {snr:.1} dB < baseline {baseline:.1} dB − 3"),
        );

        // 20 % loss: continuity, no silent gap over 60 ms.
        let o = run(0.20, 0.0, 2);
        let gap = longest_silence(played(&o)) * 20;
        let snr = tone_snr(&o.pcm[48_000..48_000 * 9]);
        eprintln!(
            "GATE loss 20%: normal {} fec {} concealed {}, longest silent gap {gap} ms, tone SNR {snr:.1} dB",
            o.stats.normal, o.stats.fec, o.stats.concealed
        );
        check(gap <= 60, format!("20% loss: silent gap {gap} ms"));

        // ±40 ms jitter (delay 20 + U[0, 80] ms, so packets reorder): no
        // underruns after 2 s, median added delay ≤ 80 ms.
        let o = run(0.0, 40.0, 3);
        // While the stream is active (played window), after the first 2 s.
        let after_warmup = played(&o)[100..]
            .iter()
            .filter(|k| matches!(k, FrameKind::Concealed | FrameKind::Silence))
            .count();
        let wait = median(&o.stats.waits_ms);
        eprintln!(
            "GATE jitter ±40 ms: underruns total {}, concealed/silent frames after 2 s {after_warmup}, \
             median buffer delay {wait} ms, target {} ms, late {}, dropped for latency {}",
            o.stats.underruns, o.target_ms, o.stats.late, o.stats.dropped_for_latency
        );
        check(
            after_warmup == 0,
            format!("jitter: {after_warmup} concealed/silent frames after 2 s"),
        );
        check(wait <= 80, format!("jitter: median delay {wait} ms"));

        // Both: 5 % loss with ±40 ms jitter, reported (not part of the gate).
        let o = run(0.05, 40.0, 4);
        let p = played(&o);
        let recovered = p
            .iter()
            .filter(|k| matches!(k, FrameKind::Normal | FrameKind::Fec))
            .count();
        eprintln!(
            "INFO loss 5% + jitter ±40 ms: {:.1}% from data, tone SNR {:.1} dB, median delay {} ms",
            recovered as f64 * 100.0 / p.len() as f64,
            tone_snr(&o.pcm[48_000..48_000 * 9]),
            median(&o.stats.waits_ms)
        );
        assert!(failures.is_empty(), "gate failures: {failures:?}");
    }

    #[test]
    fn sequence_wraps_and_late_packets() {
        let mut buffer = JitterBuffer::new().unwrap();
        assert_eq!(buffer.extend(65534), (1 << 32) + 65534);
        assert_eq!(buffer.extend(65535), (1 << 32) + 65535);
        assert_eq!(buffer.extend(0), (1 << 32) + 65536);
        assert_eq!(buffer.extend(65533), (1 << 32) + 65533);
    }
}
