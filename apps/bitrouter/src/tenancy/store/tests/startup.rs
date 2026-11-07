use super::*;
use crate::tenancy::store::startup::{self as loader, Budget, Format};
use bitrouter_ai::tenancy::operation::OperationStatus;

fn budget(rows: usize) -> Budget {
    Budget {
        rows,
        bytes: 64 * 1024 * 1024,
    }
}

async fn seed(db: &DatabaseConnection) -> Result<()> {
    let mut header = header()?;
    header.operation_ids = 1;
    let operation_id: OperationId = "seed".try_into()?;
    let result = OperationResult {
        operation_id: operation_id.clone(),
        status: OperationStatus::Applied,
        authority_revision: Some(header.revision),
        resource_revision: Some("r1".try_into()?),
        resource_id: Some("fixture".try_into()?),
        error: None,
    };
    let operation = Operation {
        principal: PrincipalId::for_control_credential("fixture")?,
        id: operation_id,
        context: ReadContext::Installation,
        digest: "a".repeat(64),
        result: result.clone(),
        response: StoredResponse::Operation(result),
        created_at: Utc::now(),
        safety: false,
    };
    let tx = db.begin().await?;
    installation(&tx, &Snapshot::from_header(&header)).await?;
    write_in(&tx, &Snapshot::from_header(&header), &[], &operation).await?;
    tx.commit().await?;
    sql(db, "UPDATE tenancy_installation SET schema_version=1").await?;
    Ok(())
}

async fn sql(db: &impl ConnectionTrait, statement: &str) -> Result<()> {
    db.execute(Statement::from_string(
        db.get_database_backend(),
        statement.to_owned(),
    ))
    .await?;
    Ok(())
}

