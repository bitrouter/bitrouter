//! Cold projections of original skill and SDK request facts. No source lookup,
//! filesystem validation, owner restoration or execution is performed here.

use bitrouter_ai::types::{Content, Prompt, Role};
use bitrouter_sdk::decision_model::policy::DecisionPolicy;
use serde::Serialize;

use super::{SkillCandidate, SkillCatalog, SkillConsent, SkillConstraints, SkillSource};
use crate::core::checkpoint::sha256;
use crate::core::context_router::{DecisionReceipt, digest, planner::SourceRevision};
use crate::core::protocol::CoreError;
use crate::core::session::{AgentStatus, ProviderWorkRecord, SessionSnapshot, skills};

#[derive(Debug, Serialize)]
pub struct Explanation {
    pub session_id: String,
    pub run_id: String,
    pub native_skills_negotiated: bool,
    pub work_units: Vec<WorkExplanation>,
}

#[derive(Debug, Serialize)]
pub struct WorkExplanation {
    pub agent_turn_id: String,
    pub agent_id: String,
    pub parent_agent_turn_id: Option<String>,
    pub status: AgentStatus,
    pub selection: Option<SelectionExplanation>,
}

#[derive(Debug, Serialize)]
pub struct SelectionExplanation {
    pub source_revision: SourceRevision,
    pub catalog: SkillCatalog,
    pub constraints: SkillConstraints,
    pub policy: DecisionPolicy,
    pub max_selected: usize,
    pub instruction_limit_bytes: u64,
    pub file_limit_bytes: Option<u64>,
    pub selected: bool,
    pub decision_id: Option<String>,
    pub decision: Option<DecisionReceipt>,
    pub failure: Option<CoreError>,
    pub candidates: Vec<CandidateExplanation>,
}

#[derive(Debug, Serialize)]
pub struct CandidateExplanation {
    pub identity: String,
    pub source: SkillSource,
    pub selected: bool,
    pub reason: String,
    pub probability: Option<f64>,
    pub consents: Vec<SkillConsent>,
    pub failure: Option<CoreError>,
    pub instructions: Option<InstructionEvidence>,
    pub supporting_files: Vec<super::files::File>,
    /// Exact prepared SDK input, not proof of a network send or model obedience.
    pub prepared_model_requests: Vec<ModelRequestEvidence>,
}

