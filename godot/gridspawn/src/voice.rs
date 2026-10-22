//! `GridspawnVoice`: in-engine voice for an attached game session.
//!
//! `gridspawn-voice` does the network and the audio processing; this node
//! connects it to Godot's own audio, on the main thread in `_process`:
//! - microphone: an `AudioStreamMicrophone` playing on a muted input bus with
//!   an `AudioEffectCapture` (or samples the game pushes, `input_source = Custom`);
//! - echo reference: an `AudioEffectCapture` on the Master bus, so the echo
//!   canceller also removes the game's own sounds;
//! - playback: one `AudioStreamPlayer` with an `AudioStreamGenerator` per
//!   participant, on `output_bus`.
//!
//! The network runs on the `GridspawnClient` node's runtime.

use std::{collections::HashMap, sync::mpsc};

use godot::{
    classes::{
        AudioEffect, AudioEffectCapture, AudioServer, AudioStreamGenerator,
        AudioStreamGeneratorPlayback, AudioStreamMicrophone, AudioStreamPlayer, INode,
        ProjectSettings,
    },
    prelude::*,
};
use gridspawn_voice::{AudioOptions, TransmitMode, VoiceEvent, VoiceOptions, VoiceSession};
use tokio::{runtime::Handle, task::JoinHandle};

use crate::GridspawnClient;

/// The voice core's sample rate (mono).
const RATE: f64 = 48_000.0;
/// The only room the backend serves today.
const ROOM: &str = "match";
/// Playback buffer per participant. Larger survives longer frame hitches,
/// smaller adds less delay.
const PLAYBACK_BUFFER_SECONDS: f32 = 0.1;
/// Capture buffers (microphone, echo reference): several frames of slack.
const CAPTURE_BUFFER_SECONDS: f32 = 0.5;

/// Where the voice sent to others comes from.
#[derive(GodotConvert, Var, Export, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[godot(via = i64)]
pub enum VoiceInput {
    /// The microphone (`AudioServer.input_device`). Needs the project setting
    /// Audio > Driver > Enable Input.
    #[default]
    Microphone,
    /// Samples the game pushes with `push_input()` (mono, 48 kHz).
    Custom,
}

/// Background results, delivered on the main thread.
enum Outcome {
    Joined(VoiceSession),
    JoinFailed(String, String),
    Event(VoiceEvent),
    Warning(String),
}

/// Linear resampling of a mono stream to 48 kHz (Godot mixes at its own
/// rate, usually 44.1 or 48 kHz).
struct Resampler {
    step: f64,
    position: f64,
    previous: f32,
}

impl Resampler {
    fn new(rate: f64) -> Self {
        Self {
            step: rate / RATE,
            position: 0.0,
            previous: 0.0,
        }
    }

    /// Appends `input` resampled to `out`. Positions count from `previous`
    /// (index 0); `input[k]` is index `k + 1`.
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        if (self.step - 1.0).abs() < 1e-9 {
            out.extend_from_slice(input);
            return;
        }
        let n = input.len() as f64;
        while self.position < n {
            let index = self.position as usize;
            let fraction = (self.position - index as f64) as f32;
            let a = if index == 0 {
                self.previous
            } else {
                input[index - 1]
            };
            let b = input[index];
            out.push(a + (b - a) * fraction);
            self.position += self.step;
        }
        self.position -= n;
        self.previous = input[input.len() - 1];
    }
}

/// One `AudioEffectCapture` on a bus, read every frame.
struct Tap {
    bus: StringName,
    capture: Gd<AudioEffectCapture>,
    resampler: Resampler,
}

impl Tap {
    fn install(bus: &StringName) -> Self {
        let mut capture = AudioEffectCapture::new_gd();
        capture.set_buffer_length(CAPTURE_BUFFER_SECONDS);
        let mut server = AudioServer::singleton();
        let index = server.get_bus_index(bus);
        server.add_bus_effect(index, &capture);
        Self {
            bus: bus.clone(),
            capture,
            resampler: Resampler::new(server.get_mix_rate() as f64),
        }
    }

