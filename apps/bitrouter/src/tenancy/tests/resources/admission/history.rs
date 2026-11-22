use super::*;
use crate::tenancy::{actor::AuthoritySubject, resources::lifecycle};
use sea_orm::TransactionTrait;

async fn count(fixture: &Fixture, table: &str) -> Result<i64> {
    fixture
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            format!("SELECT COUNT(*) AS n FROM {table}"),
        ))
        .await?
        .context("count")?
        .try_get("", "n")
        .map_err(Into::into)
}

async fn only_witness(fixture: &Fixture) -> Result<(String, String)> {
    ensure!(count(fixture, "tenancy_admission_index").await? == 1);
    let row = fixture
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT index_kind, reference_json FROM tenancy_admission_index LIMIT 1",
        ))
        .await?
        .context("remaining witness")?;
    Ok((
        row.try_get("", "index_kind")?,
        row.try_get("", "reference_json")?,
    ))
}

#[tokio::test]
async fn admission_rotation_refuses_to_recreate_a_missing_witness() -> Result<()> {
    for kind in ["resource", "health"] {
        let fixture = keyed_fixture().await?;
        let mut original = profile(&fixture, "initial-profile", None, 1)?;
        let Definition::AdmissionProfile { profile } = &mut original.definition else {
            anyhow::bail!("profile fixture");
        };
        profile.health_group_id = Some("circuit".try_into()?);
        profile.health = Some(serde_json::from_value(json!({
            "rules": "availability_v1",
            "failure_threshold": 3,
            "transient_cooldown_seconds": 10,
            "persistent_cooldown_seconds": 30,
            "probe_lease_seconds": 5
        }))?);
        let applied = put(&fixture, fixture.operator.clone(), original.clone()).await?;
        ensure!(count(&fixture, "tenancy_admission_index").await? == 2);
        let before = lifecycle::head(&fixture.db, &fixture.installation, &original.key)
            .await?
            .context("initial head")?;
        ensure!(before.reference == reference(&original, &applied)?);
        let operations = count(&fixture, "tenancy_operations").await?;
        let revisions = count(&fixture, "tenancy_resource_revisions").await?;
        fixture
            .db
            .execute_unprepared(&format!(
                "DELETE FROM tenancy_admission_index WHERE index_kind='{kind}'"
            ))
            .await?;
        let remaining = only_witness(&fixture).await?;

        let mut next = original;
        next.operation_id = "rotate-profile".try_into()?;
        next.expected_revision = Some(before.reference.revision_id.clone());
        let Definition::AdmissionProfile { profile } = &mut next.definition else {
            anyhow::bail!("rotation fixture");
        };
        profile.rate.as_mut().context("rate")?.member_rpm = 7;
        ensure!(
            matches!(
                fixture.service.execute(fixture.operator.clone(),
                    Command::Resource(fixture.tenant.clone(), next)).await,
                Err(error) if error.0.code == ErrorCode::AuthorityUnavailable
            ),
            "rotation with missing {kind} witness"
        );
        let after = lifecycle::head(&fixture.db, &fixture.installation, &before.reference.key())
            .await?
            .context("head after failed rotation")?;
        ensure!(
            after.reference == before.reference
                && after.authority_revision == before.authority_revision
        );
        ensure!(count(&fixture, "tenancy_operations").await? == operations);
        ensure!(count(&fixture, "tenancy_resource_revisions").await? == revisions);
        ensure!(only_witness(&fixture).await? == remaining);
        fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn admission_startup_rejects_an_orphan_witness_in_an_empty_catalog() -> Result<()> {
    let recording = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut db = crate::db::connect("sqlite::memory:").await?;
    let enabled = recording.clone();
    let observed = writes.clone();
    db.set_metric_callback(move |info| {
        use std::sync::atomic::Ordering::Relaxed;
        if enabled.load(Relaxed)
            && matches!(
                info.statement.sql.split_whitespace().next(),
                Some("INSERT" | "UPDATE" | "DELETE" | "REPLACE")
            )
        {
            observed.fetch_add(1, Relaxed);
        }
    });
    let mut fixture = Fixture::with_database(db, &mut |_| {}).await?;
    restart(&mut fixture).await?;
    ensure!(count(&fixture, "tenancy_installation").await? == 1);
    ensure!(count(&fixture, "tenancy_resource_revisions").await? == 0);
    let reference = ResourceRef {
        owner_kind: OwnerKind::OperatorPool,
        owner_id: "pool".try_into()?,
        resource_kind: ResourceKind::Policy,
        resource_id: "orphan".try_into()?,
        revision_id: "v1".try_into()?,
    };
    fixture
        .db
        .execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO tenancy_admission_index
         (index_kind, owner_kind, owner_id, resource_kind, resource_id, reference_json)
         VALUES ('resource', 'operator_pool', 'pool', 'policy', 'orphan', ?)",
            [serde_json::to_string(&reference)?.into()],
        ))
        .await?;
    let orphan = only_witness(&fixture).await?;
    recording.store(true, std::sync::atomic::Ordering::Relaxed);
    ensure!(restart(&mut fixture).await.is_err());
    ensure!(writes.load(std::sync::atomic::Ordering::Relaxed) == 0);
    ensure!(count(&fixture, "tenancy_resource_revisions").await? == 0);
    ensure!(only_witness(&fixture).await? == orphan);
    Ok(())
}

