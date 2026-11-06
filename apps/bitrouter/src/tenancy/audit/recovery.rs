//! Reuse the original read-only adapter and codecs under the caller's SQL owner.
pub(crate) mod v2;
use super::super::{configuration_store, model::AuthorityHeader, resources::Resources, store};
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
    authority::{CredentialState, GrantScope, Permission, TenantRole, TenantState},
    identity::{InstallationId, OperationId, PrincipalId},
    validation::Validate,
};
use sea_orm::{ConnectionTrait, Statement, sqlx::SqliteConnection};

#[cfg(test)]
mod tests;

fn facts(state: &AuthorityHeader) -> AuthorityFacts {
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
    validation_budget: crate::offline::ValidationBudget,
) -> Result<AuthorityFacts> {
    check()?;
    let reader = ReadConnection::new(db).with_row_limit(validation_budget.rows.max(128));
    crate::db::migration::validate_receipt_archive(&reader).await?;
    check()?;
    let state = super::validation::load(&reader, validation_budget)
        .await?
        .header;
    ensure!(
        &state.installation == expected,
        "source authority identity mismatch"
    );
    check()?;
    Ok(facts(&state))
}
async fn fresh_operations(
    reader: &impl ConnectionTrait,
    installation: &InstallationId,
    observed: &Provisioning,
    budget: &OperationBudget<'_>,
    validation_budget: crate::offline::ValidationBudget,
) -> Result<AuthorityHeader> {
    budget.check()?;
    // Complete single-body allocation preflight precedes fresh readback. The
    // caller's SAME fixed transaction binds those checked bytes to the exact
    // reads below; only keys may accumulate in a page alongside one body.
    let state = super::validation::load(reader, validation_budget)
        .await?
        .header;
    let mut count = 0_usize;
    let mut after: Option<(String, String, String)> = None;
    loop {
        budget.check()?;
        let mut sql = String::from(
            "SELECT installation_id,principal_id,operation_id FROM tenancy_operations",
        );
        let mut values = Vec::<sea_orm::Value>::new();
        if let Some((installation, principal, operation)) = &after {
            sql.push_str(" WHERE (installation_id,principal_id,operation_id)>(?,?,?)");
            values.extend([
                installation.as_str().into(),
                principal.as_str().into(),
                operation.as_str().into(),
            ]);
        }
        // Qualified source columns preserve the composite primary-key seek.
        // https://www.sqlite.org/rowvalue.html#scrolling_window_queries
        sql.push_str(" ORDER BY tenancy_operations.installation_id,tenancy_operations.principal_id,tenancy_operations.operation_id LIMIT ?");
        values.push(i64::try_from(validation_budget.rows)?.into());
        let keys = reader
            .query_all(Statement::from_sql_and_values(
                reader.get_database_backend(),
                sql,
                values,
            ))
            .await?;
        if keys.is_empty() {
            break;
        }
        for key in keys {
            budget.check()?;
            let installation_key: String = key.try_get("", "installation_id")?;
            let principal: PrincipalId = key.try_get::<String>("", "principal_id")?.try_into()?;
            let operation: OperationId = key.try_get::<String>("", "operation_id")?.try_into()?;
            let cursor = (
                installation_key,
                principal.to_string(),
                operation.to_string(),
            );
            ensure!(
                after.as_ref().is_none_or(|previous| previous < &cursor),
                "fresh operation key order"
            );
            let row = reader.query_one(Statement::from_sql_and_values(
                reader.get_database_backend(),
                "SELECT installation_id,principal_id,operation_id,hash_version,safety,result_json,response_json,read_context_json,created_at,command_digest FROM tenancy_operations WHERE installation_id=? AND principal_id=? AND operation_id=? LIMIT 1",
                [cursor.0.as_str().into(), principal.as_str().into(), operation.as_str().into()],
            )).await?.context("fresh operation disappeared after preflight")?;
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
                row.try_get::<String>("", "installation_id")? == installation.as_str()
                    && !stored.safety
                    && result.operation_id == operation
                    && serde_json::to_value(&result)? == serde_json::to_value(&expected.result)?,
                "durable provisioning result differs from readback"
            );
            count = count
                .checked_add(1)
                .context("fresh operation count overflow")?;
            after = Some(cursor);
        }
    }
    ensure!(
        count == observed.operations.len(),
        "provisioning history coverage mismatch"
    );
    ensure!(
        &state.installation == installation
            && !state.saturated
            && state.safety_operation_ids == 0
            && state.operation_ids == u64::try_from(count)?
            && state.revision.get() == u64::try_from(count)?,
        "fresh authority counters mismatch"
    );
    Ok(state)
}

