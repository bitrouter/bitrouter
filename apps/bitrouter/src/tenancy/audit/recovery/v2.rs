//! Complete borrowed v2 proofs. The caller owns the fixed transaction and close.

use super::super::connection::ReadConnection;
use crate::offline::{
    format::Inventory,
    large::sort::{Records, Sorted},
    recovery::{
        AuthorityFacts,
        plan::Target,
        v2::{
            catalog,
            format::{self, ProvisioningRecord},
            inputs::Prepared,
            memory,
            streams::{Approval, Provisioning},
        },
    },
};
use crate::tenancy::{
    configuration_store,
    model::AuthorityHeader,
    store::{
        self,
        directory::{self, PageBudget},
    },
};
use crate::{
    offline::{ValidationBudget, large::Workspace},
    tenancy::resources::Resources,
};
use anyhow::{Context, Result, ensure};
#[cfg(feature = "core")]
use bitrouter_ai::tenancy::identity::{PrincipalId, TenantId};
use bitrouter_ai::tenancy::{
    authority::{CredentialState, GrantScope, Permission, TenantRole, TenantState},
    configuration::ConfigurationRevision,
    identity::InstallationId,
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, Statement, sqlx::SqliteConnection};

#[cfg(all(test, unix))]
mod tests;

pub(crate) struct TargetProof<'a> {
    pub facts: AuthorityFacts,
    pub credentials: Sorted<'a, 32>,
}

pub(crate) async fn inspect_target_in<'a>(
    db: &mut SqliteConnection,
    inventory: &Inventory,
    target: &Target,
    provisioning: &mut Prepared,
    approval: &mut Prepared,
    validation: ValidationBudget,
    workspace: &'a Workspace,
) -> Result<TargetProof<'a>> {
    let reader = ReadConnection::new(db)
        .with_row_limit(validation.rows.max(128))
        .with_workspace(workspace);
    let header = validated_header(&reader, &target.installation_id, validation, workspace).await?;
    catalog::recovery(&reader, inventory, &target.installation_id).await?;
    let (mut qualifiers, credentials, mut earliest) = fresh_directory(&reader, workspace).await?;
    let check = || workspace.check();
    let mut observed = Provisioning::new(
        provisioning.reader(workspace)?,
        &target.installation_id,
        &check,
    )?;
    ensure!(
        observed.header.configuration_count > 0 && observed.header.active_count > 0,
        "fresh recovery configuration/active evidence required"
    );
    operations(&reader, inventory, &header, &mut observed, workspace).await?;
    for (table, count) in [
        (
            "tenancy_configurations",
            observed.header.configuration_count,
        ),
        (
            "tenancy_active_configurations",
            observed.header.active_count,
        ),
        ("tenancy_resource_revisions", observed.header.resource_count),
    ] {
        ensure!(
            catalog::count(inventory, table)? == count,
            "fresh readback coverage mismatch"
        );
    }
    let settings = crate::config::tenancy::TenancyConfig {
        enabled: true,
        wrapping_key_env: Some(target.wrapping_key_env.clone()),
        ..Default::default()
    };
    let resources = Resources::load_key(&header.installation, &settings)?;
    resources.inspect_key_epoch_in(&reader).await?;
    let mut reached = Records::<416>::new(workspace, sort_bytes(workspace))?;
    let mut approved = None;
    let mut declared = Records::<416>::new(workspace, sort_bytes(workspace))?;
    while let Some(record) = observed.next(&check)? {
        match record.value {
            ProvisioningRecord::Configuration { revision } => {
                let bytes = workspace.available();
                let _phase = workspace.reserve(bytes)?;
                memory::preflight(
                    &reader,
                    "tenancy_configurations",
                    &[
                        ("tenant_id", revision.tenant_id.as_str()),
                        ("revision_id", revision.revision_id.as_str()),
                    ],
                    &["document_json", "compiled_json", "dependencies_json"],
                    bytes / 2,
                )
                .await?;
                let stored = configuration_store::configuration(
                    &reader,
                    &revision.tenant_id,
                    &revision.revision_id,
                )
                .await?
                .context("configuration missing")?;
                ensure!(
                    serde_json::to_value(&stored.revision)? == serde_json::to_value(&revision)?
                        && stored.authority_revision <= header.revision,
                    "configuration readback changed"
                );
                let expiry =
                    check_creator(&reader, &stored, &mut qualifiers, bytes / 2, workspace).await?;
                earliest = earliest.min(expiry);
                resources.inspect_static_configuration(&stored, bytes)?;
                for reference in stored.dependencies {
                    reached.push(format::resource_key(&reference)?)?;
                }
            }
            ProvisioningRecord::Active {
                tenant_id,
                revision_id,
                authority_revision,
            } => {
                let _phase = workspace.reserve(64 * 1024)?;
                let row = reader.query_one(Statement::from_sql_and_values(reader.get_database_backend(), "SELECT revision_id,authority_revision FROM tenancy_active_configurations WHERE tenant_id=? LIMIT 1", [tenant_id.as_str().into()])).await?.context("active pointer missing")?;
                ensure!(
                    row.try_get::<String>("", "revision_id")? == revision_id.as_str()
                        && u64::try_from(row.try_get::<i64>("", "authority_revision")?)?
                            == authority_revision.get(),
                    "active pointer changed"
                );
                // Every stored configuration was compared in the preceding
                // complete unique section; this exact pointer cannot bypass it.
            }
            ProvisioningRecord::Resource { reference } => {
                if approved.is_none() {
                    let stream = Approval::new(
                        approval.reader(workspace)?,
                        &target.installation_id,
                        &check,
                    )?;
                    ensure!(
                        stream.header.resource_count == observed.header.resource_count,
                        "approval coverage count mismatch"
                    );
                    approved = Some(stream);
                }
                let supplied = approved
                    .as_mut()
                    .context("approval stream missing")?
                    .next(&check)?
                    .context("approved definition missing")?;
                ensure!(
                    supplied.value.reference == reference,
                    "approval/readback resource mismatch"
                );
                let bytes = workspace.available();
                let _phase = workspace.reserve(bytes)?;
                resources
                    .inspect_approved_resource_in(&reader, supplied.value, bytes, &check)
                    .await?;
                declared.push(format::resource_key(&reference)?)?;
            }
            ProvisioningRecord::Operation { .. } => {
                anyhow::bail!("operation outside provisioning prefix")
            }
        }
    }
    if let Some(mut approved) = approved {
        ensure!(approved.next(&check)?.is_none(), "extra approved resource");
    } else {
        let mut empty =
            Approval::new(approval.reader(workspace)?, &target.installation_id, &check)?;
        ensure!(
            empty.header.resource_count == 0 && empty.next(&check)?.is_none(),
            "undeclared approved resources"
        );
    }
    drop(observed);
    let mut reached = reached.finish()?;
    let mut declared = declared.finish()?;
    let mut previous = None;
    while let Some(reference) = reached.next()? {
        if previous == Some(reference) {
            continue;
        }
        ensure!(
            declared.next()? == Some(reference),
            "unapproved reached or orphan approved resource"
        );
        previous = Some(reference);
    }
    ensure!(declared.next()?.is_none(), "unreferenced approved resource");
    ensure!(
        earliest > Utc::now(),
        "fresh grant expired during verification"
    );
    check()?;
    Ok(TargetProof {
        facts: super::facts(&header),
        credentials,
    })
}

