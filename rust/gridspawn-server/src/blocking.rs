//! A small synchronous facade (feature `blocking`). It owns a current-thread
//! tokio runtime. Do not call it from async code or from a game's frame loop;
//! there is no background renewer here, so call [`Client::renew`] yourself.

use std::{sync::Arc, time::Duration};

use serde_json::Value;

use crate::{
    Action, ActionReceiverOptions, ActionReply, ActionStats, AdmitGuest, AdmitParticipant,
    ClientBuilder, ClientTicket, CreateSession, DataAck, DataBatch, DataListing, Error,
    EventReceipt, IdempotencyKey, Lease, Module, Participant, ParticipantId, ParticipantRef, Post,
    PostReceipt, Presence, PresenceAck, Session, VoiceAck, VoiceConfig, VoicePlayerState,
    VoicePosition,
};

pub struct Client {
    runtime: tokio::runtime::Runtime,
    client: crate::Client,
}

impl Client {
    pub fn connect(builder: ClientBuilder) -> Result<Self, Error> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| Error::Runtime(Arc::new(error)))?;
        let client = runtime.block_on(builder.connect())?;
        Ok(Self { runtime, client })
    }

    /// The async client inside.
    pub fn as_async(&self) -> &crate::Client {
        &self.client
    }

    pub fn create_session(&self, request: CreateSession) -> Result<Session, Error> {
        self.runtime.block_on(self.client.create_session(request))
    }

    pub fn post(
        &self,
        feature: &str,
        body: Post,
        key: &IdempotencyKey,
    ) -> Result<PostReceipt, Error> {
        self.runtime.block_on(self.client.post(feature, body, key))
    }

    pub fn renew(&self, session: &Session) -> Result<Lease, Error> {
        self.runtime.block_on(session.renew())
    }

    pub fn admit_guest(
        &self,
        session: &Session,
        request: AdmitGuest,
    ) -> Result<Participant, Error> {
        self.runtime.block_on(session.admit_guest(request))
    }

    pub fn admit_participant(
        &self,
        session: &Session,
        request: AdmitParticipant,
    ) -> Result<Participant, Error> {
        self.runtime.block_on(session.admit_participant(request))
    }

    pub fn issue_ticket(
        &self,
        participant: &Participant,
        modules: &[Module],
    ) -> Result<ClientTicket, Error> {
        self.runtime.block_on(participant.issue_ticket(modules))
    }

    /// Re-send every 60 s while the player is in the session.
    pub fn set_presence(
        &self,
        participant: &ParticipantRef,
        presence: &Presence,
    ) -> Result<PresenceAck, Error> {
        self.runtime.block_on(participant.set_presence(presence))
    }

    pub fn clear_presence(&self, participant: &ParticipantRef) -> Result<(), Error> {
        self.runtime.block_on(participant.clear_presence())
    }

    /// As [`crate::VoiceControl::configure`].
    pub fn voice_configure(
        &self,
        session: &Session,
        config: &VoiceConfig,
    ) -> Result<VoiceAck, Error> {
        self.runtime.block_on(session.voice().configure(config))
    }

    /// As [`crate::VoiceControl::set_player_state`]: before the player's voice ticket.
    pub fn voice_set_player_state(
        &self,
        session: &Session,
        participant: &ParticipantId,
        state: &VoicePlayerState,
    ) -> Result<VoiceAck, Error> {
        self.runtime
            .block_on(session.voice().set_player_state(participant, state))
    }

    /// As [`crate::VoiceControl::mute`].
    pub fn voice_mute(
        &self,
        session: &Session,
        participant: &ParticipantId,
        muted: bool,
    ) -> Result<VoiceAck, Error> {
        self.runtime
            .block_on(session.voice().mute(participant, muted))
    }

    /// As [`crate::VoiceControl::publish_positions`]. There is no background
    /// publisher here: call it several times a second yourself.
    pub fn voice_publish_positions(
        &self,
        session: &Session,
        positions: &[VoicePosition],
    ) -> Result<VoiceAck, Error> {
        self.runtime
            .block_on(session.voice().publish_positions(positions))
    }

    /// As [`crate::GameData::set`], or [`crate::GameData::set_with_ttl`] with a TTL.
    pub fn game_data_set(
        &self,
        key: &str,
        value: Value,
        ttl: Option<Duration>,
    ) -> Result<DataAck, Error> {
        let data = self.client.game().data();
        match ttl {
            Some(ttl) => self.runtime.block_on(data.set_with_ttl(key, value, ttl)),
            None => self.runtime.block_on(data.set(key, value)),
        }
    }

    /// As [`crate::GameData::remove`].
    pub fn game_data_remove(&self, key: &str) -> Result<DataAck, Error> {
        self.runtime.block_on(self.client.game().data().remove(key))
    }

    /// As [`crate::GameData::set_many`].
    pub fn game_data_set_many(&self, batch: &DataBatch) -> Result<DataAck, Error> {
        self.runtime
            .block_on(self.client.game().data().set_many(batch))
    }

    /// As [`crate::GameData::list`]: for start-up and checks, not for polling.
    pub fn game_data_list(&self) -> Result<DataListing, Error> {
        self.runtime.block_on(self.client.game().data().list())
    }

    /// As [`crate::GameEvents::emit`], or [`crate::GameEvents::emit_with_id`]
    /// with your own id. There is no background publisher here.
    pub fn game_event_emit(
        &self,
        name: &str,
        payload: Value,
        id: Option<&str>,
    ) -> Result<EventReceipt, Error> {
        let events = self.client.game().events();
        match id {
            Some(id) => self
                .runtime
                .block_on(events.emit_with_id(name, payload, id)),
            None => self.runtime.block_on(events.emit(name, payload)),
        }
    }
}

