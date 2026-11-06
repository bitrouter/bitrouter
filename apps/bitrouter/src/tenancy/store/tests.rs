use super::directory::*;
use super::*;
use crate::tenancy::{
    command::Command,
    model::{AuthorityHeader, Caller, Deadline, ManagedBy},
};
use bitrouter_ai::tenancy::{
    authority::{
        CredentialCreate, CredentialState, GrantScope, Permission, PrincipalPut, TenantRole,
        TenantState,
    },
    identity::{CredentialId, TenantId},
};
use sea_orm::{DatabaseBackend, Statement, TransactionTrait};
use sea_orm_migration::{MigratorTrait, SchemaManager};

const ROW_BYTES: usize = 8 * 1024 * 1024;

fn capacity<T>(result: Result<T>) -> Result<()> {
    match result {
        Err(error) if error.is::<CapacityExceeded>() => Ok(()),
        Err(error) => Err(error.context("expected typed capacity refusal")),
        Ok(_) => anyhow::bail!("expected capacity refusal"),
    }
}

fn header() -> Result<AuthorityHeader> {
    Ok(AuthorityHeader {
        installation: "directory-fixture".try_into()?,
        revision: 10_u64.try_into()?,
        operation_ids: 0,
        safety_operation_ids: 0,
        saturated: false,
        created_at: "2026-01-01T00:00:00Z".into(),
    })
}

fn operator() -> Result<Caller> {
    Ok(Caller::Installation(PrincipalId::for_control_credential(
        "fixture",
    )?))
}

async fn database() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    let manager = SchemaManager::new(&db);
    for migration in crate::db::migration::Migrator::migrations() {
        if [
            "m20240101_000028_",
            "m20240101_000029_",
            "m20240101_000039_",
        ]
        .iter()
        .any(|name| migration.name().starts_with(name))
        {
            migration.up(&manager).await?;
        }
    }
    Ok(db)
}

async fn add(db: &impl ConnectionTrait, index: usize) -> Result<()> {
    let tenant: TenantId = format!("tenant-{index:04}").try_into()?;
    let principal: PrincipalId = format!("principal-{index:04}").try_into()?;
    write_change(
        db,
        &Change::Tenant(Tenant {
            id: tenant.clone(),
            revision: "r1".try_into()?,
            state: TenantState::Active,
            created_at: "2026-01-01T00:00:00Z".into(),
        }),
    )
    .await?;
    write_change(
        db,
        &Change::Principal(Principal {
            id: principal.clone(),
            revision: "r1".try_into()?,
            issuer: "issuer".into(),
            subject: format!("subject-{index}"),
            created_at: "2026-01-01T00:00:00Z".into(),
        }),
    )
    .await?;
    let scope = GrantScope::Tenant {
        operator_pools: Vec::new(),
    };
    let expires = Deadline::parse("2099-01-01T00:00:00Z".try_into()?)?;
    write_change(
        db,
        &Change::Binding(Binding {
            tenant: tenant.clone(),
            id: "shared-id".try_into()?,
            principal: principal.clone(),
            revision: "r1".try_into()?,
            role: TenantRole::TenantAdmin,
            scope: scope.clone(),
            expires: expires.clone(),
            managed_by: Some(ManagedBy {
                principal: PrincipalId::for_control_credential("fixture")?,
                class: AuthorityClass::Installation,
            }),
        }),
    )
    .await?;
    let state = match index % 3 {
        0 => CredentialState::Active,
        1 => CredentialState::Revoked,
        _ => CredentialState::Expired,
    };
    write_change(
        db,
        &Change::Credential(Credential {
            id: format!("credential-{index:04}").try_into()?,
            tenant,
            principal,
            binding: Some("shared-id".try_into()?),
            revision: "r1".try_into()?,
            key_hash: format!("{index:064x}"),
            state,
            audit_reader: false,
            permissions: vec![Permission::ExecutionSubmit],
            scope,
            expires: if state == CredentialState::Expired {
                Deadline::parse("2000-01-01T00:00:00Z".try_into()?)?
            } else {
                expires
            },
            created_at: "2026-01-01T00:00:00Z".into(),
        }),
    )
    .await
}