    /// Everything captured since the last call, mono 48 kHz.
    fn read(&mut self, out: &mut Vec<f32>) {
        out.clear();
        let frames = self.capture.get_frames_available();
        if frames <= 0 {
            return;
        }
        let buffer = self.capture.get_buffer(frames);
        let mono: Vec<f32> = buffer
            .as_slice()
            .iter()
            .map(|frame| 0.5 * (frame.x + frame.y))
            .collect();
        self.resampler.process(&mono, out);
    }

    fn remove(self) {
        let mut server = AudioServer::singleton();
        let index = server.get_bus_index(&self.bus);
        if index < 0 {
            return;
        }
        let ours: Gd<AudioEffect> = self.capture.upcast();
        for effect in (0..server.get_bus_effect_count(index)).rev() {
            if server.get_bus_effect(index, effect).as_ref() == Some(&ours) {
                server.remove_bus_effect(index, effect);
            }
        }
    }
}

struct Remote {
    player: Gd<AudioStreamPlayer>,
    playback: Gd<AudioStreamGeneratorPlayback>,
}

/// In-engine voice for the session attached with `Gridspawn.attach(ticket)`.
/// Add it next to the `Gridspawn` autoload (the addon's `gridspawn_voice.tscn`
/// is an autoload scene named `GridspawnVoice`).
#[derive(GodotClass)]
#[class(base = Node)]
pub struct GridspawnVoice {
    /// The `GridspawnClient` node (the `Gridspawn` autoload).
    #[export]
    client_path: NodePath,
    #[export]
    input_source: VoiceInput,
    /// The microphone's bus. Created, muted, when it does not exist.
    #[export]
    input_bus: StringName,
    /// The bus the other players' voices play on.
    #[export]
    output_bus: StringName,
    #[export]
    echo_cancellation: bool,
    #[export]
    noise_suppression: bool,
    #[export]
    auto_gain: bool,
    /// Send only while `set_transmitting(true)` (the key is held). Off: the
    /// microphone is open and sends while the player speaks.
    #[export]
    #[var(set = set_push_to_talk)]
    push_to_talk: bool,

    session: Option<VoiceSession>,
    handle: Option<Handle>,
    joining: bool,
    /// `leave()` was called while joining.
    cancel_join: bool,
    events_task: Option<JoinHandle<()>>,
    outcomes_tx: mpsc::Sender<Outcome>,
    outcomes_rx: mpsc::Receiver<Outcome>,
    microphone: Option<(Gd<AudioStreamPlayer>, Tap)>,
    reference: Option<Tap>,
    remotes: HashMap<String, Remote>,
    samples: Vec<f32>,
    base: Base<Node>,
}

#[godot_api]
impl INode for GridspawnVoice {
    fn init(base: Base<Node>) -> Self {
        let (outcomes_tx, outcomes_rx) = mpsc::channel();
        Self {
            client_path: NodePath::from("/root/Gridspawn"),
            input_source: VoiceInput::Microphone,
            input_bus: StringName::from("GridspawnVoiceInput"),
            output_bus: StringName::from("Master"),
            echo_cancellation: true,
            noise_suppression: true,
            auto_gain: true,
            push_to_talk: false,
            session: None,
            handle: None,
            joining: false,
            cancel_join: false,
            events_task: None,
            outcomes_tx,
            outcomes_rx,
            microphone: None,
            reference: None,
            remotes: HashMap::new(),
            samples: Vec::new(),
            base,
        }
    }

    fn process(&mut self, _delta: f64) {
        let outcomes: Vec<Outcome> = self.outcomes_rx.try_iter().collect();
        for outcome in outcomes {
            self.deliver(outcome);
        }
        self.pump_audio();
    }

    fn exit_tree(&mut self) {
        self.leave();
    }
}

#[godot_api]
impl GridspawnVoice {
    #[signal]
    fn joined(room: GString);
    #[signal]
    fn left(reason: GString);
    #[signal]
    fn participant_joined(participant_id: GString, display_name: GString);
    #[signal]
    fn participant_left(participant_id: GString);
    #[signal]
    fn speaking_started(participant_id: GString);
    #[signal]
    fn speaking_stopped(participant_id: GString);
    #[signal]
    fn error(code: GString, message: GString);

