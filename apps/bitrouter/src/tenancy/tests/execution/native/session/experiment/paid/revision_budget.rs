//! Immutable paid allowances are independent of credential and activation changes.
use super::*;
use crate::tenancy::tests::issued;
use bitrouter_ai::{
    core::protocol::{CommitStatus, ErrorCode as CoreCode},
    tenancy::{
        configuration::{Activation, ConfigurationPut},
        identity::RevisionId,
    },
};
use std::path::Path;

async fn activate(
    fixture: &Fixture,
    id: &str,
    expected: Option<&RevisionId>,
    revision: &RevisionId,
) -> Result<()> {
    let applied = result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Activate(
                    fixture.tenant.clone(),
                    Activation {
                        operation_id: id.try_into()?,
                        expected_active_revision: expected.cloned(),
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
            .map_err(failure)?,
    )?;
    ensure!(applied.resource_revision.as_ref() == Some(revision));
    Ok(())
}

async fn source(
    fixture: &Fixture,
    headers: &HeaderMap,
    url: &str,
    root: &Path,
    name: &str,
    judge: &mut Judge,
    allowed: bool,
) -> Result<()> {
    let mut current = open(fixture, headers, url, root, name).await?;
    let checked = async {
        current
            .run(
                human::observed_task(),
                Arc::new(Approval),
                CancellationToken::new(),
            )
            .await?;
        let review = current.review_run().await?;
        ensure!(review.history.is_empty());
        if allowed {
            judge.answer(&review)?;
            judge.release.cancel();
            current
                .assess_run(&review.scoring.evidence.checkpoint_id, None)
                .await?;
            ensure!(judge.received().await?["model"] == "judge-model");
            let after = current.review_run().await?;
            ensure!(after.history.len() == 1 && after.history[0].selected_on_submission);
        } else {
            let error = current
                .assess_run(&review.scoring.evidence.checkpoint_id, None)
                .await
                .err()
                .context("spent immutable revision admitted another paid source")?;
            ensure!(
                error.code == CoreCode::LimitExceeded
                    && error.commit_status == CommitStatus::NotCommitted
            );
            ensure!(current.review_run().await?.history.is_empty() && !judge.extra_request());
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let closed = tokio::time::timeout(TIMEOUT, current.close()).await;
    checked?;
    closed??;
    Ok(())
}

#[tokio::test]
async fn native_paid_revision_budget_survives_rotation_reactivation_and_two_revision_bootstrap()
-> Result<()> {
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        Box::pin(async {
            let provider = upstream().await?;
            let mut judge = Judge::start().await?;
            let root = tempfile::tempdir()?;
            let url = format!("{}/v1", provider.uri());
            let (mut fixture, original_headers) = ready(root.path(), &url, &judge.url).await?;
            let checked = async {
                let first: RevisionId = original_headers
                    .get("Bitrouter-Execution-Revision")
                    .context("actual retained revision header")?
                    .to_str()?
                    .try_into()?;
                let original = fixture.issue("budget-before-rotation").await?;
                activate(&fixture, "budget-first-activation", None, &first).await?;
                let headers = crate::tenancy::tests::execution::headers(
                    &fixture,
                    original.secret.as_deref().context("first ingress secret")?,
                    &first,
                )?;
                source(
                    &fixture,
                    &headers,
                    &url,
                    root.path(),
                    "budget-r1-first",
                    &mut judge,
                    true,
                )
                .await?;
                let first_jobs = records(&fixture, "owned_judge_job").await?;
                let first_budget = records(&fixture, "owned_judge_budget").await?;
                ensure!(
                    first_jobs.len() == 1
                        && first_budget.len() == 1
                        && first_budget[0]["configuration"] == first.as_str()
                        && first_budget[0]["accepted"] == 1
                );
                let mut rotation = fixture.issuance("budget-rotate-ingress")?;
                rotation.rotate_credential_id = Some(original.credential_id);
                let rotated = issued(
                    fixture
                        .service
                        .execute(
                            fixture.operator.clone(),
                            Command::Issue(fixture.tenant.clone(), rotation),
                        )
                        .await
                        .map_err(failure)?,
                )?;
                ensure!(
                    fixture
                        .service
                        .authenticate(original.secret.as_deref().context("old key")?)
                        .is_err()
                );
                let rotated_headers = crate::tenancy::tests::execution::headers(
                    &fixture,
                    rotated
                        .secret
                        .as_deref()
                        .context("rotated ingress secret")?,
                    &first,
                )?;
                source(
                    &fixture,
                    &rotated_headers,
                    &url,
                    root.path(),
                    "budget-r1-rotated",
                    &mut judge,
                    false,
                )
                .await?;
                ensure!(
                    records(&fixture, "owned_judge_job").await? == first_jobs
                        && records(&fixture, "owned_judge_budget").await? == first_budget
                );
                let mut document = fixture
                    .service
                    .configuration(&fixture.operator, &fixture.tenant, &first)
                    .await
                    .map_err(failure)?
                    .document;
                // A different actual retained document receives a distinct revision;
                // a new operation ID with identical content must not reset a budget.
                document.generation_defaults.temperature = 0.375;
                let response = fixture
                    .service
                    .execute(
                        fixture.operator.clone(),
                        Command::Configuration(
                            fixture.tenant.clone(),
                            ConfigurationPut {
                                operation_id: "budget-second-revision".try_into()?,
                                expected_authority_revision: fixture
                                    .service
                                    .current()
                                    .map_err(failure)?
                                    .revision,
                                document,
                                installation_id: fixture.installation.clone(),
                            },
                        ),
                    )
                    .await
                    .map_err(failure)?;
                let StoredResponse::Configuration(second) = response else {
                    anyhow::bail!("second retained revision missing");
                };
                ensure!(second.revision_id != first);
                activate(
                    &fixture,
                    "budget-second-activation",
                    Some(&first),
                    &second.revision_id,
                )
                .await?;
                let second_headers = crate::tenancy::tests::execution::headers(
                    &fixture,
                    rotated.secret.as_deref().context("rotated key")?,
                    &second.revision_id,
                )?;
                source(
                    &fixture,
                    &second_headers,
                    &url,
                    root.path(),
                    "budget-r2-first",
                    &mut judge,
                    true,
                )
                .await?;
                let mixed = records(&fixture, "owned_judge_job").await?;
                let counters = records(&fixture, "owned_judge_budget").await?;
                ensure!(mixed.len() == 2 && counters.len() == 2);
                for revision in [&first, &second.revision_id] {
                    ensure!(
                        counters
                            .iter()
                            .filter(|row| row["configuration"] == revision.as_str()
                                && row["accepted"] == 1)
                            .count()
                            == 1
                    );
                    ensure!(
                        mixed
                            .iter()
                            .filter(|row| row["configuration"] == revision.as_str()
                                && row["status"] == "completed")
                            .count()
                            == 1
                    );
                }
                activate(
                    &fixture,
                    "budget-reactivate-first",
                    Some(&second.revision_id),
                    &first,
                )
                .await?;
                let headers = crate::tenancy::tests::execution::headers(
                    &fixture,
                    rotated.secret.as_deref().context("rotated key")?,
                    &first,
                )?;
                source(
                    &fixture,
                    &headers,
                    &url,
                    root.path(),
                    "budget-r1-reactivated",
                    &mut judge,
                    false,
                )
                .await?;
                ensure!(
                    records(&fixture, "owned_judge_job").await? == mixed
                        && records(&fixture, "owned_judge_budget").await? == counters
                        && records(&fixture, "owned_judge_pending").await?.is_empty()
                );
                ensure!(
                    provider
                        .received_requests()
                        .await
                        .context("actual subject requests")?
                        .len()
                        == 4
                        && !judge.extra_request()
                        && fixture.service.current().is_ok()
                );
                Ok::<_, anyhow::Error>((mixed, counters))
            }
            .await;
            let (jobs, counters) = match checked {
                Ok(value) => value,
                Err(error) => return finish(&fixture, None, judge, Err(error)).await,
            };
            fixture = match restart(fixture, root.path()).await {
                Ok(value) => value,
                Err(error) => return Err::<(), _>(error).and(judge.finish().await),
            };
            let checked = async {
                ensure!(
                    records(&fixture, "owned_judge_job").await? == jobs
                        && records(&fixture, "owned_judge_budget").await? == counters
                        && records(&fixture, "owned_judge_pending").await?.is_empty()
                        && !judge.extra_request()
                        && fixture.service.current().is_ok()
                );
                for row in &jobs {
                    let execution = row["execution"]
                        .as_str()
                        .context("actual mixed paid execution")?
                        .try_into()?;
                    let receipt = crate::tenancy::receipts::store::lookup(
                        &fixture.db,
                        &fixture.installation,
                        &fixture.tenant,
                        &execution,
                    )
                    .await?
                    .context("actual mixed paid receipt")?;
                    ensure!(
                        receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Committed
                            && receipt.usage.input_tokens == Some(4)
                            && receipt.usage.output_tokens == Some(2)
                    );
                }
                ensure!(
                    provider
                        .received_requests()
                        .await
                        .context("actual source requests after restart")?
                        .len()
                        == 4
                );
                Ok::<_, anyhow::Error>(())
            }
            .await;
            finish(&fixture, None, judge, checked).await
        }),
    )
    .await?
}
