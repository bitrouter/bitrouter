use super::*;
use bitrouter_ai::{
    eval::EvalSubject,
    tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::ResourceRef,
        execution::{
            Attempt, Dispatch, ExecutionReceipt, Outcome, Provenance, ReceiptState, Target, Usage,
        },
        operation::DispatchState,
    },
};

pub(super) fn reference(fixture: &Fixture, id: &str) -> Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: fixture.tenant.as_str().try_into()?,
        resource_kind: ResourceKind::Credential,
        resource_id: id.try_into()?,
        revision_id: "v1".try_into()?,
    })
}

async fn resources(fixture: &Fixture, rows: usize) -> Result<()> {
    // Use bounded import batches, independently of total retained cardinality.
    for start in (0..rows).step_by(128) {
        let batch = (start..rows.min(start + 128)).map(|index| -> Result<_> {
            Ok(serde_json::json!({
                "reference":reference(fixture, &format!("retained-slot-{index:05}"))?,
                "definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":format!("fixture-secret-{index}")}}}
            }))
        }).collect::<Result<Vec<_>>>()?;
        fixture
            .service
            .resources
            .provision_bytes(
                &fixture.db,
                &serde_json::to_vec(&serde_json::json!({
                    "format":1,"installation_id":fixture.installation,"resources":batch,
                }))?,
            )
            .await?;
    }
    Ok(())
}

pub(super) async fn seed(fixture: &mut Fixture, rows: usize) -> Result<()> {
    keyed(fixture).await?;
    seed_prepared(fixture, rows).await
}

