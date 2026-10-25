use super::*;
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::{GenerationDefaults, Learning, LearningMode, Limits},
};
use sea_orm::{DatabaseConnection, Statement, TransactionTrait};
use sea_orm_migration::{MigratorTrait, SchemaManager};
use serde_json::json;

#[cfg(any(feature = "db-postgres", feature = "db-mysql"))]
#[path = "server_tests.rs"]
mod server;

async fn database() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    let manager = SchemaManager::new(&db);
    for migration in crate::db::migration::Migrator::migrations() {
        if migration.name().starts_with("m20240101_000028_")
            || migration.name().starts_with("m20240101_000029_")
        {
            migration.up(&manager).await?;
        }
    }
    for tenant in ["tenant-a", "tenant-b", "Tenant-a"] {
        add_tenant(&db, tenant).await?;
    }
    Ok(db)
}

async fn add_tenant(db: &impl ConnectionTrait, tenant: &str) -> Result<()> {
    db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO tenancy_tenants (id,revision,state,created_at) VALUES (?,'initial','active','2026-01-01T00:00:00Z')",
        [tenant.into()])).await?;
    Ok(())
}

fn artifact(owner: &str, revision: &str) -> Result<StoredResource> {
    Ok(StoredResource {
        reference: ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: owner.try_into()?,
            resource_kind: ResourceKind::Route,
            resource_id: "route".try_into()?,
            revision_id: revision.try_into()?,
        },
        compiler_version: "fixture/v1".into(),
        payload: json!({"version": 1, "ciphertext": "sealed-fixture-bytes"}),
        dependencies: json!([]),
        content_digest: "a".repeat(64).try_into()?,
        created_at: "2026-01-01T00:00:00Z".try_into()?,
    })
}