fn sort_bytes(workspace: &Workspace) -> usize {
    workspace.available().min(1024 * 1024)
}

async fn validated_header(
    db: &impl ConnectionTrait,
    expected: &InstallationId,
    validation: ValidationBudget,
    workspace: &Workspace,
) -> Result<AuthorityHeader> {
    workspace.check()?;
    let bytes = workspace.available().min(validation.bytes);
    let _phase = workspace.reserve(bytes)?;
    let header = super::super::validation::load(
        db,
        ValidationBudget {
            rows: validation.rows,
            bytes,
        },
    )
    .await?
    .header;
    ensure!(
        &header.installation == expected,
        "target installation mismatch"
    );
    Ok(header)
}

async fn operations(
    db: &impl ConnectionTrait,
    inventory: &Inventory,
    header: &AuthorityHeader,
    observed: &mut Provisioning<'_>,
    workspace: &Workspace,
) -> Result<()> {
    let count = observed.header.operation_count;
    ensure!(
        !header.saturated
            && header.safety_operation_ids == 0
            && header.operation_ids == count
            && header.revision.get() == count
            && catalog::count(inventory, "tenancy_operations")? == count,
        "fresh operation counters/coverage mismatch"
    );
    let mut ids = Records::<128>::new(workspace, sort_bytes(workspace))?;
    for _ in 0..count {
        let record = observed
            .next(&|| workspace.check())?
            .context("operation evidence missing")?;
        let ProvisioningRecord::Operation {
            principal_id,
            result,
        } = &record.value
        else {
            anyhow::bail!("operation section missing");
        };
        let bytes = workspace.available();
        let _phase = workspace.reserve(bytes)?;
        memory::preflight(
            db,
            "tenancy_operations",
            &[
                ("installation_id", header.installation.as_str()),
                ("principal_id", principal_id.as_str()),
                ("operation_id", result.operation_id.as_str()),
            ],
            &["read_context_json", "result_json", "response_json"],
            bytes / 2,
        )
        .await?;
        let row = db.query_one(Statement::from_sql_and_values(db.get_database_backend(), "SELECT installation_id,principal_id,operation_id,hash_version,safety,result_json,response_json,read_context_json,created_at,command_digest FROM tenancy_operations WHERE installation_id=? AND principal_id=? AND operation_id=? LIMIT 1", [header.installation.as_str().into(), principal_id.as_str().into(), result.operation_id.as_str().into()])).await?.context("declared operation missing")?;
        let stored = store::operation_from(&row, principal_id, &result.operation_id)?;
        ensure!(
            !stored.safety
                && serde_json::to_value(&stored.result)? == serde_json::to_value(result)?,
            "operation readback mismatch"
        );
        match stored.response {
            store::StoredResponse::Operation(returned) => ensure!(
                serde_json::to_value(&returned)? == serde_json::to_value(result)?,
                "operation response mismatch"
            ),
            store::StoredResponse::Issued(issued) => ensure!(
                issued.secret.is_none()
                    && issued.operation_id == result.operation_id
                    && Some(issued.authority_revision) == result.authority_revision,
                "issuance response mismatch"
            ),
            store::StoredResponse::Configuration(configuration) => {
                configuration_response(db, &configuration, bytes / 2).await?
            }
            store::StoredResponse::Failure(_) => anyhow::bail!("failed fresh operation"),
        }
        ids.push(format::padded::<128>(result.operation_id.as_str())?)?;
    }
    let mut sorted = ids.finish()?;
    let mut previous = None;
    while let Some(id) = sorted.next()? {
        ensure!(
            previous != Some(id),
            "duplicate global provisioning operation ID"
        );
        previous = Some(id);
    }
    Ok(())
}

