//! Borrowed capture participants; live permission and custody belong to Service.
//!
//! No participant begins, commits, rolls back, spawns, or opens a database. The
//! caller retains the exact Append through H's definitive/ambiguous outcomes.

pub(crate) mod entities;
pub(crate) mod io;
pub(crate) mod prefix;
pub(crate) mod protocol;
mod replay;
#[cfg(test)]
mod tests;
mod write;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::configuration::LearningMode;
use sea_orm::{
    ColumnTrait, Condition, DatabaseTransaction, EntityTrait, QueryFilter, QuerySelect, Select,
};
use serde::{Deserialize, Serialize};

use crate::storage::{
    capture::CaptureEvent,
    operation_budget::OperationBudget,
    scope::{KeyDomain, MetadataRef, Provenance, Scope},
};
use entities::{connections, events, sessions};

pub(crate) const MAX_EVENTS: usize = 512;
pub(crate) const MAX_EVENT_BYTES: usize = 256 * 1024;
pub(crate) const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_SESSIONS: usize = 16;
#[derive(Debug, thiserror::Error)]
#[error("capture observation is outside the accepted profile")]
pub(crate) struct InvalidObservation;

const MAX_CALLS: usize = 32;

/// Immutable storage association, not authorization. The controller owner will
/// obtain these values from its checked host binding, never a caller DTO.
#[derive(Clone)]
pub(crate) struct Association {
    scope: Scope,
    provenance: Provenance,
    descriptor: Descriptor,
    connection: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Descriptor {
    pub(crate) version: u32,
    pub(crate) installation: String,
    pub(crate) tenant: String,
    pub(crate) principal: String,
    pub(crate) controller: String,
    pub(crate) source: String,
    pub(crate) model: String,
    pub(crate) revision: String,
    pub(crate) mode: LearningMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) block_link: Option<ProviderLink>,
}

/// Storage commitment only; it never reconstructs a live capture binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderLink {
    pub(crate) version: u32,
    pub(crate) commitment: String,
    pub(crate) credential: String,
}