fn draft(tenant: &str, revision: &str, created: u64) -> Result<StoredConfiguration> {
    let route = artifact(tenant, "route-v1")?.reference;
    Ok(StoredConfiguration {
        revision: ConfigurationRevision {
            tenant_id: tenant.try_into()?,
            revision_id: revision.try_into()?,
            schema_version: 1,
            compiler_version: "fixture/v1".into(),
            content_digest: "b".repeat(64).try_into()?,
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
        compiled: json!({"version": 1, "ciphertext": "sealed-compiled-fixture"}),
        dependencies: vec![route],
        creator: PrincipalId::for_control_credential("fixture")?,
        authority_revision: created.try_into()?,
        created_at: "2026-01-01T00:00:00Z".try_into()?,
    })
}

fn conflict(result: Result<()>) -> Result<()> {
    let error = result
        .err()
        .context("conflicting mutation unexpectedly succeeded")?;
    ensure!(
        error.downcast_ref::<RevisionConflict>().is_some(),
        "wrong conflict classification"
    );
    Ok(())
}

#[tokio::test]
async fn resources_are_qualified_case_exact_immutable_and_canonical() -> Result<()> {
    let db = database().await?;
    let first = artifact("tenant-a", "v1")?;
    insert_resource(&db, &first).await?;
    let mut retry = first.clone();
    retry.payload = serde_json::from_str(r#"{"ciphertext":"sealed-fixture-bytes","version":1}"#)?;
    retry.created_at = "2026-02-01T00:00:00Z".try_into()?;
    insert_resource(&db, &retry).await?;
    let retained = resource(&db, &first.reference)
        .await?
        .context("resource missing")?;
    ensure!(retained.created_at == first.created_at);
    ensure!(retained.payload == first.payload);
    for field in ["payload", "dependencies", "compiler", "digest"] {
        let mut changed = first.clone();
        match field {
            "payload" => changed.payload["ciphertext"] = json!("different-sealed-bytes"),
            "dependencies" => {
                changed.dependencies = json!([artifact("tenant-a", "dep-v1")?.reference])
            }
            "compiler" => changed.compiler_version = "fixture/v2".into(),
            _ => changed.content_digest = "c".repeat(64).try_into()?,
        }
        conflict(insert_resource(&db, &changed).await)?;
    }
    let mut variants = vec![
        artifact("tenant-b", "v1")?,
        artifact("Tenant-a", "v1")?,
        artifact("tenant-a", "V1")?,
    ];
    let mut pool = first.clone();
    pool.reference.owner_kind = OwnerKind::OperatorPool;
    variants.push(pool);
    let mut policy = first.clone();
    policy.reference.resource_kind = ResourceKind::Policy;
    variants.push(policy);
    let mut route_case = first.clone();
    route_case.reference.resource_id = "Route".try_into()?;
    variants.push(route_case);
    for mut other in variants {
        // Equal content digests are not a cross-owner deduplication identity.
        other.payload = json!({"ciphertext": "another-sealed-artifact"});
        insert_resource(&db, &other).await?;
        ensure!(
            resource(&db, &other.reference)
                .await?
                .context("qualified resource missing")?
                .payload
                == other.payload
        );
    }
    ensure!(
        resource(&db, &artifact("ungranted-owner", "v1")?.reference)
            .await?
            .is_none()
    );
    ensure!(
        resource(&db, &first.reference)
            .await?
            .context("original missing")?
            .payload
            == first.payload
    );
    Ok(())
}

#[tokio::test]
async fn configuration_retains_document_order_closure_and_creation_fence() -> Result<()> {
    let db = database().await?;
    let mut first = draft("tenant-a", "v1", 7)?;
    let mut extra = first.dependencies[0].clone();
    extra.resource_id = "second-route".try_into()?;
    first.revision.document.routes.push(extra.clone());
    first.dependencies.push(extra);
    insert_configuration(&db, &first).await?;
    insert_configuration(&db, &first).await?;
    let loaded = configuration(&db, &first.revision.tenant_id, &first.revision.revision_id)
        .await?
        .context("configuration missing")?;
    ensure!(loaded.revision.document == first.revision.document);
    ensure!(loaded.dependencies == first.dependencies);
    ensure!(
        loaded.creator == first.creator && loaded.authority_revision == first.authority_revision
    );
    for field in [
        "document",
        "compiled",
        "dependencies",
        "creator",
        "authority",
        "timestamp",
    ] {
        let mut changed = first.clone();
        match field {
            "document" => changed.revision.document.routes.reverse(),
            "compiled" => changed.compiled["ciphertext"] = json!("changed-sealed-envelope"),
            "dependencies" => changed.dependencies.reverse(),
            "creator" => changed.creator = "other-principal".try_into()?,
            "authority" => changed.authority_revision = 8.try_into()?,
            _ => changed.created_at = "2026-01-02T00:00:00Z".try_into()?,
        }
        conflict(insert_configuration(&db, &changed).await)?;
    }
    for tenant in ["tenant-b", "Tenant-a"] {
        let other = draft(tenant, "v1", 9)?;
        ensure!(
            configuration(&db, &other.revision.tenant_id, &other.revision.revision_id)
                .await?
                .is_none()
        );
        insert_configuration(&db, &other).await?;
        ensure!(
            configuration(&db, &other.revision.tenant_id, &other.revision.revision_id)
                .await?
                .context("other tenant configuration missing")?
                .authority_revision
                == other.authority_revision
        );
    }
    Ok(())
}

#[tokio::test]
async fn caller_transaction_atomically_rolls_back_artifacts_drafts_and_activation() -> Result<()> {
    let db = database().await?;
    let artifact = artifact("tenant-a", "v1")?;
    let first = draft("tenant-a", "v1", 7)?;
    let tx = db.begin().await?;
    insert_resource(&tx, &artifact).await?;
    insert_configuration(&tx, &first).await?;
    set_active(
        &tx,
        &first.revision.tenant_id,
        None,
        &first.revision.revision_id,
        8.try_into()?,
    )
    .await?;
    // Absence CAS rejects even an identical pointer, without poisoning the
    // transaction or changing the stored authority revision.
    for proposed in [8_u64, 9] {
        conflict(
            set_active(
                &tx,
                &first.revision.tenant_id,
                None,
                &first.revision.revision_id,
                proposed.try_into()?,
            )
            .await,
        )?;
        let pointers = active_revisions(&tx).await?;
        let pointer = pointers
            .first()
            .context("active pointer missing after rejected CAS")?;
        ensure!(
            pointer.revision == first.revision.revision_id && pointer.authority_revision.get() == 8
        );
    }
    // A subsequent write in the same transaction must still succeed.
    set_active(
        &tx,
        &first.revision.tenant_id,
        Some(&first.revision.revision_id),
        &first.revision.revision_id,
        9.try_into()?,
    )
    .await?;
    ensure!(active_revisions(&tx).await?.len() == 1);
    tx.rollback().await?;
    ensure!(resource(&db, &artifact.reference).await?.is_none());
    ensure!(
        configuration(&db, &first.revision.tenant_id, &first.revision.revision_id)
            .await?
            .is_none()
    );
    ensure!(active_revisions(&db).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn activation_is_same_tenant_cas_and_monotonic_in_authority() -> Result<()> {
    let db = database().await?;
    let first = draft("tenant-a", "v1", 7)?;
    let next = draft("tenant-a", "v2", 9)?;
    let foreign = draft("tenant-b", "foreign-only", 8)?;
    for record in [&first, &next, &foreign] {
        insert_configuration(&db, record).await?;
    }
    let tenant = &first.revision.tenant_id;
    let v1 = &first.revision.revision_id;
    let v2 = &next.revision.revision_id;
    ensure!(
        set_active(&db, tenant, None, v1, 6.try_into()?)
            .await
            .is_err()
    );
    ensure!(
        set_active(
            &db,
            tenant,
            None,
            &foreign.revision.revision_id,
            8.try_into()?
        )
        .await
        .is_err()
    );
    set_active(&db, tenant, None, v1, 8.try_into()?).await?;
    conflict(set_active(&db, tenant, None, v2, 10.try_into()?).await)?;
    conflict(set_active(&db, tenant, Some(v2), v2, 10.try_into()?).await)?;
    conflict(set_active(&db, tenant, Some(v1), v1, 8.try_into()?).await)?;
    set_active(&db, tenant, Some(v1), v2, 10.try_into()?).await?;
    set_active(&db, tenant, Some(v2), v1, 11.try_into()?).await?;
    ensure!(
        active_revisions(&db).await?
            == vec![StoredActiveConfiguration {
                tenant: tenant.clone(),
                revision: v1.clone(),
                authority_revision: 11.try_into()?,
            }]
    );
    // A corrupted pointer must fail startup even when foreign-key checks were bypassed.
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "UPDATE tenancy_active_configurations SET authority_revision=1".to_owned(),
    ))
    .await?;
    ensure!(active_revisions(&db).await.is_err());
    Ok(())
}

#[tokio::test]
async fn storage_bounds_apply_before_insert_and_to_corrupted_database_reads() -> Result<()> {
    let db = database().await?;
    let mut artifact = artifact("tenant-a", "v1")?;
    artifact.payload = json!({"ciphertext": "x".repeat(70_000)});
    insert_resource(&db, &artifact).await?;
    ensure!(
        resource(&db, &artifact.reference)
            .await?
            .context("large artifact missing")?
            .payload
            == artifact.payload
    );
    let mut oversized = artifact.clone();
    oversized.payload = json!("x".repeat(MAX_ENVELOPE_BYTES));
    ensure!(insert_resource(&db, &oversized).await.is_err());
    let mut record = draft("tenant-a", "v1", 1)?;
    record.compiled = json!({"ciphertext": "x".repeat(70_000)});
    insert_configuration(&db, &record).await?;
    record.dependencies.clear();
    ensure!(insert_configuration(&db, &record).await.is_err());
    record = draft("tenant-a", "bounded", 1)?;
    let additional = (1..MAX_DEPENDENCIES)
        .map(|index| {
            let mut reference = artifact.reference.clone();
            reference.resource_id = format!("dependency-{index}").try_into()?;
            Ok(reference)
        })
        .collect::<Result<Vec<_>>>()?;
    record.dependencies.extend(additional);
    insert_configuration(&db, &record).await?;
    record.revision.revision_id = "too-many-dependencies".try_into()?;
    let mut extra = artifact.reference.clone();
    extra.resource_id = "extra-dependency".try_into()?;
    record.dependencies.push(extra);
    ensure!(insert_configuration(&db, &record).await.is_err());
    record.revision.revision_id = "bounded".try_into()?;
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE tenancy_configurations SET compiled_json=?",
        [json!("é".repeat(MAX_ENVELOPE_BYTES / 2)).to_string().into()],
    ))
    .await?;
    ensure!(
        configuration(
            &db,
            &record.revision.tenant_id,
            &record.revision.revision_id
        )
        .await
        .is_err()
    );
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE tenancy_resource_revisions SET payload_json=?",
        [json!("é".repeat(MAX_ENVELOPE_BYTES / 2)).to_string().into()],
    ))
    .await?;
    ensure!(resource(&db, &artifact.reference).await.is_err());
    // The SQL projection itself returns NULL, without transferring the huge cell.
    let projected = resource_row(&db, &artifact.reference, false)
        .await?
        .context("row missing")?;
    ensure!(
        projected
            .try_get::<Option<String>>("", "payload_json")?
            .is_none()
    );
    let projected = configuration_row(
        &db,
        &record.revision.tenant_id,
        &record.revision.revision_id,
        false,
    )
    .await?
    .context("configuration row missing")?;
    ensure!(
        projected
            .try_get::<Option<String>>("", "compiled_json")?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn invalid_retained_schema_and_dependency_metadata_fail_closed() -> Result<()> {
    let db = database().await?;
    let record = draft("tenant-a", "v1", 1)?;
    insert_configuration(&db, &record).await?;
    for schema_version in [-1, 2] {
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE tenancy_configurations SET schema_version=?",
            [schema_version.into()],
        ))
        .await?;
        ensure!(
            configuration(
                &db,
                &record.revision.tenant_id,
                &record.revision.revision_id
            )
            .await
            .is_err()
        );
    }
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "UPDATE tenancy_configurations SET schema_version=1, dependencies_json='[]'".to_owned(),
    ))
    .await?;
    ensure!(
        configuration(
            &db,
            &record.revision.tenant_id,
            &record.revision.revision_id
        )
        .await
        .is_err()
    );
    let mut artifact = artifact("tenant-a", "v1")?;
    artifact.dependencies = json!({"api_key": "must-not-be-persisted"});
    ensure!(insert_resource(&db, &artifact).await.is_err());
    ensure!(resource(&db, &artifact.reference).await?.is_none());
    let mut nested = JsonValue::Null;
    for _ in 0..=MAX_JSON_DEPTH {
        nested = json!([nested]);
    }
    artifact.dependencies = json!([]);
    artifact.payload = nested;
    ensure!(insert_resource(&db, &artifact).await.is_err());
    Ok(())
}