async fn configuration_response(
    db: &impl ConnectionTrait,
    expected: &ConfigurationRevision,
    bytes: usize,
) -> Result<()> {
    memory::preflight(
        db,
        "tenancy_configurations",
        &[
            ("tenant_id", expected.tenant_id.as_str()),
            ("revision_id", expected.revision_id.as_str()),
        ],
        &["document_json"],
        bytes,
    )
    .await?;
    let row = db.query_one(Statement::from_sql_and_values(db.get_database_backend(), "SELECT schema_version,compiler_version,content_digest,document_json FROM tenancy_configurations WHERE tenant_id=? AND revision_id=? LIMIT 1", [expected.tenant_id.as_str().into(), expected.revision_id.as_str().into()])).await?.context("configuration response target missing")?;
    let document: bitrouter_ai::tenancy::configuration::Configuration =
        serde_json::from_str(&row.try_get::<String>("", "document_json")?)?;
    ensure!(
        u32::try_from(row.try_get::<i64>("", "schema_version")?)? == expected.schema_version
            && row.try_get::<String>("", "compiler_version")? == expected.compiler_version
            && row.try_get::<String>("", "content_digest")? == expected.content_digest.as_str()
            && document == expected.document,
        "configuration response differs from retained revision"
    );
    Ok(())
}

