//! Transaction-borrowing owned ledger. The actor owns commit/rollback and authority.
//!
//! Access comes from an admitted execution. Borrowed methods never commit or
//! authorize independently. Legacy and shared model projections are unchanged.

use super::{
    episode_entity as old_episode, outbox_entity as old_outbox, prefix_entity as old_prefix,
    request_entity as old_request,
};
use crate::storage::scope::{KeyDomain, Provenance, Scope};
use crate::trajectory::types::*;
use anyhow::{Context, Result, ensure};
use sea_orm::{
    ColumnTrait, DatabaseConnection, DatabaseTransaction, EntityTrait, IntoActiveModel,
    QueryFilter, QueryOrder, QueryTrait, Select,
};

mod codec;
mod correlation;
mod entities;
#[cfg(feature = "harness")]
mod experiment;
pub(crate) mod headroom;
pub(crate) mod io;
#[cfg(test)]
mod maintenance;
#[cfg(test)]
mod tests;
mod write;
use entities::*;

const MAX_EVENTS: usize = 512;

pub(crate) struct OwnedStore {
    db: DatabaseConnection,
    scope: Scope,
    provenance: Provenance,
}

impl super::TrajectoryStore {
    pub(crate) fn for_execution(
        execution: &crate::tenancy::execution::TenantExecution,
    ) -> OwnedStore {
        let (scope, provenance) = Scope::for_execution(execution);
        OwnedStore {
            db: execution.storage_database().clone(),
            scope,
            provenance,
        }
    }
}

impl OwnedStore {
    #[cfg(feature = "harness")]
    pub(crate) fn for_paid_recovery(
        identity: &crate::tenancy::learning::recovery::RecoveryIdentity,
    ) -> Self {
        let (scope, provenance) = Scope::for_paid_recovery(identity);
        Self {
            db: identity.database().clone(),
            scope,
            provenance,
        }
    }
    #[cfg(feature = "harness")]
    pub(crate) fn for_learner(identity: &crate::tenancy::learner::identity::ReadIdentity) -> Self {
        let (scope, provenance) = Scope::for_learner(identity);
        Self {
            db: identity.database().clone(),
            scope,
            provenance,
        }
    }

    #[cfg(feature = "harness")]
    pub(crate) fn for_native_session(
        authority: &crate::tenancy::execution::session::Authority,
    ) -> Self {
        let (scope, provenance) = Scope::for_session(authority);
        Self {
            db: authority.database().clone(),
            scope,
            provenance,
        }
    }
    /// The actor must open its H-owned transaction from this admitted DB handle.
    pub(crate) fn database(&self) -> &DatabaseConnection {
        &self.db
    }

    pub(crate) fn owner(&self) -> Result<String> {
        self.scope.physical_key(KeyDomain::Owner, "", &[])
    }
    fn key(&self, domain: KeyDomain, logical: &str) -> Result<String> {
        super::validate_opaque(logical, "logical trajectory key")?;
        self.scope.physical_key(domain, logical, &[logical])
    }
    fn episodes(&self) -> Select<episode_entity::Entity> {
        episode_entity::Entity::find().filter(self.scope.predicate(
            episode_entity::Column::InstallationId,
            episode_entity::Column::TenantId,
        ))
    }
    fn requests(&self) -> Select<request_entity::Entity> {
        request_entity::Entity::find().filter(self.scope.predicate(
            request_entity::Column::InstallationId,
            request_entity::Column::TenantId,
        ))
    }
    fn events(&self) -> Select<event_entity::Entity> {
        event_entity::Entity::find().filter(self.scope.predicate(
            event_entity::Column::InstallationId,
            event_entity::Column::TenantId,
        ))
    }
    fn prefixes(&self) -> Select<prefix_entity::Entity> {
        prefix_entity::Entity::find().filter(self.scope.predicate(
            prefix_entity::Column::InstallationId,
            prefix_entity::Column::TenantId,
        ))
    }
    fn outboxes(&self) -> Select<outbox_entity::Entity> {
        outbox_entity::Entity::find().filter(self.scope.predicate(
            outbox_entity::Column::InstallationId,
            outbox_entity::Column::TenantId,
        ))
    }

