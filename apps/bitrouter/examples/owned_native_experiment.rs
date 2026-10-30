//! An embedding already owns the authenticated App and protected control client.
//! No listener, credential constructor, configuration activation or retry is hidden
//! in this handler. The host still owns Service/listener shutdown.
use anyhow::{Context, Result};
use bitrouter::{
    core::optimization::cohort::CohortAssessment,
    gateway::assembly::App,
    harness::resources::{
        HarnessConfig,
        managed::session::{NativeApproval, NativeSession},
    },
};
use bitrouter_ai::{
    core::protocol::TaskInput,
    eval::FreezeRequest,
    harness::agent::ToolMode,
    tenancy::identity::{InstallationId, TenantId},
};
use bitrouter_sdk::tenancy::TenancyClient;
use std::{path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;

/// Run one explicit unit, freeze its tenant's ledger through the existing bounded
/// protected transport, and read the actual owned producer closure locally.
/// The retained route must already be an authorized Explore experiment profile.
/// Operational Inconclusive evidence never becomes a quality Pass here.
pub struct Embedding<'a> {
    pub app: Arc<App>,
    pub headers: http::HeaderMap,
    pub control: &'a TenancyClient,
    pub installation: &'a InstallationId,
    pub tenant: &'a TenantId,
}

pub async fn run_and_assess(
    embedding: Embedding<'_>,
    workspace: &Path,
    local_name: &str,
    input: TaskInput,
    approval: Arc<dyn NativeApproval>,
    cancel: CancellationToken,
) -> Result<CohortAssessment> {
    let Embedding {
        app,
        headers,
        control,
        installation,
        tenant,
    } = embedding;
    let model = input.model.clone();
    let mut session = NativeSession::open_authenticated(
        app,
        headers,
        local_name,
        workspace,
        ToolMode::ReadOnly,
        &HarnessConfig::default(),
    )
    .await?;
    let result = async {
        session.run(input, approval, cancel).await?;
        // None chooses the server clock once; an explicit decoded spelling would
        // be preserved verbatim by the existing frozen-root contract.
        let snapshot = control
            .freeze_eval_snapshot(installation, tenant, &FreezeRequest { frozen_at: None })
            .await?;
        Ok::<_, anyhow::Error>(
            session
                .experiment_cohort(&model, &snapshot.evidence_root)
                .await?,
        )
    }
    .await;
    // A response error is not evidence that a native/provider effect ended.
    // Join the original owner even if freeze/read/delivery failed; never retry it.
    session
        .close()
        .await
        .context("native owner cleanup remains unconfirmed")?;
    result
}

/// The host supplies an explicit Human decision after inspecting real native
/// evidence. The callback is UI/application code, not an inferred evaluator.
/// Keep the authenticated session open until this command and cleanup finish.
pub async fn review_and_submit<F>(
    session: &NativeSession,
    human: F,
) -> Result<bitrouter::core::evolution::scoring::ScoringReceipt>
where
    F: FnOnce(
        &bitrouter::harness::resources::managed::session::assessment::NativeRunReview,
    ) -> Result<bitrouter::core::evolution::scoring::RubricSubmission>,
{
    let review = session.review_run().await?;
    let submitted = human(&review)?;
    // The input carries the reviewed checkpoint and explicit expected revision.
    // The server validates both; this adapter does not repair stale input.
    Ok(session.submit_assessment(submitted).await?)
}

/// A later explicit Human action may retract only its current reviewed head.
pub async fn retract_review(
    session: &NativeSession,
    input: bitrouter::acp_trajectory::checkpoint::types::RevisionInput,
) -> Result<bitrouter::acp_trajectory::checkpoint::types::AssessmentRevision> {
    Ok(session.retract_assessment(input).await?)
}

/// Request one explicit paid assessment of the reviewed current run. The retained
/// experiment profile must already name a qualified static evaluator. Acceptance
/// spends one logical job from that immutable configuration revision; a caller
/// timeout or dropped future does not refund it or authorize an automatic retry.
/// The original session remains responsible for checked close after this call.
pub async fn assess_with_retained_evaluator(session: &NativeSession) -> Result<()> {
    let review = session.review_run().await?;
    session
        .assess_run(
            &review.scoring.evidence.checkpoint_id,
            review.scoring.expected_revision.as_deref(),
        )
        .await?;
    Ok(())
}