async fn fresh_directory<'a>(
    db: &impl ConnectionTrait,
    workspace: &'a Workspace,
) -> Result<(Sorted<'a, 384>, Sorted<'a, 32>, DateTime<Utc>)> {
    let mut qualifiers = Records::<384>::new(workspace, sort_bytes(workspace))?;
    let mut hashes = Records::<32>::new(workspace, sort_bytes(workspace))?;
    let bytes = workspace.available() / 4;
    let _phase = workspace.reserve(bytes)?;
    let row_bytes = bytes / 64;
    let mut after = None;
    let mut tenants = 0u64;
    loop {
        workspace.check()?;
        let page = directory::tenant_page_in(
            db,
            after.as_ref(),
            PageBudget {
                rows: 1,
                bytes: row_bytes,
            },
        )
        .await?;
        if page.is_empty() {
            break;
        }
        for tenant in page {
            ensure!(tenant.state == TenantState::Active, "fresh tenant inactive");
            after = Some(tenant.id);
            tenants = tenants.checked_add(1).context("tenant count overflow")?;
        }
    }
    ensure!(tenants > 0, "fresh tenant missing");
    let mut after = None;
    let mut earliest = None::<DateTime<Utc>>;
    loop {
        workspace.check()?;
        let page = directory::credential_page_in(
            db,
            after.as_ref(),
            PageBudget {
                rows: 1,
                bytes: row_bytes,
            },
        )
        .await?;
        if page.is_empty() {
            break;
        }
        for credential in page {
            ensure!(
                credential.state == CredentialState::Active
                    && credential.expires.instant > Utc::now(),
                "expired/revoked fresh credential"
            );
            earliest = Some(earliest.map_or(credential.expires.instant, |old| {
                old.min(credential.expires.instant)
            }));
            let mut hash = [0; 32];
            hex::decode_to_slice(&credential.key_hash, &mut hash)?;
            hashes.push(hash)?;
            if let Some(binding_id) = &credential.binding
                && matches!(credential.scope, GrantScope::Tenant { .. })
                && [
                    Permission::ResourceRead,
                    Permission::ResourceDraft,
                    Permission::PolicyActivate,
                ]
                .iter()
                .all(|permission| credential.permissions.contains(permission))
                && let Some(binding) =
                    directory::binding_in(db, &credential.tenant, binding_id, row_bytes).await?
                && binding.principal == credential.principal
                && binding.role == TenantRole::TenantAdmin
                && matches!(binding.scope, GrantScope::Tenant { .. })
                && binding.expires.instant > Utc::now()
            {
                let mut key = [0; 384];
                key[..128].copy_from_slice(&format::padded::<128>(credential.tenant.as_str())?);
                key[128..256]
                    .copy_from_slice(&format::padded::<128>(credential.principal.as_str())?);
                key[256..].copy_from_slice(&format::padded::<128>(credential.id.as_str())?);
                qualifiers.push(key)?;
            }
            after = Some(credential.id);
        }
    }
    Ok((
        qualifiers.finish()?,
        hashes.finish()?,
        earliest.context("fresh credential missing")?,
    ))
}

async fn check_creator(
    db: &impl ConnectionTrait,
    stored: &configuration_store::StoredConfiguration,
    qualifiers: &mut Sorted<'_, 384>,
    bytes: usize,
    workspace: &Workspace,
) -> Result<DateTime<Utc>> {
    ensure!(
        directory::principal_in(db, &stored.creator, bytes / 64)
            .await?
            .is_some(),
        "configuration creator missing"
    );
    let mut prefix = [0; 256];
    prefix[..128].copy_from_slice(&format::padded::<128>(stored.revision.tenant_id.as_str())?);
    prefix[128..].copy_from_slice(&format::padded::<128>(stored.creator.as_str())?);
    let (mut low, mut high) = (0, qualifiers.len());
    while low < high {
        workspace.check()?;
        let middle = low + (high - low) / 2;
        let record = qualifiers
            .read_at(middle)?
            .context("creator qualifier truncated")?;
        if record[..256].cmp(prefix.as_slice()).is_lt() {
            low = middle.checked_add(1).context("qualifier offset overflow")?;
        } else {
            high = middle;
        }
    }
    while let Some(record) = qualifiers.read_at(low)? {
        workspace.check()?;
        if record[..256] != prefix {
            break;
        }
        let id = std::str::from_utf8(&record[256..])?
            .trim_end_matches('\0')
            .try_into()?;
        let credential = directory::credential_in(db, &id, bytes / 64)
            .await?
            .context("creator credential disappeared")?;
        if let Some(binding_id) = &credential.binding
            && let Some(binding) =
                directory::binding_in(db, &credential.tenant, binding_id, bytes / 64).await?
            && credential.expires.instant > Utc::now()
            && binding.expires.instant > Utc::now()
            && stored.dependencies.iter().all(|reference| {
                crate::tenancy::authority::covers_key(
                    &credential.scope,
                    &credential.tenant,
                    &reference.key(),
                ) && crate::tenancy::authority::covers_key(
                    &binding.scope,
                    &binding.tenant,
                    &reference.key(),
                )
            })
        {
            return Ok(credential.expires.instant.min(binding.expires.instant));
        }
        low = low.checked_add(1).context("qualifier offset overflow")?;
    }
    anyhow::bail!("configuration lacks current tenant-wide creator credential")
}

