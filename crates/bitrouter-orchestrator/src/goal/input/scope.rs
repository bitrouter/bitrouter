//! Modification boundaries selected from the original input, not UI focus.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{Interpretation, PendingInput};
use crate::goal::workflow::ArtifactRef;
use crate::goal::{GoalError, GoalSnapshot, OperationKind, identifier, invalid, message};

pub const MAX_EVIDENCE_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum Selection {
    Goal,
    Tasks { task_ids: Vec<String> },
    NoChange,
    Unknown { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub input_id: String,
    pub requirements_revision: u64,
    pub request_id: String,
    pub evidence: ArtifactRef,
    pub selection: Selection,
}

impl Binding {
    pub(super) fn validate(
        &self,
        goal: &GoalSnapshot,
        pending: &PendingInput,
        proposal: &Interpretation,
    ) -> Result<(), GoalError> {
        identifier(&self.request_id)?;
        self.evidence.validate()?;
        if self.evidence.bytes > MAX_EVIDENCE_BYTES {
            return Err(invalid("modification scope evidence exceeds its bound"));
        }
        if self.input_id != pending.operation.id
            || self.requirements_revision != pending.requirements_revision
        {
            return Err(invalid(
                "modification scope belongs to another input or version",
            ));
        }
        let OperationKind::Input { task_reference, .. } = &pending.operation.kind else {
            return Err(GoalError::Conflict);
        };
        match &self.selection {
            Selection::Goal if task_reference.is_some() => {
                return Err(invalid(
                    "modification scope exceeds the original task reference",
                ));
            }
            Selection::Goal => {}
            Selection::Tasks { task_ids } => {
                let mut seen = BTreeSet::new();
                if task_ids.is_empty() || task_ids.len() > super::super::workflow::MAX_TASKS {
                    return Err(invalid("invalid modification task scope"));
                }
                for id in task_ids {
                    identifier(id)?;
                    if !seen.insert(id)
                        || !goal.workflow.tasks.iter().any(|task| &task.spec.id == id)
                        || task_reference
                            .as_ref()
                            .is_some_and(|reference| reference != id)
                    {
                        return Err(invalid(
                            "modification scope has an invalid or mismatched task",
                        ));
                    }
                }
                if proposal.objective.is_some()
                    || proposal
                        .task_updates
                        .iter()
                        .any(|update| !seen.contains(&update.task_id))
                {
                    return Err(invalid(
                        "proposed modification exceeds its selected task scope",
                    ));
                }
            }
            Selection::NoChange | Selection::Unknown { .. } => {
                if proposal.objective.is_some() || !proposal.task_updates.is_empty() {
                    return Err(invalid(
                        "modification scope does not permit requirement changes",
                    ));
                }
                if let Selection::Unknown { reason } = &self.selection {
                    message(reason)?;
                    if proposal.clarification.is_none() {
                        return Err(invalid("unknown modification scope requires clarification"));
                    }
                    if proposal.clarification_kind == Some(super::ClarificationKind::Query) {
                        return Err(invalid(
                            "unknown modification scope is not a query-only clarification",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}
