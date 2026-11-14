//! Normalized authority storage; command results and authority commit together.

pub(super) mod directory;
mod records;
pub(super) mod startup;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    authority::CredentialIssued,
    configuration::ConfigurationRevision,
    identity::{InstallationId, OperationId, PrincipalId},
    operation::{Error, OperationResult},
    validation::Validate,
};
use chrono::{DateTime, Utc};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, QueryResult, Value,
    sea_query::{Alias, Asterisk, Expr, OnConflict, Query},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::configuration_store::{self, StoredConfiguration};
use super::model::{AuthorityClass, Binding, Credential, Principal, ReadContext, Snapshot, Tenant};
use super::transaction::Control;

pub(super) const MAX_OPERATIONS: u32 = 100_000;
pub(super) const SAFETY_OPERATIONS: u32 = 512;
pub(super) const ORDINARY_OPERATIONS: u32 = MAX_OPERATIONS - SAFETY_OPERATIONS;

#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(super) enum StoredResponse {
    Operation(OperationResult),
    Issued(CredentialIssued),
    Configuration(ConfigurationRevision),
    Failure(Error),
}

impl StoredResponse {
    pub fn without_secret(&self) -> Self {
        let mut redacted = self.clone();
        if let Self::Issued(issued) = &mut redacted {
            issued.secret = None;
        }
        redacted
    }
}

#[derive(Serialize)]
pub(super) struct Operation {
    pub principal: PrincipalId,
    pub id: OperationId,
    pub context: ReadContext,
    pub digest: String,
    pub result: OperationResult,
    pub response: StoredResponse,
    pub created_at: DateTime<Utc>,
    pub safety: bool,
}

#[derive(Serialize)]
pub(super) enum Change {
    Tenant(Tenant),
    Principal(Principal),
    Binding(Binding),
    Credential(Credential),
    Configuration(StoredConfiguration),
    Resource(configuration_store::StoredResource),
    ResourceLifecycle {
        installation: InstallationId,
        record: super::resources::lifecycle::Record,
    },
    Activation {
        tenant: bitrouter_ai::tenancy::identity::TenantId,
        expected: Option<bitrouter_ai::tenancy::identity::RevisionId>,
        revision: bitrouter_ai::tenancy::identity::RevisionId,
        authority_revision: bitrouter_ai::tenancy::identity::AuthorityRevision,
    },
}

async fn upsert(
    db: &impl ConnectionTrait,
    table: &str,
    keys: &[&str],
    fields: Vec<(&str, Value)>,
) -> Result<()> {
    let mut statement = Query::insert();
    statement
        .into_table(Alias::new(table))
        .columns(fields.iter().map(|(key, _)| Alias::new(*key)));
    let mut conflict = OnConflict::columns(keys.iter().map(|key| Alias::new(*key)));
    conflict.update_columns(
        fields
            .iter()
            .filter(|(key, _)| !keys.contains(key))
            .map(|(key, _)| Alias::new(*key)),
    );
    statement.values(fields.into_iter().map(|(_, value)| value.into()))?;
    statement.on_conflict(conflict.to_owned());
    db.execute(db.get_database_backend().build(&statement))
        .await?;
    Ok(())
}

fn enum_text(value: &impl Serialize) -> Result<String> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(value) => Ok(value),
        _ => anyhow::bail!("authority field is not a string enum"),
    }
}

fn enum_from<T: DeserializeOwned>(row: &QueryResult, name: &str) -> Result<T> {
    let value: String = row.try_get("", name)?;
    Ok(serde_json::from_value(serde_json::Value::String(value))?)
}

fn json_from<T: DeserializeOwned>(row: &QueryResult, name: &str) -> Result<T> {
    let value: String = row.try_get("", name)?;
    Ok(serde_json::from_str(&value)?)
}

fn string(row: &QueryResult, name: &str) -> Result<String> {
    Ok(row.try_get("", name)?)
}

async fn installation(db: &impl ConnectionTrait, state: &Snapshot) -> Result<()> {
    upsert(
        db,
        "tenancy_installation",
        &["singleton"],
        vec![
            ("singleton", 1_i32.into()),
            ("installation_id", state.installation.as_str().into()),
            ("schema_version", super::AUTHORITY_SCHEMA_VERSION.into()),
            (
                "authority_revision",
                i64::try_from(state.revision.get())?.into(),
            ),
            ("operation_ids", i64::try_from(state.operation_ids)?.into()),
            (
                "safety_operation_ids",
                i64::try_from(state.safety_operation_ids)?.into(),
            ),
            ("saturated", state.saturated.into()),
            ("created_at", state.created_at.as_str().into()),
        ],
    )
    .await
}