/// Target SQL must be closed before the caller establishes this source phase.
pub(crate) async fn no_copied_credentials_in(
    db: &mut SqliteConnection,
    source: &InstallationId,
    credentials: &mut Sorted<'_, 32>,
    validation: ValidationBudget,
    workspace: &Workspace,
) -> Result<AuthorityFacts> {
    let reader = ReadConnection::new(db)
        .with_row_limit(validation.rows.max(128))
        .with_workspace(workspace);
    let header = validated_header(&reader, source, validation, workspace).await?;
    let bytes = workspace.available() / 4;
    let _phase = workspace.reserve(bytes)?;
    while let Some(hash) = credentials.next()? {
        workspace.check()?;
        ensure!(
            directory::credential_by_hash_in(&reader, &hex::encode(hash), bytes / 32)
                .await?
                .is_none(),
            "old ingress credential copied to target"
        );
    }
    Ok(super::facts(&header))
}

/// Eval's fresh profile has no configurations/resources. The bounded imported
/// graph supplies only its exact destination tenant/principal assignments.
#[cfg(feature = "core")]
pub(crate) async fn inspect_eval_authority_in(
    db: &mut SqliteConnection,
    inventory: &Inventory,
    installation: &InstallationId,
    provisioning: &mut Prepared,
    assignments: &[(TenantId, PrincipalId)],
    validation: ValidationBudget,
    workspace: &Workspace,
) -> Result<AuthorityFacts> {
    let reader = ReadConnection::new(db)
        .with_row_limit(validation.rows.max(128))
        .with_workspace(workspace);
    let header = validated_header(&reader, installation, validation, workspace).await?;
    catalog::eval(&reader, inventory, installation).await?;
    let mut observed = Provisioning::new(provisioning.reader(workspace)?, installation, &|| {
        workspace.check()
    })?;
    ensure!(
        observed.header.configuration_count == 0
            && observed.header.active_count == 0
            && observed.header.resource_count == 0,
        "Eval requires zero static provisioning sections"
    );
    operations(&reader, inventory, &header, &mut observed, workspace).await?;
    ensure!(
        observed.next(&|| workspace.check())?.is_none(),
        "extra Eval provisioning evidence"
    );
    drop(observed);
    let mut qualifiers = Records::<384>::new(workspace, sort_bytes(workspace))?;
    let bytes = workspace.available() / 4;
    let _phase = workspace.reserve(bytes)?;
    let mut after = None;
    loop {
        workspace.check()?;
        let page = directory::tenant_page_in(
            &reader,
            after.as_ref(),
            PageBudget {
                rows: 1,
                bytes: bytes / 64,
            },
        )
        .await?;
        if page.is_empty() {
            break;
        }
        for tenant in page {
            ensure!(
                tenant.state == TenantState::Active,
                "Eval target tenant inactive"
            );
            after = Some(tenant.id);
        }
    }
    let mut after = None;
    loop {
        workspace.check()?;
        let page = directory::binding_page_in(
            &reader,
            after.as_ref(),
            PageBudget {
                rows: 1,
                bytes: bytes / 64,
            },
        )
        .await?;
        if page.is_empty() {
            break;
        }
        for binding in page {
            if binding.role == TenantRole::TenantAdmin
                && matches!(binding.scope, GrantScope::Tenant { .. })
                && binding.expires.instant > Utc::now()
            {
                let mut key = [0; 384];
                key[..128].copy_from_slice(&format::padded::<128>(binding.tenant.as_str())?);
                key[128..256].copy_from_slice(&format::padded::<128>(binding.principal.as_str())?);
                key[256..].copy_from_slice(&format::padded::<128>(binding.id.as_str())?);
                qualifiers.push(key)?;
            }
            after = Some((binding.tenant, binding.id));
        }
    }
    let mut qualifiers = qualifiers.finish()?;
    let mut earliest = None::<DateTime<Utc>>;
    for (tenant, principal) in assignments {
        let mut prefix = [0; 256];
        prefix[..128].copy_from_slice(&format::padded::<128>(tenant.as_str())?);
        prefix[128..].copy_from_slice(&format::padded::<128>(principal.as_str())?);
        let (mut low, mut high) = (0, qualifiers.len());
        while low < high {
            workspace.check()?;
            let middle = low + (high - low) / 2;
            let key = qualifiers
                .read_at(middle)?
                .context("Eval qualifier truncated")?;
            if key[..256].cmp(prefix.as_slice()).is_lt() {
                low = middle.checked_add(1).context("qualifier offset overflow")?;
            } else {
                high = middle;
            }
        }
        let mut expiry = None;
        while let Some(key) = qualifiers.read_at(low)? {
            workspace.check()?;
            if key[..256] != prefix {
                break;
            }
            let id = std::str::from_utf8(&key[256..])?
                .trim_end_matches('\0')
                .try_into()?;
            let binding = directory::binding_in(&reader, tenant, &id, bytes / 64)
                .await?
                .context("Eval binding disappeared")?;
            if binding.expires.instant > Utc::now() {
                expiry = Some(binding.expires.instant);
                break;
            }
            low = low.checked_add(1).context("qualifier offset overflow")?;
        }
        let expiry = expiry.context("mapping lacks current tenant-wide admin binding")?;
        earliest = Some(earliest.map_or(expiry, |old| old.min(expiry)));
    }
    ensure!(
        earliest.is_none_or(|expiry| expiry > Utc::now()),
        "Eval mapping grant expired during verification"
    );
    workspace.check()?;
    Ok(super::facts(&header))
}