#[tokio::test]
async fn indexed_reads_borrow_transaction_and_preserve_exact_identity() -> Result<()> {
    let db = database().await?;
    add(&db, 0).await?;
    let tx = db.begin().await?;
    add(&tx, 1).await?;
    let id: CredentialId = "credential-0001".try_into()?;
    assert_eq!(
        credential_in(&tx, &id, ROW_BYTES)
            .await?
            .context("credential missing")?
            .state,
        CredentialState::Revoked
    );
    assert_eq!(
        credential_by_hash_in(&tx, &format!("{:064x}", 1), ROW_BYTES)
            .await?
            .context("digest missing")?
            .id,
        id
    );
    assert!(
        principal_by_identity_in(&tx, "Issuer", "subject-0", ROW_BYTES)
            .await?
            .is_none()
    );
    assert!(
        principal_by_identity_in(&tx, "issuer", "subject-0 ", ROW_BYTES)
            .await?
            .is_none()
    );
    assert_eq!(
        principal_by_identity_in(&tx, "issuer", "subject-0", ROW_BYTES)
            .await?
            .context("principal missing")?
            .id
            .as_str(),
        "principal-0000"
    );
    tx.rollback().await?;
    assert!(credential_in(&db, &id, ROW_BYTES).await?.is_none());
    assert!(
        tenant_in(&db, &"tenant-0001".try_into()?, ROW_BYTES)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn complete_pages_cross_legacy_caps_and_include_credential_history() -> Result<()> {
    let db = database().await?;
    let tx = db.begin().await?;
    for index in 0..300 {
        add(&tx, index).await?;
    }
    tx.commit().await?;
    let budget = PageBudget {
        rows: 7,
        bytes: 7 * ROW_BYTES,
    };
    let mut after = None;
    let mut count = 0;
    loop {
        let page = tenant_page_in(&db, after.as_ref(), budget).await?;
        assert!(page.len() <= budget.rows);
        if page.is_empty() {
            break;
        }
        count += page.len();
        after = page.last().map(|tenant| tenant.id.clone());
    }
    assert_eq!(count, 300);
    let mut after = None;
    let mut states = [0; 3];
    loop {
        let page = credential_page_in(&db, after.as_ref(), budget).await?;
        if page.is_empty() {
            break;
        }
        for credential in page {
            states[match credential.state {
                CredentialState::Active => 0,
                CredentialState::Revoked => 1,
                CredentialState::Expired => 2,
            }] += 1;
            after = Some(credential.id);
        }
    }
    assert_eq!(states, [100, 100, 100]);
    assert_eq!(
        principal_page_in(&db, Some(&"principal-0298".try_into()?), budget)
            .await?
            .len(),
        1
    );
    assert_eq!(
        binding_page_in(
            &db,
            Some(&("tenant-0298".try_into()?, "shared-id".try_into()?)),
            budget
        )
        .await?
        .len(),
        1
    );
    validate_directory_in(&db, budget).await?;
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "UPDATE tenancy_credentials SET expires_at='invalid' WHERE id='credential-0299'".to_owned(),
    ))
    .await?;
    assert!(validate_directory_in(&db, budget).await.is_err());
    Ok(())
}

#[tokio::test]
async fn binding_cursor_uses_both_primary_key_columns() -> Result<()> {
    let db = database().await?;
    add(&db, 0).await?;
    add(&db, 1).await?;
    let tenant: TenantId = "tenant-0000".try_into()?;
    let mut other = binding_in(&db, &tenant, &"shared-id".try_into()?, ROW_BYTES)
        .await?
        .context("binding missing")?;
    other.id = "z-last".try_into()?;
    write_change(&db, &Change::Binding(other)).await?;
    let budget = PageBudget {
        rows: 1,
        bytes: ROW_BYTES,
    };
    let mut after = None;
    let mut seen = Vec::new();
    loop {
        let page = binding_page_in(&db, after.as_ref(), budget).await?;
        if page.is_empty() {
            break;
        }
        for binding in page {
            after = Some((binding.tenant.clone(), binding.id.clone()));
            seen.push((binding.tenant.to_string(), binding.id.to_string()));
        }
    }
    assert_eq!(
        seen,
        vec![
            ("tenant-0000".into(), "shared-id".into()),
            ("tenant-0000".into(), "z-last".into()),
            ("tenant-0001".into(), "shared-id".into())
        ]
    );
    Ok(())
}

#[tokio::test]
async fn command_facts_load_indexed_uniqueness_and_explicit_absence() -> Result<()> {
    let db = database().await?;
    for index in 0..40 {
        add(&db, index).await?;
    }
    let header = header()?;
    let caller = Caller::Credential("credential-0000".try_into()?);
    let target: PrincipalId = "new-principal".try_into()?;
    let command = Command::Principal(
        target.clone(),
        PrincipalPut {
            installation_id: header.installation.clone(),
            operation_id: "put-principal".try_into()?,
            expected_revision: None,
            issuer: "issuer".into(),
            subject: "subject-39".into(),
        },
    );
    let facts = load_command_in(&db, &header, &caller, &command, ROW_BYTES).await?;
    assert_eq!(facts.snapshot.principals.len(), 2);
    assert_eq!(facts.snapshot.tenants.len(), 1);
    assert_eq!(facts.snapshot.bindings.len(), 1);
    assert_eq!(facts.snapshot.credentials.len(), 1);
    assert_eq!(facts.presence(&Lookup::Principal(target)), Presence::Absent);
    assert_eq!(
        facts.presence(&Lookup::PrincipalIdentity(
            "issuer".into(),
            "subject-39".into()
        )),
        Presence::Present
    );
    assert_eq!(
        facts.presence(&Lookup::Tenant("tenant-0039".try_into()?)),
        Presence::NotLoaded
    );
    assert!(
        facts
            .snapshot
            .principals
            .contains_key(&"principal-0039".try_into()?)
    );
    let missing = load_caller_in(
        &db,
        &header,
        &Caller::Credential("missing".try_into()?),
        ROW_BYTES,
    )
    .await?;
    assert_eq!(
        missing.presence(&Lookup::Credential("missing".try_into()?)),
        Presence::Absent
    );
    assert!(missing.snapshot.credentials.is_empty());
    let installation = load_caller_in(&db, &header, &operator()?, ROW_BYTES).await?;
    assert!(installation.loaded.is_empty());
    Ok(())
}