#[tokio::test]
async fn admission_history_4095_4096_4097_publish_replay_and_failed_commit() -> Result<()> {
    let observed = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recording = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut db = crate::db::connect("sqlite::memory:").await?;
    let queries = observed.clone();
    let enabled = recording.clone();
    db.set_metric_callback(move |info| {
        if enabled.load(std::sync::atomic::Ordering::Relaxed)
            && let Ok(mut queries) = queries.lock()
        {
            queries.push(info.statement.sql.clone());
        }
    });
    let mut fixture = Fixture::with_database(db, &mut |_| {}).await?;
    restart(&mut fixture).await?;
    fixture.service.shutdown().await?;
    let tx = fixture.db.begin().await?;
    let (head, historical_route) = fixture
        .service
        .resources
        .seed_admission_history_for_test(&tx, 3647, 448)
        .await?;
    tx.commit().await?;
    ensure!(count(&fixture, "tenancy_resource_revisions").await? == 4096); // 4095 facts + credential
    ensure!(count(&fixture, "tenancy_admission_index").await? == 2);
    restart(&mut fixture).await?;

    let mut pinned = head.clone();
    pinned.revision_id = "v1".try_into()?;
    let identity = fixture
        .service
        .authority_read(AuthoritySubject::Profiles {
            caller: fixture.operator.clone(),
            references: vec![pinned.clone(), historical_route.clone()],
        })
        .await
        .map_err(failure)?;
    let mut slot = pinned.clone();
    slot.resource_kind = ResourceKind::Credential;
    slot.resource_id = "credential".try_into()?;
    let mut route = route_input(&fixture, &slot)?;
    route.operation_id = "fact-4096".try_into()?;
    route.key = historical_route.key();
    route.expected_revision = Some(historical_route.revision_id.clone());
    let Definition::Route {
        routing,
        selector,
        policy,
        credentials,
    } = route.definition
    else {
        anyhow::bail!("route fixture");
    };
    route.definition = Definition::ProfiledRoute {
        routing,
        selector,
        policy,
        credentials,
        attachments: [
            GenerationProtocol::ChatCompletions,
            GenerationProtocol::Messages,
            GenerationProtocol::Responses,
        ]
        .into_iter()
        .map(|inbound_protocol| AdmissionAttachment {
            inbound_protocol,
            candidate_index: 0,
            credential: slot.clone(),
            profile: pinned.clone(),
        })
        .collect(),
    };
    put(&fixture, fixture.operator.clone(), route).await?;
    ensure!(count(&fixture, "tenancy_resource_revisions").await? == 4097); // 3647 + 449 + credential

    let next = profile(&fixture, "fact-4097", Some(head.revision_id.clone()), 7)?;
    let revisions = count(&fixture, "tenancy_resource_revisions").await?;
    fixture.db.execute_unprepared("CREATE TRIGGER refuse_admission_operation BEFORE INSERT ON tenancy_operations WHEN NEW.operation_id='fact-4097' BEGIN SELECT RAISE(ABORT,'injected commit rollback'); END").await?;
    ensure!(
        put(&fixture, fixture.operator.clone(), next.clone())
            .await
            .is_err()
    );
    ensure!(count(&fixture, "tenancy_resource_revisions").await? == revisions);
    ensure!(count(&fixture, "tenancy_admission_index").await? == 2);
    ensure!(
        lifecycle::head(&fixture.db, &fixture.installation, &head.key())
            .await?
            .context("head")?
            .reference
            == head
    );
    fixture
        .db
        .execute_unprepared("DROP TRIGGER refuse_admission_operation")
        .await?;
    drop(identity);
    restart(&mut fixture).await?;
    let identity = fixture
        .service
        .authority_read(AuthoritySubject::Profiles {
            caller: fixture.operator.clone(),
            references: vec![pinned.clone(), historical_route.clone()],
        })
        .await
        .map_err(failure)?;
    recording.store(true, std::sync::atomic::Ordering::Relaxed);
    let applied = put(&fixture, fixture.operator.clone(), next.clone()).await?;
    let replay = put(&fixture, fixture.operator.clone(), next).await?;
    recording.store(false, std::sync::atomic::Ordering::Relaxed);
    ensure!(
        applied.resource_revision == replay.resource_revision
            && applied.authority_revision == replay.authority_revision
    );
    ensure!(count(&fixture, "tenancy_resource_revisions").await? == revisions + 1);
    ensure!(count(&fixture, "tenancy_admission_index").await? == 2);
    fixture
        .service
        .authority
        .read()
        .map_err(|code| failure(code.into()))?
        .check(&identity.lease, |view| {
            let state = view.facts();
            if !state.resource_admission.protected(&historical_route)
                || state
                    .resource_admission
                    .current_profile(&pinned)
                    .map(|(r, _)| r.revision_id)
                    != applied.resource_revision
            {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            Ok(())
        })
        .map_err(|code| failure(code.into()))?;
    {
        let queries = observed
            .lock()
            .map_err(|_| anyhow::anyhow!("SQL observation poisoned"))?;
        ensure!(
            queries.len() < 100,
            "ordinary publish/replay queried history: {} statements",
            queries.len()
        );
        ensure!(
            !queries
                .iter()
                .any(|sql| sql.contains("ORDER BY") && sql.contains("tenancy_resource_revisions"))
        );
    }
    drop(identity);
    restart(&mut fixture).await?;
    let identity = fixture
        .service
        .authority_read(AuthoritySubject::Profiles {
            caller: fixture.operator.clone(),
            references: vec![pinned.clone(), historical_route.clone()],
        })
        .await
        .map_err(failure)?;
    fixture
        .service
        .authority
        .read()
        .map_err(|code| failure(code.into()))?
        .check(&identity.lease, |view| {
            view.facts()
                .resource_admission
                .configuration(std::slice::from_ref(&historical_route))
        })
        .map_err(|code| failure(code.into()))?;
    drop(identity);
    fixture.service.shutdown().await?;
    Ok(())
}
