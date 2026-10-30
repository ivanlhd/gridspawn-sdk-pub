//! The companion's state: one task that handles the DLL's requests, the
//! window's actions and the results of slow work (attach, voice join,
//! sign-in), and sends events back to the DLL.

use std::{
    str::FromStr,
    sync::{Arc, Mutex},
};

use gridspawn_client::{
    AttachedSession, Client, Environment, Presence, PresenceUpdater, SignIn, User,
};
use gridspawn_voice::{TransmitMode, VoiceEvent, VoiceOptions, VoiceSession};
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::{
    audio::{self, AudioCmd},
    ui::{Model, Participant},
    Launch,
};

/// Scopes asked at sign-in (protocol 01).
const SCOPES: [&str; 3] = ["identity.basic", "presence.write", "voice.join"];

pub enum Cmd {
    Op {
        id: u64,
        op: String,
        args: Value,
    },
    Ui(UiAction),
    /// Tagged with the attach generation (see `Core::attach_gen`).
    Attached(u64, Result<AttachedSession, gridspawn_client::Error>),
    /// Tagged with the voice generation of the join (see `Core::voice_gen`).
    VoiceJoined(u64, Result<VoiceSession, gridspawn_voice::VoiceError>),
    Voice(u64, VoiceEvent),
    SignedIn(Result<User, gridspawn_client::Error>),
    PipeClosed,
    Shutdown,
}

pub enum UiAction {
    SetMuted(bool),
    SetDeafened(bool),
    SetPushToTalk(bool),
    SetVolume(String, f32),
    SetParticipantMuted(String, bool),
    SelectDevice { input: bool, name: Option<String> },
    Join,
    Leave,
}

struct Core {
    launch: Launch,
    model: Arc<Mutex<Model>>,
    cmd: UnboundedSender<Cmd>,
    out: UnboundedSender<Value>,
    audio: std::sync::mpsc::Sender<AudioCmd>,
    client: Option<Client>,
    attached: Option<AttachedSession>,
    voice: Option<VoiceSession>,
    /// Bumped by every join and leave: a join result or voice event from an
    /// earlier voice session (one left, or replaced by a group switch) is
    /// ignored instead of touching the current one.
    voice_gen: u64,
    /// Bumped by every attach and detach: the result of an attach that was
    /// replaced or cancelled meanwhile is dropped (dropping detaches it).
    attach_gen: u64,
    joining: bool,
    rejoin: bool,
    muted: bool,
    deafened: bool,
    push_to_talk: bool,
    input: Option<String>,
    output: Option<String>,
    presence: Option<PresenceUpdater>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Ends the companion when the game process ends (also on a crash).
pub fn watch_game(pid: u32, cmd: UnboundedSender<Cmd>) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE,
        };
        let _ = std::thread::Builder::new()
            .name("gridspawn-companion-watch".into())
            .spawn(move || {
                // SAFETY: a process handle for waiting only.
                let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
                if !handle.is_null() {
                    // SAFETY: a valid handle.
                    unsafe { WaitForSingleObject(handle, INFINITE) };
                }
                tracing::info!("the game process ended");
                let _ = cmd.send(Cmd::Shutdown);
                std::thread::sleep(std::time::Duration::from_secs(5));
                std::process::exit(0);
            });
    }
    #[cfg(not(windows))]
    {
        let _ = (pid, cmd);
    }
}

pub async fn run(
    launch: Launch,
    model: Arc<Mutex<Model>>,
    cmd: UnboundedSender<Cmd>,
    mut rx: UnboundedReceiver<Cmd>,
) {
    let (out, out_rx) = unbounded_channel::<Value>();
    #[cfg(windows)]
    {
        match crate::pipes::accept(launch.game_pid, &launch.token).await {
            Ok((incoming, outgoing)) => {
                spawn_pipe_tasks(incoming, outgoing, out_rx, cmd.clone());
            }
            Err(error) => {
                tracing::error!(%error, "pipes not established");
                return;
            }
        }
    }
    #[cfg(not(windows))]
    drop(out_rx);
    lock(&model).status = "Ready".into();
    let _ = out.send(json!({"event": "ready", "version": env!("CARGO_PKG_VERSION")}));
    let mut core = Core {
        audio: audio::spawn(out.clone()),
        launch,
        model,
        cmd,
        out,
        client: None,
        attached: None,
        voice: None,
        voice_gen: 0,
        attach_gen: 0,
        joining: false,
        rejoin: false,
        muted: false,
        deafened: false,
        push_to_talk: false,
        input: None,
        output: None,
        presence: None,
    };
    core.devices();
    while let Some(command) = rx.recv().await {
        match command {
            Cmd::Shutdown | Cmd::PipeClosed => {
                core.stop().await;
                break;
            }
            other => core.handle(other).await,
        }
    }
}