    /// Joins the attached session's voice room → `joined` | `error`. This
    /// version has one room per session ("match"), where everyone hears
    /// everyone.
    #[func]
    fn join(&mut self) {
        if self.session.is_some() || self.joining {
            godot_warn!("GridspawnVoice: already joined or joining");
            return;
        }
        let Some((client, attached, handle)) = self.context() else {
            self.emit_error(
                "voice/not_attached",
                "Attach a game session first (Gridspawn.attach(ticket)).",
            );
            return;
        };
        if self.input_source == VoiceInput::Microphone && !input_enabled() {
            self.emit_error(
                "voice/microphone_unavailable",
                "Turn on Project Settings > Audio > Driver > Enable Input to use the microphone.",
            );
            return;
        }
        let options = VoiceOptions::default()
            .audio(
                AudioOptions::default()
                    .echo_cancellation(self.echo_cancellation)
                    .noise_suppression(self.noise_suppression)
                    .auto_gain(self.auto_gain),
            )
            .transmit(self.transmit_mode());
        self.joining = true;
        self.cancel_join = false;
        let outcomes = self.outcomes_tx.clone();
        handle.spawn(async move {
            let outcome = match VoiceSession::join(&client, &attached, options).await {
                Ok(session) => Outcome::Joined(session),
                Err(error) => Outcome::JoinFailed(error.code(), error.to_string()),
            };
            let _ = outcomes.send(outcome);
        });
        self.handle = Some(handle);
    }

    /// Leaves the room → `left`.
    #[func]
    fn leave(&mut self) {
        if self.joining {
            self.cancel_join = true;
        }
        if let (Some(session), Some(handle)) = (self.session.clone(), self.handle.as_ref()) {
            handle.spawn(async move { session.leave().await });
        }
    }

    #[func]
    fn is_joined(&self) -> bool {
        self.session.is_some()
    }

    /// Mutes or unmutes this player's microphone.
    #[func]
    fn set_muted(&mut self, muted: bool) {
        let (Some(session), Some(handle)) = (self.session.clone(), self.handle.as_ref()) else {
            return;
        };
        let outcomes = self.outcomes_tx.clone();
        handle.spawn(async move {
            if let Err(error) = session.set_muted(muted).await {
                let _ = outcomes.send(Outcome::Warning(format!("mute: {error}")));
            }
        });
    }

    #[func]
    fn is_muted(&self) -> bool {
        self.session.as_ref().is_some_and(VoiceSession::is_muted)
    }

    /// Silences every other player for this player (locally).
    #[func]
    fn set_deafened(&mut self, deafened: bool) {
        if let Some(session) = &self.session {
            session.set_deafened(deafened);
        }
    }

    #[func]
    fn set_push_to_talk(&mut self, enabled: bool) {
        self.push_to_talk = enabled;
        let mode = self.transmit_mode();
        if let Some(session) = &self.session {
            session.set_transmit_mode(mode);
        }
    }

    /// The push-to-talk key state.
    #[func]
    fn set_transmitting(&mut self, on: bool) {
        if let Some(session) = &self.session {
            session.set_transmitting(on);
        }
    }

    /// This player's volume for one other player, 0.0 to 2.0 (1.0 = as sent).
    #[func]
    fn set_player_volume(&mut self, participant_id: GString, gain: f32) {
        if let Some(session) = &self.session {
            session.set_participant_volume(&participant_id.to_string(), gain);
        }
    }

    /// `[{participant_id, display_name, speaking, gain}]`. `participant_id`
    /// is opaque, never for display; `display_name` is empty in this version.
    #[func]
    fn get_participants(&self) -> Array<VarDictionary> {
        let mut list = Array::new();
        let Some(session) = &self.session else {
            return list;
        };
        for participant in session.participants() {
            let mut entry = VarDictionary::new();
            put(&mut entry, "participant_id", participant.id.to_variant());
            put(
                &mut entry,
                "display_name",
                participant
                    .display_name
                    .as_deref()
                    .unwrap_or("")
                    .to_variant(),
            );
            put(&mut entry, "speaking", participant.speaking.to_variant());
            put(&mut entry, "gain", participant.volume.to_variant());
            list.push(&entry);
        }
        list
    }

