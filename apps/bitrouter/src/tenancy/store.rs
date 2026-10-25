//! Normalized authority storage; command results and authority commit together.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    authority::{CredentialIssued, CredentialState},
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
use super::model::{
    AuthorityClass, Binding, Credential, Deadline, ManagedBy, Principal, ReadContext, Snapshot,
    Tenant,
};
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

pub(super) enum Change {
    Tenant(Tenant),
    Principal(Principal),
    Binding(Binding),
    Credential(Credential),
    Configuration(StoredConfiguration),
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
            ("schema_version", 1_i32.into()),
            (
                "authority_revision",
                i64::try_from(state.revision.get())?.into(),
            ),
            ("operation_ids", i32::try_from(state.operation_ids)?.into()),
            (
                "safety_operation_ids",
                i32::try_from(state.safety_operation_ids)?.into(),
            ),
            ("saturated", state.saturated.into()),
            ("created_at", state.created_at.as_str().into()),
        ],
    )
    .await
}

async fn write_change(db: &impl ConnectionTrait, change: &Change) -> Result<()> {
    match change {
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
                state.installation.as_str().into(),
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
    let statement = db.get_database_backend().build(&statement);
    let txn = control.begin(db).await?;
    let result = write_transaction(&txn, state, changes, statement).await;
    match result {
        Ok(()) => control.commit(txn).await,
        Err(error) => {
            control.rollback(txn).await?;
            Err(error)
        }
    }
}

async fn write_transaction(
    txn: &DatabaseTransaction,
    state: &Snapshot,
    changes: &[Change],
    statement: sea_orm::Statement,
) -> Result<()> {
    for change in changes {
        write_change(txn, change).await?;
    }
    installation(txn, state).await?;
    txn.execute(statement).await?;
    Ok(())
}

pub(super) async fn operation(
    db: &DatabaseConnection,
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
    ensure!(
        row.try_get::<i32>("", "hash_version")? == 1,
        "unsupported command hash version"
    );
    let result: OperationResult = json_from(&row, "result_json")?;
    result.validate()?;
    let response: StoredResponse = json_from(&row, "response_json")?;
    ensure!(
        !matches!(&response, StoredResponse::Issued(issued) if issued.secret.is_some()),
        "unexpected persisted issuance secret"
    );
    Ok(Some(Operation {
        principal: principal.clone(),
        id: id.clone(),
        context: json_from(&row, "read_context_json")?,
        digest: string(&row, "command_digest")?,
        result,
        response,
        created_at: DateTime::parse_from_rfc3339(&string(&row, "created_at")?)?.with_timezone(&Utc),
        safety: row.try_get("", "safety")?,
    }))
}

fn credential_from(row: &QueryResult) -> Result<Credential> {
    let scope: bitrouter_ai::tenancy::authority::GrantScope = json_from(row, "scope_json")?;
    scope.validate()?;
    Ok(Credential {
        id: string(row, "id")?.try_into()?,
        tenant: string(row, "tenant_id")?.try_into()?,
        principal: string(row, "principal_id")?.try_into()?,
        binding: row
            .try_get::<Option<String>>("", "binding_id")?
            .map(TryInto::try_into)
            .transpose()?,
        revision: string(row, "revision")?.try_into()?,
        key_hash: string(row, "key_hash")?,
        state: enum_from(row, "state")?,
        audit_reader: row.try_get("", "audit_reader")?,
        permissions: json_from(row, "permissions_json")?,
        scope,
        expires: Deadline::parse(string(row, "expires_at")?.try_into()?)?,
        created_at: string(row, "created_at")?,
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
        .map(credential_from)
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

async fn rows(db: &DatabaseConnection, table: &str, max: u64) -> Result<Vec<QueryResult>> {
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

pub(super) async fn load(db: &DatabaseConnection, now: DateTime<Utc>) -> Result<Snapshot> {
    let persisted = rows(db, "tenancy_installation", 1).await?;
    let mut state = if let Some(row) = persisted.first() {
        ensure!(
            row.try_get::<i32>("", "singleton")? == 1
                && row.try_get::<i32>("", "schema_version")? == 1,
            "unsupported authority store identity"
        );
        let operations = u32::try_from(row.try_get::<i32>("", "operation_ids")?)?;
        let safety = u32::try_from(row.try_get::<i32>("", "safety_operation_ids")?)?;
        ensure!(
            operations <= ORDINARY_OPERATIONS && safety <= SAFETY_OPERATIONS,
            "invalid operation counters"
        );
        Snapshot {
            installation: string(row, "installation_id")?.try_into()?,
            revision: u64::try_from(row.try_get::<i64>("", "authority_revision")?)?.try_into()?,
            operation_ids: operations,
            safety_operation_ids: safety,
            saturated: row.try_get("", "saturated")?,
            created_at: string(row, "created_at")?,
            tenants: Default::default(),
            principals: Default::default(),
            bindings: Default::default(),
            credentials: Default::default(),
            credential_hashes: Default::default(),
            active_configurations: Default::default(),
        }
    } else {
        // An absent singleton with existing authority records is an incomplete
        // cutover, never an invitation to assign a new installation identity.
        ensure!(
            !super::continuation::has_epoch(db).await?,
            "authority has no installation identity"
        );
        for table in [
            "tenancy_tenants",
            "tenancy_principals",
            "tenancy_bindings",
            "tenancy_credentials",
            "tenancy_operations",
            "tenancy_resource_revisions",
            "tenancy_configurations",
            "tenancy_active_configurations",
        ] {
            ensure!(
                rows(db, table, 0).await?.is_empty(),
                "authority has no installation identity"
            );
        }
        let state = Snapshot {
            installation: uuid::Uuid::new_v4().to_string().try_into()?,
            revision: 0_u64.try_into()?,
            operation_ids: 0,
            safety_operation_ids: 0,
            saturated: false,
            created_at: now.to_rfc3339(),
            tenants: Default::default(),
            principals: Default::default(),
            bindings: Default::default(),
            credentials: Default::default(),
            credential_hashes: Default::default(),
            active_configurations: Default::default(),
        };
        installation(db, &state).await?;
        state
    };
    ensure!(
        state.operation_ids < ORDINARY_OPERATIONS || state.saturated,
        "missing saturation latch"
    );
    let counters = Query::select()
        .columns([Alias::new("installation_id"), Alias::new("safety")])
        .expr_as(
            Expr::col(Alias::new("operation_id")).count(),
            Alias::new("amount"),
        )
        .from(Alias::new("tenancy_operations"))
        .group_by_columns([Alias::new("installation_id"), Alias::new("safety")])
        .limit(3)
        .to_owned();
    let mut ordinary = 0_u32;
    let mut safety = 0_u32;
    for row in db
        .query_all(db.get_database_backend().build(&counters))
        .await?
    {
        ensure!(
            string(&row, "installation_id")? == state.installation.as_str(),
            "operation history belongs to another installation"
        );
        let amount = u32::try_from(row.try_get::<i64>("", "amount")?)?;
        if row.try_get::<bool>("", "safety")? {
            safety = amount;
        } else {
            ordinary = amount;
        }
    }
    ensure!(
        ordinary == state.operation_ids && safety == state.safety_operation_ids,
        "operation history disagrees with retained capacity counters"
    );
    for row in rows(db, "tenancy_tenants", 32).await? {
        let tenant = Tenant {
            id: string(&row, "id")?.try_into()?,
            revision: string(&row, "revision")?.try_into()?,
            state: enum_from(&row, "state")?,
            created_at: string(&row, "created_at")?,
        };
        state.tenants.insert(tenant.id.clone(), tenant);
    }
    for row in rows(db, "tenancy_principals", 128).await? {
        let principal = Principal {
            id: string(&row, "id")?.try_into()?,
            revision: string(&row, "revision")?.try_into()?,
            issuer: string(&row, "issuer")?,
            subject: string(&row, "subject")?,
            created_at: string(&row, "created_at")?,
        };
        state.principals.insert(principal.id.clone(), principal);
    }
    for row in rows(db, "tenancy_bindings", 256).await? {
        let owner: Option<String> = row.try_get("", "management_owner")?;
        let class: Option<String> = row.try_get("", "management_class")?;
        let managed_by = match (owner, class) {
            (Some(owner), Some(class)) => Some(ManagedBy {
                principal: owner.try_into()?,
                class: serde_json::from_value(serde_json::Value::String(class))?,
            }),
            (None, None) => None,
            _ => anyhow::bail!("inconsistent binding management authority"),
        };
        let scope: bitrouter_ai::tenancy::authority::GrantScope = json_from(&row, "scope_json")?;
        scope.validate()?;
        let binding = Binding {
            tenant: string(&row, "tenant_id")?.try_into()?,
            id: string(&row, "id")?.try_into()?,
            principal: string(&row, "principal_id")?.try_into()?,
            revision: string(&row, "revision")?.try_into()?,
            role: enum_from(&row, "role")?,
            scope,
            expires: Deadline::parse(string(&row, "expires_at")?.try_into()?)?,
            managed_by,
        };
        ensure!(
            state.tenants.contains_key(&binding.tenant)
                && state.principals.contains_key(&binding.principal),
            "orphan authority binding"
        );
        state
            .bindings
            .insert((binding.tenant.clone(), binding.id.clone()), binding);
    }
    // Expiries are parsed as instants, not compared lexically: legal RFC3339
    // fractional spellings do not sort by timestamp. Scan bounded history in
    // pages so rotated credentials cannot turn startup into an unbounded load.
    let mut after = String::new();
    loop {
        let query = Query::select()
            .column(Asterisk)
            .from(Alias::new("tenancy_credentials"))
            .and_where(Expr::col(Alias::new("state")).eq("active"))
            .and_where(Expr::col(Alias::new("id")).gt(after.as_str()))
            .order_by(Alias::new("id"), sea_orm::sea_query::Order::Asc)
            .limit(128)
            .to_owned();
        let rows = db
            .query_all(db.get_database_backend().build(&query))
            .await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let credential = credential_from(&row).context("load authority credential")?;
            after = credential.id.to_string();
            if credential.state == CredentialState::Active && credential.expires.instant > now {
                ensure!(
                    state.credentials.len() < 256,
                    "live credential capacity exceeded"
                );
                ensure!(
                    state.tenants.contains_key(&credential.tenant)
                        && state.principals.contains_key(&credential.principal),
                    "orphan authority credential"
                );
                ensure!(
                    state
                        .credential_hashes
                        .insert(credential.key_hash.clone(), credential.id.clone())
                        .is_none(),
                    "duplicate credential digest"
                );
                state.credentials.insert(credential.id.clone(), credential);
            }
        }
    }
    for pointer in configuration_store::active_revisions(db).await? {
        ensure!(
            state.tenants.contains_key(&pointer.tenant)
                && pointer.authority_revision <= state.revision,
            "active configuration is outside published authority"
        );
        state
            .active_configurations
            .insert(pointer.tenant, pointer.revision);
    }
    Ok(state)
}