#[cfg(windows)]
fn spawn_pipe_tasks(
    mut incoming: tokio::net::windows::named_pipe::NamedPipeServer,
    mut outgoing: tokio::net::windows::named_pipe::NamedPipeServer,
    mut out_rx: UnboundedReceiver<Value>,
    cmd: UnboundedSender<Cmd>,
) {
    tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if crate::frames::write_frame(&mut outgoing, &frame)
                .await
                .is_err()
            {
                break;
            }
        }
    });
    tokio::spawn(async move {
        loop {
            match crate::frames::read_frame(&mut incoming).await {
                Ok(Some(frame)) => {
                    let id = frame.get("id").and_then(Value::as_u64).unwrap_or(0);
                    let op = frame
                        .get("op")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let args = frame.get("args").cloned().unwrap_or(Value::Null);
                    if cmd.send(Cmd::Op { id, op, args }).is_err() {
                        break;
                    }
                }
                _ => {
                    let _ = cmd.send(Cmd::PipeClosed);
                    break;
                }
            }
        }
    });
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

impl Core {
    fn emit(&self, event: Value) {
        let _ = self.out.send(event);
    }

    /// Id 0 = an action from the voice window: no reply goes to the DLL.
    fn fail(&self, id: u64, code: &str, message: &str) {
        if id == 0 {
            tracing::warn!(code, message, "window action failed");
            self.status(message);
            return;
        }
        self.emit(json!({"id": id, "ok": false, "error": {"code": code, "message": message}}));
    }

    fn ok(&self, id: u64) {
        if id != 0 {
            self.emit(json!({"id": id, "ok": true, "result": {}}));
        }
    }

    fn status(&self, text: &str) {
        lock(&self.model).status = text.to_owned();
    }

    async fn client(&mut self) -> Result<Client, gridspawn_client::Error> {
        if let Some(client) = &self.client {
            return Ok(client.clone());
        }
        let environment =
            Environment::from_str(&self.launch.environment).unwrap_or(Environment::Production);
        let mut builder = Client::builder(self.launch.app_id.clone(), environment);
        if let Some(url) = self
            .launch
            .sdk_base_url
            .as_deref()
            .and_then(|u| url::Url::parse(u).ok())
        {
            builder = builder.sdk_base_url(url);
        }
        let client = builder.connect().await?;
        self.client = Some(client.clone());
        Ok(client)
    }

    fn devices(&self) {
        let inputs = gridspawn_voice::devices::device_names(true);
        let outputs = gridspawn_voice::devices::device_names(false);
        self.emit(
            json!({"event": "devices", "input": true, "names": inputs, "selected": self.input}),
        );
        self.emit(
            json!({"event": "devices", "input": false, "names": outputs, "selected": self.output}),
        );
        let mut m = lock(&self.model);
        m.inputs = inputs;
        m.outputs = outputs;
        m.input = self.input.clone();
        m.output = self.output.clone();
    }

    fn start_audio(&self) {
        if let Some(voice) = &self.voice {
            let _ = self.audio.send(AudioCmd::Start {
                voice: voice.clone(),
                input: self.input.clone(),
                output: self.output.clone(),
                tone: self.launch.test_tone,
            });
        }
    }