async fn write_change(db: &impl ConnectionTrait, change: &Change) -> Result<()> {
    match change {
        Change::Resource(record) => configuration_store::insert_resource(db, record).await,
        Change::ResourceLifecycle {
            installation,
            record,
        } => super::resources::lifecycle::write(db, installation, record).await,
        Change::Configuration(value) => configuration_store::insert_configuration(db, value).await,
        Change::Activation {
            tenant,
            expected,
            revision,
            authority_revision,
        } => {
            configuration_store::set_active(
                db,
                tenant,
                expected.as_ref(),
                revision,
                *authority_revision,
            )
            .await
        }
        Change::Tenant(value) => {
            upsert(
                db,
                "tenancy_tenants",
                &["id"],
                vec![
                    ("id", value.id.as_str().into()),
                    ("revision", value.revision.as_str().into()),
                    ("state", enum_text(&value.state)?.into()),
                    ("created_at", value.created_at.as_str().into()),
                ],
            )
            .await
        }
        Change::Principal(value) => {
            upsert(
                db,
                "tenancy_principals",
                &["id"],
                vec![
                    ("id", value.id.as_str().into()),
                    ("revision", value.revision.as_str().into()),
                    ("issuer", value.issuer.as_str().into()),
                    ("subject", value.subject.as_str().into()),
                    ("created_at", value.created_at.as_str().into()),
                ],
            )
            .await
        }
        Change::Binding(value) => {
            let management_class = value
                .managed_by
                .as_ref()
                .map(|owner| enum_text(&owner.class))
                .transpose()?;
            upsert(
                db,
                "tenancy_bindings",
                &["tenant_id", "id"],
                vec![
                    ("tenant_id", value.tenant.as_str().into()),
                    ("id", value.id.as_str().into()),
                    ("principal_id", value.principal.as_str().into()),
                    ("revision", value.revision.as_str().into()),
                    ("role", enum_text(&value.role)?.into()),
                    ("scope_json", serde_json::to_string(&value.scope)?.into()),
                    ("expires_at", value.expires.wire.as_str().into()),
                    (
                        "management_owner",
                        value
                            .managed_by
                            .as_ref()
                            .map(|owner| owner.principal.as_str())
                            .into(),
                    ),
                    ("management_class", management_class.into()),
                ],
            )
            .await
        }
        Change::Credential(value) => {
            upsert(
                db,
                "tenancy_credentials",
                &["id"],
                vec![
                    ("id", value.id.as_str().into()),
                    ("tenant_id", value.tenant.as_str().into()),
                    ("principal_id", value.principal.as_str().into()),
                    (
                        "binding_id",
                        value
                            .binding
                            .as_ref()
                            .map(|binding| binding.as_str())
                            .into(),
                    ),
                    ("revision", value.revision.as_str().into()),
                    ("key_hash", value.key_hash.as_str().into()),
                    ("state", enum_text(&value.state)?.into()),
                    ("audit_reader", value.audit_reader.into()),
                    (
                        "permissions_json",
                        serde_json::to_string(&value.permissions)?.into(),
                    ),
                    ("scope_json", serde_json::to_string(&value.scope)?.into()),
                    ("expires_at", value.expires.wire.as_str().into()),
                    ("created_at", value.created_at.as_str().into()),
                ],
            )
            .await
        }
    }
}

/// The owner actor is the only writer. Never upsert/recycle an operation ID.
pub(super) async fn commit(
    db: &DatabaseConnection,
    state: &Snapshot,
    changes: &[Change],
    operation: &Operation,
    control: &Control,
) -> Result<()> {
    #[cfg(test)]
    if let Some(probe) = control
        .rotation_overlap_for_test()
        .filter(|probe| probe.matches(&state.installation, &operation.id))
    {
        // Exact operation entry, BEFORE BEGIN. This is not SQLite acquisition.
        // It binds the later original Proof counters to this serial actor job.
        probe.target_store_entered.cancel();
    }
    let txn = control.begin_write(db).await?;
    #[cfg(test)]
    let result = super::actor::provider_technical::tests::observe_rotation_write(
        control.rotation_overlap_for_test(),
        write_in(&txn, state, changes, operation),
    )
    .await;
    #[cfg(not(test))]
    let result = write_in(&txn, state, changes, operation).await;
    match result {
        Ok(()) => control.commit(txn).await,
        Err(error) => {
            control.rollback(txn).await?;
            Err(error)
        }
    }
}

