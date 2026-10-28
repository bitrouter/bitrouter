//! A complete native journal is evidence, never a reconstructed execution grant.
use super::{AckFacts, Binding, Journal, Readback, StepFacts, checked_contents, checked_snapshot};
use crate::{
    acp_trajectory::checkpoint::types::EvidenceCitation,
    core::evolution::evidence::{
        EvidenceItem, EvidenceKind, EvidencePacket, NATIVE_EVIDENCE_VERSION,
    },
    harness::trajectory::owned::io,
    managed_store::owned::Loaded,
    storage::{
        checkpoint::snapshot::{RunStatus, SessionSnapshot, accounting::work::CostWorkState},
        evaluation::canonical_digest,
        operation_budget::{Error, OperationBudget},
    },
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::{
    core::{checkpoint::DurableHead, protocol::ToolOutcome},
    harness::inspection::AgentStatus,
    tenancy::operation::ErrorCode,
};
use serde::Serialize;
use std::collections::BTreeSet;

const EVIDENCE_BYTES: usize = 8 * 1024 * 1024;
const EVIDENCE_ITEMS: usize = 4096;

/// Constructed only after the actual owned row, whole checkpoint, ACK and
/// artifact closure have passed their existing decoders. Storage callers must
/// additionally prove every experiment execution/receipt/publication edge.
pub(crate) struct CompletedRun {
    readback: Readback,
    run: String,
    harness: String,
    principal: String,
    checkpoint: String,
    packet: EvidencePacket,
    executions: BTreeSet<String>,
}
impl CompletedRun {
    pub(crate) fn into_packet(self) -> EvidencePacket {
        self.packet
    }
    /// Actual native save candidate under H. This validates source preservation;
    /// it neither manufactures an ACK nor grants execution from serialized data.
    pub(crate) fn proposed(
        binding: &Binding,
        bytes: &[u8],
        budget: &OperationBudget<'_>,
    ) -> Result<Self> {
        budget.check()?;
        let journal: Journal = serde_json::from_slice(bytes)?;
        let state = checked_snapshot(&journal, binding)?;
        Self::validated(
            journal,
            state,
            binding.descriptor().principal.as_str(),
            budget,
        )
    }

    pub(crate) fn live(
        binding: &Binding,
        loaded: &Loaded,
        budget: &OperationBudget<'_>,
    ) -> Result<Self> {
        budget.check()?;
        ensure!(
            loaded.descriptor == *binding.descriptor(),
            "native completed owner changed"
        );
        let journal: Journal = serde_json::from_slice(&loaded.journal)?;
        ensure!(
            journal.checkpoint.is_some(),
            Error::Rejected(ErrorCode::NotFound)
        );
        let state = checked_snapshot(&journal, binding)?;
        ensure!(
            !journal.released,
            Error::Rejected(ErrorCode::InvalidRequest)
        );
        Self::validated(journal, state, loaded.descriptor.principal.as_str(), budget)
    }

    pub(crate) fn stopped(
        loaded: &Loaded,
        run: &str,
        budget: &OperationBudget<'_>,
    ) -> Result<Self> {
        budget.check()?;
        let journal: Journal = serde_json::from_slice(&loaded.journal)?;
        let state = checked_contents(&journal, None)?;
        ensure!(
            journal.released
                && journal.grant.session_id == loaded.descriptor.session
                && journal.grant.harness_id == loaded.descriptor.harness
                && state.run.as_ref().is_some_and(|value| value.run_id == run),
            Error::Rejected(ErrorCode::InvalidRequest)
        );
        Self::validated(journal, state, loaded.descriptor.principal.as_str(), budget)
    }

    fn validated(
        journal: Journal,
        state: SessionSnapshot,
        principal: &str,
        budget: &OperationBudget<'_>,
    ) -> Result<Self> {
        let root = state
            .run
            .as_ref()
            .ok_or(Error::Rejected(ErrorCode::NotFound))?;
        ensure!(
            root.status == RunStatus::Completed,
            Error::Rejected(ErrorCode::InvalidRequest)
        );
        // Recovery archives are integrity checked above. This projection does
        // not interpret an archived lifecycle as a fresh complete run.
        ensure!(
            state.recovery_archive.is_none(),
            Error::Rejected(ErrorCode::CapabilityMismatch)
        );
        let costs = state
            .cost_work
            .get(&root.run_id)
            .ok_or(Error::Rejected(ErrorCode::CapabilityMismatch))?;
        ensure!(
            costs
                .work
                .values()
                .all(|work| work.state == CostWorkState::OutcomeRecorded),
            Error::Rejected(ErrorCode::InvalidRequest)
        );
        let turns = state
            .agents
            .iter()
            .filter_map(|(id, agent)| {
                agent
                    .turn
                    .as_ref()
                    .filter(|turn| turn.run_id == root.run_id)
                    .map(|turn| (id, turn))
            })
            .collect::<Vec<_>>();
        // A later durable owner release changes the session head, not this
        // completed run's input, settled decisions, tools or accounting facts.
        let identity = (
            NATIVE_EVIDENCE_VERSION,
            &state.session_id,
            root,
            &turns,
            costs,
        );
        budget.charge(
            0,
            io::json_len(&identity, EVIDENCE_BYTES).map_err(|_| Error::Capacity)?,
        )?;
        let checkpoint = canonical_digest(&identity)?
            .strip_prefix("sha256:")
            .context("native source digest encoding")?
            .to_owned();
        let mut builder = Projection::new(&checkpoint);
        #[derive(Serialize)]
        struct Task<'a> {
            text: &'a str,
            acceptance_criteria: &'a [String],
            required_materials: &'a [String],
            verification: &'a Option<bitrouter_ai::core::protocol::Verification>,
        }
        builder.push(
            EvidenceKind::UserMessage,
            "native.task",
            &root.run_id,
            &Task {
                text: &root.input.text,
                acceptance_criteria: &root.input.acceptance_criteria,
                required_materials: &root.input.required_materials,
                verification: &root.input.verification,
            },
        )?;
        let final_answer = root
            .final_answer
            .as_deref()
            .ok_or(Error::Rejected(ErrorCode::InvalidRequest))?;
        builder.push(
            EvidenceKind::AgentMessage,
            "native.final",
            &root.run_id,
            &final_answer,
        )?;
        let mut executions = BTreeSet::new();
        let mut invocations = BTreeSet::new();
        for (agent_id, agent) in &state.agents {
            let Some(turn) = &agent.turn else { continue };
            if turn.run_id != root.run_id {
                continue;
            }
            ensure!(
                matches!(
                    turn.status,
                    AgentStatus::Completed | AgentStatus::Failed | AgentStatus::Cancelled
                ),
                Error::Rejected(ErrorCode::InvalidRequest)
            );
            for step in &turn.steps {
                budget.check()?;
                ensure!(
                    step.settled && !step.interrupted,
                    Error::Rejected(ErrorCode::InvalidRequest)
                );
                ensure!(
                    step.preparation_work
                        .iter()
                        .all(|work| work.report.is_some())
                        && step.attempts.iter().all(|attempt| attempt.receipt.is_some()
                            && attempt
                                .provider_work
                                .iter()
                                .all(|work| work.report.is_some())),
                    Error::Rejected(ErrorCode::InvalidRequest)
                );
                if let Some(plan) = &step.plan {
                    executions.insert(plan.request_id.clone());
                }
            }
            for invocation in &turn.invocations {
                let command = &invocation.dispatch;
                ensure!(
                    command.run_id == root.run_id
                        && command.agent_id == *agent_id
                        && command.agent_turn_id == turn.agent_turn_id
                        && invocations.insert(command.invocation_id.clone()),
                    "native tool source edge changed"
                );
                let result = invocation
                    .result
                    .as_ref()
                    .ok_or(Error::Rejected(ErrorCode::InvalidRequest))?;
                ensure!(
                    invocation.consumed
                        && result.status != ToolOutcome::EffectUnknown
                        && result.invocation_id == command.invocation_id
                        && result.attempt_id == command.attempt_id,
                    Error::Rejected(ErrorCode::InvalidRequest)
                );
                let started = journal
                    .starts
                    .get(&command.invocation_id)
                    .context("native tool ledger is absent")?;
                ensure!(
                    started.command == *command && started.result.as_ref() == Some(result),
                    "native tool ledger differs from completed checkpoint"
                );
                builder.push(
                    EvidenceKind::ToolCall,
                    "native.tool.call",
                    &command.invocation_id,
                    &(&command.tool, &command.arguments),
                )?;
                builder.push(
                    EvidenceKind::ToolObservation,
                    "native.tool.result",
                    &command.invocation_id,
                    result,
                )?;
            }
        }
        ensure!(
            !executions.is_empty(),
            Error::Rejected(ErrorCode::InvalidRequest)
        );
        for tool in journal
            .starts
            .values()
            .filter(|value| value.command.run_id == root.run_id)
        {
            ensure!(
                invocations.contains(&tool.command.invocation_id),
                "native completed checkpoint omitted a tool"
            );
        }
        let prefix_digest =
            canonical_digest(&(NATIVE_EVIDENCE_VERSION, &checkpoint, &builder.items))?;
        let packet = EvidencePacket {
            projection_version: NATIVE_EVIDENCE_VERSION.into(),
            checkpoint_id: checkpoint.clone(),
            prefix_digest,
            gaps: Vec::new(),
            items: builder.items,
        };
        let size = io::json_len(&packet, EVIDENCE_BYTES).map_err(|_| Error::Capacity)?;
        budget.charge(0, size)?;
        Ok(Self {
            principal: principal.to_owned(),
            harness: journal.grant.harness_id,
            run: root.run_id.clone(),
            checkpoint,
            packet,
            executions,
            readback: Readback {
                state,
                head: journal.head,
            },
        })
    }
    pub(crate) fn principal(&self) -> &str {
        &self.principal
    }
    pub(crate) fn harness(&self) -> &str {
        &self.harness
    }
    pub(crate) fn session(&self) -> &str {
        &self.readback.state.session_id
    }
    pub(crate) fn run(&self) -> &str {
        &self.run
    }
    pub(crate) fn checkpoint(&self) -> &str {
        &self.checkpoint
    }
    pub(crate) fn head(&self) -> &DurableHead {
        &self.readback.head
    }
    pub(crate) fn packet(&self) -> &EvidencePacket {
        &self.packet
    }
    pub(crate) fn executions(&self) -> &BTreeSet<String> {
        &self.executions
    }
    pub(crate) fn settled(&self, facts: &StepFacts, execution: &str) -> Result<AckFacts> {
        ensure!(
            facts.run == self.run && self.executions.contains(execution),
            "native completed execution is not a member"
        );
        // Reopened/archived epoch translation needs an explicit persisted edge;
        // this reader never treats a later lease as the original execution.
        ensure!(
            self.readback.head.execution_epoch == facts.epoch,
            Error::Rejected(ErrorCode::CapabilityMismatch)
        );
        self.readback
            .settled(facts, execution)?
            .context("native completed execution has no ACK")
    }
}