#[tokio::test]
async fn sqlite_numeric_affinity_cannot_bypass_bounded_projection() -> Result<()> {
    let db = database().await?;
    let record = draft("tenant-a", "v1", 1)?;
    insert_configuration(&db, &record).await?;
    set_active(
        &db,
        &record.revision.tenant_id,
        None,
        &record.revision.revision_id,
        2.try_into()?,
    )
    .await?;
    for column in ["schema_version", "authority_revision"] {
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            format!(
                "UPDATE tenancy_configurations SET {column}=zeroblob({})",
                MAX_ENVELOPE_BYTES + 1
            ),
        ))
        .await?;
        let row = configuration_row(
            &db,
            &record.revision.tenant_id,
            &record.revision.revision_id,
            false,
        )
        .await?
        .context("configuration row missing")?;
        ensure!(row.try_get::<Option<i64>>("", column)?.is_none());
        ensure!(
            configuration(
                &db,
                &record.revision.tenant_id,
                &record.revision.revision_id
            )
            .await
            .is_err()
        );
        if column == "authority_revision" {
            ensure!(
                creation_revision(
                    &db,
                    &record.revision.tenant_id,
                    &record.revision.revision_id
                )
                .await
                .is_err()
            );
        }
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            format!("UPDATE tenancy_configurations SET {column}=1"),
        ))
        .await?;
    }
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        format!(
            "UPDATE tenancy_active_configurations SET authority_revision=zeroblob({})",
            MAX_ENVELOPE_BYTES + 1
        ),
    ))
    .await?;
    ensure!(active_revisions(&db).await.is_err());
    Ok(())
}

#[tokio::test]
async fn active_pointer_loading_never_silently_truncates() -> Result<()> {
    let db = database().await?;
    for index in 0..=MAX_ACTIVE {
        let tenant = format!("bounded-tenant-{index}");
        add_tenant(&db, &tenant).await?;
        let record = draft(&tenant, "v1", 1)?;
        insert_configuration(&db, &record).await?;
        set_active(
            &db,
            &record.revision.tenant_id,
            None,
            &record.revision.revision_id,
            2.try_into()?,
        )
        .await?;
        if index + 1 == MAX_ACTIVE {
            ensure!(active_revisions(&db).await?.len() == MAX_ACTIVE);
        }
    }
    ensure!(active_revisions(&db).await.is_err());
    Ok(())
}
