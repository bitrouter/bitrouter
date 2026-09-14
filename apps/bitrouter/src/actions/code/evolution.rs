//! Local operational ports and presentation data for checkpoint evaluation.
//! The conversation driver owns draft lifetime; all daemon I/O stays here.

use anyhow::{Context, Result, bail, ensure};
use bitrouter_tui::code::{Inspector, Selector, SelectorRow};

use crate::daemon::{DaemonCommand, DaemonResponse, send_command};
use crate::evolution::control::EvolutionMode;
use crate::evolution::control::restoration::RestoreRequest;
use crate::evolution::operator::checkpoint::{CheckpointAction, CheckpointReport};
use crate::evolution::operator::{EvolutionOperation, EvolutionReport, EvolutionStatus};
use crate::evolution::runtime::candidates::{CandidateAction, CandidateReport};

use super::CodeServices;
use candidate::CandidateDraft;
use manual::ReviewDraft;

pub(crate) mod candidate;
pub(crate) mod manual;

/// The selector carries an immutable review target through reason entry and
/// confirmation. A failed submission keeps that target instead of rebasing it.
#[derive(serde::Serialize, serde::Deserialize)]
struct RestorePreview {
    request: RestoreRequest,
    baseline_routes: Vec<(String, String)>,
}

impl RestorePreview {
    fn text(&self) -> String {
        let mut text = format!(
            "Policy block: {}\nExperiment: {}\nReviewed revision: {}\n\nWithdraw this candidate and return to the last supported baseline.\n",
            self.request.block, self.request.expected_experiment, self.request.expected_revision
        );
        for (selector, route) in &self.baseline_routes {
            text.push_str(&format!("{selector} → {route}\n"));
        }
        text.push_str("\nBaseline support is checked again when requests are routed. If route dependencies have changed, configured routing applies. Session overrides keep their precedence. Requests already dispatched are not restarted.\n\nThe current experiment stays withdrawn. Feedback history and unrelated blocks are retained. Evolution mode stays unchanged; this action also works while Off.\n");
        if !self.request.reason.is_empty() {
            text.push_str(&format!("\nYour reason: {}\n", self.request.reason));
        }
        text
    }
}

#[derive(Clone)]
pub(crate) struct SessionRef {
    pub source: String,
    pub session_id: String,
}

pub(crate) struct Panel {
    pub selector: Option<Selector>,
    pub inspector: Option<Inspector>,
    pub review: Option<ReviewDraft>,
    pub candidate: Option<CandidateDraft>,
    pub clear_drafts: bool,
}

impl Panel {
    fn selector(selector: Selector) -> Self {
        Self {
            selector: Some(selector),
            inspector: None,
            review: None,
            candidate: None,
            clear_drafts: false,
        }
    }
    fn inspector(title: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            selector: None,
            inspector: Some(Inspector::new(title, content)),
            review: None,
            candidate: None,
            clear_drafts: false,
        }
    }
}

fn mode_name(mode: EvolutionMode) -> &'static str {
    match mode {
        EvolutionMode::Off => "Off",
        EvolutionMode::Manual => "Manual",
        EvolutionMode::Automatic => "Automatic",
    }
}