/// Restore preserves historical authority and unresolved custody. This does not
/// run feature recovery, clear a fence, initialize storage or admit dispatch.
pub(crate) async fn inspect_restore_in(
    db: &mut SqliteConnection,
    expected: &InstallationId,
    wrapping_key_env: &str,
    validation: ValidationBudget,
    workspace: &Workspace,
) -> Result<()> {
    workspace.check()?;
    let reader = ReadConnection::new(db)
        .with_row_limit(validation.rows.max(128))
        .with_workspace(workspace);
    let bytes = validation.bytes.min(workspace.available());
    let _charge = workspace.reserve(bytes)?;
    let state = super::super::validation::load(
        &reader,
        ValidationBudget {
            rows: validation.rows,
            bytes,
        },
    )
    .await?
    .header;
    ensure!(
        &state.installation == expected,
        "restore installation mismatch"
    );
    let settings = crate::config::tenancy::TenancyConfig {
        enabled: true,
        wrapping_key_env: Some(wrapping_key_env.to_owned()),
        ..Default::default()
    };
    let resources = Resources::load_key(expected, &settings)?;
    resources.inspect_key_epoch_in(&reader).await?;
    // Key metadata and the 32-reference page are covered separately from the
    // one original authenticated decoder. Neither charge accumulates by row.
    let body = bytes
        .checked_sub(512 * 1024)
        .context("restore metadata working capacity")?;
    resources
        .inspect_all_retained_in(&reader, body, &|| workspace.check())
        .await?;
    inspect_oauth_in(&reader, expected, &resources, body, workspace).await?;
    workspace.check()
}

async fn inspect_oauth_in(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    resources: &Resources,
    bytes: usize,
    workspace: &Workspace,
) -> Result<()> {
    use crate::tenancy::oauth::storage;
    let crypto = resources.oauth_crypto()?;
    let mut after = String::new();
    loop {
        workspace.check()?;
        // Read one bounded metadata row through the slot PK, then invoke the
        // original decoder/token authentication on that exact current revision.
        let row = db.query_one(Statement::from_sql_and_values(db.get_database_backend(), "SELECT CASE WHEN length(CAST(slot_key AS BLOB))<=64 THEN slot_key ELSE NULL END AS slot, CASE WHEN length(CAST(metadata_json AS BLOB))<=8192 THEN metadata_json ELSE NULL END AS metadata FROM tenancy_oauth_slots WHERE slot_key>? ORDER BY tenancy_oauth_slots.slot_key LIMIT 1", [after.as_str().into()])).await?;
        let Some(row) = row else {
            break;
        };
        ensure!(
            bytes >= 8 * 1024 * 1024,
            crate::offline::large::CapacityExceeded
        );
        let slot: String = row.try_get("", "slot")?;
        let metadata: storage::Metadata =
            serde_json::from_str(&row.try_get::<String>("", "metadata")?)?;
        metadata.validate()?;
        ensure!(
            &metadata.installation == installation && metadata.slot == slot && slot > after,
            "restore OAuth identity/order mismatch"
        );
        let retained = storage::load(db, installation, &metadata.reference)
            .await?
            .context("restore OAuth slot disappeared")?;
        ensure!(
            retained.metadata == metadata,
            "restore OAuth metadata changed"
        );
        retained.token(&crypto)?;
        after = slot;
    }
    Ok(())
}
