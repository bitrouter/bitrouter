//! Reuse the original read-only adapter and codecs under the caller's SQL owner.
use super::super::{configuration_store, model::Snapshot, resources::Resources, store};
use super::connection::ReadConnection;
use crate::{
    offline::recovery::{
        AuthorityFacts,
        plan::{Provisioning, Target},
    },
    storage::operation_budget::OperationBudget,
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    authority::{GrantScope, Permission, TenantRole, TenantState},
    identity::{InstallationId, OperationId, PrincipalId},
    validation::Validate,
};
use sea_orm::{ConnectionTrait, Statement, sqlx::SqliteConnection};

fn facts(state: &Snapshot) -> AuthorityFacts {
    AuthorityFacts {
        installation_id: state.installation.clone(),
        authority_revision: state.revision.get(),
        ordinary_operations: state.operation_ids,
        safety_operations: state.safety_operation_ids,
        saturated: state.saturated,
    }
}
pub(crate) async fn inspect_source(
    db: &mut SqliteConnection,
    expected: &InstallationId,
    check: &(dyn Fn() -> Result<()> + Sync),
) -> Result<AuthorityFacts> {
    check()?;
    let reader = ReadConnection::new(db);
    crate::db::migration::validate_receipt_archive(&reader).await?;
    check()?;
    let state = store::load_existing(&reader, chrono::Utc::now()).await?;
    ensure!(
        &state.installation == expected,
        "source authority identity mismatch"
    );
    check()?;
    // Existing receipt integrity inventory, not a new approximate state decoder.
    super::super::receipts::store::inventory(&reader, expected).await?;
    check()?;
    Ok(facts(&state))
}
pub(crate) async fn inspect_target(
    db: &mut SqliteConnection,
    target: &Target,
    observed: &Provisioning,
    manifest: &[u8],
    budget: &OperationBudget<'_>,
) -> Result<AuthorityFacts> {
    budget.check()?;
    // The offline preflight bounded all cells/rows before this reader. Page real
    // durable operations; do not accept a supplied operation list as write proof.
    let reader = ReadConnection::new(db);
    let mut count = 0;
    for page in 0..=64_u64 {
        budget.check()?;
        let rows = reader.query_all(Statement::from_sql_and_values(
            reader.get_database_backend(),
            "SELECT installation_id,principal_id,operation_id,hash_version,safety,result_json,response_json,read_context_json,created_at,command_digest FROM tenancy_operations ORDER BY installation_id COLLATE BINARY,principal_id COLLATE BINARY,operation_id COLLATE BINARY LIMIT 64 OFFSET ?",
            [((page * 64) as i64).into()],
        )).await?;
        if rows.is_empty() {
            break;
        }
        ensure!(page < 64, "fresh operation page bound");
        for row in rows {
            let mut bytes = 0_usize;
            for name in [
                "installation_id",
                "principal_id",
                "operation_id",
                "result_json",
                "response_json",
                "read_context_json",
                "created_at",
                "command_digest",
            ] {
                let value: String = row.try_get("", name)?;
                ensure!(value.len() <= 1024 * 1024, "fresh operation field bound");
                bytes = bytes
                    .checked_add(value.len())
                    .context("operation byte overflow")?;
            }
            budget.charge(1, bytes)?;
            let principal: PrincipalId = row.try_get::<String>("", "principal_id")?.try_into()?;
            let operation: OperationId = row.try_get::<String>("", "operation_id")?.try_into()?;
            let stored = store::operation_from(&row, &principal, &operation)?;
            let result = stored.result;
            let response = stored.response;
            match &response {
                store::StoredResponse::Operation(returned) => ensure!(
                    serde_json::to_value(returned)? == serde_json::to_value(&result)?,
                    "durable operation response differs from its result"
                ),
                store::StoredResponse::Issued(issued) => ensure!(
                    issued.secret.is_none()
                        && issued.operation_id == result.operation_id
                        && Some(issued.authority_revision) == result.authority_revision,
                    "durable issuance response differs from its result"
                ),
                store::StoredResponse::Configuration(configuration) => {
                    configuration.validate()?;
                    ensure!(
                        observed.configurations.iter().any(|expected| {
                            expected.tenant_id == configuration.tenant_id
                                && expected.revision_id == configuration.revision_id
                                && expected.content_digest == configuration.content_digest
                                && expected.document == configuration.document
                        }),
                        "unapproved durable configuration response"
                    );
                }
                store::StoredResponse::Failure(_) => {
                    anyhow::bail!("failed operation outside fresh provisioning profile")
                }
            }
            let expected = observed
                .operations
                .iter()
                .find(|o| o.principal_id == principal && o.result.operation_id == operation)
                .context("undeclared provisioning operation")?;
            ensure!(
                row.try_get::<String>("", "installation_id")? == target.installation_id.as_str()
                    && !stored.safety
                    && result.operation_id == operation
                    && serde_json::to_value(&result)? == serde_json::to_value(&expected.result)?,
                "durable provisioning result differs from readback"
            );
            count += 1;
        }
    }
    ensure!(
        count == observed.operations.len(),
        "provisioning history coverage mismatch"
    );
    let state = store::load_existing(&reader, chrono::Utc::now()).await?;
    ensure!(
        state.installation == target.installation_id
            && !state.saturated
            && state.safety_operation_ids == 0
            && state.operation_ids as usize == count
            && state.revision.get() == count as u64,
        "fresh authority counters mismatch"
    );
    ensure!(
        !state.tenants.is_empty()
            && state
                .tenants
                .values()
                .all(|t| t.state == TenantState::Active)
            && !state.credentials.is_empty(),
        "fresh authority is inactive"
    );
    for (table, expected) in [
        ("tenancy_credentials", state.credentials.len()),
        ("tenancy_configurations", observed.configurations.len()),
        ("tenancy_active_configurations", observed.active.len()),
    ] {
        let row = reader
            .query_one(Statement::from_string(
                reader.get_database_backend(),
                format!("SELECT count(*) AS count FROM {table}"),
            ))
            .await?
            .context("fresh count")?;
        ensure!(
            usize::try_from(row.try_get::<i64>("", "count")?)? == expected,
            "hidden expired/revoked or unapproved fresh rows"
        );
    }
    let active = configuration_store::active_revisions(&reader).await?;
    ensure!(
        active.len() == observed.active.len(),
        "active coverage mismatch"
    );
    for pointer in active {
        ensure!(
            observed.active.iter().any(|p| p.tenant == pointer.tenant
                && p.revision == pointer.revision
                && p.authority_revision == pointer.authority_revision),
            "active pointer changed"
        );
    }
    let check_creator = |stored: &configuration_store::StoredConfiguration| -> Result<()> {
        let config = &stored.revision;
        budget.check()?;
        ensure!(
            state.principals.contains_key(&stored.creator)
                && stored.authority_revision <= state.revision,
            "configuration creator outside fresh authority"
        );
        // Inspect existing ceilings, without constructing Caller, Scope or Grant.
        ensure!(
            state
                .credentials
                .values()
                .any(|credential| credential.tenant == config.tenant_id
                    && credential.principal == stored.creator
                    && credential.expires.instant > chrono::Utc::now()
                    && matches!(credential.scope, GrantScope::Tenant { .. })
                    && [
                        Permission::ResourceRead,
                        Permission::ResourceDraft,
                        Permission::PolicyActivate
                    ]
                    .iter()
                    .all(|p| credential.permissions.contains(p))
                    && credential
                        .binding
                        .as_ref()
                        .and_then(|id| state.bindings.get(&(credential.tenant.clone(), id.clone())))
                        .is_some_and(|binding| binding.principal == credential.principal
                            && binding.role == TenantRole::TenantAdmin
                            && matches!(binding.scope, GrantScope::Tenant { .. })
                            && binding.expires.instant > chrono::Utc::now()
                            && stored.dependencies.iter().all(|reference| {
                                super::super::authority::covers_key(
                                    &credential.scope,
                                    &credential.tenant,
                                    &reference.key(),
                                ) && super::super::authority::covers_key(
                                    &binding.scope,
                                    &binding.tenant,
                                    &reference.key(),
                                )
                            }))),
            "fresh configuration lacks its actual current tenant-wide credential ceiling"
        );
        Ok(())
    };
    let settings = crate::config::tenancy::TenancyConfig {
        enabled: true,
        wrapping_key_env: Some(target.wrapping_key_env.clone()),
        ..Default::default()
    };
    let resources = Resources::load_key(&state.installation, &settings)?;
    resources
        .inspect_fresh_static(&reader, manifest, observed, budget, &check_creator)
        .await?;
    budget.check()?;
    ensure!(
        state
            .credentials
            .values()
            .all(|credential| credential.expires.instant > chrono::Utc::now()),
        "fresh credential expired during verification"
    );
    Ok(facts(&state))
}