fn status_text(status: &EvolutionStatus) -> String {
    let mut text = format!(
        "Evolution: {}\nJudge: {}\nBackground worker: {}\n",
        mode_name(status.control.mode),
        status
            .control
            .judge_model
            .as_deref()
            .unwrap_or("not configured"),
        if status.worker.running {
            "running"
        } else {
            "not running"
        }
    );
    text.push_str(match status.control.mode {
        EvolutionMode::Off => "New automatic feedback and exploration are disabled. Adopted baselines remain in use.\n",
        EvolutionMode::Manual => "Recorded stops create checkpoints. Supply rubric feedback manually.\n",
        EvolutionMode::Automatic => "Recorded stops are judged in the background. The judge has no product token or cost cap.\n",
    });
    if let Some(report) = &status.worker.last_report {
        text.push_str(&format!(
            "\nLast pass: {} checkpoints, {} completed evaluations, {} routing updates\n",
            report.checkpoints_created, report.jobs_completed, report.publications
        ));
        for (scope, error) in &report.errors {
            text.push_str(&format!("Attention: {scope}: {error}\n"));
        }
    }
    text.push_str(&format!(
        "\nJudge overhead across recorded sessions: {}\nRetries (included above): {}\n",
        cost_text(&status.judge_costs.summary),
        cost_text(&status.judge_costs.retries)
    ));
    text.push_str("Amounts use current metering evidence and may change after reconciliation. Coding spend is separate. This report does not establish net routing savings.\n");
    if status.judge_costs.without_job_details.requests != 0 {
        text.push_str(&format!(
            "Retained attempts without job details (included above): {}\n",
            cost_text(&status.judge_costs.without_job_details)
        ));
    }
    text.push_str("\nPolicy blocks:\n");
    if status.control.blocks.is_empty() {
        text.push_str(
            "No policy experiments registered. Feedback alone does not create candidate routes.\n",
        );
    }
    for (id, block) in &status.control.blocks {
        text.push_str(&format!(
            "{id}: {:?}\n  Experiment: {}\n",
            block.status, block.experiment_id
        ));
        for rule in &block.definition.rules {
            text.push_str(&format!(
                "  {}: {} → {}\n",
                rule.selector, rule.baseline_route, rule.challenger_route
            ));
        }
    }
    text.push_str(&format!(
        "Archived experiments: {}. Open Experiment history to inspect their separate evidence.\n",
        status.control.archived_experiments.len()
    ));
    text.push_str("\nRecorded evaluation jobs:\n");
    for job in status.jobs.iter().rev().take(30) {
        text.push_str(&format!(
            "{} · {}: {:?}, {} request attempts{}\n",
            job.source,
            job.session_id,
            job.status,
            job.attempted_requests,
            job.error_code
                .as_ref()
                .map(|e| format!(", {e}"))
                .unwrap_or_default()
        ));
        if let Some(costs) = status.judge_costs.jobs.get(&job.job_id) {
            text.push_str(&format!("  Judge cost: {}\n", cost_text(&costs.summary)));
            for attempt in &costs.attempts {
                if !attempt.incomplete_reasons.is_empty() {
                    text.push_str(&format!(
                        "  {}: {}\n",
                        attempt.request_id,
                        attempt
                            .incomplete_reasons
                            .iter()
                            .map(|reason| cost_reason(reason))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ));
                }
            }
        }
    }
    if status.jobs.len() > 30 {
        text.push_str("Showing 30 jobs; CLI status exposes the full inventory.\n");
    }
    text
}

fn cost_text(cost: &crate::evolution::costs::report::CostSummary) -> String {
    let amount = |value: u64| format!("${}.{:06}", value / 1_000_000, value % 1_000_000);
    match cost.total_cost_micro_usd {
        Some(total) => format!(
            "{} metered estimate, {} requests with complete cost evidence",
            amount(total),
            cost.requests
        ),
        None => format!(
            "{} known metered subtotal; total unknown ({} of {} requests incomplete)",
            amount(cost.known_cost_micro_usd),
            cost.incomplete_requests,
            cost.requests
        ),
    }
}

fn cost_reason(reason: &str) -> &str {
    match reason {
        "dispatch_inventory_missing" => "Historical dispatch details are unavailable",
        "dispatch_not_confirmed" => "Request dispatch is not confirmed",
        "terminal_settlement_missing" => "The request has no confirmed final settlement",
        "pipeline_usage_not_observed" | "usage_not_observed" => "Usage was not directly observed",
        "upstream_attempt_unfinished" => "An upstream attempt did not finish",
        "metered_target_mismatch" => "Metering does not match the executed model",
        "upstream_attempt_coverage_unknown" => "Upstream attempt coverage is unknown",
        "earlier_upstream_attempt_costs_missing" => "Earlier fallback attempts have missing costs",
        "metering_record_missing" => "No metering record is available",
        "charge_evidence_unknown" => "Usage or pricing evidence is incomplete",
        "reconciliation_pending" => "Waiting for cost reconciliation",
        "reconciliation_unknown" => "Cost reconciliation is unresolved",
        _ => reason,
    }
}

