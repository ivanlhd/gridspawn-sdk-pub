//! A small synchronous facade (feature `blocking`). It owns a current-thread
//! tokio runtime. Do not call it from async code or from a game's frame loop;
//! there is no background renewer here, so call [`Client::renew`] yourself.

use std::{sync::Arc, time::Duration};

use serde_json::Value;

use crate::{
    AdmitGuest, AdmitParticipant, ClientBuilder, ClientTicket, CreateSession, DataAck, DataBatch,
    DataListing, Error, EventReceipt, IdempotencyKey, Lease, Module, Participant, ParticipantId,
    ParticipantRef, Post, PostReceipt, Presence, PresenceAck, Session, VoiceAck, VoiceConfig,
    VoicePlayerState, VoicePosition,
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

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("blocking::Client")
            .field(&self.client)
            .finish()
    }
}
