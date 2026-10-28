//! Bounded stopped-owner facts. No dispatch or authority constructors live here.
use crate::tenancy::{origin::Stamp, receipts::recovery::Seed};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

pub(crate) const MAX_BYTES: usize = 32 * 1024 * 1024;
#[derive(Default)]
pub(crate) struct Budget(Mutex<(usize, usize)>);
impl Budget {
    pub(crate) fn charge(&self, rows: usize, bytes: usize) -> Result<()> {
        let mut used = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("recovery budget unavailable"))?;
        let next = (used.0.checked_add(rows), used.1.checked_add(bytes));
        let (Some(rows), Some(bytes)) = next else {
            return Err(super::Rejected(
                bitrouter_ai::tenancy::operation::ErrorCode::CapacityExceeded,
            )
            .into());
        };
        if rows > 4096 || bytes > MAX_BYTES {
            return Err(super::Rejected(
                bitrouter_ai::tenancy::operation::ErrorCode::CapacityExceeded,
            )
            .into());
        }
        *used = (rows, bytes);
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Opening,
    Ready,
    Retiring,
    Released,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Metadata {
    pub origin: Stamp,
    pub phase: Phase,
    pub eligible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_digest: Option<String>,
    // While retirement is incomplete the workspace can still retain the older
    // exact claim. It is never a wildcard or a standalone stopped-owner proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_grant: Option<bitrouter_ai::core::protocol::OwnershipGrant>,
}
impl Metadata {
    pub(crate) fn validate(&self) -> Result<()> {
        self.origin.validate()?;
        ensure!(
            self.eligible == self.profile_digest.is_some()
                && self
                    .profile_digest
                    .as_ref()
                    .is_none_or(|value| value.len() == 64
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))),
            "native recovery profile digest invalid"
        );
        if let Some(grant) = &self.workspace_grant {
            grant
                .validate()
                .map_err(|_| anyhow::anyhow!("invalid recovery workspace grant"))?;
            ensure!(
                self.phase == Phase::Retiring,
                "workspace predecessor outside retirement"
            );
        }
        ensure!(
            !matches!(self.phase, Phase::Opening) || !self.eligible,
            "opening cannot establish recovery eligibility"
        );
        Ok(())
    }
}

