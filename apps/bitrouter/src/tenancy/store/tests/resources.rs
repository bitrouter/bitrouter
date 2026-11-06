use super::*;
use crate::tenancy::resources::lifecycle;
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::{
        Activation, Configuration, ConfigurationRevision, GenerationDefaults, Learning,
        LearningMode, Limits, ResourceRef,
    },
    resources::{Definition, ResourcePut, ResourceRevoke},
};

fn reference(revision: &str) -> Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: "tenant-0000".try_into()?,
        resource_kind: ResourceKind::Credential,
        resource_id: "slot".try_into()?,
        revision_id: revision.try_into()?,
    })
}

#[tokio::test]
async fn scoped_dependency_absence_is_loaded_but_unknown_and_failed_reads_stay_denied() -> Result<()>
{
    let db = database().await?;
    let header = header()?;
    let mut facts = load_caller_in(&db, &header, &operator()?, ROW_BYTES).await?;
    let unmanaged = reference("unmanaged")?;
    let unknown = reference("unknown")?;
    load_dependencies_in(&db, &mut facts, std::slice::from_ref(&unmanaged), ROW_BYTES).await?;
    // Live views restrict the snapshot after loading its exact fact closure.
    facts.snapshot.resource_lifecycle.restrict();
    assert_eq!(
        facts.presence(&Lookup::Resource(unmanaged.clone())),
        Presence::Absent
    );
    assert!(facts.snapshot.resource_lifecycle.loaded(&unmanaged));
    assert!(
        facts
            .snapshot
            .resource_lifecycle
            .record(&unmanaged)
            .is_none()
    );
    assert!(!facts.snapshot.resource_lifecycle.revoked(&unmanaged));
    assert_eq!(
        facts.presence(&Lookup::Resource(unknown.clone())),
        Presence::NotLoaded
    );
    assert!(!facts.snapshot.resource_lifecycle.loaded(&unknown));
    assert!(facts.snapshot.resource_lifecycle.revoked(&unknown));
    let view = crate::tenancy::authority::live::View::new(facts.snapshot.clone(), Vec::new())
        .map_err(|code| anyhow::anyhow!("scoped view construction failed: {code:?}"))?;
    assert!(view.facts().resource_lifecycle.loaded(&unmanaged));
    assert!(!view.facts().resource_lifecycle.revoked(&unmanaged));
    assert!(view.facts().resource_lifecycle.revoked(&unknown));

    let mut snapshot = facts.snapshot.clone();
    let extra = reference("snapshot-only")?;
    extend_dependencies_in(&db, &mut snapshot, std::slice::from_ref(&extra), ROW_BYTES).await?;
    assert!(snapshot.resource_lifecycle.loaded(&extra));
    assert!(!snapshot.resource_lifecycle.revoked(&extra));
    assert!(snapshot.resource_lifecycle.loaded(&unmanaged));
    assert!(snapshot.resource_lifecycle.revoked(&unknown));
    capacity(extend_dependencies_in(&db, &mut snapshot, std::slice::from_ref(&unknown), 1).await)?;
    assert!(!snapshot.resource_lifecycle.loaded(&unknown));

    write_change(
        &db,
        &Change::ResourceLifecycle {
            installation: header.installation.clone(),
            record: lifecycle::Record {
                reference: unknown.clone(),
                is_head: false,
                revoked: false,
                authority_revision: 11_u64.try_into()?,
            },
        },
    )
    .await?;
    assert!(
        load_dependencies_in(&db, &mut facts, std::slice::from_ref(&unknown), ROW_BYTES)
            .await
            .is_err()
    );
    assert_eq!(
        facts.presence(&Lookup::Resource(unknown.clone())),
        Presence::NotLoaded
    );
    assert!(!facts.snapshot.resource_lifecycle.loaded(&unknown));
    assert!(facts.snapshot.resource_lifecycle.revoked(&unknown));
    Ok(())
}