    async fn leave_voice(&mut self) {
        let _ = self.audio.send(AudioCmd::Stop);
        self.voice_gen += 1;
        if let Some(voice) = self.voice.take() {
            voice.leave().await;
            self.emit(json!({"event": "voice_state", "state": 0}));
        } else if std::mem::take(&mut self.joining) {
            // The pending join is left when its (now stale) result arrives.
            self.emit(json!({"event": "voice_state", "state": 0}));
        }
        let mut m = lock(&self.model);
        m.voice_state = 0;
        m.participants.clear();
    }

    async fn stop(&mut self) {
        self.leave_voice().await;
        if let Some(presence) = self.presence.take() {
            let _ = presence.clear().await;
        }
        self.attached = None;
    }

    fn join_voice(&mut self) -> Result<(), (&'static str, &'static str)> {
        if self.voice.is_some() || self.joining {
            return Err((
                "voice/already_joined",
                "Voice is already joined or joining.",
            ));
        }
        let (Some(client), Some(attached)) = (self.client.clone(), self.attached.clone()) else {
            return Err((
                "voice/not_attached",
                "Attach a session first (gs_rag_attach).",
            ));
        };
        self.joining = true;
        self.emit(json!({"event": "voice_state", "state": 1}));
        lock(&self.model).voice_state = 1;
        self.status("Joining voice…");
        let transmit = if self.push_to_talk {
            TransmitMode::PushToTalk
        } else {
            TransmitMode::default()
        };
        self.voice_gen += 1;
        let generation = self.voice_gen;
        let cmd = self.cmd.clone();
        // The default capture processing, except in a testing build measuring it.
        let audio = crate::audio::test_audio_options(Default::default(), self.launch.test_tone);
        tokio::spawn(async move {
            let options = VoiceOptions::default().transmit(transmit).audio(audio);
            let result = VoiceSession::join(&client, &attached, options).await;
            let _ = cmd.send(Cmd::VoiceJoined(generation, result));
        });
        Ok(())
    }