#[tokio::test]
async fn header_loader_validates_all_pages_and_returns_no_directory() -> Result<()> {
    let db = database().await?;
    assert!(loader::load_header_in(&db, budget(3)).await?.is_none());
    add(&db, 0).await?;
    assert!(loader::load_header_in(&db, budget(3)).await.is_err());
    seed(&db).await?;
    for index in 1..300 {
        add(&db, index).await?;
    }
    let (loaded, totals) = loader::load_header_observed_in(&db, budget(7))
        .await?
        .context("missing header")?;
    assert_eq!(
        totals,
        DirectoryTotals {
            tenants: 300,
            principals: 300,
            bindings: 300,
            credentials: 300
        }
    );
    assert_eq!(loaded.format, Format::V1);
    assert_eq!(loaded.header.installation, header()?.installation);
    assert_eq!(loaded.header.operation_ids, 1);
    let facts = Snapshot::from_header(&loaded.header);
    assert!(
        facts.tenants.is_empty() && facts.credentials.is_empty() && facts.principals.is_empty()
    );
    assert!(load_existing(&db, Utc::now()).await?.credentials.is_empty());
    sql(
        &db,
        "UPDATE tenancy_credentials SET expires_at='invalid' WHERE id='credential-0299'",
    )
    .await?;
    // The final corrupt page must prevent a complete summary, not return the
    // earlier valid directory totals as a completed startup observation.
    assert!(
        loader::load_header_observed_in(&db, budget(7))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn format_upgrade_is_explicit_transactional_and_preserves_stop() -> Result<()> {
    let db = database().await?;
    seed(&db).await?;
    sql(&db, "UPDATE tenancy_installation SET saturated=1").await?;
    let loaded = loader::load_header_in(&db, budget(3))
        .await?
        .context("missing header")?;
    assert!(loaded.header.saturated);
    assert_eq!(loaded.format, Format::V1);
    let tx = db.begin().await?;
    let validated = loader::load_header_in(&tx, budget(3))
        .await?
        .context("missing header")?;
    loader::upgrade_format_in(&tx, &validated).await?;
    assert_eq!(
        loader::read_header_in(&tx, ROW_BYTES)
            .await?
            .context("missing header")?
            .format,
        Format::V5
    );
    tx.rollback().await?;
    assert_eq!(
        loader::read_header_in(&db, ROW_BYTES)
            .await?
            .context("missing header")?
            .format,
        Format::V1
    );
    let tx = db.begin().await?;
    let validated = loader::load_header_in(&tx, budget(3))
        .await?
        .context("missing header")?;
    loader::upgrade_format_in(&tx, &validated).await?;
    tx.commit().await?;
    let mut loaded = loader::load_header_in(&db, budget(3))
        .await?
        .context("missing header")?;
    assert_eq!(loaded.format, Format::V5);
    assert!(loaded.header.saturated);
    // The new reader accepts v2 without rebuilding the retained directory.
    assert!(load_existing(&db, Utc::now()).await?.credentials.is_empty());
    let tx = db.begin().await?;
    loader::write_header_in(&tx, &loaded).await?;
    loaded.header.saturated = false;
    assert!(loader::write_header_in(&tx, &loaded).await.is_err());
    loaded.header.saturated = true;
    loaded.format = Format::V1;
    assert!(loader::write_header_in(&tx, &loaded).await.is_err());
    loaded.format = Format::V5;
    loaded.header.operation_ids = u64::from(u32::MAX) + 1;
    loader::write_header_in(&tx, &loaded).await?;
    assert_eq!(
        loader::read_header_in(&tx, ROW_BYTES)
            .await?
            .context("missing header")?
            .header
            .operation_ids,
        u64::from(u32::MAX) + 1
    );
    loaded.header.operation_ids = u64::MAX;
    assert!(loader::write_header_in(&tx, &loaded).await.is_err());
    tx.rollback().await?;
    assert!(
        loader::read_header_in(&db, ROW_BYTES)
            .await?
            .context("missing header")?
            .header
            .saturated
    );
    Ok(())
}

#[tokio::test]
async fn operation_history_crosses_100000_and_validates_the_last_row() -> Result<()> {
    let db = database().await?;
    seed(&db).await?;
    let tx = db.begin().await?;
    let validated = loader::load_header_in(&tx, budget(3))
        .await?
        .context("missing header")?;
    loader::upgrade_format_in(&tx, &validated).await?;
    // Accelerated durable fixture: all original indexed identities and complete
    // response bodies remain, while each operation gets its own stable ID.
    sql(
        &tx,
        r#"WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<100000)
        INSERT INTO tenancy_operations
        SELECT installation_id,principal_id,printf('op-%06d',n),tenant_id,authority_class,
        read_context_json,hash_version,command_digest,
        replace(result_json,'"seed"','"'||printf('op-%06d',n)||'"'),
        replace(response_json,'"seed"','"'||printf('op-%06d',n)||'"'),created_at,safety
        FROM tenancy_operations CROSS JOIN numbers WHERE operation_id='seed'"#,
    )
    .await?;
    sql(&tx, "UPDATE tenancy_installation SET operation_ids=100001").await?;
    tx.commit().await?;
    let loaded = loader::load_header_in(&db, budget(127))
        .await?
        .context("missing header")?;
    assert_eq!(loaded.header.operation_ids, 100001);
    assert!(!loaded.header.saturated);
    assert_eq!(loaded.format, Format::V5);
    let retained = operation(
        &db,
        &loaded.header.installation,
        &PrincipalId::for_control_credential("fixture")?,
        &"op-000001".try_into()?,
    )
    .await?
    .context("lost operation")?;
    assert_eq!(retained.digest, "a".repeat(64));
    // 'seed' sorts after every op-* row: a prefix-only validation would miss it.
    sql(
        &db,
        "UPDATE tenancy_operations SET hash_version=2 WHERE operation_id='seed'",
    )
    .await?;
    assert!(loader::load_header_in(&db, budget(127)).await.is_err());
    Ok(())
}

#[tokio::test]
async fn header_projection_has_exact_byte_boundary_and_rejects_bad_format() -> Result<()> {
    let db = database().await?;
    seed(&db).await?;
    let header = header()?;
    let bytes = header.installation.as_str().len() + header.created_at.len() + 6 * 8;
    capacity(loader::read_header_in(&db, bytes - 1).await)?;
    for limit in [bytes, bytes + 1] {
        assert!(loader::read_header_in(&db, limit).await?.is_some());
    }
    capacity(loader::load_header_in(&db, Budget { rows: 1, bytes: 1 }).await)?;
    sql(
        &db,
        "UPDATE tenancy_installation SET schema_version=2,operation_ids=4294967296, safety_operation_ids=2147483648",
    )
    .await?;
    let loaded = loader::read_header_in(&db, ROW_BYTES)
        .await?
        .context("missing header")?;
    assert_eq!(loaded.header.operation_ids, 4294967296);
    assert_eq!(loaded.header.safety_operation_ids, 2147483648);
    assert!(loader::load_header_in(&db, budget(3)).await.is_err());
    sql(&db, "UPDATE tenancy_installation SET operation_ids=-1").await?;
    assert!(loader::read_header_in(&db, ROW_BYTES).await.is_err());
    sql(
        &db,
        "UPDATE tenancy_installation SET operation_ids=1,safety_operation_ids=0,schema_version=6",
    )
    .await?;
    assert!(loader::load_header_in(&db, budget(3)).await.is_err());
    sql(
        &db,
        "UPDATE tenancy_installation SET schema_version=1,created_at=zeroblob(1000000)",
    )
    .await?;
    let error = loader::read_header_in(&db, bytes)
        .await
        .err()
        .context("malformed cell accepted")?;
    assert!(!error.is::<CapacityExceeded>());
    Ok(())
}

#[tokio::test]
async fn v1_header_enforces_historical_counts_and_missing_latch_only() -> Result<()> {
    let db = database().await?;
    seed(&db).await?;
    for (ordinary, safety, saturated, valid) in [
        (99_487, 511, false, true),
        (99_488, 512, true, true),
        (99_488, 512, false, false),
        (99_489, 512, true, false),
        (99_487, 513, true, false),
        // A different stop cause remains readable but cannot qualify for reset.
        (1, 0, true, true),
    ] {
        sql(&db, &format!("UPDATE tenancy_installation SET operation_ids={ordinary},safety_operation_ids={safety},saturated={}", i32::from(saturated))).await?;
        assert_eq!(loader::read_header_in(&db, ROW_BYTES).await.is_ok(), valid);
    }
    sql(&db, "UPDATE tenancy_installation SET schema_version=2,operation_ids=100001,safety_operation_ids=513,saturated=0").await?;
    assert!(loader::read_header_in(&db, ROW_BYTES).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn full_object_allowance_is_independent_of_key_page_width() -> Result<()> {
    use crate::tenancy::configuration_store::{
        MAX_DOCUMENT_BYTES, MAX_ENVELOPE_BYTES, StoredResource,
    };
    use bitrouter_ai::tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::{
            Configuration, ConfigurationRevision, GenerationDefaults, Learning, LearningMode,
            Limits, ResourceRef,
        },
    };
    let db = database().await?;
    seed(&db).await?;
    add(&db, 0).await?;
    let reference = ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: "tenant-0000".try_into()?,
        resource_kind: ResourceKind::Route,
        resource_id: "route".try_into()?,
        revision_id: "r1".try_into()?,
    };
    let resource = StoredResource {
        reference: reference.clone(),
        compiler_version: "fixture/v1".into(),
        payload: serde_json::Value::String("x".repeat(MAX_ENVELOPE_BYTES - 2)),
        dependencies: serde_json::json!([]),
        content_digest: "a".repeat(64).try_into()?,
        created_at: "2026-01-01T00:00:00Z".try_into()?,
    };
    configuration_store::insert_resource(&db, &resource).await?;
    let configuration = StoredConfiguration {
        revision: ConfigurationRevision {
            tenant_id: "tenant-0000".try_into()?,
            revision_id: "configuration-v1".try_into()?,
            schema_version: 1,
            document: Configuration {
                routes: vec![reference.clone()],
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
            compiler_version: "fixture/v1".into(),
            content_digest: "b".repeat(64).try_into()?,
        },
        compiled: resource.payload.clone(),
        dependencies: vec![reference],
        creator: PrincipalId::for_control_credential("fixture")?,
        authority_revision: header()?.revision,
        created_at: "2026-01-01T00:00:00Z".try_into()?,
    };
    configuration_store::insert_configuration(&db, &configuration).await?;
    drop(configuration);
    drop(resource);
    // Legal JSON whitespace fills the document's encoded limit without changing
    // its meaning; the canonical decoder still validates the complete document.
    sql(&db, &format!("UPDATE tenancy_configurations SET document_json=printf('%{MAX_DOCUMENT_BYTES}s',document_json)")).await?;
    for rows in [1, 64, 128] {
        assert!(loader::load_header_in(&db, budget(rows)).await?.is_some());
    }
    capacity(
        loader::load_header_in(
            &db,
            Budget {
                rows: 1,
                bytes: 32 * 1024 * 1024,
            },
        )
        .await,
    )?;
    // Encoded size alone cannot bound a dense JSON tree's allocation.
    sql(&db, "UPDATE tenancy_resource_revisions SET payload_json='['||replace(printf('%200000s',''),' ','0,')||'0]'").await?;
    capacity(loader::load_header_in(&db, budget(64)).await)?;
    Ok(())
}

#[tokio::test]
async fn operation_corruption_and_operator_budget_refusal_are_distinct() -> Result<()> {
    let db = database().await?;
    seed(&db).await?;
    let tx = db.begin().await?;
    sql(
        &tx,
        "UPDATE tenancy_operations SET response_json=printf('%100000s','x')",
    )
    .await?;
    capacity(
        loader::load_header_in(
            &tx,
            Budget {
                rows: 1,
                bytes: 128 * 1024,
            },
        )
        .await,
    )?;
    tx.rollback().await?;
    for change in [
        "UPDATE tenancy_operations SET command_digest='invalid'",
        "UPDATE tenancy_operations SET authority_class='tenant'",
        "UPDATE tenancy_operations SET installation_id='foreign'",
        "UPDATE tenancy_operations SET operation_id='different'",
        "UPDATE tenancy_operations SET safety=1",
        "UPDATE tenancy_operations SET safety=2",
        "UPDATE tenancy_operations SET response_json='{}'",
    ] {
        let tx = db.begin().await?;
        sql(&tx, change).await?;
        let error = loader::load_header_in(&tx, budget(3))
            .await
            .err()
            .context("corruption accepted")?;
        assert!(!error.is::<CapacityExceeded>());
        tx.rollback().await?;
    }
    assert!(loader::load_header_in(&db, budget(1)).await?.is_some());
    Ok(())
}