#[tokio::test]
async fn resource_heads_exact_denials_and_absence_are_loaded_explicitly() -> Result<()> {
    let db = database().await?;
    add(&db, 0).await?;
    let header = header()?;
    let target = reference("r1")?;
    write_change(
        &db,
        &Change::ResourceLifecycle {
            installation: header.installation.clone(),
            record: lifecycle::Record {
                reference: target.clone(),
                is_head: true,
                revoked: true,
                authority_revision: 7_u64.try_into()?,
            },
        },
    )
    .await?;
    let command = Command::Resource(
        "tenant-0000".try_into()?,
        ResourcePut {
            format: 1,
            operation_id: "rotate-slot".try_into()?,
            installation_id: header.installation.clone(),
            key: target.key(),
            expected_revision: Some(target.revision_id.clone()),
            definition: Definition::Credential {
                secret: "fixture-secret".into(),
            },
        },
    );
    let facts = load_command_in(&db, &header, &operator()?, &command, ROW_BYTES).await?;
    assert_eq!(
        facts.presence(&Lookup::Head(target.key())),
        Presence::Present
    );
    assert_eq!(
        facts.presence(&Lookup::Resource(target.clone())),
        Presence::Present
    );
    assert!(facts.snapshot.resource_lifecycle.revoked(&target));
    let missing = reference("future")?;
    let revoke = Command::RevokeResource(
        "tenant-0000".try_into()?,
        ResourceRevoke {
            format: 1,
            operation_id: "revoke-slot".try_into()?,
            installation_id: header.installation.clone(),
            reference: missing.clone(),
            expected_authority_revision: header.revision,
        },
    );
    let facts = load_command_in(&db, &header, &operator()?, &revoke, ROW_BYTES).await?;
    assert_eq!(
        facts.presence(&Lookup::Resource(missing.clone())),
        Presence::Absent
    );
    assert_eq!(
        facts.presence(&Lookup::Head(missing.key())),
        Presence::NotLoaded
    );
    write_change(
        &db,
        &Change::ResourceLifecycle {
            installation: header.installation.clone(),
            record: lifecycle::Record {
                reference: missing,
                is_head: false,
                revoked: true,
                authority_revision: 11_u64.try_into()?,
            },
        },
    )
    .await?;
    assert!(
        load_command_in(&db, &header, &operator()?, &revoke, ROW_BYTES)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn activation_reads_only_current_pointer_and_creation_metadata() -> Result<()> {
    let db = database().await?;
    add(&db, 0).await?;
    let header = header()?;
    let tenant: TenantId = "tenant-0000".try_into()?;
    let revision = "configuration-v1".try_into()?;
    let mut route = reference("route-v1")?;
    route.resource_kind = ResourceKind::Route;
    let stored = StoredConfiguration {
        revision: ConfigurationRevision {
            tenant_id: tenant.clone(),
            revision_id: revision,
            schema_version: 1,
            compiler_version: "fixture/v1".into(),
            content_digest: "a".repeat(64).try_into()?,
            document: Configuration {
                routes: vec![route.clone()],
                policy: None,
                generation_defaults: GenerationDefaults {
                    max_output_tokens: 1024,
                    temperature: 0.5,
                },
                learning: Learning {
                    mode: LearningMode::Off,
                    max_evaluations: 0,
                },
                limits: Limits {
                    active_executions: 1,
                    queued_work: 0,
                    learning_jobs: 0,
                },
            },
        },
        compiled: serde_json::json!({"ciphertext": "fixture"}),
        dependencies: vec![route],
        creator: PrincipalId::for_control_credential("fixture")?,
        authority_revision: 7_u64.try_into()?,
        created_at: "2026-01-01T00:00:00Z".try_into()?,
    };
    configuration_store::insert_configuration(&db, &stored).await?;
    let command = Command::Activate(
        tenant.clone(),
        Activation {
            installation_id: header.installation.clone(),
            operation_id: "activate".try_into()?,
            revision_id: stored.revision.revision_id.clone(),
            expected_authority_revision: header.revision,
            expected_active_revision: None,
        },
    );
    let facts = load_command_in(&db, &header, &operator()?, &command, ROW_BYTES).await?;
    assert_eq!(
        facts.presence(&Lookup::ActiveConfiguration(tenant.clone())),
        Presence::Absent
    );
    configuration_store::set_active(
        &db,
        &tenant,
        None,
        &stored.revision.revision_id,
        header.revision,
    )
    .await?;
    let facts = load_command_in(&db, &header, &operator()?, &command, ROW_BYTES).await?;
    assert_eq!(
        facts.presence(&Lookup::ActiveConfiguration(tenant.clone())),
        Presence::Present
    );
    assert_eq!(
        facts.snapshot.active_configurations.get(&tenant),
        Some(&stored.revision.revision_id)
    );
    assert!(facts.snapshot.credentials.is_empty());
    assert_eq!(facts.snapshot.resource_lifecycle.len(), 0);
    let caller = Caller::Credential("credential-0000".try_into()?);
    let facts = load_caller_in(&db, &header, &caller, ROW_BYTES).await?;
    assert_eq!(
        facts.presence(&Lookup::ActiveConfiguration(tenant.clone())),
        Presence::NotLoaded
    );
    assert!(facts.snapshot.active_configurations.is_empty());
    let facts = load_tenant_in(&db, &header, &operator()?, &tenant, true, ROW_BYTES).await?;
    assert_eq!(facts.snapshot.tenants.len(), 1);
    assert!(facts.snapshot.credentials.is_empty());
    assert_eq!(
        facts.snapshot.active_configurations.get(&tenant),
        Some(&stored.revision.revision_id)
    );
    let tx = db.begin().await?;
    tx.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "UPDATE tenancy_configurations SET authority_revision=11".to_owned(),
    ))
    .await?;
    assert!(
        load_tenant_in(&tx, &header, &operator()?, &tenant, true, ROW_BYTES)
            .await
            .is_err()
    );
    // A tenant-only read does not inspect an unrelated active selection.
    assert!(
        load_tenant_in(&tx, &header, &operator()?, &tenant, false, ROW_BYTES)
            .await
            .is_ok()
    );
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn target_tenant_facts_preserve_caller_and_optional_active_absence() -> Result<()> {
    let db = database().await?;
    add(&db, 0).await?;
    add(&db, 1).await?;
    let header = header()?;
    let target: TenantId = "tenant-0001".try_into()?;
    let caller = Caller::Credential("credential-0000".try_into()?);
    let facts = load_tenant_in(&db, &header, &caller, &target, false, ROW_BYTES).await?;
    assert_eq!(facts.snapshot.credentials.len(), 1);
    assert_eq!(facts.snapshot.principals.len(), 1);
    assert_eq!(facts.snapshot.tenants.len(), 2);
    assert_eq!(
        facts.presence(&Lookup::Tenant(target.clone())),
        Presence::Present
    );
    assert_eq!(
        facts.presence(&Lookup::ActiveConfiguration(target.clone())),
        Presence::NotLoaded
    );
    let facts = load_tenant_in(&db, &header, &operator()?, &target, true, ROW_BYTES).await?;
    assert_eq!(facts.snapshot.tenants.len(), 1);
    assert!(facts.snapshot.credentials.is_empty());
    assert_eq!(
        facts.presence(&Lookup::ActiveConfiguration(target.clone())),
        Presence::Absent
    );
    let missing: TenantId = "absent-tenant".try_into()?;
    let facts = load_tenant_in(&db, &header, &operator()?, &missing, false, ROW_BYTES).await?;
    assert_eq!(facts.presence(&Lookup::Tenant(missing)), Presence::Absent);
    capacity(load_tenant_in(&db, &header, &caller, &target, true, 1).await)?;
    Ok(())
}
