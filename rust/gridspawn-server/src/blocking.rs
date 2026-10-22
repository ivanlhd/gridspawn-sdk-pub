//! A small synchronous facade (feature `blocking`). It owns a current-thread
//! tokio runtime. Do not call it from async code or from a game's frame loop;
//! there is no background renewer here, so call [`Client::renew`] yourself.

use std::sync::Arc;

use crate::{
    AdmitGuest, AdmitParticipant, ClientBuilder, ClientTicket, CreateSession, Error,
    IdempotencyKey, Lease, Module, Participant, Post, PostReceipt, Session,
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
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("blocking::Client")
            .field(&self.client)
            .finish()
    }
}