    /// This player's own participant id (in `speaking_started`/`stopped`).
    #[func]
    fn get_local_participant_id(&self) -> GString {
        self.session
            .as_ref()
            .map(|session| GString::from(session.local_id()))
            .unwrap_or_default()
    }

    /// Mono 48 kHz samples to send, with `input_source = Custom`.
    #[func]
    fn push_input(&mut self, samples: PackedFloat32Array) {
        if self.input_source != VoiceInput::Custom {
            godot_warn!("GridspawnVoice: push_input() needs input_source = Custom");
            return;
        }
        if let Some(session) = &self.session {
            session.push_capture(samples.as_slice());
        }
    }

    /// Counters for diagnostics: `packets_sent`, `auth_renewals`,
    /// `join_ms` (`[[step, milliseconds], …]`) and `participants`
    /// (`{participant_id: {packets_received, frames_from_data,
    /// frames_concealed, target_delay_ms}}`).
    #[func]
    fn get_stats(&self) -> VarDictionary {
        let mut stats = VarDictionary::new();
        let Some(session) = &self.session else {
            return stats;
        };
        let counters = session.stats();
        put(
            &mut stats,
            "packets_sent",
            (counters.packets_sent as i64).to_variant(),
        );
        put(
            &mut stats,
            "auth_renewals",
            (counters.auth_renewals as i64).to_variant(),
        );
        let mut join = VarArray::new();
        for (step, took) in session.join_timings() {
            let mut pair = VarArray::new();
            pair.push(&step.to_variant());
            pair.push(&(took.as_millis() as i64).to_variant());
            join.push(&pair.to_variant());
        }
        put(&mut stats, "join_ms", join.to_variant());
        let mut participants = VarDictionary::new();
        for (id, received) in &counters.packets_received {
            let mut entry = VarDictionary::new();
            let get = |map: &HashMap<String, u64>| {
                (map.get(id).copied().unwrap_or(0) as i64).to_variant()
            };
            put(
                &mut entry,
                "packets_received",
                (*received as i64).to_variant(),
            );
            put(
                &mut entry,
                "frames_from_data",
                get(&counters.frames_from_data),
            );
            put(
                &mut entry,
                "frames_concealed",
                get(&counters.frames_concealed),
            );
            put(
                &mut entry,
                "target_delay_ms",
                get(&counters.target_delay_ms),
            );
            put(&mut participants, id, entry.to_variant());
        }
        put(&mut stats, "participants", participants.to_variant());
        stats
    }

    #[func]
    fn get_input_devices(&self) -> PackedStringArray {
        AudioServer::singleton().get_input_device_list()
    }

    #[func]
    fn set_input_device(&mut self, name: GString) {
        AudioServer::singleton().set_input_device(&name);
    }

    #[func]
    fn get_output_devices(&self) -> PackedStringArray {
        AudioServer::singleton().get_output_device_list()
    }

    /// Godot's output device: the whole game's, not only the voices.
    #[func]
    fn set_output_device(&mut self, name: GString) {
        AudioServer::singleton().set_output_device(&name);
    }
}

fn put(dictionary: &mut VarDictionary, key: &str, value: Variant) {
    dictionary.set(&key.to_variant(), &value);
}

fn input_enabled() -> bool {
    ProjectSettings::singleton()
        .get_setting("audio/driver/enable_input")
        .try_to::<bool>()
        .unwrap_or(false)
}

impl GridspawnVoice {
    fn transmit_mode(&self) -> TransmitMode {
        if self.push_to_talk {
            TransmitMode::PushToTalk
        } else {
            TransmitMode::VoiceActivation
        }
    }

    fn context(
        &self,
    ) -> Option<(
        gridspawn_client::Client,
        gridspawn_client::AttachedSession,
        Handle,
    )> {
        let node = self.base().get_node_or_null(&self.client_path)?;
        let mut client = node.try_cast::<GridspawnClient>().ok()?;
        let mut client = client.bind_mut();
        client.voice_context()
    }