/// Pre-encoded once before admission, and retained outside the actor work future.
pub(crate) struct Append {
    pub(crate) sequence: i64,
    pub(crate) at: String,
    pub(crate) event: CaptureEvent,
    bytes: String,
}
impl Append {
    pub(crate) fn new(sequence: i64, at: String, event: CaptureEvent) -> Result<Self> {
        ensure!(
            sequence > 0 && sequence <= i64::try_from(MAX_EVENTS)?,
            "capture sequence outside bound"
        );
        ensure!(at.len() <= 64, "capture timestamp outside bound");
        chrono::DateTime::parse_from_rfc3339(&at)?;
        let bytes = io::json(&event, MAX_EVENT_BYTES)?;
        Ok(Self {
            sequence,
            at,
            event,
            bytes,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Appended {
    pub(crate) head: i64,
    pub(crate) duplicate: bool,
}

impl Association {
    pub(crate) fn new(
        scope: Scope,
        provenance: Provenance,
        controller: String,
        source: String,
        revision: String,
        model: String,
        mode: LearningMode,
    ) -> Result<Self> {
        let metadata = scope.metadata(&provenance)?;
        let principal = metadata
            .creator
            .context("owned capture provenance missing")?;
        for value in [&controller, &source, &revision, &model] {
            bounded_id(value, 128)?;
        }
        let descriptor = Descriptor {
            version: 1,
            installation: metadata
                .installation
                .context("owned capture namespace missing")?,
            tenant: metadata.tenant.context("owned capture namespace missing")?,
            principal,
            controller,
            source,
            revision,
            model,
            mode,
            block_link: None,
        };
        let connection = scope.physical_key(
            KeyDomain::CaptureConnection,
            "",
            &[&descriptor.principal, &descriptor.controller],
        )?;
        Ok(Self {
            scope,
            provenance,
            descriptor,
            connection,
        })
    }
    pub(crate) fn with_block_link(mut self, link: ProviderLink) -> Result<Self> {
        ensure!(
            link.version == 1
                && link.commitment.len() == 64
                && link.commitment.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid Block link commitment"
        );
        bounded_id(&link.credential, 128)?;
        self.descriptor.block_link = Some(link);
        Ok(self)
    }
    pub(crate) fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }
    pub(crate) fn connection(&self) -> &str {
        &self.connection
    }
    pub(crate) fn session_key(&self, native: &str) -> Result<String> {
        bounded_id(native, 256)?;
        self.scope
            .physical_key(KeyDomain::CaptureSession, "", &[&self.connection, native])
    }
    pub(crate) fn key(&self, domain: KeyDomain, values: &[&str]) -> Result<String> {
        self.scope.physical_key(domain, "", values)
    }
    pub(crate) fn metadata(&self) -> Result<crate::storage::scope::Metadata> {
        self.scope.metadata(&self.provenance)
    }
    pub(crate) fn scope(&self) -> &Scope {
        &self.scope
    }
    pub(crate) fn predicate(&self, table: &str) -> Condition {
        use sea_orm::sea_query::{Alias, Expr};
        self.scope
            .predicate(
                (Alias::new(table), Alias::new("installation_id")),
                (Alias::new(table), Alias::new("tenant_id")),
            )
            .add(
                Expr::col((Alias::new(table), Alias::new("creator_principal_id")))
                    .eq(&self.descriptor.principal),
            )
    }
    pub(crate) fn validate(
        &self,
        installation: Option<&str>,
        tenant: Option<&str>,
        creator: Option<&str>,
    ) -> Result<()> {
        ensure!(
            self.scope.validate_metadata(MetadataRef {
                installation,
                tenant,
                creator
            })? == self.provenance,
            "capture creator association mismatch"
        );
        Ok(())
    }
    pub(crate) fn events(&self) -> Select<events::Entity> {
        events::Entity::find()
            .filter(self.predicate("acp_capture_events"))
            .filter(events::Column::ConnectionId.eq(&self.connection))
    }
    pub(crate) fn sessions(&self, native: &str) -> Result<Select<sessions::Entity>> {
        Ok(sessions::Entity::find_by_id(self.session_key(native)?)
            .filter(self.predicate("acp_canonical_sessions"))
            .filter(sessions::Column::Owner.eq(&self.descriptor.principal))
            .filter(sessions::Column::Source.eq(&self.descriptor.source))
            .filter(sessions::Column::NativeSessionId.eq(native)))
    }
    pub(crate) async fn connection_in(
        &self,
        tx: &DatabaseTransaction,
        budget: &OperationBudget<'_>,
    ) -> Result<(connections::Model, protocol::State)> {
        // Current read takes the serialization lock before the first consistent
        // RR snapshot, including on MySQL. SQLite's actor serializes writers.
        // https://dev.mysql.com/doc/refman/8.4/en/innodb-locking-reads.html
        let query = connections::Entity::find_by_id(&self.connection)
            .filter(self.predicate("acp_capture_connections"))
            .filter(connections::Column::Owner.eq(&self.descriptor.principal))
            .filter(connections::Column::Source.eq(&self.descriptor.source))
            .filter(connections::Column::ControllerInstanceId.eq(&self.descriptor.controller))
            .lock_exclusive();
        let row = io::one(tx, query, MAX_METADATA_BYTES + 4096, budget)
            .await?
            .context("owned capture connection unavailable")?;
        self.validate(
            row.installation_id.as_deref(),
            row.tenant_id.as_deref(),
            row.creator_principal_id.as_deref(),
        )?;
        let state: protocol::State = serde_json::from_str(&row.metadata_json)?;
        state.validate(self, &row)?;
        Ok((row, state))
    }
}

fn bounded_id(value: &str, maximum: usize) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control),
        "capture identifier outside bound"
    );
    Ok(())
}