fn learning_text(report: &EvolutionReport) -> Result<String> {
    let EvolutionReport::Learning(report) = report else {
        bail!("Unexpected policy evidence response")
    };
    let plan = &report.plan;
    let action = match plan.recommendation {
        crate::evolution::bandit::Recommendation::Explore => "Continue controlled exploration",
        crate::evolution::bandit::Recommendation::Hold => "Wait for more comparable evidence",
        crate::evolution::bandit::Recommendation::Promote => "Adopt the candidate",
        crate::evolution::bandit::Recommendation::Rollback => "Withdraw the candidate",
    };
    let mut text = format!(
        "Policy block: {}\nOriginal trial recommendation: {action}\nReason: {}\nRouting update published: {}\nTrial cohort resolved: {}\nTrial candidate allocation: {:.1}%\n",
        report.block_id,
        plan.reason,
        report.published,
        report.cohort_resolved,
        f64::from(plan.challenger_propensity_ppm) / 10_000.0
    );
    text.push_str(&format!("Current block state: {:?}\n", report.block_status));
    text.push_str(&format!("Experiment: {}\n", report.experiment_id));
    if report.archived {
        text.push_str("Archived experiment: no new enrollments or new adoption. Late feedback can still withdraw its adoption and dependent later versions.\n");
    }
    if let Some(minimum) = report.minimum_families_per_arm {
        text.push_str(&format!("\nEvidence minimum: {minimum} session groups per arm for each measure. Reaching this count does not by itself permit adoption; quality and resource criteria must also pass.\n"));
    }
    for (name, arm) in [
        ("Baseline", &plan.baseline),
        ("Candidate", &plan.challenger),
    ] {
        text.push_str(&format!(
            "\n{name}: {} assigned sessions, {} incomplete, {} material violations\n",
            arm.assigned_sessions, arm.incomplete_sessions, arm.severe_sessions
        ));
        text.push_str(&format!(
            "Comparable session groups: quality {}, cost {}, duration {}\n",
            arm.quality.observed_families,
            arm.log_cost.observed_families,
            arm.log_latency.observed_families
        ));
        if let Some(cost) = arm.arithmetic_mean_cost_micro_usd {
            text.push_str(&format!(
                "Observed mean session cost: ${:.4}\n",
                cost / 1_000_000.0
            ));
        } else {
            text.push_str("Complete comparable session cost is unavailable.\n");
        }
    }
    for (session, reasons) in &report.unavailable {
        text.push_str(&format!(
            "\nUnavailable evidence · {session}: {}",
            reasons.join(", ")
        ));
    }
    let monitoring = &report.monitoring;
    text.push_str(&format!("\n\nAfter-adoption quality monitoring: {} sessions, {} recent families, {} with incomplete feedback\n{}\n", monitoring.sessions, monitoring.recent_families, monitoring.incomplete_recent_families, monitoring.reason));
    if monitoring.rollback {
        text.push_str("Quality alarm: the adopted policy should be withdrawn. This alarm takes precedence over the original trial's recommendation.\n");
    }
    text.push_str("Deployment monitoring does not add randomized evidence to the original trial or establish ongoing comparative cost savings.\n");
    text.push_str("\nRelated forks share a session group. Assignments, repeat assessments and prior strength do not add observed groups. Counts reflect usable evidence for each measure; they are not proof of statistical independence. Trial allocation is subject to exposure and pending-feedback limits. Reconciliation rechecks current evidence and routes before publishing.\n");
    text.push_str(&format!(
        "\nCurrent revision: {}\nPublication history:\n",
        report.block_revision
    ));
    if report.publications.is_empty() {
        text.push_str("No routing updates published for this experiment.\n");
    }
    for publication in &report.publications {
        let action = match publication.action.as_str() {
            "operator_restore" => "Operator restored the supported baseline",
            "promote" => "Candidate adopted",
            "rollback" | "adopted_quality_rollback" => "Candidate withdrawn after feedback",
            "withdraw_unsupported_promotion" => "Adoption withdrawn after evidence changed",
            "inherited_baseline_withdrawn" => "Inherited baseline withdrawn",
            "experiment_revised" => "Next experiment registered",
            "allocation" => "Trial allocation updated",
            "adoption_revalidated" => "Adoption revalidated",
            "archived_evidence" => "Archived evidence updated",
            other => other,
        };
        text.push_str(&format!("{} · {action}\n", publication.recorded_at));
        if let Some(reason) = &publication.operator_reason {
            text.push_str(&format!("  Reason: {reason}\n"));
        }
    }
    Ok(text)
}