    async fn episode_row(
        &self,
        tx: &DatabaseTransaction,
        id: &str,
        budget: &mut io::Budget<'_>,
    ) -> Result<Option<(old_episode::Model, Provenance)>> {
        io::one(
            tx,
            self.episodes().filter(
                episode_entity::Column::EpisodeId.eq(self.key(KeyDomain::TrajectoryEpisode, id)?),
            ),
            budget,
        )
        .await?
        .map(|row| self.decode_episode(row))
        .transpose()
    }
    async fn request_row(
        &self,
        tx: &DatabaseTransaction,
        id: &str,
        budget: &mut io::Budget<'_>,
    ) -> Result<Option<(old_request::Model, Provenance)>> {
        let row = io::one(
            tx,
            self.requests().filter(
                request_entity::Column::RequestId.eq(self.key(KeyDomain::TrajectoryRequest, id)?),
            ),
            budget,
        )
        .await?
        .map(|row| self.decode_request(row))
        .transpose()?;
        if let Some((request, _)) = &row {
            KeyedDigest::parse(request.full_input_digest.clone())?;
            super::parse_request_status(&request.status)?;
            ensure!(
                request.status == "started" || request.settlement_event_id.is_some(),
                "terminal trajectory request lacks settlement"
            );
            ensure!(
                request.status != "started"
                    || (request.settlement_event_id.is_none()
                        && request.settlement_outbox_id.is_none()),
                "started request has terminal projections"
            );
        }
        Ok(row)
    }
    pub(crate) async fn request_in(
        &self,
        tx: &DatabaseTransaction,
        id: &str,
        budget: &mut io::Budget<'_>,
    ) -> Result<Option<StoredRequest>> {
        let Some((request, _)) = self.request_row(tx, id, budget).await? else {
            return Ok(None);
        };
        let (_, events) = self.history(tx, &request.episode_id, budget).await?;
        self.validate_request_edges(&request, &events)?;
        Ok(Some(super::stored_request(request)?))
    }
    async fn history(
        &self,
        tx: &DatabaseTransaction,
        id: &str,
        budget: &mut io::Budget<'_>,
    ) -> Result<((old_episode::Model, Provenance), Vec<TrajectoryEvent>)> {
        let episode = self
            .episode_row(tx, id, budget)
            .await?
            .context("unknown scoped trajectory episode")?;
        let rows = io::load(
            tx,
            self.events()
                .filter(
                    event_entity::Column::EpisodeId.eq(self.key(KeyDomain::TrajectoryEpisode, id)?),
                )
                .order_by_asc(event_entity::Column::Sequence),
            budget,
        )
        .await?;
        ensure!(
            rows.len() <= MAX_EVENTS,
            "trajectory episode event capacity exceeded"
        );
        let events = rows
            .into_iter()
            .map(|row| self.decode_event(row))
            .collect::<Result<Vec<_>>>()?;
        super::validate_episode_head(&episode.0, &events)?;
        let first = events
            .first()
            .context("trajectory history missing first event")?;
        let last = events
            .last()
            .context("trajectory history missing last event")?;
        ensure!(
            first.captured_at == episode.0.first_captured_at
                && last.captured_at == episode.0.last_captured_at,
            "trajectory timestamp projection mismatch"
        );
        let digest = KeyedDigest::parse(episode.0.correlation_digest.clone())?;
        ensure!(
            digest.key_id() == episode.0.correlation_key_id,
            "trajectory key epoch projection mismatch"
        );
        super::reduce_prefix(&events, &Default::default())?;
        for event in &events {
            ensure!(
                event.episode_id == id,
                "trajectory event belongs to another episode"
            );
        }
        Ok((episode, events))
    }
    fn validate_request_edges(
        &self,
        row: &old_request::Model,
        history: &[TrajectoryEvent],
    ) -> Result<()> {
        let start = history
            .iter()
            .find(|e| e.event_id == row.start_event_id)
            .context("trajectory request lost its start event")?;
        ensure!(
            start.kind == TrajectoryEventKind::RequestStarted
                && start.request_id.as_deref() == Some(&row.request_id),
            "trajectory request start mismatch"
        );
        if let Some(id) = &row.settlement_event_id {
            let end = history
                .iter()
                .find(|e| &e.event_id == id)
                .context("trajectory request lost its terminal event")?;
            ensure!(
                end.kind == TrajectoryEventKind::RequestSettled
                    && end.request_id.as_deref() == Some(&row.request_id),
                "trajectory terminal request mismatch"
            );
        }
        // Native continuation proofs are not part of this staged profile.
        ensure!(
            row.native_parent_id.is_none(),
            "owned native parent requires a qualified continuation proof"
        );
        Ok(())
    }
    async fn outbox_row(
        &self,
        tx: &DatabaseTransaction,
        id: &str,
        budget: &mut io::Budget<'_>,
    ) -> Result<Option<(old_outbox::Model, Provenance)>> {
        let value = io::one(
            tx,
            self.outboxes().filter(
                outbox_entity::Column::OutboxId.eq(self.key(KeyDomain::TrajectoryOutbox, id)?),
            ),
            budget,
        )
        .await?
        .map(|row| self.decode_outbox(row))
        .transpose()?;
        if let Some((row, _)) = &value {
            super::stored_outbox(row.clone())?;
        }
        Ok(value)
    }
}