/// Seed through the caller's already-keyed Service and original owned session.
pub(super) async fn seed_prepared(fixture: &mut Fixture, rows: usize) -> Result<()> {
    // Real management operations produce durable replay records as well as
    // directory facts; keep the currently used tenant/binding unchanged.
    for index in 0..rows {
        result(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Principal(
                        format!("retained-principal-{index:05}").try_into()?,
                        PrincipalPut {
                            operation_id: format!("retained-operation-{index:05}").try_into()?,
                            expected_revision: None,
                            issuer: "startup-history".into(),
                            subject: format!("subject-{index}"),
                            installation_id: fixture.installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
    }
    fixture.service.shutdown().await?;
    resources(fixture, rows).await?;
    let tx = fixture.db.begin().await?;
    for index in 0..rows {
        let usage = Usage {
            input_tokens: Some(2),
            output_tokens: Some(3),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            reasoning_tokens: Some(0),
            provenance: Provenance::Provider,
            complete: true,
        };
        let candidate = receipts::Candidate::new(
            ExecutionReceipt {
                dispatch: Dispatch::Strict,
                installation_id: fixture.installation.clone(),
                tenant_id: fixture.tenant.clone(),
                principal_id: fixture.principal.clone(),
                execution_id: format!("completed-{index:05}").try_into()?,
                correlation_id: "completed-correlation".try_into()?,
                execution_revision: "completed-revision".try_into()?,
                state: ReceiptState::Committed,
                outcome: Outcome::Completed,
                attempts: vec![Attempt {
                    candidate_index: None,
                    observed_http_status: None,
                    attempt_id: "completed-attempt".try_into()?,
                    target: Some(Target {
                        provider_id: "fixture-provider".try_into()?,
                        model_id: "fixture-model".into(),
                        account_id: "default".try_into()?,
                        credential_id: "retained-slot-00000".try_into()?,
                        credential_version: "v1".try_into()?,
                    }),
                    outcome: Outcome::Completed,
                    usage: usage.clone(),
                    native_usage: Vec::new(),
                }],
                usage,
                recorded_at: "2026-11-22T00:00:00Z".try_into()?,
            },
            receipts::Disposition {
                dispatch: DispatchState::Started,
                statuses: vec![Some(200)],
                auxiliary_digest: None,
            },
        )?;
        let staged = receipts::stage_terminal(&tx, &candidate).await?;
        ensure!(staged.inserted());
        drop(staged.reconcile(&|_, _| Ok(())).await?);
    }
    tx.commit().await?;
    // The gateway ledger shares the database with tenancy and must likewise
    // remain outside normal ready. Its canonical store validates every seed.
    let ledger = crate::eval::store::EvalStore::new(fixture.db.clone());
    for index in 0..rows.min(65) {
        let subject: EvalSubject = serde_json::from_value(serde_json::json!({
            "spec_version":bitrouter_ai::eval::SPEC_VERSION,"identity":{"profile":"gateway_generic"},
            "eval_id":format!("retained-eval-{index:05}"),"scope":"request","subject_id":format!("request-{index}"),
            "holdout":false,"decisions":[],"requested_dimensions":[],"evidence":[],
            "evidence_digest":crate::storage::evaluation::evidence_digest(&[])?,"observed_at":"2026-11-22T00:00:00Z"
        }))?;
        ledger.insert_subject(&subject).await?;
    }
    Ok(())
}

#[tokio::test]
async fn mature_restart_does_not_traverse_completed_history_or_retained_directory() -> Result<()> {
    let mut baseline = None;
    for rows in [3, 4097] {
        let home = tempfile::tempdir()?;
        let path = home.path().join("startup.sqlite");
        let (mut fixture, database) = fixture(&path).await?;
        let key = fixture.wrapping_key;
        let healthy = fixture.issue("startup-healthy").await?;
        history::seed(&mut fixture, rows).await?;
        stop_fixture(fixture, database).await?;
        let queries = Arc::new(Queries::default());
        // First open may initialize the key epoch. Compare only mature restarts.
        close(open(&path, &queries, key).await?).await?;
        for _ in 0..2 {
            let service = open(&path, &queries, key).await?;
            let outcome: Result<()> = async {
                ensure!(service.current().is_ok() && service.admission.receipts_ready());
                queries.require_indexed_retained_reads(&service.db).await?;
                queries.require_no_history_writes()?;
                let actual = queries.require_bounded()?;
                if let Some(expected) = &baseline {
                    ensure!(
                        expected == &actual,
                        "startup SQL changed with completed history cardinality: {rows} rows"
                    );
                } else {
                    baseline = Some(actual);
                }
                service
                    .authenticate_live(healthy.secret.as_deref().context("healthy token")?)
                    .await
                    .map_err(failure)?;
                Ok(())
            }
            .await;
            let stopped = close(service).await;
            outcome.and(stopped)?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn connector_format_inspection_and_migration_do_not_scan_retained_resources() -> Result<()> {
    let mut baseline = None;
    for rows in [3, 4097] {
        let home = tempfile::tempdir()?;
        let path = home.path().join("connector.sqlite");
        let (mut fixture, database) = fixture(&path).await?;
        keyed(&mut fixture).await?;
        fixture.service.shutdown().await?;
        resources(&fixture, rows).await?;
        stop_fixture(fixture, database).await?;
        let mut database =
            crate::db::connect_owned_tenancy(&format!("sqlite://{}?mode=rw", path.display()))
                .await?;
        let queries = Arc::new(Queries::default());
        let observed = queries.clone();
        database
            .database_mut_for_test()
            .set_metric_callback(move |info| observed.record(info));
        queries.start()?;
        let outcome: Result<()> = async {
            // The connector's early read-only pool uses this same inspector.
            // Observe it explicitly on the connector-produced pool, then run
            // the real migrate path; neither is replaced with synthetic SQL.
            ensure!(
                crate::db::store_format::inspect(database.database()).await?
                    == crate::db::store_format::State::Current
            );
            database.migrate().await?;
            queries.stop();
            queries
                .require_indexed_retained_reads(database.database())
                .await?;
            queries.require_no_history_writes()?;
            let actual = queries.require_bounded()?;
            ensure!(
                actual
                    .iter()
                    .any(|sql| sql.contains("from main.seaql_migrations")),
                "format inspection observation missing"
            );
            if let Some(expected) = &baseline {
                ensure!(
                    expected == &actual,
                    "format/migration SQL changed with resource cardinality"
                );
            }
            baseline = Some(actual);
            Ok(())
        }
        .await;
        queries.stop();
        let db = database.database().clone();
        drop(database);
        let closed = db.close().await;
        outcome.and(closed.map_err(Into::into))?;
    }
    Ok(())
}