impl Client {
    /// As [`crate::Client::spawn_action_receiver`], with a plain function.
    /// The receiver runs on its own thread with its own runtime; each accepted
    /// action calls `handler` on a blocking-pool thread, at most
    /// `concurrency` at once. Dedupe on [`crate::Action::id`] as the async
    /// receiver documents.
    pub fn spawn_action_receiver_blocking<F>(
        &self,
        options: ActionReceiverOptions,
        handler: F,
    ) -> Result<ActionReceiver, Error>
    where
        F: Fn(Action) -> ActionReply + Send + Sync + 'static,
    {
        let client = self.client.clone();
        let handler = Arc::new(handler);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<Duration>();
        let thread = std::thread::Builder::new()
            .name("gridspawn-actions".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(Error::Runtime(Arc::new(error))));
                        return 0;
                    }
                };
                runtime.block_on(async move {
                    let receiver = client.spawn_action_receiver(options, move |action| {
                        let handler = handler.clone();
                        async move {
                            match tokio::task::spawn_blocking(move || handler(action)).await {
                                Ok(reply) => reply,
                                // Passed on, so the receiver treats it as the
                                // async one does: nothing is posted.
                                Err(error) => match error.try_into_panic() {
                                    Ok(panic) => std::panic::resume_unwind(panic),
                                    // Cancelled: only when the runtime shuts down.
                                    Err(_) => std::future::pending::<ActionReply>().await,
                                },
                            }
                        }
                    });
                    let receiver = match receiver {
                        Ok(receiver) => Arc::new(receiver),
                        Err(error) => {
                            let _ = ready_tx.send(Err(error));
                            return 0;
                        }
                    };
                    let _ = ready_tx.send(Ok(receiver.clone()));
                    // Until stop() or the handle is dropped (the sender goes).
                    let timeout = stop_rx.await.unwrap_or(Duration::ZERO);
                    receiver.stop(timeout).await
                })
            })
            .map_err(|error| Error::Runtime(Arc::new(error)))?;
        let receiver = ready_rx
            .recv()
            .map_err(|_| Error::InvalidResponse("the action receiver thread ended early"))??;
        Ok(ActionReceiver {
            receiver,
            stop: Some(stop_tx),
            thread: Some(thread),
        })
    }
}

/// The blocking receiver's handle. Dropping it stops the receiver without
/// waiting for running handlers.
pub struct ActionReceiver {
    receiver: Arc<crate::ActionReceiver>,
    stop: Option<tokio::sync::oneshot::Sender<Duration>>,
    thread: Option<std::thread::JoinHandle<usize>>,
}

impl ActionReceiver {
    pub fn stats(&self) -> ActionStats {
        self.receiver.stats()
    }

    pub fn last_error(&self) -> Option<Error> {
        self.receiver.last_error()
    }

    /// As [`crate::ActionReceiver::stop`]: blocks up to `timeout` (plus the
    /// edge's stop call) and returns how many handlers were still running.
    pub fn stop(mut self, timeout: Duration) -> usize {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(timeout);
        }
        self.thread
            .take()
            .and_then(|thread| thread.join().ok())
            .unwrap_or(0)
    }
}

impl Drop for ActionReceiver {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(Duration::ZERO);
        }
    }
}

impl std::fmt::Debug for ActionReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("blocking::ActionReceiver")
            .field(&self.receiver)
            .finish()
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("blocking::Client")
            .field(&self.client)
            .finish()
    }
}