/// Borrow the original H transaction for a resource/registration participant.
/// Operation retention, installation counters and configuration changes remain
/// one atomic write, with the original actor owning COMMIT and its outcome.
pub(super) async fn write_in(
    tx: &DatabaseTransaction,
    state: &Snapshot,
    changes: &[Change],
    operation: &Operation,
) -> Result<()> {
    let format = startup::read_header_in(tx, 4096)
        .await?
        .context("authority installation disappeared")?
        .format;
    #[cfg(test)]
    if let Some(probe) = super::actor::provider_technical::tests::after_authority_header(
        &state.installation,
        &operation.id,
    )
    .await?
    {
        // Same real mutation, after the actual SELECT has established its
        // snapshot. Observe typed SQLx error BEFORE the public error mapping.
        let result = startup::write_in(
            tx,
            &startup::LoadedHeader {
                header: state.header(),
                format,
            },
            changes,
            operation,
        )
        .await;
        probe.record_result(&result);
        return result;
    }
    startup::write_in(
        tx,
        &startup::LoadedHeader {
            header: state.header(),
            format,
        },
        changes,
        operation,
    )
    .await
}

async fn insert_operation_in(
    tx: &DatabaseTransaction,
    installation: &InstallationId,
    operation: &Operation,
) -> Result<()> {
    let (class, tenant) = match &operation.context {
        ReadContext::Installation => (AuthorityClass::Installation, None),
        ReadContext::Tenant { tenant, .. } => (AuthorityClass::Tenant, Some(tenant.as_str())),
    };
    let mut statement = Query::insert();
    statement
        .into_table(Alias::new("tenancy_operations"))
        .columns(
            [
                "installation_id",
                "principal_id",
                "operation_id",
                "tenant_id",
                "authority_class",
                "read_context_json",
                "hash_version",
                "command_digest",
                "result_json",
                "response_json",
                "created_at",
                "safety",
            ]
            .map(Alias::new),
        )
        .values(
            [
                installation.as_str().into(),
                operation.principal.as_str().into(),
                operation.id.as_str().into(),
                tenant.into(),
                enum_text(&class)?.into(),
                serde_json::to_string(&operation.context)?.into(),
                1_i32.into(),
                operation.digest.as_str().into(),
                serde_json::to_string(&operation.result)?.into(),
                serde_json::to_string(&operation.response.without_secret())?.into(),
                operation.created_at.to_rfc3339().into(),
                operation.safety.into(),
            ]
            .map(|value: Value| value.into()),
        )?;
    tx.execute(tx.get_database_backend().build(&statement))
        .await?;
    Ok(())
}

pub(super) async fn operation(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    principal: &PrincipalId,
    id: &OperationId,
) -> Result<Option<Operation>> {
    let query = Query::select()
        .column(Asterisk)
        .from(Alias::new("tenancy_operations"))
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .and_where(Expr::col(Alias::new("principal_id")).eq(principal.as_str()))
        .and_where(Expr::col(Alias::new("operation_id")).eq(id.as_str()))
        .to_owned();
    let Some(row) = db
        .query_one(db.get_database_backend().build(&query))
        .await?
    else {
        return Ok(None);
    };
    operation_from(&row, principal, id).map(Some)
}

/// Shared by ordinary operation reads and the stopped-target verifier, under
/// their existing connection owner. Recovery must decode every field that a
/// later operation lookup or idempotent command replay will consume.
pub(super) fn operation_from(
    row: &QueryResult,
    principal: &PrincipalId,
    id: &OperationId,
) -> Result<Operation> {
    ensure!(
        row.try_get::<i32>("", "hash_version")? == 1,
        "unsupported command hash version"
    );
    let result: OperationResult = json_from(row, "result_json")?;
    result.validate()?;
    let response: StoredResponse = json_from(row, "response_json")?;
    ensure!(
        !matches!(&response, StoredResponse::Issued(issued) if issued.secret.is_some()),
        "unexpected persisted issuance secret"
    );
    Ok(Operation {
        principal: principal.clone(),
        id: id.clone(),
        context: json_from(row, "read_context_json")?,
        digest: string(row, "command_digest")?,
        result,
        response,
        created_at: DateTime::parse_from_rfc3339(&string(row, "created_at")?)?.with_timezone(&Utc),
        safety: row.try_get("", "safety")?,
    })
}

pub(super) async fn credential(db: &impl ConnectionTrait, id: &str) -> Result<Option<Credential>> {
    let query = Query::select()
        .column(Asterisk)
        .from(Alias::new("tenancy_credentials"))
        .and_where(Expr::col(Alias::new("id")).eq(id))
        .to_owned();
    db.query_one(db.get_database_backend().build(&query))
        .await?
        .as_ref()
        .map(records::credential_from)
        .transpose()
}

