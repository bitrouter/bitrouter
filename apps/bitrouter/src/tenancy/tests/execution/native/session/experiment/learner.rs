//! Actual file-owned NativeSession evidence feeds the shipped owned worker mode.
mod human;
use super::*;
use crate::tenancy::learner::{
    self, RegistrationInput,
    queue::{self, Reply, Work},
};
use bitrouter_ai::tenancy::{configuration::Activation, identity::RevisionId};
use std::time::Duration;
use tokio::time::Instant;

async fn activate(
    fixture: &Fixture,
    revision: &RevisionId,
    expected: Option<RevisionId>,
    id: &str,
) -> Result<()> {
    fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Activate(
                fixture.tenant.clone(),
                Activation {
                    operation_id: id.try_into()?,
                    expected_active_revision: expected,
                    revision_id: revision.clone(),
                    expected_authority_revision: fixture
                        .service
                        .current()
                        .map_err(failure)?
                        .revision,
                    installation_id: fixture.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    Ok(())
}

async fn source(
    fixture: &Fixture,
    headers: &HeaderMap,
    credential: &CredentialIssued,
    url: &str,
    directory: &std::path::Path,
) -> Result<(RegistrationInput, RevisionId)> {
    let old: RevisionId = headers
        .get("bitrouter-execution-revision")
        .context("R1")?
        .to_str()?
        .try_into()?;
    let retained =
        crate::tenancy::configuration_store::configuration(&fixture.db, &fixture.tenant, &old)
            .await?
            .context("source config")?;
    let mut configured = fixture.configuration_input("learner-config")?;
    configured.document = retained.revision.document;
    configured.document.limits.learning_jobs = 2;
    let selected_route = configured.document.routes[0].clone();
    let StoredResponse::Configuration(configuration) = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), configured),
        )
        .await
        .map_err(failure)?
    else {
        anyhow::bail!("expected config");
    };
    let r1 = configuration.revision_id;
    activate(fixture, &r1, None, "learner-activate-r1").await?;
    let headers = crate::tenancy::tests::execution::headers(
        fixture,
        credential.secret.as_deref().context("secret")?,
        &r1,
    )?;
    let mut session = open(fixture, &headers, url, directory, "learner-source").await?;
    let checked = async {
        let state = session
            .run(task(), Arc::new(Approval), CancellationToken::new())
            .await?;
        ensure!(state.run.as_ref().is_some_and(
            |run| run.status == crate::storage::checkpoint::snapshot::RunStatus::Completed
        ));
        let snapshot = freeze(fixture, &headers).await?;
        ensure!(snapshot.entries.len() == 1);
        Ok::<_, anyhow::Error>(snapshot)
    }
    .await;
    let closed = tokio::time::timeout(TIMEOUT, session.close()).await;
    let snapshot = checked?;
    closed??;
    ensure!(
        fixture.service.host_slots_for_test() == 1,
        "native close did not release actual slot"
    );
    Ok((
        RegistrationInput {
            installation: fixture.installation.clone(),
            tenant: fixture.tenant.clone(),
            id: "native-retreat".into(),
            operation_id: "register-learner".try_into()?,
            expected_authority_revision: fixture.service.current().map_err(failure)?.revision,
            expected_active_revision: r1.clone(),
            expected_schedule_revision: None,
            route: selected_route,
            evidence_root: snapshot.evidence_root,
            enabled: true,
        },
        r1,
    ))
}

async fn register(
    fixture: &Fixture,
    credential: &CredentialIssued,
    input: RegistrationInput,
) -> Result<()> {
    let caller = fixture
        .service
        .authenticate(credential.secret.as_deref().context("secret")?)
        .map_err(failure)?;
    queue::register(&fixture.service, caller, input)
        .await
        .map_err(failure)?;
    Ok(())
}

async fn claimed_fixture(fixture: Fixture, directory: &std::path::Path) -> Result<Fixture> {
    let Fixture {
        service,
        db,
        operator,
        installation,
        tenant,
        principal,
        binding,
        binding_result,
    } = fixture;
    let stopped = tokio::time::timeout(TIMEOUT, service.shutdown()).await;
    let closed = tokio::time::timeout(TIMEOUT, db.clone().close()).await;
    drop(service);
    drop(db);
    stopped??;
    closed??;
    let database = crate::db::connect_owned_tenancy(&format!(
        "sqlite://{}",
        directory.join("experiment.db").display()
    ))
    .await?;
    // Preserve the exact connector-produced pool, lease, and one-use claim.
    let (db, owner, claim) = database.claim()?;
    let state = store::load(&db, Utc::now()).await?;
    let service = Service::start_resources_with_continuations(
        db.clone(),
        Some(owner),
        OperationLimits::default(),
        state,
        Arc::new(Resources::with_key(installation.clone(), [29; 32])?),
        (
            None,
            &crate::config::ContinuationConfig::default(),
            Some(claim),
            crate::telemetry::receipt_latency::Sink::default(),
        ),
        None,
    )
    .await?;
    service.dispatch_test_ready.store(true, Ordering::Release);
    Ok(Fixture {
        service,
        db,
        operator,
        installation,
        tenant,
        principal,
        binding,
        binding_result,
    })
}