    fn emit_error(&mut self, code: &str, message: &str) {
        self.base_mut().emit_signal(
            "error",
            &[
                GString::from(code).to_variant(),
                GString::from(message).to_variant(),
            ],
        );
    }

    fn emit_participant(&mut self, signal: &str, participant: &str) {
        self.base_mut()
            .emit_signal(signal, &[GString::from(participant).to_variant()]);
    }

    fn deliver(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Joined(session) => {
                self.joining = false;
                if self.cancel_join {
                    if let Some(handle) = &self.handle {
                        handle.spawn(async move { session.leave().await });
                    }
                    self.emit_left("left");
                    return;
                }
                self.start(session);
            }
            Outcome::JoinFailed(code, message) => {
                self.joining = false;
                self.emit_error(&code, &message);
            }
            Outcome::Event(event) => self.on_event(event),
            Outcome::Warning(message) => godot_warn!("GridspawnVoice: {message}"),
        }
    }

    /// Joined: audio in and out, then the events.
    fn start(&mut self, session: VoiceSession) {
        // Subscribe before the snapshot so no participant is missed.
        let mut events = session.events();
        if self.input_source == VoiceInput::Microphone {
            self.microphone = Some(self.open_microphone());
        }
        if self.echo_cancellation {
            self.reference = Some(Tap::install(&StringName::from("Master")));
        }
        self.session = Some(session.clone());
        self.base_mut()
            .emit_signal("joined", &[GString::from(ROOM).to_variant()]);
        // Everyone already in the room, after `joined`.
        for participant in session.participants() {
            self.add_remote(&participant.id);
        }
        let outcomes = self.outcomes_tx.clone();
        self.events_task = self.handle.as_ref().map(|handle| {
            handle.spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(event) => {
                            let left = matches!(event, VoiceEvent::Left { .. });
                            if outcomes.send(Outcome::Event(event)).is_err() || left {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            })
        });
    }

    fn on_event(&mut self, event: VoiceEvent) {
        match event {
            VoiceEvent::ParticipantJoined { participant } => self.add_remote(&participant),
            VoiceEvent::ParticipantLeft { participant } => {
                if let Some(mut remote) = self.remotes.remove(&participant) {
                    remote.player.queue_free();
                    self.emit_participant("participant_left", &participant);
                }
            }
            VoiceEvent::SpeakingStarted { participant } => {
                self.emit_participant("speaking_started", &participant)
            }
            VoiceEvent::SpeakingStopped { participant } => {
                self.emit_participant("speaking_stopped", &participant)
            }
            VoiceEvent::LocalSpeaking { active } => {
                let local = self.get_local_participant_id().to_string();
                let signal = if active {
                    "speaking_started"
                } else {
                    "speaking_stopped"
                };
                self.emit_participant(signal, &local);
            }
            VoiceEvent::Error { code, message } => self.emit_error(&code, &message),
            VoiceEvent::Warning { code, message } => {
                godot_warn!("GridspawnVoice: {code}: {message}")
            }
            VoiceEvent::Left { reason } => {
                self.stop();
                self.emit_left(&reason);
            }
            _ => {}
        }
    }

    fn emit_left(&mut self, reason: &str) {
        self.base_mut()
            .emit_signal("left", &[GString::from(reason).to_variant()]);
    }

    /// A player and its generator for one participant.
    fn add_remote(&mut self, participant: &str) {
        if self.remotes.contains_key(participant) {
            return;
        }
        let mut generator = AudioStreamGenerator::new_gd();
        generator.set_mix_rate(RATE as f32);
        generator.set_buffer_length(PLAYBACK_BUFFER_SECONDS);
        let mut player = AudioStreamPlayer::new_alloc();
        player.set_stream(&generator);
        player.set_bus(&self.output_bus);
        self.base_mut().add_child(&player);
        player.play();
        let Some(playback) = player
            .get_stream_playback()
            .and_then(|playback| playback.try_cast::<AudioStreamGeneratorPlayback>().ok())
        else {
            godot_warn!("GridspawnVoice: no generator playback");
            player.queue_free();
            return;
        };
        self.remotes
            .insert(participant.to_owned(), Remote { player, playback });
        self.base_mut().emit_signal(
            "participant_joined",
            &[
                GString::from(participant).to_variant(),
                // Display names come with the voice roster (server work).
                GString::new().to_variant(),
            ],
        );
    }

    /// The microphone on its (muted) input bus, with a capture.
    fn open_microphone(&mut self) -> (Gd<AudioStreamPlayer>, Tap) {
        let mut server = AudioServer::singleton();
        if server.get_bus_index(&self.input_bus) < 0 {
            server.add_bus();
            let index = server.get_bus_count() - 1;
            server.set_bus_name(index, &GString::from(&self.input_bus));
            // Captured, not heard.
            server.set_bus_mute(index, true);
        }
        let tap = Tap::install(&self.input_bus);
        let mut player = AudioStreamPlayer::new_alloc();
        player.set_stream(&AudioStreamMicrophone::new_gd());
        player.set_bus(&self.input_bus);
        self.base_mut().add_child(&player);
        player.play();
        (player, tap)
    }

    /// Main thread, every frame: echo reference, microphone, playback.
    fn pump_audio(&mut self) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let mut samples = std::mem::take(&mut self.samples);
        if let Some(reference) = &mut self.reference {
            reference.read(&mut samples);
            session.push_playback_reference(&samples);
        }
        if let Some((_, microphone)) = &mut self.microphone {
            microphone.read(&mut samples);
            session.push_capture(&samples);
        }
        for (id, remote) in &mut self.remotes {
            let frames = remote.playback.get_frames_available();
            if frames <= 0 {
                continue;
            }
            samples.clear();
            samples.resize(frames as usize, 0.0);
            session.pull_participant(id, &mut samples);
            let stereo: PackedVector2Array = samples.iter().map(|s| Vector2::new(*s, *s)).collect();
            remote.playback.push_buffer(&stereo);
        }
        self.samples = samples;
    }

    /// Left: removes the players and the captures.
    fn stop(&mut self) {
        self.session = None;
        if let Some(task) = self.events_task.take() {
            task.abort();
        }
        for (_, mut remote) in self.remotes.drain() {
            remote.player.queue_free();
        }
        if let Some((mut player, tap)) = self.microphone.take() {
            player.queue_free();
            tap.remove();
        }
        if let Some(reference) = self.reference.take() {
            reference.remove();
        }
    }
}