fn menu(
    status: &EvolutionStatus,
    session: Option<&SessionRef>,
    review: Option<&ReviewDraft>,
    candidate: Option<&CandidateDraft>,
) -> Selector {
    let mut checkpoints = SelectorRow::new(
        "checkpoints",
        "Session checkpoints",
        "Inspect recorded evidence or create a manual evaluation",
    );
    if session.is_none() {
        checkpoints = checkpoints.unavailable("Connect a recorded local ACP session first");
    }
    if review.is_some() || candidate.is_some() {
        checkpoints =
            checkpoints.unavailable("Finish or discard the current evolution draft first");
    }
    let mut rows = vec![
        SelectorRow::new(
            "status",
            "Evolution status",
            "Feedback jobs, policy blocks and background progress",
        ),
        SelectorRow::new("mode", "Evolution mode", mode_name(status.control.mode)),
        SelectorRow::new(
            "judge",
            "Judge model",
            status
                .control
                .judge_model
                .as_deref()
                .unwrap_or("Not configured"),
        ),
        checkpoints,
        SelectorRow::new(
            "blocks",
            "Policy block evidence",
            "Inspect evidence and reconcile eligible improvements",
        ),
    ];
    let mut create = SelectorRow::new(
        "candidate",
        "Create a candidate experiment",
        "Choose route changes, review the trial and register it",
    );
    if session.is_none() {
        create = create.unavailable("Connect a local ACP session to select its agent scope");
    } else if review.is_some() || candidate.is_some() {
        create = create.unavailable("Finish or discard the current evolution draft first");
    }
    rows.push(create);
    let mut revise = SelectorRow::new(
        "revise",
        "Start the next experiment",
        "Inherit a block's supported baseline and review new candidate routes",
    );
    if session.is_none() || review.is_some() || candidate.is_some() {
        revise = revise.unavailable("Connect a local ACP session and finish any open draft first");
    }
    rows.push(revise);
    rows.push(SelectorRow::new(
        "history",
        "Experiment history",
        "Inspect archived trials and any later withdrawal of their evidence",
    ));
    if candidate.is_some() {
        rows.push(SelectorRow::new(
            "resume_candidate",
            "Resume candidate draft",
            "Unsaved policy experiment",
        ));
    }
    if review.is_some() {
        rows.push(SelectorRow::new(
            "review",
            "Resume manual evaluation",
            "Unsaved checkpoint draft",
        ));
    }
    Selector::new(
        "evolution:menu",
        "Checkpoint evaluation and evolution",
        "Settings apply to this local router. Recorded evidence drives feedback after execution.",
        rows,
    )
}

impl CodeServices {
    pub fn evolution_available(&self) -> bool {
        !self.operations_only && self.target.local_socket().is_some()
    }

    async fn evolution_request(&self, operation: EvolutionOperation) -> Result<EvolutionReport> {
        ensure!(
            self.evolution_available(),
            "Checkpoint evolution requires the local coding environment"
        );
        let socket = self
            .target
            .local_socket()
            .context("No local serving daemon is selected")?;
        match send_command(socket, &DaemonCommand::Evolution { operation })
            .await
            .context("Checkpoint evolution requires the local serving daemon")?
        {
            DaemonResponse::Evolution { report } => Ok(*report),
            DaemonResponse::Error { message } => bail!(message),
            _ => bail!("The selected daemon does not support checkpoint evolution"),
        }
    }

    async fn evolution_status(&self) -> Result<Box<EvolutionStatus>> {
        match self.evolution_request(EvolutionOperation::Status).await? {
            EvolutionReport::Status(status) => Ok(status),
            _ => bail!("Unexpected evolution status response"),
        }
    }

    async fn checkpoint_request(
        &self,
        session: &SessionRef,
        action: CheckpointAction,
    ) -> Result<CheckpointReport> {
        match self
            .evolution_request(EvolutionOperation::Checkpoint {
                source: session.source.clone(),
                session_id: session.session_id.clone(),
                action,
            })
            .await?
        {
            EvolutionReport::Checkpoint(report) => Ok(*report),
            _ => bail!("Unexpected checkpoint response"),
        }
    }

    async fn candidate_request(&self, action: CandidateAction) -> Result<CandidateReport> {
        match self
            .evolution_request(EvolutionOperation::Candidate { action })
            .await?
        {
            EvolutionReport::Candidate(report) => Ok(*report),
            _ => bail!("Unexpected candidate response"),
        }
    }