    fn participant_event(&self, joined: bool, id: &str) {
        // A player who left is no longer in the voice session's list: use the
        // name stored when they joined.
        let known = lock(&self.model)
            .participants
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone());
        let name = known
            .or_else(|| {
                self.voice
                    .as_ref()
                    .and_then(|v| v.participant(id))
                    .and_then(|p| p.display_name)
            })
            .unwrap_or_default();
        self.emit(json!({"event": "participant", "joined": joined, "id": id, "displayName": name}));
        let mut m = lock(&self.model);
        if joined {
            if !m.participants.iter().any(|p| p.id == id) {
                m.participants.push(Participant {
                    id: id.to_owned(),
                    name,
                    speaking: false,
                    volume: 1.0,
                    muted: false,
                });
            }
        } else {
            m.participants.retain(|p| p.id != id);
        }
    }

    async fn handle(&mut self, command: Cmd) {
        match command {
            Cmd::Op { id, op, args } => self.op(id, &op, &args).await,
            Cmd::Ui(action) => self.ui(action).await,
            Cmd::Attached(generation, _) if generation != self.attach_gen => {
                tracing::debug!("result of a replaced or cancelled attach dropped");
            }
            Cmd::Attached(_, result) => match result {
                Ok(attached) => {
                    let name = attached
                        .participant()
                        .display_name
                        .clone()
                        .unwrap_or_default();
                    let roster: Vec<Value> = self
                        .voice
                        .as_ref()
                        .map(|v| {
                            v.participants()
                                .into_iter()
                                .map(|p| json!({"id": p.id, "displayName": p.display_name.unwrap_or_default()}))
                                .collect()
                        })
                        .unwrap_or_default();
                    self.attached = Some(attached);
                    self.emit(json!({"event": "attached", "displayName": name, "roster": roster}));
                    {
                        let mut m = lock(&self.model);
                        m.attached_as = Some(name);
                    }
                    self.status("Attached");
                    if std::mem::take(&mut self.rejoin) {
                        if let Err((code, message)) = self.join_voice() {
                            self.emit(json!({"event": "error", "code": code, "message": message}));
                        }
                    }
                }
                Err(error) => {
                    self.rejoin = false;
                    self.status("Attach failed");
                    self.emit(json!({"event": "error", "code": error.code(), "message": error.to_string()}));
                }
            },
            Cmd::VoiceJoined(generation, result) => {
                if generation != self.voice_gen {
                    // Left or replaced while joining.
                    if let Ok(voice) = result {
                        tokio::spawn(async move { voice.leave().await });
                    }
                    return;
                }
                self.joining = false;
                match result {
                    Ok(voice) => {
                        let _ = voice.set_muted(self.muted).await;
                        voice.set_deafened(self.deafened);
                        self.voice = Some(voice.clone());
                        self.start_audio();
                        let mut events = voice.events();
                        let cmd = self.cmd.clone();
                        tokio::spawn(async move {
                            loop {
                                match events.recv().await {
                                    Ok(event) => {
                                        if cmd.send(Cmd::Voice(generation, event)).is_err() {
                                            break;
                                        }
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                    Err(_) => break,
                                }
                            }
                        });
                        self.emit(json!({"event": "voice_state", "state": 2}));
                        lock(&self.model).voice_state = 2;
                        self.status("In voice");
                        for p in voice.participants() {
                            self.participant_event(true, &p.id);
                        }
                    }
                    Err(error) => {
                        self.emit(json!({"event": "voice_state", "state": 0}));
                        lock(&self.model).voice_state = 0;
                        self.status("Voice failed");
                        self.emit(json!({"event": "error", "code": error.code(), "message": error.to_string()}));
                    }
                }
            }
            Cmd::Voice(generation, event) if generation == self.voice_gen => {
                self.voice_event(event)
            }
            Cmd::Voice(_, event) => tracing::debug!(?event, "event of an earlier voice session"),
            Cmd::SignedIn(result) => match result {
                Ok(user) => {
                    lock(&self.model).signed_in = Some(user.display_name.clone());
                    self.emit(json!({"event": "signed_in", "displayName": user.display_name}));
                }
                Err(error) => {
                    self.emit(json!({"event": "error", "code": error.code(), "message": error.to_string()}));
                }
            },
            Cmd::PipeClosed | Cmd::Shutdown => {}
        }
    }

    fn voice_event(&mut self, event: VoiceEvent) {
        match event {
            VoiceEvent::ParticipantJoined { participant } => {
                self.participant_event(true, &participant)
            }
            VoiceEvent::ParticipantLeft { participant } => {
                self.participant_event(false, &participant)
            }
            VoiceEvent::SpeakingStarted { participant } => self.speaking(&participant, true),
            VoiceEvent::SpeakingStopped { participant } => self.speaking(&participant, false),
            VoiceEvent::LocalSpeaking { active } => {
                lock(&self.model).local_speaking = active;
                self.emit(json!({"event": "local_speaking", "active": active}));
            }
            VoiceEvent::ServerMuted { participant, muted } => {
                self.emit(json!({"event": "server_muted", "id": participant, "muted": muted}));
            }
            VoiceEvent::LocalServerMuted { muted } => {
                lock(&self.model).server_muted = muted;
                self.emit(json!({"event": "server_muted", "id": null, "muted": muted}));
            }
            VoiceEvent::Error { code, message } => {
                self.emit(json!({"event": "error", "code": code, "message": message}));
            }
            VoiceEvent::Left { reason } => {
                tracing::info!(%reason, "voice left");
                let _ = self.audio.send(AudioCmd::Stop);
                self.voice = None;
                self.emit(json!({"event": "voice_state", "state": 0}));
                let mut m = lock(&self.model);
                m.voice_state = 0;
                m.participants.clear();
            }
            other => tracing::debug!(?other, "voice event"),
        }
    }

    fn speaking(&self, id: &str, active: bool) {
        self.emit(json!({"event": "speaking", "id": id, "active": active}));
        if let Some(p) = lock(&self.model)
            .participants
            .iter_mut()
            .find(|p| p.id == id)
        {
            p.speaking = active;
        }
    }

    async fn ui(&mut self, action: UiAction) {
        let args = match action {
            UiAction::SetMuted(m) => ("set_muted", json!({"muted": m})),
            UiAction::SetDeafened(d) => ("set_deafened", json!({"deafened": d})),
            UiAction::SetPushToTalk(p) => ("set_transmit_mode", json!({"pushToTalk": p})),
            UiAction::SetVolume(id, v) => ("set_volume", json!({"participant": id, "volume": v})),
            UiAction::SetParticipantMuted(id, m) => (
                "set_participant_muted",
                json!({"participant": id, "muted": m}),
            ),
            UiAction::SelectDevice { input, name } => {
                ("select_device", json!({"input": input, "name": name}))
            }
            UiAction::Join => ("voice_join", json!({})),
            UiAction::Leave => ("voice_leave", json!({})),
        };
        // The window's actions take the same path as the DLL's requests,
        // with id 0 (no reply; a failure shows in the window's status line).
        self.op(0, args.0, &args.1).await;
    }

    async fn op(&mut self, id: u64, op: &str, args: &Value) {
        match op {
            "attach" => {
                let Some(ticket) = arg_str(args, "ticket") else {
                    return self.fail(id, "attach/invalid_ticket", "ticket is required");
                };
                let client = match self.client().await {
                    Ok(c) => c,
                    Err(e) => return self.fail(id, &e.code(), &e.to_string()),
                };
                if self.voice.is_some() || self.joining {
                    self.rejoin = true;
                }
                self.leave_voice().await;
                if self.attached.take().is_some() {
                    self.emit(json!({"event": "detached", "reason": "replaced"}));
                    lock(&self.model).attached_as = None;
                }
                self.status("Attaching…");
                self.attach_gen += 1;
                let generation = self.attach_gen;
                let cmd = self.cmd.clone();
                tokio::spawn(async move {
                    let result = client.attach(&ticket).await;
                    let _ = cmd.send(Cmd::Attached(generation, result));
                });
                self.ok(id);
            }
            "detach" => {
                // Also cancels an attach still in flight (its result is dropped).
                self.attach_gen += 1;
                self.rejoin = false;
                self.leave_voice().await;
                if self.attached.take().is_some() {
                    self.emit(json!({"event": "detached", "reason": "requested"}));
                }
                lock(&self.model).attached_as = None;
                self.status("Not attached");
                self.ok(id);
            }
            "voice_join" => match self.join_voice() {
                Ok(()) => self.ok(id),
                Err((code, message)) => self.fail(id, code, message),
            },
            "voice_leave" => {
                self.rejoin = false;
                self.leave_voice().await;
                self.status("Attached");
                self.ok(id);
            }
            "set_muted" => {
                self.muted = arg_bool(args, "muted");
                lock(&self.model).muted = self.muted;
                if let Some(voice) = &self.voice {
                    if let Err(e) = voice.set_muted(self.muted).await {
                        return self.fail(id, &e.code(), &e.to_string());
                    }
                }
                self.ok(id);
            }
            "set_deafened" => {
                self.deafened = arg_bool(args, "deafened");
                lock(&self.model).deafened = self.deafened;
                if let Some(voice) = &self.voice {
                    voice.set_deafened(self.deafened);
                }
                self.ok(id);
            }
            "set_transmit_mode" => {
                self.push_to_talk = arg_bool(args, "pushToTalk");
                lock(&self.model).push_to_talk = self.push_to_talk;
                if let Some(voice) = &self.voice {
                    voice.set_transmit_mode(if self.push_to_talk {
                        TransmitMode::PushToTalk
                    } else {
                        TransmitMode::default()
                    });
                }
                self.ok(id);
            }
            "set_transmitting" => {
                let pressed = arg_bool(args, "pressed");
                lock(&self.model).ptt_pressed = pressed;
                if let Some(voice) = &self.voice {
                    voice.set_transmitting(pressed);
                }
                self.ok(id);
            }
            "set_volume" => {
                let (Some(participant), Some(volume)) = (
                    arg_str(args, "participant"),
                    args.get("volume").and_then(Value::as_f64),
                ) else {
                    return self.fail(
                        id,
                        "voice/invalid_request",
                        "participant and volume are required",
                    );
                };
                let volume = (volume as f32).clamp(0.0, 2.0);
                if let Some(voice) = &self.voice {
                    voice.set_participant_volume(&participant, volume);
                }
                if let Some(p) = lock(&self.model)
                    .participants
                    .iter_mut()
                    .find(|p| p.id == participant)
                {
                    p.volume = volume;
                }
                self.ok(id);
            }
            "set_participant_muted" => {
                let Some(participant) = arg_str(args, "participant") else {
                    return self.fail(id, "voice/invalid_request", "participant is required");
                };
                let muted = arg_bool(args, "muted");
                if let Some(voice) = &self.voice {
                    if let Err(e) = voice.set_participant_muted(&participant, muted).await {
                        return self.fail(id, &e.code(), &e.to_string());
                    }
                }
                if let Some(p) = lock(&self.model)
                    .participants
                    .iter_mut()
                    .find(|p| p.id == participant)
                {
                    p.muted = muted;
                }
                self.ok(id);
            }
            "list_devices" => {
                self.devices();
                self.ok(id);
            }
            "select_device" => {
                let name = arg_str(args, "name");
                if arg_bool(args, "input") {
                    self.input = name;
                } else {
                    self.output = name;
                }
                self.devices();
                if self.voice.is_some() {
                    self.start_audio();
                }
                self.ok(id);
            }
            "sign_in" => {
                let client = match self.client().await {
                    Ok(c) => c,
                    Err(e) => return self.fail(id, &e.code(), &e.to_string()),
                };
                let cmd = self.cmd.clone();
                tokio::spawn(async move {
                    let result = client.sign_in(SignIn::new(SCOPES)).await;
                    let _ = cmd.send(Cmd::SignedIn(result));
                });
                self.ok(id);
            }
            "sign_out" => {
                if let Some(client) = &self.client {
                    let _ = client.sign_out().await;
                }
                self.presence = None;
                lock(&self.model).signed_in = None;
                self.emit(json!({"event": "signed_out"}));
                self.ok(id);
            }
            "player_assertion" => {
                let request_id = args.get("requestId").and_then(Value::as_u64).unwrap_or(0);
                let Some(challenge) = arg_str(args, "challenge") else {
                    return self.fail(id, "auth/invalid_challenge", "challenge is required");
                };
                let Some(client) = self.client.clone() else {
                    return self.fail(id, "auth/not_signed_in", "Sign in first (gs_rag_sign_in).");
                };
                match client.player_assertion(&challenge).await {
                    Ok(assertion) => {
                        self.emit(json!({"event": "assertion", "requestId": request_id, "assertion": assertion.as_str()}));
                        self.ok(id);
                    }
                    Err(e) => self.fail(id, &e.code(), &e.to_string()),
                }
            }
            "presence_set" => {
                let Some(client) = self.client.clone() else {
                    return self.fail(
                        id,
                        "presence/not_signed_in",
                        "Sign in first (gs_rag_sign_in).",
                    );
                };
                let mut presence = Presence::new();
                if let Some(details) = arg_str(args, "details") {
                    presence = presence.details(details);
                }
                if let Some(state) = arg_str(args, "state") {
                    presence = presence.state(state);
                }
                if self.presence.is_none() {
                    self.presence = Some(client.presence());
                }
                let result = match &self.presence {
                    Some(updater) => updater.set(presence).await,
                    None => return,
                };
                match result {
                    Ok(_) => self.ok(id),
                    Err(e) => self.fail(id, &e.code(), &e.to_string()),
                }
            }
            "presence_clear" => {
                if let Some(updater) = self.presence.take() {
                    if let Err(e) = updater.clear().await {
                        return self.fail(id, &e.code(), &e.to_string());
                    }
                }
                self.ok(id);
            }
            "show_window" => {
                if !self.launch.show_window {
                    return self.fail(
                        id,
                        "companion/no_window",
                        "The companion runs without a window (show_window = 0 at init).",
                    );
                }
                crate::ui::show(&self.model, arg_bool(args, "visible"));
                self.ok(id);
            }
            "shutdown" => {
                self.ok(id);
                let _ = self.cmd.send(Cmd::Shutdown);
            }
            other => self.fail(id, "companion/unknown_op", &format!("unknown op {other}")),
        }
    }
}