struct Projection<'a> {
    checkpoint: &'a str,
    bytes: usize,
    items: Vec<EvidenceItem>,
}
impl<'a> Projection<'a> {
    fn new(checkpoint: &'a str) -> Self {
        Self {
            checkpoint,
            bytes: 0,
            items: Vec::new(),
        }
    }
    fn push(
        &mut self,
        kind: EvidenceKind,
        method: &str,
        identity: &str,
        content: &impl Serialize,
    ) -> Result<()> {
        ensure!(self.items.len() < EVIDENCE_ITEMS, Error::Capacity);
        let remaining = EVIDENCE_BYTES
            .checked_sub(self.bytes)
            .ok_or(Error::Capacity)?;
        let body = io::json(content, remaining).map_err(|_| Error::Capacity)?;
        let content = serde_json::from_str(&body)?;
        let digest = canonical_digest(&(
            NATIVE_EVIDENCE_VERSION,
            self.checkpoint,
            method,
            identity,
            &content,
        ))?;
        let item = EvidenceItem {
            citation: EvidenceCitation {
                node_id: format!("native:{digest}"),
                digest,
            },
            kind,
            method: method.into(),
            call_id: None,
            content,
        };
        self.bytes = self
            .bytes
            .checked_add(io::json_len(&item, remaining).map_err(|_| Error::Capacity)?)
            .and_then(|size| size.checked_add(1))
            .ok_or(Error::Capacity)?;
        ensure!(self.bytes <= EVIDENCE_BYTES, Error::Capacity);
        self.items.push(item);
        Ok(())
    }
}