    pub async fn evolution_step(
        &self,
        selector: &str,
        choice: &str,
        session: Option<SessionRef>,
        draft: Option<ReviewDraft>,
        candidate: Option<CandidateDraft>,
    ) -> Result<Panel> {
        ensure!(
            self.evolution_available(),
            "Checkpoint evolution requires the local coding environment"
        );
        ensure!(
            draft.is_none() || candidate.is_none(),
            "Finish the current evolution draft first"
        );
        // A disconnected coding transport does not invalidate an explicit
        // local review. Its frozen identity remains authoritative for feedback.
        let session = session
            .or_else(|| {
                draft.as_ref().map(|draft| SessionRef {
                    source: draft.input.identity.source.clone(),
                    session_id: draft.input.identity.native_session_id.clone(),
                })
            })
            .or_else(|| candidate.as_ref().map(|draft| draft.session.clone()));
        if let Some(candidate) = &candidate {
            ensure!(
                session
                    .as_ref()
                    .is_some_and(|s| s.source == candidate.session.source
                        && s.session_id == candidate.session.session_id),
                "The candidate draft belongs to a different native session"
            );
        }
        if let Some(draft) = &draft {
            ensure!(
                session
                    .as_ref()
                    .is_some_and(|session| session.source == draft.input.identity.source
                        && session.session_id == draft.input.identity.native_session_id),
                "The review belongs to a different native session"
            );
        }
        match selector {
            "evolution:menu" => {
                let status = self.evolution_status().await?;
                match choice {
                    "open" => Ok(Panel::selector(menu(
                        &status,
                        session.as_ref(),
                        draft.as_ref(),
                        candidate.as_ref(),
                    ))),
                    "status" => {
                        let mut panel = Panel::selector(menu(
                            &status,
                            session.as_ref(),
                            draft.as_ref(),
                            candidate.as_ref(),
                        ));
                        panel.inspector =
                            Some(Inspector::new("Evolution status", status_text(&status)));
                        Ok(panel)
                    }
                    "mode" => {
                        let mut automatic = SelectorRow::new(
                            "automatic",
                            "Automatic",
                            "Judge recorded checkpoints and evolve from comparable feedback",
                        );
                        if status.control.judge_model.is_none() {
                            automatic = automatic.unavailable("Choose a judge model first");
                        }
                        Ok(Panel::selector(Selector::new(
                            "evolution:mode",
                            "Evolution mode",
                            format!(
                                "Current: {}. Changes apply to future feedback and supported routing boundaries.",
                                mode_name(status.control.mode)
                            ),
                            vec![
                                SelectorRow::new(
                                    "off",
                                    "Off",
                                    "Stop automatic feedback and unpromoted exploration",
                                ),
                                SelectorRow::new(
                                    "manual",
                                    "Manual",
                                    "Supply rubric feedback yourself; no model judge",
                                ),
                                automatic,
                            ],
                        )))
                    }
                    "judge" => {
                        let models = self.target.models(None).await?;
                        Ok(Panel::selector(Selector::new("evolution:judge", "Judge model", "Choose a configured model. This changes the evaluator while preserving the current evolution mode.", models.models.into_iter().map(|model| SelectorRow::new(&model.id, &model.id, model.providers.join(", "))).collect()).allow_custom("Configured model or policy selector")))
                    }
                    "checkpoints" => {
                        ensure!(
                            draft.is_none() && candidate.is_none(),
                            "Finish or discard the current evolution draft first"
                        );
                        let session = session
                            .as_ref()
                            .context("Connect a recorded local ACP session first")?;
                        match self
                            .checkpoint_request(session, CheckpointAction::List)
                            .await?
                        {
                            CheckpointReport::List {
                                checkpoints,
                                effective,
                            } => {
                                let mut rows = vec![SelectorRow::new(
                                    format!("freeze:{}", effective.current_watermark),
                                    "Evaluate the current recorded prefix",
                                    "Freeze a checkpoint and open a manual draft; no model call",
                                )];
                                for checkpoint in checkpoints.iter().rev() {
                                    rows.push(SelectorRow::new(
                                        &checkpoint.checkpoint_id,
                                        format!(
                                            "Prefix {} · {}",
                                            checkpoint.watermark, checkpoint.created_at
                                        ),
                                        if checkpoint.watermark == effective.current_watermark {
                                            "Current recorded prefix"
                                        } else {
                                            "Historical prefix · newer content exists"
                                        },
                                    ));
                                }
                                Ok(Panel::selector(Selector::new(
                                    "evolution:checkpoint",
                                    "Session checkpoints",
                                    format!(
                                        "{} · {} recorded events. Recording must be enabled. Existing drafts can correct stored assessments.",
                                        session.session_id, effective.current_watermark
                                    ),
                                    rows,
                                )))
                            }
                            _ => bail!("Unexpected checkpoint list"),
                        }
                    }
                    "review" => Ok(Panel::selector(
                        draft.as_ref().context("No manual review is open")?.menu(),
                    )),
                    "candidate" => {
                        ensure!(
                            draft.is_none() && candidate.is_none(),
                            "Finish or discard the current evolution draft first"
                        );
                        let session = session
                            .clone()
                            .context("Connect a local ACP session first")?;
                        let CandidateReport::Catalog(catalog) =
                            self.candidate_request(CandidateAction::Catalog).await?
                        else {
                            bail!("Unexpected candidate catalog response")
                        };
                        let candidate = CandidateDraft::new(session, catalog);
                        let mut panel = Panel::selector(candidate.menu());
                        panel.candidate = Some(candidate);
                        Ok(panel)
                    }
                    "resume_candidate" => Ok(Panel::selector(
                        candidate
                            .as_ref()
                            .context("No candidate draft is open")?
                            .menu(),
                    )),
                    "revise" => {
                        ensure!(
                            draft.is_none() && candidate.is_none(),
                            "Finish or discard the current evolution draft first"
                        );
                        let session = session
                            .as_ref()
                            .context("Connect a local ACP session first")?;
                        Ok(Panel::selector(Selector::new(
                            "evolution:revise",
                            "Select policy block",
                            "A revision preserves the block's routing matchers and retains prior evidence.",
                            status
                                .control
                                .blocks
                                .iter()
                                .filter(|(_, block)| block.definition.source == session.source)
                                .map(|(id, block)| {
                                    SelectorRow::new(id, id, format!("{:?}", block.status))
                                })
                                .collect(),
                        )))
                    }
                    "history" => {
                        let rows = status
                            .control
                            .archived_experiments
                            .values()
                            .map(|archive| {
                                let block = &archive.block;
                                Ok(SelectorRow::new(
                                    serde_json::to_string(&(
                                        &block.definition.block_id,
                                        &block.experiment_id,
                                    ))?,
                                    format!(
                                        "{} · {}",
                                        block.definition.block_id, archive.retired_at
                                    ),
                                    format!("{:?}", block.status),
                                ))
                            })
                            .collect::<Result<Vec<_>>>()?;
                        Ok(Panel::selector(Selector::new(
                            "evolution:history",
                            "Experiment history",
                            "Each version retains its own session assignments and evidence.",
                            rows,
                        )))
                    }
                    "blocks" => Ok(Panel::selector(Selector::new(
                        "evolution:block",
                        "Policy blocks",
                        if status.control.blocks.is_empty() {
                            "No experiments registered. Mode selection alone does not create candidate policies."
                        } else {
                            "Inspect a block's effective evidence before reconciling it."
                        },
                        status
                            .control
                            .blocks
                            .iter()
                            .map(|(id, block)| {
                                SelectorRow::new(id, id, format!("{:?}", block.status))
                            })
                            .collect(),
                    ))),
                    _ => bail!("Unknown evolution action"),
                }
            }
            "evolution:mode" => {
                let mode = match choice {
                    "off" => EvolutionMode::Off,
                    "manual" => EvolutionMode::Manual,
                    "automatic" => EvolutionMode::Automatic,
                    _ => bail!("Unknown evolution mode"),
                };
                match self
                    .evolution_request(EvolutionOperation::Mode {
                        mode,
                        judge_model: None,
                    })
                    .await?
                {
                    EvolutionReport::Status(status) => Ok(Panel::selector(menu(
                        &status,
                        session.as_ref(),
                        draft.as_ref(),
                        candidate.as_ref(),
                    ))),
                    _ => bail!("Unexpected mode response"),
                }
            }
            "evolution:judge" => {
                ensure!(!choice.trim().is_empty(), "Choose a configured judge model");
                // Validate the selector without invoking a model or altering routes.
                let route = self
                    .target
                    .route(bitrouter_mcp::actions::route::RouteInput {
                        model: choice.to_owned(),
                        prompt: None,
                    })
                    .await?;
                ensure!(
                    !route.provider_chain.is_empty(),
                    "This judge selector has no configured provider"
                );
                match self
                    .evolution_request(EvolutionOperation::JudgeModel {
                        model: choice.to_owned(),
                    })
                    .await?
                {
                    EvolutionReport::Status(status) => Ok(Panel::selector(menu(
                        &status,
                        session.as_ref(),
                        draft.as_ref(),
                        candidate.as_ref(),
                    ))),
                    _ => bail!("Unexpected judge setting response"),
                }
            }
            "evolution:checkpoint" => {
                ensure!(
                    draft.is_none() && candidate.is_none(),
                    "Finish or discard the current evolution draft first"
                );
                let session = session
                    .as_ref()
                    .context("Connect a recorded local ACP session first")?;
                let action = if let Some(head) = choice.strip_prefix("freeze:") {
                    CheckpointAction::Freeze {
                        expected_watermark: head.parse()?,
                    }
                } else {
                    CheckpointAction::Review {
                        checkpoint_id: choice.to_owned(),
                    }
                };
                match self.checkpoint_request(session, action).await? {
                    CheckpointReport::Review(input) => {
                        let draft = ReviewDraft::new(*input);
                        let mut panel = Panel::selector(draft.menu());
                        panel.review = Some(draft);
                        Ok(panel)
                    }
                    _ => bail!("Unexpected review response"),
                }
            }
            "evolution:submit" if choice == "save" => {
                let draft = draft.context("No manual evaluation is open")?;
                let session = session
                    .as_ref()
                    .context("The recorded session is no longer connected")?;
                match self
                    .checkpoint_request(
                        session,
                        CheckpointAction::Submit {
                            submission: Box::new(draft.submission()?),
                        },
                    )
                    .await?
                {
                    CheckpointReport::Receipt { receipt, effective } => {
                        let status = if effective.current_revision.as_deref()
                            != Some(receipt.revision.revision_id.as_str())
                        {
                            "Stored in history; another assessment is currently selected."
                                .to_owned()
                        } else if effective.stale {
                            format!(
                                "Saved for this prefix but excluded from current learning: {}. New content is not evaluated by this submission.",
                                effective.reasons.join(", ")
                            )
                        } else {
                            "Selected for the current recorded prefix. Learning still requires comparable feedback and complete resource evidence.".to_owned()
                        };
                        let mut panel = Panel::inspector(
                            "Manual evaluation saved",
                            format!(
                                "Checkpoint: {}\nRevision: {}\n{}\nQuality: {:.2}–{:.2}\n{}",
                                receipt.revision.input.checkpoint_id,
                                receipt.revision.revision_id,
                                status,
                                f64::from(receipt.quality.lower_ppm) / 1_000_000.0,
                                f64::from(receipt.quality.upper_ppm) / 1_000_000.0,
                                receipt.revision.selection_reason
                            ),
                        );
                        panel.clear_drafts = true;
                        Ok(panel)
                    }
                    _ => bail!("Unexpected manual scoring receipt"),
                }
            }
            "evolution:revise" => {
                ensure!(
                    draft.is_none() && candidate.is_none(),
                    "Finish or discard the current evolution draft first"
                );
                let session = session.context("Connect a local ACP session first")?;
                let CandidateReport::Catalog(catalog) =
                    self.candidate_request(CandidateAction::Catalog).await?
                else {
                    bail!("Unexpected candidate catalog response")
                };
                let CandidateReport::Revision(spec) = self
                    .candidate_request(CandidateAction::Revision {
                        block_id: choice.into(),
                    })
                    .await?
                else {
                    bail!("Unexpected experiment revision response")
                };
                let candidate = CandidateDraft::revise(session, catalog, *spec)?;
                let mut panel = Panel::selector(candidate.menu());
                panel.candidate = Some(candidate);
                Ok(panel)
            }
            "evolution:block" | "evolution:history" => {
                let (block, experiment) = if selector == "evolution:history" {
                    let (block, experiment): (String, String) = serde_json::from_str(choice)?;
                    (block, Some(experiment))
                } else {
                    (choice.to_owned(), None)
                };
                let report = self
                    .evolution_request(EvolutionOperation::Learning { block, experiment })
                    .await?;
                let content = learning_text(&report)?;
                let EvolutionReport::Learning(learning) = &report else {
                    bail!("Unexpected learning response")
                };
                let target = serde_json::to_string(&(&learning.block_id, &learning.experiment_id))?;
                let mut rows = vec![SelectorRow::new(
                    target,
                    "Reconcile this block",
                    "May update exploration, adopt a candidate or withdraw it",
                )];
                if !learning.archived
                    && learning.block_status != crate::evolution::control::BlockStatus::RolledBack
                {
                    rows.push(SelectorRow::new(
                        format!(
                            "restore:{}",
                            serde_json::to_string(&RestoreRequest {
                                block: learning.block_id.clone(),
                                expected_experiment: learning.experiment_id.clone(),
                                expected_revision: learning.block_revision.clone(),
                                reason: String::new(),
                            })?
                        ),
                        "Restore supported baseline",
                        "Withdraw this candidate after reviewing the target and your reason",
                    ));
                }
                let mut panel = Panel::selector(Selector::new(
                    "evolution:improve",
                    "Policy block actions",
                    "The current evidence and live configuration are checked again before any update.",
                    rows,
                ));
                panel.inspector = Some(Inspector::new("Policy block evidence", content));
                Ok(panel)
            }
            "evolution:improve" if choice.starts_with("restore:") => {
                let request: RestoreRequest =
                    serde_json::from_str(choice.trim_start_matches("restore:"))?;
                let status = self.evolution_status().await?;
                let block = status
                    .control
                    .blocks
                    .get(&request.block)
                    .context("Unknown policy block")?;
                ensure!(
                    block.experiment_id == request.expected_experiment
                        && block.revision == request.expected_revision,
                    "Policy block changed; reopen its evidence before restoring"
                );
                let (baseline, _) = status.control.baseline_source(block)?;
                let preview = RestorePreview {
                    request,
                    baseline_routes: baseline
                        .definition
                        .rules
                        .iter()
                        .map(|rule| (rule.selector.clone(), rule.baseline_route.clone()))
                        .collect(),
                };
                Ok(Panel::selector(Selector::new(
                    format!("evolution:restore:reason:{}", serde_json::to_string(&preview)?),
                    "Why restore this baseline?",
                    "The reason is retained in this block's publication history. Review before submitting.",
                    Vec::new(),
                ).allow_custom("Enter your reason")))
            }
            _ if selector.starts_with("evolution:restore:reason:") => {
                ensure!(
                    !choice.trim().is_empty(),
                    "Enter a reason before reviewing the withdrawal"
                );
                let mut preview: RestorePreview =
                    serde_json::from_str(selector.trim_start_matches("evolution:restore:reason:"))?;
                preview.request.reason = choice.to_owned();
                let mut panel = Panel::selector(Selector::new(
                    "evolution:restore:submit",
                    "Confirm baseline restoration",
                    "Submit only this reviewed withdrawal. A changed target must be reviewed again.",
                    vec![SelectorRow::new(
                        serde_json::to_string(&preview.request)?,
                        "Restore this baseline",
                        "Withdraw the reviewed experiment and save the reason",
                    )],
                ));
                panel.inspector = Some(Inspector::new(
                    "Review baseline restoration",
                    preview.text(),
                ));
                Ok(panel)
            }
            "evolution:restore:submit" => {
                let request: RestoreRequest = serde_json::from_str(choice)?;
                let report = self
                    .evolution_request(EvolutionOperation::Restore {
                        request: request.clone(),
                    })
                    .await?;
                let EvolutionReport::Status(status) = report else {
                    bail!("Unexpected restoration response")
                };
                let receipt = status
                    .control
                    .publications
                    .iter()
                    .find(|publication| {
                        publication.block_id == request.block
                            && publication.experiment_id.as_deref()
                                == Some(&request.expected_experiment)
                            && publication.previous_revision == request.expected_revision
                            && publication.action == "operator_restore"
                            && publication.operator_reason.as_deref() == Some(&request.reason)
                    })
                    .context("Withdrawal receipt missing")?;
                Ok(Panel::inspector(
                    "Policy block withdrawal",
                    format!(
                        "Withdrawal recorded: {}\nBlock: {}\nExperiment: {}\nReason: {}\n\nCurrent router status (may include later changes):\n{}",
                        receipt.recorded_at,
                        request.block,
                        request.expected_experiment,
                        request.reason,
                        status_text(&status)
                    ),
                ))
            }
            "evolution:improve" => {
                let (block, experiment): (String, String) = serde_json::from_str(choice)?;
                let report = self
                    .evolution_request(EvolutionOperation::Improve {
                        block,
                        experiment: Some(experiment),
                    })
                    .await?;
                Ok(Panel::inspector(
                    "Policy block reconciliation",
                    learning_text(&report)?,
                ))
            }
            "evolution:candidate:form" if matches!(choice, "add" | "feedback") => {
                let mut candidate = candidate.context("Create a candidate draft first")?;
                let CandidateReport::Catalog(catalog) =
                    self.candidate_request(CandidateAction::Catalog).await?
                else {
                    bail!("Unexpected candidate catalog response")
                };
                candidate.catalog = catalog;
                candidate.step(selector, choice)
            }
            "evolution:candidate:form" | "evolution:candidate:submit" if choice == "preview" => {
                let candidate = candidate.context("Create a candidate draft first")?;
                let CandidateReport::Preview(preview) = self
                    .candidate_request(CandidateAction::Preview {
                        spec: Box::new(candidate.spec.clone()),
                    })
                    .await?
                else {
                    bail!("Unexpected candidate preview response")
                };
                Ok(candidate.reviewed(*preview))
            }
            "evolution:candidate:submit" if choice == "register" => {
                let candidate = candidate.context("Create a candidate draft first")?;
                let preview = candidate
                    .preview
                    .context("Review the candidate before registering")?;
                let CandidateReport::Registered {
                    block_id,
                    status,
                    mode,
                    ..
                } = self
                    .candidate_request(CandidateAction::Register {
                        preview: Box::new(preview),
                    })
                    .await?
                else {
                    bail!("Unexpected candidate registration response")
                };
                let mut panel = Panel::inspector(
                    "Candidate registered",
                    format!(
                        "Experiment: {block_id}\nState: {status:?}\nEvolution mode: {}\n\nFuture recorded sessions can enter this trial when evolution is enabled. Inspect its progress under Policy block evidence.",
                        mode_name(mode)
                    ),
                );
                panel.clear_drafts = true;
                Ok(panel)
            }
            _ if selector.starts_with("evolution:candidate:") => candidate
                .context("Create a candidate draft first")?
                .step(selector, choice),
            _ => draft
                .context("Open a checkpoint evaluation first")?
                .step(selector, choice),
        }
    }
}

#[cfg(test)]
mod tests;