#[tokio::test]
async fn owned_learner_resumes_after_closed_native_session_and_publishes_retreat_without_activation()
-> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, credential) =
            learner_fixture(directory.path(), &url, 1_000_000, 1).await?;
        let fixture = claimed_fixture(fixture, directory.path()).await?;
        let prepared = async {
            let (input, r1) =
                source(&fixture, &headers, &credential, &url, directory.path()).await?;
            register(&fixture, &credential, input).await?;
            Ok::<_, anyhow::Error>(r1)
        }
        .await;
        let r1 = match prepared {
            Ok(r1) => r1,
            Err(error) => {
                tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await??;
                return Err(error);
            }
        };
        let fixture = claimed_fixture(fixture, directory.path()).await?;
        let checked = async {
            // Same production startup hook with no registration file: durable
            // registrations are discovered by the scheduler, not a session host.
            learner::registration::initialize(&fixture.service, None).await?;
            let stop = CancellationToken::new();
            let gate = crate::daemon_handoff::HandoffGate::default();
            let config = crate::config::Config {
                inherit_defaults: false,
                ..Default::default()
            };
            let policies = crate::policy_lock::PolicyRuntime::from_document(
                &config,
                None,
                Default::default(),
                None,
            )?;
            let legacy = crate::core::evolution::runtime::EvolutionRuntime::new(
                fixture.db.clone(),
                Arc::new(crate::config::routing_table::ConfigRoutingTable::from_config(config)),
                policies,
            );
            // Use the exact single launch function called by host.rs. A Legacy
            // fallthrough has no pipeline and cannot satisfy this positive case.
            let worker = crate::core::evolution::scheduler::EvolutionScheduler::start(
                legacy.clone(),
                None,
                Some(fixture.service.clone()),
                gate.clone(),
                stop.clone(),
            )
            .context("owned worker")?;
            let second = crate::core::evolution::scheduler::EvolutionScheduler::start(
                legacy.clone(),
                None,
                Some(fixture.service.clone()),
                gate,
                stop.clone(),
            );
            let duplicate = second.is_some();
            let observed = tokio::time::timeout(Duration::from_secs(15), async {
                ensure!(!duplicate, "second owned worker admitted");
                loop {
                    let query = sea_orm::sea_query::Query::select()
                        .column(sea_orm::sea_query::Alias::new("result_json"))
                        .from(sea_orm::sea_query::Alias::new("tenancy_operations"))
                        .and_where(
                            sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new(
                                "installation_id",
                            ))
                            .eq(fixture.installation.as_str()),
                        )
                        .and_where(
                            sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new(
                                "tenant_id",
                            ))
                            .eq(fixture.tenant.as_str()),
                        )
                        .and_where(
                            sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new(
                                "operation_id",
                            ))
                            .like("learn_%"),
                        )
                        .limit(2)
                        .to_owned();
                    let rows = fixture
                        .db
                        .query_all(fixture.db.get_database_backend().build(&query))
                        .await?;
                    if !rows.is_empty() {
                        ensure!(rows.len() == 1, "publication duplicated");
                        let result = serde_json::from_str::<
                            bitrouter_ai::tenancy::operation::OperationResult,
                        >(
                            &rows[0].try_get::<String>("", "result_json")?
                        )?;
                        return Ok::<_, anyhow::Error>(vec![result]);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            stop.cancel();
            let joined = tokio::time::timeout(TIMEOUT, worker).await;
            if let Some(second) = second {
                tokio::time::timeout(TIMEOUT, second).await??;
            }
            let published = observed??;
            joined??;
            ensure!(
                !legacy.worker_status().running && legacy.worker_status().last_started_at.is_none()
            );
            ensure!(
                published.len() == 1,
                "real operational cap must publish conservative Retreat"
            );
            let r2 = published[0]
                .resource_revision
                .as_ref()
                .context("published configuration")?;
            ensure!(
                fixture
                    .service
                    .current()
                    .map_err(failure)?
                    .active_configurations
                    .get(&fixture.tenant)
                    == Some(&r1)
            );
            let stored = crate::tenancy::configuration_store::configuration(
                &fixture.db,
                &fixture.tenant,
                r2,
            )
            .await?
            .context("learned config")?;
            let runtime = fixture
                .service
                .resources
                .open_execution(&stored)
                .map_err(|code| anyhow::anyhow!("{code:?}"))?;
            let route = runtime
                .learning_route(&stored.revision.document.routes[0])
                .map_err(|code| anyhow::anyhow!("{code:?}"))?;
            let policy = route.policy().context("learned policy")?;
            ensure!(stored.revision.document.routes.len() == 2);
            for reference in &stored.revision.document.routes {
                let affected = runtime
                    .learning_route(reference)
                    .map_err(|code| anyhow::anyhow!("{code:?}"))?;
                ensure!(
                    affected
                        .policy()
                        .context("affected route policy")?
                        .reference()
                        == policy.reference(),
                    "affected route retained old policy"
                );
            }

            ensure!(
                policy.document().policies[policy.name()]
                    .optimization
                    .as_ref()
                    .is_some_and(|state| state.active.is_none())
            );
            ensure!(
                stored
                    .dependencies
                    .iter()
                    .any(|reference| reference.resource_kind
                        == bitrouter_ai::tenancy::authority::ResourceKind::Checkpoint)
            );
            // Fresh passes cannot duplicate publication from an unchanged fence.
            let mut cursor = Default::default();
            ensure!(
                learner::worker::pass(&fixture.service, &mut cursor, &CancellationToken::new())
                    .await?
                    .is_empty()
            );
            activate(&fixture, r2, Some(r1.clone()), "activate-learned").await?;
            activate(&fixture, &r1, Some(r2.clone()), "rollback-learned").await?;
            ensure!(
                server.received_requests().await.context("requests")?.len() == 1,
                "learning replayed provider execution"
            );
            drained(&fixture).await?;
            ensure!(fixture.service.host_slots_for_test() == 1);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?;
        stopped??;
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn owned_learner_rotation_after_acquisition_denies_publication_and_future_wake() -> Result<()>
{
    tokio::time::timeout(Duration::from_secs(45), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, credential) =
            learner_fixture(directory.path(), &url, 1_000_000, 1).await?;
        let fixture = claimed_fixture(fixture, directory.path()).await?;
        let checked = async {
            let (input, r1) =
                source(&fixture, &headers, &credential, &url, directory.path()).await?;
            register(&fixture, &credential, input).await?;
            let Reply::Scanned(page) = queue::request(
                &fixture.service,
                Work::Scan(Default::default()),
                Instant::now() + TIMEOUT,
            )
            .await
            .map_err(failure)?
            else {
                anyhow::bail!("scan");
            };
            let registration = page
                .registrations
                .into_iter()
                .next()
                .context("registration")?;
            let stopped = fixture.service.stopped_learner_source().map_err(failure)?;
            let Reply::Acquired(acquired) = queue::request(
                &fixture.service,
                Work::Acquire {
                    registration,
                    stopped,
                    deadline: Instant::now() + Duration::from_secs(40),
                },
                Instant::now() + TIMEOUT,
            )
            .await
            .map_err(failure)?
            else {
                anyhow::bail!("acquire");
            };
            let calculated = learner::source::calculate(&acquired)?;
            let before = fault_fixture::retained_counts(&fixture).await?;
            let mut rotation = fixture.issuance("learner-rotate")?;
            rotation.rotate_credential_id = Some(credential.credential_id.clone());
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Issue(fixture.tenant.clone(), rotation),
                )
                .await
                .map_err(failure)?;
            ensure!(
                queue::request(
                    &fixture.service,
                    Work::Publish {
                        acquired,
                        shutdown: CancellationToken::new(),
                        calculated: Box::new(calculated)
                    },
                    Instant::now() + TIMEOUT
                )
                .await
                .is_err()
            );
            let mut cursor = Default::default();
            ensure!(
                learner::worker::pass(&fixture.service, &mut cursor, &CancellationToken::new())
                    .await?
                    .is_empty()
            );
            ensure!(
                fault_fixture::retained_counts(&fixture).await? == before,
                "rotation created a learned draft or operation"
            );
            ensure!(
                fixture
                    .service
                    .current()
                    .map_err(failure)?
                    .active_configurations
                    .get(&fixture.tenant)
                    == Some(&r1)
            );
            ensure!(server.received_requests().await.context("requests")?.len() == 1);
            drained(&fixture).await?;
            ensure!(fixture.service.host_slots_for_test() == 1);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?;
        stopped??;
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn owned_learner_rechecks_actual_journal_before_publication() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(45), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, credential) =
            learner_fixture(directory.path(), &url, 1_000_000, 1).await?;
        let fixture = claimed_fixture(fixture, directory.path()).await?;
        let checked = async {
            let (input, r1) =
                source(&fixture, &headers, &credential, &url, directory.path()).await?;
            register(&fixture, &credential, input).await?;
            let Reply::Scanned(page) = queue::request(
                &fixture.service,
                Work::Scan(Default::default()),
                Instant::now() + TIMEOUT,
            )
            .await
            .map_err(failure)?
            else {
                anyhow::bail!("scan");
            };
            let registration = page
                .registrations
                .into_iter()
                .next()
                .context("registration")?;
            let stopped = fixture.service.stopped_learner_source().map_err(failure)?;
            let Reply::Acquired(acquired) = queue::request(
                &fixture.service,
                Work::Acquire {
                    registration,
                    stopped,
                    deadline: Instant::now() + Duration::from_secs(40),
                },
                Instant::now() + TIMEOUT,
            )
            .await
            .map_err(failure)?
            else {
                anyhow::bail!("acquire");
            };
            let calculated = learner::source::calculate(&acquired)?;
            ensure!(
                calculated.step.action
                    == crate::core::optimization::controller::ControllerAction::Retreat
            );
            // Corrupt only the actual producer row after a successful bounded
            // read. Frozen Eval, receipts, experiment unit, and saved ACK remain.
            let query = sea_orm::sea_query::Query::delete()
                .from_table(sea_orm::sea_query::Alias::new("bro_managed_sessions"))
                .and_where(
                    sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new(
                        "installation_id",
                    ))
                    .eq(fixture.installation.as_str()),
                )
                .and_where(
                    sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new("tenant_id"))
                        .eq(fixture.tenant.as_str()),
                )
                .to_owned();
            ensure!(
                fixture
                    .db
                    .execute(fixture.db.get_database_backend().build(&query))
                    .await?
                    .rows_affected()
                    == 1
            );
            ensure!(
                queue::request(
                    &fixture.service,
                    Work::Publish {
                        acquired,
                        shutdown: CancellationToken::new(),
                        calculated: Box::new(calculated)
                    },
                    Instant::now() + TIMEOUT
                )
                .await
                .is_err()
            );
            ensure!(
                fixture
                    .service
                    .current()
                    .map_err(failure)?
                    .active_configurations
                    .get(&fixture.tenant)
                    == Some(&r1)
            );
            ensure!(records(&fixture, "owned_learner_schedule_v1").await?[0]["next_sequence"] == 1);
            drained(&fixture).await?;
            ensure!(fixture.service.host_slots_for_test() == 1);
            ensure!(server.received_requests().await.context("requests")?.len() == 1);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?;
        stopped??;
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn owned_learner_discovery_pages_thirty_three_durable_registrations() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, credential) =
            learner_fixture(directory.path(), &url, 1_000_000, 1).await?;
        let fixture = claimed_fixture(fixture, directory.path()).await?;
        let checked = async {
            let (input, _) =
                source(&fixture, &headers, &credential, &url, directory.path()).await?;
            for index in 0..33 {
                let mut next = input.clone();
                next.id = format!("page-{index}");
                next.operation_id = format!("register-page-{index}").try_into()?;
                next.expected_authority_revision =
                    fixture.service.current().map_err(failure)?.revision;
                register(&fixture, &credential, next).await?;
            }
            let mut cursor = Default::default();
            let mut ids = std::collections::BTreeSet::new();
            for expected in [32, 1] {
                let Reply::Scanned(page) = queue::request(
                    &fixture.service,
                    Work::Scan(cursor),
                    Instant::now() + TIMEOUT,
                )
                .await
                .map_err(failure)?
                else {
                    anyhow::bail!("scan");
                };
                ensure!(page.registrations.len() == expected);
                for registration in page.registrations {
                    ensure!(
                        ids.insert(registration.input.id.clone()),
                        "duplicated discovery key"
                    );
                }
                cursor = page.cursor;
            }
            ensure!(ids.len() == 33);
            fixture.service.current().map_err(failure)?;
            ensure!(server.received_requests().await.context("requests")?.len() == 1);
            drained(&fixture).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?;
        stopped??;
        Ok(())
    })
    .await?
}

mod cancellation;
mod deadlines;
mod fault_fixture;