fn issue(header: &AuthorityHeader, rotate: CredentialId) -> Result<Command> {
    Ok(Command::Issue(
        "tenant-0001".try_into()?,
        CredentialCreate {
            installation_id: header.installation.clone(),
            operation_id: "rotate".try_into()?,
            expected_authority_revision: header.revision,
            principal_id: "principal-0001".try_into()?,
            binding_id: "shared-id".try_into()?,
            permissions: vec![Permission::ExecutionSubmit],
            scope: GrantScope::Tenant {
                operator_pools: Vec::new(),
            },
            expires_at: "2090-01-01T00:00:00Z".try_into()?,
            rotate_credential_id: Some(rotate),
        },
    ))
}

#[tokio::test]
async fn rotation_loads_predecessor_links_without_loading_dormant_directory() -> Result<()> {
    let db = database().await?;
    for index in 0..10 {
        add(&db, index).await?;
    }
    let header = header()?;
    let caller = Caller::Credential("credential-0000".try_into()?);
    let old: CredentialId = "credential-0004".try_into()?;
    let facts = load_command_in(
        &db,
        &header,
        &caller,
        &issue(&header, old.clone())?,
        ROW_BYTES,
    )
    .await?;
    assert_eq!(facts.snapshot.tenants.len(), 3);
    assert_eq!(facts.snapshot.principals.len(), 3);
    assert_eq!(facts.snapshot.bindings.len(), 3);
    assert_eq!(facts.snapshot.credentials.len(), 2);
    assert_eq!(
        facts.presence(&Lookup::Credential(old.clone())),
        Presence::Present
    );
    let predecessor = facts
        .snapshot
        .credentials
        .get(&old)
        .context("predecessor missing")?;
    assert_eq!(predecessor.tenant.as_str(), "tenant-0004");
    assert_eq!(predecessor.state, CredentialState::Revoked);
    assert!(
        facts
            .snapshot
            .credential(&Caller::Credential(old), Utc::now())
            .is_err()
    );
    let missing: CredentialId = "missing-predecessor".try_into()?;
    let facts = load_command_in(
        &db,
        &header,
        &caller,
        &issue(&header, missing.clone())?,
        ROW_BYTES,
    )
    .await?;
    assert_eq!(
        facts.presence(&Lookup::Credential(missing)),
        Presence::Absent
    );
    Ok(())
}

#[tokio::test]
async fn encoded_byte_boundary_and_corruption_never_become_absence() -> Result<()> {
    let db = database().await?;
    add(&db, 0).await?;
    let id: TenantId = "tenant-0000".try_into()?;
    let bytes = "tenant-0000".len() + "r1".len() + "active".len() + "2026-01-01T00:00:00Z".len();
    capacity(tenant_in(&db, &id, bytes - 1).await)?;
    assert!(tenant_in(&db, &id, bytes).await?.is_some());
    assert!(tenant_in(&db, &id, bytes + 1).await?.is_some());
    for budget in [
        PageBudget {
            rows: 0,
            bytes: ROW_BYTES,
        },
        PageBudget { rows: 1, bytes: 0 },
        PageBudget {
            rows: usize::MAX,
            bytes: usize::MAX,
        },
    ] {
        assert!(tenant_page_in(&db, None, budget).await.is_err());
    }
    let caller = Caller::Credential("credential-0000".try_into()?);
    capacity(load_caller_in(&db, &header()?, &caller, 1).await)?;
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "UPDATE tenancy_bindings SET management_owner=CAST('owner' AS BLOB)".to_owned(),
    ))
    .await?;
    assert!(
        binding_in(&db, &id, &"shared-id".try_into()?, ROW_BYTES)
            .await
            .is_err()
    );
    assert!(
        load_caller_in(&db, &header()?, &caller, ROW_BYTES)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn caller_and_command_working_budgets_are_aggregate_refusals() -> Result<()> {
    let db = database().await?;
    add(&db, 0).await?;
    let header = header()?;
    let id: CredentialId = "credential-0000".try_into()?;
    assert!(credential_in(&db, &id, 1024).await?.is_some());
    let caller = Caller::Credential(id.clone());
    capacity(load_caller_in(&db, &header, &caller, 1024).await)?;
    capacity(load_command_in(&db, &header, &caller, &issue(&header, id)?, 1024).await)?;
    let facts = load_caller_in(&db, &header, &caller, ROW_BYTES).await?;
    assert_eq!(facts.snapshot.credentials.len(), 1);
    assert_eq!(facts.snapshot.tenants.len(), 1);
    Ok(())
}

#[path = "tests/resources.rs"]
mod resources;

#[path = "tests/startup.rs"]
mod startup;