#[derive(Debug, Serialize)]
pub struct InstructionEvidence {
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct ModelRequestEvidence {
    pub step_id: String,
    pub request_id: String,
    pub effective_model: String,
    pub prompt_sha256: String,
    /// Commitments whose exact verified data frame appears in this SDK input.
    pub supporting_files: Vec<super::files::File>,
    /// None means this historical step is no longer in the current agent
    /// snapshot. An empty list means that snapshot records no attempt yet.
    pub attempts: Option<Vec<AttemptEvidence>>,
}

#[derive(Debug, Serialize)]
pub struct AttemptEvidence {
    pub attempt_id: String,
    pub index: u32,
    pub report_received: bool,
    pub model_result_received: bool,
    pub provider_work: Vec<ProviderWorkRecord>,
}

pub fn capture(state: &SessionSnapshot, run_id: &str) -> Result<Explanation, CoreError> {
    skills::validate(state)?;
    let mut work_units = Vec::new();
    for work in state
        .context_store
        .work
        .values()
        .filter(|work| work.run_id == run_id)
    {
        let selection = work
            .skills
            .as_ref()
            .map(|selection| {
                let candidates = selection
                    .candidates
                    .iter()
                    .map(|candidate| {
                        let instructions = candidate
                            .content
                            .as_ref()
                            .map(|content| {
                                candidate.source.verify(content.as_bytes())?;
                                Ok::<_, CoreError>(InstructionEvidence {
                                    sha256: sha256(content.as_bytes()),
                                    bytes: content.len() as u64,
                                })
                            })
                            .transpose()?;
                        let mut prepared_model_requests = Vec::new();
                        if instructions.is_some() {
                            for execution in state
                                .context_store
                                .executions
                                .values()
                                .filter(|execution| execution.task_id == work.task_id)
                            {
                                let Some(model) = &execution.model else {
                                    continue;
                                };
                                if !contains_instructions(&model.prompt, candidate)? {
                                    continue;
                                }
                                let attempts = state
                                    .agents
                                    .get(&work.agent_id)
                                    .and_then(|agent| agent.turn.as_ref())
                                    .filter(|turn| turn.agent_turn_id == work.task_id)
                                    .and_then(|turn| {
                                        turn.steps
                                            .iter()
                                            .find(|step| step.step_id == execution.step_id)
                                    })
                                    .map(|step| {
                                        step.attempts
                                            .iter()
                                            .map(|attempt| AttemptEvidence {
                                                attempt_id: attempt.attempt_id.clone(),
                                                index: attempt.index,
                                                report_received: attempt.receipt.is_some(),
                                                model_result_received: attempt
                                                    .receipt
                                                    .as_ref()
                                                    .is_some_and(|receipt| {
                                                        receipt.report.result.is_some()
                                                    }),
                                                provider_work: attempt.provider_work.clone(),
                                            })
                                            .collect()
                                    });
                                prepared_model_requests.push(ModelRequestEvidence {
                                    step_id: execution.step_id.clone(),
                                    request_id: model.request_id.clone(),
                                    effective_model: model.effective_model.clone(),
                                    prompt_sha256: digest(&model.prompt)?,
                                    supporting_files: candidate.files.iter().filter_map(|(reference, content)| {
                                        let expected = super::files::text(candidate, reference, content);
                                        match expected {
                                            Ok(expected) if model.prompt.messages.iter().any(|message| message.role == Role::User && message.content.iter().any(|content| matches!(content, Content::Text{text,..} if text == &expected))) => Some(candidate.source.file(reference)),
                                            Err(error) => Some(Err(error)),
                                            _ => None,
                                        }
                                    }).collect::<Result<Vec<_>, CoreError>>()?,
                                    attempts,
                                });
                            }
                        }
                        Ok(CandidateExplanation {
                            identity: candidate.source.identity()?,
                            source: candidate.source.clone(),
                            selected: candidate.selected,
                            reason: candidate.reason.clone(),
                            probability: candidate.probability,
                            consents: candidate.consents.clone(),
                            failure: candidate.failure.clone(),
                            instructions,
                            supporting_files: candidate.files.keys().map(|reference| candidate.source.file(reference)).collect::<Result<Vec<_>, CoreError>>()?,
                            prepared_model_requests,
                        })
                    })
                    .collect::<Result<Vec<_>, CoreError>>()?;
                Ok::<_, CoreError>(SelectionExplanation {
                    source_revision: selection.source.clone(),
                    catalog: selection.catalog.clone(),
                    constraints: selection.constraints.clone(),
                    policy: selection.policy.clone(),
                    max_selected: selection.max_selected,
                    instruction_limit_bytes: selection.instruction_limit_bytes,
                    file_limit_bytes: selection.file_limit_bytes,
                    selected: selection.selected,
                    decision_id: selection.decision_id.clone(),
                    failure: selection.failure.clone(),
                    candidates,
                    decision: selection
                        .decision_id
                        .as_ref()
                        .and_then(|id| state.context_store.decisions.get(id))
                        .cloned(),
                })
            })
            .transpose()?;
        work_units.push(WorkExplanation {
            agent_turn_id: work.task_id.clone(),
            agent_id: work.agent_id.clone(),
            parent_agent_turn_id: work.parent_task_id.clone(),
            status: work.status,
            selection,
        });
    }
    Ok(Explanation {
        session_id: state.session_id.clone(),
        run_id: run_id.into(),
        native_skills_negotiated: state
            .manifest
            .required_features
            .iter()
            .any(|feature| feature == super::FEATURE),
        work_units,
    })
}

fn contains_instructions(prompt: &Prompt, candidate: &SkillCandidate) -> Result<bool, CoreError> {
    match &candidate.source {
        SkillSource::Mcp(_) => {
            let expected = skills::mcp_resource_text(candidate)?;
            Ok(prompt
                .messages
                .iter()
                .filter(|message| message.role == Role::User)
                .flat_map(|message| &message.content)
                .any(|content| matches!(content, Content::Text { text, .. } if text == &expected)))
        }
        SkillSource::Managed(_) => {
            let expected = skills::managed_instruction_text(candidate)?;
            Ok(prompt.system.as_ref().is_some_and(|system| system.contains(&expected))
                || prompt.messages.iter().filter(|message| message.role == Role::System)
                    .flat_map(|message| &message.content).any(|content| matches!(content, Content::Text { text, .. } if text.contains(&expected))))
        }
    }
}
