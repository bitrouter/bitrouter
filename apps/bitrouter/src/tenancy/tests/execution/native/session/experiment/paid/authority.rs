//! Current authority gates new jobs; accepted facts survive permission loss.
use super::*;
use bitrouter_ai::core::protocol::CommitStatus;

async fn ingress(
    fixture: &Fixture,
    mut headers: HeaderMap,
) -> Result<(HeaderMap, CredentialIssued)> {
    let credential = fixture.issue("native-paid-authority-key").await?;
    headers.insert(
        "authorization",
        format!(
            "Bearer {}",
            credential.secret.as_deref().context("actual ingress key")?
        )
        .parse()?,
    );
    Ok((headers, credential))
}

fn subject(fixture: &Fixture) -> Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: bitrouter_ai::tenancy::authority::OwnerKind::Tenant,
        owner_id: fixture.tenant.as_str().try_into()?,
        resource_kind: bitrouter_ai::tenancy::authority::ResourceKind::Route,
        resource_id: MODEL.try_into()?,
        revision_id: "native-paid-r1".try_into()?,
    })
}

#[tokio::test]
async fn changed_native_paid_authority_denies_acceptance_without_spending_or_sending() -> Result<()>
{
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        Box::pin(async {
            for change in ["revoke", "suspend", "scope"] {
                let provider = upstream().await?;
                let mut judge = Judge::start().await?;
                let directory = tempfile::tempdir()?;
                let url = format!("{}/v1", provider.uri());
                let (fixture, headers) = ready(directory.path(), &url, &judge.url).await?;
                let mut session = None;
                let checked = async {
                    let (headers, key) = ingress(&fixture, headers).await?;
                    session = Some(
                        open(
                            &fixture,
                            &headers,
                            &url,
                            directory.path(),
                            "paid-before-authority-loss",
                        )
                        .await?,
                    );
                    let current = session.as_mut().context("actual native host")?;
                    current
                        .run(
                            human::observed_task(),
                            Arc::new(Approval),
                            CancellationToken::new(),
                        )
                        .await?;
                    let view = current.review_run().await?;
                    judge.answer(&view)?;
                    change_authority(&fixture, &key, &subject(&fixture)?, change).await?;
                    let error = current
                        .assess_run(&view.scoring.evidence.checkpoint_id, None)
                        .await
                        .err()
                        .context("changed authority admitted new paid work")?;
                    ensure!(
                        error.commit_status == CommitStatus::NotCommitted,
                        "{change}: {error:?}"
                    );
                    ensure!(
                        records(&fixture, "owned_judge_job").await?.is_empty()
                            && records(&fixture, "owned_judge_pending").await?.is_empty()
                            && records(&fixture, "owned_judge_budget").await?.is_empty()
                    );
                    ensure!(
                        !judge.extra_request()
                            && provider
                                .received_requests()
                                .await
                                .context("subject requests")?
                                .len()
                                == 1
                            && fixture.service.current().is_ok()
                    );
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                finish(&fixture, session, judge, checked).await?;
            }
            Ok(())
        }),
    )
    .await?
}

#[tokio::test]
async fn authority_lost_during_native_judge_retains_unselected_answer_usage_and_budget()
-> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(90), Box::pin(async {
        for change in ["revoke", "suspend", "scope"] {
            let provider = upstream().await?;
            let mut judge = Judge::start().await?;
            let directory = tempfile::tempdir()?;
            let url = format!("{}/v1", provider.uri());
            let (fixture, headers) = ready(directory.path(), &url, &judge.url).await?;
            let mut session = None;
            let checked = async {
                let (headers, key) = ingress(&fixture, headers).await?;
                session = Some(open(&fixture, &headers, &url, directory.path(), "paid-late-authority-loss").await?);
                let current = session.as_mut().context("actual native host")?;
                current.run(human::observed_task(), Arc::new(Approval), CancellationToken::new()).await?;
                let view = current.review_run().await?;
                judge.answer(&view)?;
                let mut paid = Box::pin(current.assess_run(&view.scoring.evidence.checkpoint_id, None));
                let request = tokio::select! {
                    result = &mut paid => { result?; anyhow::bail!("paid work completed before the held judge response"); }
                    request = judge.received() => request?,
                };
                ensure!(request["model"] == "judge-model");
                ensure!(records(&fixture, "owned_judge_pending").await?.len() == 1);
                change_authority(&fixture, &key, &subject(&fixture)?, change).await?;
                judge.release.cancel();
                paid.await?;
                let jobs = records(&fixture, "owned_judge_job").await?;
                let counters = records(&fixture, "owned_judge_budget").await?;
                ensure!(jobs.len() == 1 && jobs[0]["status"] == "unselected"
                    && jobs[0]["output"]["usage"]["prompt_tokens"] == 4
                    && counters.len() == 1 && counters[0]["accepted"] == 1);
                ensure!(records(&fixture, "owned_judge_pending").await?.is_empty());
                let execution = jobs[0]["execution"].as_str().context("actual paid execution")?.try_into()?;
                let receipt = crate::tenancy::receipts::store::lookup(
                    &fixture.db, &fixture.installation, &fixture.tenant, &execution,
                ).await?.context("actual paid terminal")?;
                ensure!(receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Committed
                    && receipt.usage.input_tokens == Some(4) && receipt.usage.output_tokens == Some(2));
                // Observe only the revision produced by the real terminal. No
                // test-created assessment, receipt or effective head is used.
                let row = fixture.db.query_one(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT revision_json FROM acp_assessment_revisions WHERE revision_id=? AND installation_id=? AND tenant_id=?",
                    [jobs[0]["assessment"].as_str().context("actual assessment")?.into(),
                        fixture.installation.as_str().into(), fixture.tenant.as_str().into()],
                )).await?.context("actual unselected revision")?;
                let revision: crate::acp_trajectory::checkpoint::types::AssessmentRevision =
                    serde_json::from_str(&row.try_get::<String>("", "revision_json")?)?;
                ensure!(!revision.selected_on_submission && revision.selection_reason == "authority_changed");
                let heads = fixture.db.query_all(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT revision_id FROM acp_effective_assessments WHERE installation_id=? AND tenant_id=?",
                    [fixture.installation.as_str().into(), fixture.tenant.as_str().into()],
                )).await?;
                ensure!(heads.is_empty(), "permission loss selected the paid result");
                ensure!(current.review_run().await.is_err());
                ensure!(current.assess_run(&view.scoring.evidence.checkpoint_id, None).await.is_err());
                ensure!(!judge.extra_request()
                    && provider.received_requests().await.context("subject requests")?.len() == 1
                    && fixture.service.current().is_ok());
                Ok::<_, anyhow::Error>(())
            }.await;
            finish(&fixture, session, judge, checked).await?;
        }
        Ok(())
    })).await?
}