pub(super) async fn credential_view(
    db: &DatabaseConnection,
    id: &str,
    control: &Control,
) -> Result<(
    bitrouter_ai::tenancy::identity::AuthorityRevision,
    Option<Credential>,
)> {
    let txn = control.begin(db).await?;
    let result = credential_view_in(&txn, id, control).await;
    // Success and every failure have the same explicit read cleanup boundary.
    control.rollback(txn).await?;
    result
}

async fn credential_view_in(
    txn: &DatabaseTransaction,
    id: &str,
    control: &Control,
) -> Result<(
    bitrouter_ai::tenancy::identity::AuthorityRevision,
    Option<Credential>,
)> {
    control.check_read()?;
    let query = Query::select()
        .column(Alias::new("authority_revision"))
        .from(Alias::new("tenancy_installation"))
        .and_where(Expr::col(Alias::new("singleton")).eq(1_i32))
        .to_owned();
    let row = txn
        .query_one(txn.get_database_backend().build(&query))
        .await?
        .context("authority identity missing")?;
    control.check_read()?;
    let revision = u64::try_from(row.try_get::<i64>("", "authority_revision")?)?.try_into()?;
    let credential = credential(txn, id).await?;
    control.check_read()?;
    Ok((revision, credential))
}

async fn rows(db: &impl ConnectionTrait, table: &str, max: u64) -> Result<Vec<QueryResult>> {
    let query = Query::select()
        .column(Asterisk)
        .from(Alias::new(table))
        .limit(max + 1)
        .to_owned();
    let rows = db
        .query_all(db.get_database_backend().build(&query))
        .await?;
    ensure!(
        rows.len() as u64 <= max,
        "authority table exceeds its capacity"
    );
    Ok(rows)
}

/// Read only the seed header before spawning the owning startup actor. Complete
/// fixed-snapshot validation and any format migration happen under that actor.
/// The exclusive-owner preflight runs only when a new header will be inserted.
pub(super) async fn initial_header_with(
    db: &DatabaseConnection,
    now: DateTime<Utc>,
    before_write: impl FnOnce() -> Result<()>,
) -> Result<Snapshot> {
    if let Some(loaded) = startup::read_header_in(db, 4096).await? {
        return Ok(Snapshot::from_header(&loaded.header));
    }
    ensure!(
        !super::continuation::has_epoch(db).await?
            && !super::continuation::http::bootstrap::has_owned(db).await?,
        "authority has no installation identity"
    );
    // This is a new empty installation, never recovery or an implicit reset.
    for table in [
        "tenancy_tenants",
        "tenancy_principals",
        "tenancy_bindings",
        "tenancy_credentials",
        "tenancy_operations",
        "tenancy_resource_revisions",
        "tenancy_resource_lifecycle",
        "tenancy_configurations",
        "tenancy_active_configurations",
    ] {
        ensure!(
            rows(db, table, 0).await?.is_empty(),
            "authority has no installation identity"
        );
    }
    let header = super::model::AuthorityHeader {
        installation: uuid::Uuid::new_v4().to_string().try_into()?,
        revision: 0_u64.try_into()?,
        operation_ids: 0,
        safety_operation_ids: 0,
        saturated: false,
        created_at: now.to_rfc3339(),
    };
    let state = Snapshot::from_header(&header);
    before_write()?;
    installation(db, &state).await?;
    Ok(state)
}

#[cfg(test)]
pub(super) async fn initial_header(
    db: &DatabaseConnection,
    now: DateTime<Utc>,
) -> Result<Snapshot> {
    initial_header_with(db, now, || Ok(())).await
}

#[cfg(test)]
pub(super) async fn load(db: &DatabaseConnection, now: DateTime<Utc>) -> Result<Snapshot> {
    initial_header(db, now).await?;
    load_existing(db, now).await
}

/// Test adapter for complete, non-mutating validation through the startup reader.
/// The returned snapshot contains only the publication header.
#[cfg(test)]
pub(super) async fn load_existing(
    db: &impl ConnectionTrait,
    _now: DateTime<Utc>,
) -> Result<Snapshot> {
    let loaded = startup::load_header_in(
        db,
        startup::Budget {
            rows: 32,
            bytes: crate::config::tenancy::capacity::AuthorityViews::default().bytes,
        },
    )
    .await?
    .context("archive authority identity missing")?;
    Ok(Snapshot::from_header(&loaded.header))
}