impl Drop for GridspawnVoice {
    fn drop(&mut self) {
        if let (Some(session), Some(handle)) = (self.session.take(), self.handle.as_ref()) {
            handle.spawn(async move { session.leave().await });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_keeps_a_tone() {
        let mut resampler = Resampler::new(44_100.0);
        let tone: Vec<f32> = (0..44_100)
            .map(|n| (2.0 * std::f32::consts::PI * 440.0 * n as f32 / 44_100.0).sin())
            .collect();
        let mut out = Vec::new();
        for chunk in tone.chunks(735) {
            resampler.process(chunk, &mut out);
        }
        assert!((out.len() as i64 - 48_000).abs() <= 2, "{}", out.len());
        // The same tone at 48 kHz, one sample of delay (the previous sample).
        let worst = out
            .iter()
            .enumerate()
            .skip(10)
            .map(|(n, s)| {
                let t = (n as f64 - 48_000.0 / 44_100.0) / 48_000.0;
                (s - (2.0 * std::f64::consts::PI * 440.0 * t).sin() as f32).abs()
            })
            .fold(0f32, f32::max);
        assert!(worst < 0.01, "worst error {worst}");
    }

    #[test]
    fn resampler_passes_48k_through() {
        let mut resampler = Resampler::new(48_000.0);
        let mut out = Vec::new();
        resampler.process(&[0.1, 0.2, 0.3], &mut out);
        assert_eq!(out, [0.1, 0.2, 0.3]);
    }
}
