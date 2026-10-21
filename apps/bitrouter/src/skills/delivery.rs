//! ACP delivery reuses the installed library and shared candidate policy.
//! The supervisor remains the sole owner of decisions, input identity and journal.

use anyhow::{Result, ensure};
use async_trait::async_trait;
use bitrouter_orchestrator::acp::skills::{self, Capabilities, Instructions, Prepared, Target};
use bitrouter_orchestrator::core::protocol::{CoreError, ErrorCode};
use bitrouter_orchestrator::core::skills::{
    SkillCandidate, SkillConstraints, SkillSelector, SkillSource, selection,
};
use bitrouter_orchestrator::harness::skills::library::SkillLibrary;
use bitrouter_sdk::decision_model::DecisionRuntime;
use bitrouter_sdk::decision_model::types::{
    DecisionError, DecisionFailure, DecisionRequest, DecisionResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionEvidence {
    pub request: DecisionRequest,
    pub pricing: Value,
    pub outcome: Option<Result<DecisionResponse, DecisionError>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub session_id: String,
    pub turn_id: String,
    pub stage: String,
    pub capabilities: Capabilities,
    pub constraints: SkillConstraints,
    pub candidates: Vec<SkillCandidate>,
    pub problems: Vec<String>,
    pub decision: Option<DecisionEvidence>,
    pub receipt: Option<skills::Receipt>,
    pub failure: Option<String>,
}

#[async_trait]
pub(crate) trait Journal: Send + Sync {
    /// Acknowledgement is a dispatch barrier, not a best-effort telemetry write.
    async fn save(&self, report: &Report) -> Result<()>;
}

pub(crate) struct Context {
    pub constraints: SkillConstraints,
    pub decision: Option<DecisionRuntime>,
}

impl Context {
    pub async fn prepare(
        &self,
        target: Target<'_>,
        prompt: &str,
        capabilities: &Capabilities,
        cancel: &CancellationToken,
        journal: &dyn Journal,
    ) -> Result<Option<Prepared>> {
        self.constraints.validate()?;
        ensure!(
            !self
                .constraints
                .include
                .iter()
                .any(|selector| matches!(selector, SkillSelector::Mcp { .. })),
            "unsupported: external ACP cannot preserve MCP-origin manifest/consent/cache/execution boundaries"
        );
        ensure!(
            !cancel.is_cancelled(),
            "skill delivery cancelled before discovery"
        );
        if skills::is_native_command(prompt) {
            ensure!(
                self.constraints.include.is_empty(),
                "unsupported: required skill delivery cannot preserve native slash-command semantics"
            );
            return Ok(None);
        }
        if self.constraints.include.is_empty() && self.decision.is_none() {
            return Ok(None);
        }
        let mut report = Report {
            session_id: target.session_id.into(),
            turn_id: target.turn_id.into(),
            stage: "catalogued".into(),
            capabilities: capabilities.clone(),
            constraints: self.constraints.clone(),
            candidates: vec![],
            problems: vec![],
            decision: None,
            receipt: None,
            failure: None,
        };
        if !capabilities.prompt_instructions {
            report.stage = "unsupported".into();
            report.problems.push(capabilities.reason.clone());
            journal.save(&report).await?;
            ensure!(
                self.constraints.include.is_empty(),
                "unsupported: {}",
                capabilities.reason
            );
            return Ok(None);
        }
        let policy = self
            .decision
            .as_ref()
            .map(|runtime| runtime.policy.clone())
            .unwrap_or_default();
        policy.validate().map_err(anyhow::Error::msg)?;
        let library = super::native::Library::runtime();
        let catalog = tokio::select! {
            biased;
            ()=cancel.cancelled()=>anyhow::bail!("skill discovery cancelled"),
            result=library.catalog(target.workspace,target.caller)=>result.map_err(anyhow::Error::msg)?,
        };
        report.problems = catalog
            .problems
            .iter()
            .take(32)
            .map(|problem| truncate(problem, 2048))
            .collect();
        // Required identities reserve metadata capacity before automatic choices.
        let mut sources: Vec<_> = catalog
            .skills
            .into_iter()
            .map(SkillSource::Managed)
            .collect();
        sources.sort_by_key(|source| !self.constraints.include.contains(&source.selector()));
        let mut metadata_bytes = 0_usize;
        for source in sources
            .into_iter()
            .take(policy.max_candidates.saturating_add(4))
        {
            let bytes = serde_json::to_vec(&source)?.len();
            if bytes > 64 * 1024 || metadata_bytes.saturating_add(bytes) > 512 * 1024 {
                ensure!(
                    !self.constraints.include.contains(&source.selector()),
                    "required skill metadata exceeds the ACP instruction/evidence budget"
                );
                report.problems.push(format!(
                    "{:?}: candidate_metadata_budget",
                    source.selector()
                ));
                continue;
            }
            metadata_bytes += bytes;
            report.candidates.push(SkillCandidate {
                source,
                reason: String::new(),
                selected: false,
                probability: None,
                content: None,
                files: Default::default(),
                failure: None,
                consents: vec![],
            });
        }
        journal.save(&report).await?;
        let questions = selection::questions(
            &mut report.candidates,
            &self.constraints,
            policy.max_candidates,
        )?;
        if !questions.is_empty() {
            if let Some(runtime) = &self.decision {
                let request = DecisionRequest {
                    model: runtime.model.clone(),
                    state: json!({"task":prompt,"workflow":"unknown","agent_role":"supervised_acp",
                        "session_id":target.session_id,"turn_id":target.turn_id,
                        "workspace":target.workspace,"caller":{"api_key_id":target.caller.api_key_id(),"user_id":target.caller.user_id(),"local":target.caller.is_local()},
                        "catalog_revisions":catalog.revisions,"constraints":self.constraints,"host_capabilities":capabilities}),
                    questions,
                };
                if serde_json::to_vec(&request)?.len() <= policy.max_request_bytes {
                    request.validate()?;
                    report.stage = "decision_intended".into();
                    report.decision = Some(DecisionEvidence {
                        request: request.clone(),
                        pricing: serde_json::to_value(&runtime.pricing)?,
                        outcome: None,
                    });
                    journal.save(&report).await?;
                    let mut outcome = tokio::select! {
                        biased;
                        ()=cancel.cancelled()=>Err(DecisionError{kind:DecisionFailure::Cancelled,message:"ACP skill assessment cancelled".into(),usage:None,may_have_run:true}),
                        result=runtime.executor.execute(&request,cancel)=>result,
                    };
                    if let Ok(response) = &outcome
                        && let Err(error) = response.validate(&request)
                    {
                        outcome = Err(error);
                    }
                    if serde_json::to_vec(&outcome)?.len()
                        > policy
                            .max_request_bytes
                            .saturating_mul(2)
                            .saturating_add(8192)
                    {
                        let usage = match &outcome {
                            Ok(response) => Some(response.usage),
                            Err(error) => error.usage,
                        };
                        outcome = Err(DecisionError {
                            kind: DecisionFailure::ResponseTooLarge,
                            message: "ACP skill assessment response budget".into(),
                            usage,
                            may_have_run: true,
                        });
                    }
                    for candidate in report
                        .candidates
                        .iter_mut()
                        .filter(|candidate| candidate.reason == "awaiting_workflow_assessment")
                    {
                        let id = candidate.source.identity()?;
                        let answer = outcome
                            .as_ref()
                            .ok()
                            .and_then(|response| response.answers.get(&id));
                        selection::apply_answer(candidate, answer, policy.confidence_threshold);
                    }
                    if let Some(decision) = &mut report.decision {
                        decision.outcome = Some(outcome);
                    }
                    report.stage = "decision_recorded".into();
                    journal.save(&report).await?;
                } else {
                    mark_unassessed(&mut report, "decision_request_budget");
                }
            } else {
                mark_unassessed(&mut report, "decision_model_unavailable");
            }
        }
        selection::bound(
            &mut report.candidates,
            &self.constraints,
            4,
            64 * 1024,
            |source| {
                skills::instruction_size(source, target).map_err(|error| {
                    CoreError::rejected(ErrorCode::ArtifactUnavailable, error.to_string())
                })
            },
        )?;
        report.stage = "selected".into();
        journal.save(&report).await?;
        let mut instructions = Vec::new();
        for candidate in &mut report.candidates {
            if !candidate.selected {
                continue;
            }
            ensure!(!cancel.is_cancelled(), "skill loading cancelled");
            let SkillSource::Managed(binding) = &candidate.source else {
                anyhow::bail!("unsupported: MCP-origin ACP skill");
            };
            let bytes = tokio::select! {
                biased;
                ()=cancel.cancelled()=>anyhow::bail!("skill loading cancelled"),
                result=library.read(target.workspace,target.caller,binding,"SKILL.md",64*1024)=>result,
            };
            match bytes {
                Ok(bytes) => instructions.push(Instructions {
                    source: candidate.source.clone(),
                    bytes,
                }),
                Err(error) => {
                    candidate.selected = false;
                    candidate.reason = "instructions_unavailable".into();
                    candidate.failure = Some(CoreError::rejected(
                        ErrorCode::ArtifactUnavailable,
                        truncate(&error, 2048),
                    ));
                    if self
                        .constraints
                        .include
                        .contains(&candidate.source.selector())
                    {
                        report.stage = "failed".into();
                        report.failure = Some(truncate(&error, 2048));
                        journal.save(&report).await?;
                        anyhow::bail!(
                            "required skill instructions unavailable: {}",
                            truncate(&error, 2048)
                        );
                    }
                }
            }
        }
        ensure!(
            !cancel.is_cancelled(),
            "skill delivery cancelled before preparation"
        );
        if instructions.is_empty() {
            report.stage = "no_selection".into();
            journal.save(&report).await?;
            return Ok(None);
        }
        let prepared = skills::prepare(capabilities, target, prompt, instructions)?;
        report.receipt = Some(prepared.receipt.clone());
        report.stage = "prepared".into();
        journal.save(&report).await?;
        ensure!(
            !cancel.is_cancelled(),
            "skill delivery cancelled before dispatch"
        );
        Ok(Some(prepared))
    }
}

fn mark_unassessed(report: &mut Report, reason: &str) {
    for candidate in report
        .candidates
        .iter_mut()
        .filter(|candidate| candidate.reason == "awaiting_workflow_assessment")
    {
        candidate.reason = reason.into();
    }
}

pub(crate) fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}
