//! A real closed native Human source survives learner acquisition and retained reload.
use super::super::human::{label, observed_task};
use super::*;

#[tokio::test]
async fn real_native_human_head_is_retained_in_a_loaded_learned_checkpoint() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(90), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, credential) =
            learner_fixture(directory.path(), &url, 1_000_000, 1).await?;
        let fixture = claimed_fixture(fixture, directory.path()).await?;
        let mut session = None;
        let prepared = async {
            let old: RevisionId = headers
                .get("bitrouter-execution-revision")
                .context("original R1")?
                .to_str()?
                .try_into()?;
            let retained = crate::tenancy::configuration_store::configuration(
                &fixture.db,
                &fixture.tenant,
                &old,
            )
            .await?
            .context("source configuration")?;
            let mut configured = fixture.configuration_input("native-human-learner-r1")?;
            configured.document = retained.revision.document;
            configured.document.limits.learning_jobs = 2;
            let route = configured.document.routes[0].clone();
            let StoredResponse::Configuration(configuration) = fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Configuration(fixture.tenant.clone(), configured),
                )
                .await
                .map_err(failure)?
            else {
                anyhow::bail!("source configuration reply")
            };
            let r1 = configuration.revision_id;
            activate(&fixture, &r1, None, "activate-human-source").await?;
            let headers = crate::tenancy::tests::execution::headers(
                &fixture,
                credential.secret.as_deref().context("secret")?,
                &r1,
            )?;
            session = Some(
                open(
                    &fixture,
                    &headers,
                    &url,
                    directory.path(),
                    "human-learner-source",
                )
                .await?,
            );
            let native = session.as_mut().context("actual native session")?;
            native
                .run(
                    observed_task(),
                    Arc::new(Approval),
                    CancellationToken::new(),
                )
                .await?;
            let view = native.review_run().await?;
            let accepted = native
                .submit_assessment(label(&view, "human-learner-label", true)?)
                .await?;
            let frozen = freeze(&fixture, &headers).await?;
            ensure!(
                frozen.entries.len() == 1,
                "Human source rewrote or inserted an operational result"
            );
            let report = native
                .experiment_cohort(MODEL, &frozen.evidence_root)
                .await?;
            ensure!(report.challenger.conclusive == 1 && report.challenger.pass == 1);
            Ok::<_, anyhow::Error>((r1, route, frozen, accepted.revision.revision_id))
        }
        .await;
        let closed = match session {
            Some(session) => tokio::time::timeout(TIMEOUT, session.close())
                .await
                .map_err(anyhow::Error::from)
                .and_then(|value| value.map_err(anyhow::Error::from)),
            None => Ok(()),
        };
        let checked =
            async {
                let (r1, route, frozen, human_revision) = prepared?;
                closed?;
                register(
                    &fixture,
                    &credential,
                    RegistrationInput {
                        installation: fixture.installation.clone(),
                        tenant: fixture.tenant.clone(),
                        id: "native-human-learning".into(),
                        operation_id: "register-human-learning".try_into()?,
                        expected_authority_revision: fixture
                            .service
                            .current()
                            .map_err(failure)?
                            .revision,
                        expected_active_revision: r1.clone(),
                        expected_schedule_revision: None,
                        route,
                        evidence_root: frozen.evidence_root,
                        enabled: true,
                    },
                )
                .await?;
                // Exercise the actual H acquisition and production optimizer, with
                // the exact stopped-host lease held across computation/publication.
                let acquired =
                    fault_fixture::acquire(&fixture, Instant::now() + Duration::from_secs(40))
                        .await?;
                ensure!(acquired.fence.units() == 1 && acquired.fence.conclusive() == 1);
                ensure!(acquired.snapshot.records.iter().any(
                    |record| record.result.evaluator.kind
                        == bitrouter_ai::eval::EvaluatorKind::Human
                        && record.result.idempotency_key.ends_with(&human_revision)
                ));
                let head_digest = acquired.fence.effective_heads().to_owned();
                let calculated = learner::source::calculate(&acquired)?;
                let Reply::Published(published) = queue::request(
                    &fixture.service,
                    Work::Publish {
                        acquired,
                        calculated: Box::new(calculated),
                        shutdown: CancellationToken::new(),
                    },
                    Instant::now() + TIMEOUT,
                )
                .await
                .map_err(failure)?
                else {
                    anyhow::bail!("actual learned publication absent")
                };
                let r2 = published.resource_revision.context("learned revision")?;
                let stored = crate::tenancy::configuration_store::configuration(
                    &fixture.db,
                    &fixture.tenant,
                    &r2,
                )
                .await?
                .context("retained learned configuration")?;
                fixture
                    .service
                    .resources
                    .validate_compiled(&stored)
                    .map_err(|code| anyhow::anyhow!("retained integrity: {code:?}"))?;
                let runtime = fixture
                    .service
                    .resources
                    .open_execution(&stored)
                    .map_err(|code| anyhow::anyhow!("retained load: {code:?}"))?;
                let route = runtime
                    .learning_route(&stored.revision.document.routes[0])
                    .map_err(|code| anyhow::anyhow!("loaded route: {code:?}"))?;
                ensure!(route.policy().is_some());
                let checkpoint = stored
                    .dependencies
                    .iter()
                    .find(|reference| {
                        reference.resource_kind
                            == bitrouter_ai::tenancy::authority::ResourceKind::Checkpoint
                    })
                    .context("actual learned checkpoint dependency")?;
                let evidence = fixture
                    .service
                    .resources
                    .native_learned_evidence_for_test(&fixture.db, checkpoint)
                    .await?;
                ensure!(
                    evidence["units"] == 1
                        && evidence["conclusive"] == 1
                        && evidence["unavailable"] == 0
                );
                ensure!(evidence["effective_head_digest"] == head_digest);
                ensure!(
                    fixture
                        .service
                        .current()
                        .map_err(failure)?
                        .active_configurations
                        .get(&fixture.tenant)
                        == Some(&r1),
                    "learning implicitly activated a configuration"
                );
                ensure!(
                    server.received_requests().await.context("requests")?.len() == 1,
                    "learning replayed paid work"
                );
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