pub(crate) async fn inspect_target(
    db: &mut SqliteConnection,
    target: &Target,
    observed: &Provisioning,
    manifest: &[u8],
    budget: &OperationBudget<'_>,
    validation_budget: crate::offline::ValidationBudget,
) -> Result<AuthorityFacts> {
    let reader = ReadConnection::new(db).with_row_limit(validation_budget.rows.max(128));
    let state = fresh_operations(
        &reader,
        &target.installation_id,
        observed,
        budget,
        validation_budget,
    )
    .await?;
    let earliest_expiry = fresh_directory(&reader, validation_budget).await?;
    for (table, expected) in [
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
    let mut after = None;
    let mut active_count = 0_usize;
    loop {
        let page = configuration_store::active_revision_page(&reader, after.as_ref()).await?;
        if page.is_empty() {
            break;
        }
        for pointer in page {
            ensure!(
                observed.active.iter().any(|p| p.tenant == pointer.tenant
                    && p.revision == pointer.revision
                    && p.authority_revision == pointer.authority_revision),
                "active pointer changed"
            );
            active_count = active_count
                .checked_add(1)
                .context("active count overflow")?;
            after = Some(pointer.tenant);
        }
    }
    ensure!(
        active_count == observed.active.len(),
        "active coverage mismatch"
    );
    for expected in &observed.configurations {
        budget.check()?;
        let record =
            configuration_store::configuration(&reader, &expected.tenant_id, &expected.revision_id)
                .await?
                .context("fresh configuration missing")?;
        check_creator(&reader, &state, &record, validation_budget).await?;
    }
    // This caller-owned fixed read transaction binds the checked creators to
    // the subsequent authenticated static inspection of exactly these revisions.
    let checked_creator = |_: &configuration_store::StoredConfiguration| -> Result<()> {
        budget.check()?;
        Ok(())
    };
    let settings = crate::config::tenancy::TenancyConfig {
        enabled: true,
        wrapping_key_env: Some(target.wrapping_key_env.clone()),
        ..Default::default()
    };
    let resources = Resources::load_key(&state.installation, &settings)?;
    resources
        .inspect_fresh_static(&reader, manifest, observed, budget, &checked_creator)
        .await?;
    budget.check()?;
    ensure!(
        earliest_expiry > chrono::Utc::now(),
        "fresh credential expired during verification"
    );
    Ok(facts(&state))
}

// Fresh provisioning is a declared bounded profile. Page every row even though
// the manifest is bounded; retain only the earliest expiry and one exact grant.
async fn fresh_directory(
    db: &impl ConnectionTrait,
    budget: crate::offline::ValidationBudget,
) -> Result<chrono::DateTime<chrono::Utc>> {
    let page_budget = store::directory::PageBudget {
        rows: 1,
        bytes: budget.bytes / 128,
    };
    let mut after = None;
    let mut any = false;
    loop {
        let page = store::directory::tenant_page_in(db, after.as_ref(), page_budget).await?;
        if page.is_empty() {
            break;
        }
        for tenant in page {
            ensure!(
                tenant.state == TenantState::Active,
                "fresh authority is inactive"
            );
            any = true;
            after = Some(tenant.id);
        }
    }
    ensure!(any, "fresh tenant missing");
    let mut after = None;
    let mut earliest = None::<chrono::DateTime<chrono::Utc>>;
    loop {
        let page = store::directory::credential_page_in(db, after.as_ref(), page_budget).await?;
        if page.is_empty() {
            break;
        }
        for credential in page {
            ensure!(
                credential.state == CredentialState::Active
                    && credential.expires.instant > chrono::Utc::now(),
                "hidden expired/revoked fresh credential"
            );
            earliest = Some(earliest.map_or(credential.expires.instant, |old| {
                old.min(credential.expires.instant)
            }));
            after = Some(credential.id);
        }
    }
    earliest.context("fresh credential missing")
}
async fn check_creator(
    db: &impl ConnectionTrait,
    header: &AuthorityHeader,
    stored: &configuration_store::StoredConfiguration,
    budget: crate::offline::ValidationBudget,
) -> Result<()> {
    let row_bytes = budget.bytes / 128;
    ensure!(
        stored.authority_revision <= header.revision
            && store::directory::principal_in(db, &stored.creator, row_bytes)
                .await?
                .is_some(),
        "configuration creator outside fresh authority"
    );
    let mut after = None;
    let mut authorized = false;
    loop {
        let page = store::directory::credential_page_in(
            db,
            after.as_ref(),
            store::directory::PageBudget {
                rows: 1,
                bytes: row_bytes,
            },
        )
        .await?;
        if page.is_empty() {
            break;
        }
        for credential in page {
            if credential.tenant == stored.revision.tenant_id
                && credential.principal == stored.creator
                && credential.state == CredentialState::Active
                && credential.expires.instant > chrono::Utc::now()
                && matches!(credential.scope, GrantScope::Tenant { .. })
                && [
                    Permission::ResourceRead,
                    Permission::ResourceDraft,
                    Permission::PolicyActivate,
                ]
                .iter()
                .all(|p| credential.permissions.contains(p))
                && let Some(id) = &credential.binding
                && let Some(binding) =
                    store::directory::binding_in(db, &credential.tenant, id, row_bytes).await?
                && binding.principal == credential.principal
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
                })
            {
                authorized = true;
            }
            after = Some(credential.id);
        }
    }
    ensure!(
        authorized,
        "fresh configuration lacks its actual current tenant-wide credential ceiling"
    );
    Ok(())
}