/// Opening retains this participant outside the caught H future. A reply drop
/// cannot discard its budget, fixed candidates, reservation or durable marker.
pub(crate) struct CheckedRecovery {
    #[cfg(test)]
    pub(in crate::tenancy) observation: std::sync::OnceLock<usize>,
    pub(in crate::tenancy) expected: crate::managed_store::owned::Loaded,
    pub(crate) budget: Arc<Budget>,
    pub(in crate::tenancy) seeds: Vec<Seed>,
    pub(in crate::tenancy) workspace: std::path::PathBuf,
    pub(in crate::tenancy) source_grant: bitrouter_ai::core::protocol::OwnershipGrant,
    pub(in crate::tenancy) workspace_grant: bitrouter_ai::core::protocol::OwnershipGrant,
    pub(in crate::tenancy) steps:
        std::collections::BTreeMap<String, Vec<bitrouter_ai::tenancy::identity::ExecutionId>>,
    pub(in crate::tenancy) pending_steps: std::collections::BTreeSet<String>,
    pub(in crate::tenancy) receipts: Vec<(Arc<crate::tenancy::receipts::store::Candidate>, bool)>,
    pub(in crate::tenancy) committed: std::sync::atomic::AtomicBool,
    pub(in crate::tenancy) retired: std::sync::atomic::AtomicBool,
    pub(in crate::tenancy) reservation: Mutex<Option<crate::tenancy::admission::Repair>>,
}
impl CheckedRecovery {
    pub(crate) fn binds(
        &self,
        descriptor: &crate::tenancy::execution::session::Descriptor,
    ) -> bool {
        &self.expected.descriptor == descriptor
    }
    pub(crate) fn workspace_matches(
        &self,
        path: &std::path::Path,
        instance: &str,
        epoch: u64,
        execution: &str,
    ) -> bool {
        path == self.workspace
            && [&self.workspace_grant, &self.source_grant]
                .iter()
                .any(|grant| {
                    grant.core_instance_id == instance
                        && grant.execution_epoch == epoch
                        && grant.session_id == execution
                })
    }
    pub(crate) fn workspace_predecessor(&self) -> &bitrouter_ai::core::protocol::OwnershipGrant {
        &self.workspace_grant
    }
    pub(crate) fn retired(&self) -> bool {
        self.retired.load(std::sync::atomic::Ordering::Acquire)
    }
    pub(crate) fn confirm_retired(&self) -> Result<()> {
        ensure!(self.committed(), "native recovery is not committed");
        self.retired
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    pub(crate) fn terminal_digests(
        &self,
        parent: &bitrouter_ai::tenancy::identity::ExecutionId,
        child: &bitrouter_ai::tenancy::identity::ExecutionId,
    ) -> Result<(String, String)> {
        let find = |id| {
            self.receipts
                .iter()
                .find(|(value, _)| &value.receipt.execution_id == id)
                .map(|(value, _)| value.digest.clone())
                .ok_or_else(|| anyhow::anyhow!("recovery terminal missing"))
        };
        Ok((find(parent)?, find(child)?))
    }
    pub(crate) fn validate_snapshot(
        &self,
        state: &crate::storage::checkpoint::snapshot::SessionSnapshot,
    ) -> Result<()> {
        for agent in state.agents.values() {
            if let Some(turn) = &agent.turn {
                ensure!(
                    turn.invocations.is_empty(),
                    "native invocation history is not recoverable"
                );
                for step in &turn.steps {
                    let parents = self
                        .steps
                        .get(&step.step_id)
                        .ok_or_else(|| anyhow::anyhow!("uncovered native Generation step"))?;
                    ensure!(
                        step.count_plan.is_none() && step.input_counts.is_empty(),
                        "native Count work has no recovery proof"
                    );
                    let before_generation = self.pending_steps.contains(&step.step_id)
                        && step.plan.is_none()
                        && step.attempts.is_empty()
                        && stopped_preparation(&step.preparation_work, parents);
                    let observed_generation = !step.attempts.is_empty()
                        && step
                            .attempts
                            .iter()
                            .all(|attempt| attempt.receipt.is_some())
                        && parents.iter().all(|parent| {
                            self.receipts.iter().any(|(candidate, missing)| {
                                &candidate.receipt.execution_id == parent && !missing
                            })
                        });
                    ensure!(
                        before_generation || observed_generation,
                        "Generation has neither a pending Choice barrier nor complete durable evidence"
                    );
                }
            }
        }
        Ok(())
    }

    pub(crate) fn committed(&self) -> bool {
        self.committed.load(std::sync::atomic::Ordering::Acquire)
    }
}

// Before Choice, the isolated static pipeline acknowledges Entry, the inactive
// HTTP continuation hook and retained selector lookup. No other callback or
// unfinished preparation belongs to this cancellation-only recovery profile.
fn stopped_preparation(
    records: &[crate::storage::checkpoint::snapshot::PreparationWorkRecord],
    parents: &[bitrouter_ai::tenancy::identity::ExecutionId],
) -> bool {
    use crate::gateway::pipeline::native_preparation::NativePreparationWorkKind;
    // Preserve the earlier empty-history profile without treating it as proof
    // that arbitrary preparation performed no work or incurred no expense.
    if records.is_empty() {
        return true;
    }
    let expected = [
        NativePreparationWorkKind::PreResolutionHook,
        NativePreparationWorkKind::PreResolutionHook,
        NativePreparationWorkKind::RouterLookup,
    ];
    let request = &records[0].work.request_id;
    records.len() == expected.len()
        && parents.iter().any(|parent| parent.as_str() == request)
        && records
            .iter()
            .zip(expected)
            .enumerate()
            .all(|(index, (record, kind))| {
                record.work.kind == kind
                    && record.work.work_index == index as u32
                    && &record.work.request_id == request
                    && record.report.as_ref().is_some_and(|report| {
                        report.work == record.work && report.error_code.is_none()
                    })
            })
}

mod participant;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_preparation_requires_the_complete_builtin_lookup_ack() -> Result<()> {
        use crate::gateway::pipeline::native_preparation::{
            NativePreparationWork, NativePreparationWorkKind, NativePreparationWorkReport,
        };
        use crate::storage::checkpoint::snapshot::PreparationWorkRecord;
        let parent: bitrouter_ai::tenancy::identity::ExecutionId = "parent-1".try_into()?;
        let kinds = [
            NativePreparationWorkKind::PreResolutionHook,
            NativePreparationWorkKind::PreResolutionHook,
            NativePreparationWorkKind::RouterLookup,
        ];
        let records: Vec<_> = kinds
            .into_iter()
            .enumerate()
            .map(|(index, kind)| {
                let work = NativePreparationWork {
                    request_id: parent.as_str().into(),
                    kind,
                    work_index: index as u32,
                };
                PreparationWorkRecord {
                    work: work.clone(),
                    report: Some(NativePreparationWorkReport {
                        work,
                        elapsed_ms: 7,
                        error_code: None,
                    }),
                }
            })
            .collect();
        let parents = std::slice::from_ref(&parent);
        assert!(stopped_preparation(&[], parents));
        assert!(stopped_preparation(&records, parents));
        assert!(!stopped_preparation(&records, &[]));
        assert!(!stopped_preparation(&records[..2], parents));
        assert!(!stopped_preparation(&records[2..], parents));
        let mut extra = records.clone();
        extra.push(records[2].clone());
        assert!(!stopped_preparation(&extra, parents));
        for case in [
            "pending", "error", "identity", "index", "callback", "foreign", "mixed", "order",
        ] {
            let mut changed = records.clone();
            match case {
                "pending" => changed[1].report = None,
                "error" => {
                    changed[1]
                        .report
                        .as_mut()
                        .ok_or_else(|| anyhow::anyhow!("report"))?
                        .error_code = Some("rejected".into())
                }
                "identity" => {
                    changed[1]
                        .report
                        .as_mut()
                        .ok_or_else(|| anyhow::anyhow!("report"))?
                        .work
                        .request_id = "another-request".into()
                }
                "index" => {
                    changed[1].work.work_index = 0;
                    changed[1]
                        .report
                        .as_mut()
                        .ok_or_else(|| anyhow::anyhow!("report"))?
                        .work
                        .work_index = 0;
                }
                "callback" => {
                    changed[1].work.kind = NativePreparationWorkKind::RequestCheck;
                    changed[1]
                        .report
                        .as_mut()
                        .ok_or_else(|| anyhow::anyhow!("report"))?
                        .work
                        .kind = NativePreparationWorkKind::RequestCheck;
                }
                "foreign" | "mixed" => {
                    for (index, record) in changed.iter_mut().enumerate() {
                        if case == "foreign" || index == 1 {
                            record.work.request_id = "foreign-parent".into();
                            record
                                .report
                                .as_mut()
                                .ok_or_else(|| anyhow::anyhow!("report"))?
                                .work = record.work.clone();
                        }
                    }
                }
                "order" => changed.swap(0, 2),
                _ => anyhow::bail!("unknown test case"),
            }
            assert!(!stopped_preparation(&changed, parents), "{case}");
        }
        Ok(())
    }

    #[test]
    fn native_recovery_budget_is_cumulative_and_overflow_does_not_reset_it() -> Result<()> {
        let budget = Budget::default();
        budget.charge(4095, MAX_BYTES - 1)?;
        ensure!(budget.charge(2, 0).is_err());
        ensure!(budget.charge(0, usize::MAX).is_err());
        budget.charge(1, 1)?;
        ensure!(budget.charge(1, 0).is_err() && budget.charge(0, 1).is_err());
        Ok(())
    }
}
