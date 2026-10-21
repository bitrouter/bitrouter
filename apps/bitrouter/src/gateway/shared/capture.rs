//! Bounded original projection authority and retained pre-dispatch evidence.
//!
//! This is private durable data, never a public pipeline event or caller input.

mod policy;
pub(super) mod projection;
mod response;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::eval::{EvalDecisionRef, EvidenceItem};
use bitrouter_ai::types::ModelOperation;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::policy_lock::{PolicyLock, semantic_digest};

const MAX_BYTES: usize = 64 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Requirements {
    format: u32,
    policy_lock_digest: Option<String>,
    trajectory: TrajectoryRequirement,
    policy_evaluation: PolicyEvaluation,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TrajectoryRequirement {
    Disabled,
    GuardedPolicies,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PolicyEvaluation {
    SelectedUntrackedRequestSubject,
}

impl Requirements {
    pub fn from_config(config: &Config, document: Option<&PolicyLock>) -> Result<Self> {
        if let Some(document) = document {
            crate::policy_lock::validate_for_config(config, document)?;
        }
        let value = Self {
            format: 1,
            policy_lock_digest: document.map(semantic_digest).transpose()?,
            trajectory: if config.trajectory.enabled {
                TrajectoryRequirement::GuardedPolicies
            } else {
                TrajectoryRequirement::Disabled
            },
            policy_evaluation: PolicyEvaluation::SelectedUntrackedRequestSubject,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.format == 1, "unsupported projection requirements");
        if let Some(digest) = &self.policy_lock_digest {
            validate_digest(digest)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn fixture() -> Self {
        Self {
            format: 1,
            policy_lock_digest: None,
            trajectory: TrajectoryRequirement::Disabled,
            policy_evaluation: PolicyEvaluation::SelectedUntrackedRequestSubject,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Bypass {
    UnboundRoute,
    Decisions,
    FixedNative,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum Policy {
    Pending,
    Bypassed {
        reason: Bypass,
    },
    Evaluated {
        policy: String,
        digest: String,
        guarded: bool,
        treatment: Option<Box<Treatment>>,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Treatment {
    decision: EvalDecisionRef,
    preset: Option<String>,
    holdout: bool,
    attributes: BTreeMap<String, String>,
    observed_at: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum Phase {
    Preparing,
    Ready,
    Terminal { route_ready: bool },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Inputs {
    requirements: Requirements,
    execution_id: String,
    owner: String,
    operation: ModelOperation,
    phase: Phase,
    policy: Policy,
    routing: Vec<EvidenceItem>,
    // Optional observed response facts are separate from the frozen treatment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response_observation: Option<response::ResponseObservation>,
}

impl Inputs {
    /// Guard writes must use the contract admitted before policy selection.
    pub fn check_guard_policy(&self, name: &str, digest: &str) -> Result<()> {
        self.validate()?;
        ensure!(
            self.requirements.trajectory == TrajectoryRequirement::GuardedPolicies
                && self.requirements.policy_lock_digest.as_deref() == Some(digest)
                && !matches!(self.phase, Phase::Terminal { .. }),
            "guard differs from original admitted contract"
        );
        ensure!(
            matches!(&self.policy, Policy::Pending)
                || matches!(&self.policy, Policy::Evaluated { policy, digest: original, guarded: true, .. }
                    if policy == name && original == digest),
            "guard differs from retained policy selection"
        );
        Ok(())
    }

    #[cfg(test)]
    pub fn fixture_ready(&mut self) -> Result<()> {
        if self.phase == Phase::Preparing && self.policy == Policy::Pending {
            self.policy = Policy::Bypassed {
                reason: if self.operation == ModelOperation::Decisions {
                    Bypass::Decisions
                } else {
                    Bypass::UnboundRoute
                },
            };
            self.phase = Phase::Ready;
        }
        self.require_ready()
    }

    pub fn new(
        requirements: Requirements,
        id: &str,
        owner: &str,
        operation: ModelOperation,
    ) -> Result<Self> {
        let value = Self {
            requirements,
            execution_id: id.into(),
            owner: owner.into(),
            operation,
            phase: Phase::Preparing,
            policy: Policy::Pending,
            routing: Vec::new(),
            response_observation: None,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        self.requirements.validate()?;
        identifier(&self.execution_id)?;
        identifier(&self.owner)?;
        ensure!(
            self.routing.len() <= 2,
            "too many projection routing receipts"
        );
        ensure!(
            self.routing.len() != 2 || self.routing[0].kind != self.routing[1].kind,
            "duplicate projection routing evidence"
        );
        for item in &self.routing {
            ensure!(
                item.redacted
                    && matches!(item.kind.as_str(), "routing.assessment" | "routing.plan"),
                "invalid projection routing evidence"
            );
            identifier(&item.evidence_id)?;
            validate_digest(&item.digest)?;
            ensure!(item.attributes.len() <= 16, "too many routing attributes");
        }
        if let Policy::Evaluated {
            policy,
            digest,
            guarded,
            treatment,
        } = &self.policy
        {
            identifier(policy)?;
            validate_digest(digest)?;
            ensure!(
                self.requirements.policy_lock_digest.as_ref() == Some(digest),
                "projection policy differs from admitted lock"
            );
            ensure!(
                !guarded || self.requirements.trajectory == TrajectoryRequirement::GuardedPolicies,
                "guarded projection was not enabled"
            );
            if let Some(treatment) = treatment {
                ensure!(
                    treatment.decision.policy == *policy
                        && treatment.decision.policy_digest == *digest,
                    "projection treatment policy mismatch"
                );
                ensure!(
                    !treatment
                        .attributes
                        .keys()
                        .any(|key| response::is_response_attribute(key)),
                    "response evidence cannot be part of immutable treatment"
                );
                identifier(&treatment.decision.selected_tier)?;
                chrono::DateTime::parse_from_rfc3339(&treatment.observed_at)
                    .context("invalid policy observation time")?;
            }
        }
        if matches!(
            self.phase,
            Phase::Ready | Phase::Terminal { route_ready: true }
        ) {
            ensure!(
                matches!(
                    self.policy,
                    Policy::Bypassed { .. }
                        | Policy::Evaluated {
                            treatment: Some(_),
                            ..
                        }
                ),
                "projection selection is incomplete"
            );
        }
        if let Policy::Bypassed {
            reason: Bypass::Decisions,
        } = self.policy
        {
            ensure!(
                self.operation == ModelOperation::Decisions,
                "projection bypass operation mismatch"
            );
        }
        if let Some(observation) = &self.response_observation {
            observation.typed()?;
            ensure!(
                self.operation == ModelOperation::Generation
                    && matches!(
                        self.phase,
                        Phase::Ready | Phase::Terminal { route_ready: true }
                    )
                    && matches!(
                        self.policy,
                        Policy::Evaluated {
                            treatment: Some(_),
                            ..
                        }
                    ),
                "response observation has no completed policy route"
            );
        }
        check_size_limit(
            self,
            if matches!(self.phase, Phase::Terminal { .. }) {
                MAX_BYTES
            } else {
                MAX_BYTES - 128
            },
        )
    }

    pub fn check_identity(&self, id: &str, owner: &str, operation: ModelOperation) -> Result<()> {
        self.validate()?;
        ensure!(
            self.execution_id == id && self.owner == owner && self.operation == operation,
            "projection input identity mismatch"
        );
        Ok(())
    }

    pub fn require_ready(&self) -> Result<()> {
        self.validate()?;
        ensure!(
            self.phase == Phase::Ready,
            "projection input is not ready for dispatch"
        );
        Ok(())
    }

    pub fn check_routing(
        &self,
        ctx: &crate::gateway::pipeline::context::PipelineContext,
    ) -> Result<()> {
        ensure!(
            self.routing == routing(ctx)?,
            "dispatch routing receipts changed"
        );
        if let Policy::Evaluated {
            treatment: Some(treatment),
            ..
        } = &self.policy
        {
            ensure!(
                ctx.generation_prompt()
                    .and_then(|prompt| prompt.params.reasoning_effort)
                    == treatment.decision.selected_effort,
                "dispatch effort differs from captured policy"
            );
        }
        if let Some(plan) = ctx.get_event::<crate::gateway::routing::plan::Selection>() {
            ensure!(
                plan.selected_model == ctx.model(),
                "dispatch model differs from captured route"
            );
        }
        Ok(())
    }

    /// Merge only genuinely observed progress; an absent legacy value is handled
    /// by the caller and never converted to an explicit no-policy outcome.
    pub fn merge(&mut self, incoming: &Self) -> Result<()> {
        incoming.check_identity(&self.execution_id, &self.owner, self.operation)?;
        self.validate()?;
        ensure!(
            self.requirements == incoming.requirements,
            "projection requirements changed"
        );
        if matches!(self.phase, Phase::Terminal { .. }) {
            ensure!(self == incoming, "terminal projection evidence changed");
            return Ok(());
        }
        ensure!(
            self.policy == Policy::Pending || self.policy == incoming.policy,
            "projection policy evidence changed"
        );
        ensure!(
            self.routing
                .iter()
                .all(|prior| incoming.routing.contains(prior)),
            "projection routing evidence changed"
        );
        if self.phase == Phase::Ready {
            ensure!(
                matches!(
                    incoming.phase,
                    Phase::Ready | Phase::Terminal { route_ready: true }
                ) && self.policy == incoming.policy
                    && self.routing == incoming.routing,
                "ready projection evidence changed"
            );
        }
        if let Some(prior) = &self.response_observation {
            let next = incoming
                .response_observation
                .as_ref()
                .context("response observation disappeared")?;
            ensure!(
                prior.typed()?.merge(next.typed()?) == next.typed()?,
                "response observation regressed"
            );
        }
        *self = incoming.clone();
        Ok(())
    }

    pub fn seal(&mut self) -> Result<()> {
        self.phase = match self.phase {
            Phase::Preparing => Phase::Terminal { route_ready: false },
            Phase::Ready => Phase::Terminal { route_ready: true },
            terminal => terminal,
        };
        self.validate()
    }
}

/// Lives on the existing custody object, whose registry retains it after a
/// request disappears. No separate writer, task or unbounded map is introduced.
#[derive(Default)]
pub(super) struct Capture(Mutex<Option<Arc<Inputs>>>);

impl Capture {
    pub fn initialize(&self, inputs: Inputs) -> Result<()> {
        inputs.validate()?;
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(state.is_none(), "projection capture already initialized");
        *state = Some(Arc::new(inputs));
        Ok(())
    }

    pub fn snapshot(&self) -> Option<Arc<Inputs>> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn update(&self, change: impl FnOnce(&mut Inputs) -> Result<()>) -> Result<()> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let prior = state.as_ref().context("projection capture is missing")?;
        let mut next = prior.as_ref().clone();
        change(&mut next)?;
        next.validate()?;
        let mut checked = prior.as_ref().clone();
        checked.merge(&next)?;
        *state = Some(Arc::new(next));
        Ok(())
    }

    pub fn bypass(&self, reason: Bypass) -> Result<()> {
        self.update(|input| {
            input.policy = Policy::Bypassed { reason };
            Ok(())
        })
    }

    pub fn route(&self, evidence: Vec<EvidenceItem>) -> Result<()> {
        ensure!(evidence.len() <= 2, "too many routing receipts");
        check_size(&evidence)?;
        self.update(|input| {
            input.routing = evidence;
            Ok(())
        })
    }

    pub fn ready(&self) -> Result<()> {
        self.update(|input| {
            input.phase = Phase::Ready;
            // Reserve space before dispatch, without invalidating historical
            // ready/terminal rows that predate optional response capture.
            if input.response_observation.is_none() {
                check_size_limit(input, MAX_BYTES - 128 - response::MAX_RESPONSE_BYTES)?;
            }
            Ok(())
        })
    }

    pub fn terminal(&self) -> Result<Option<Arc<Inputs>>> {
        if self.snapshot().is_none() {
            return Ok(None);
        }
        self.update(Inputs::seal)?;
        Ok(self.snapshot())
    }
}

fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control),
        "invalid projection identifier"
    );
    Ok(())
}

fn validate_digest(value: &str) -> Result<()> {
    ensure!(
        value
            .strip_prefix("sha256:")
            .is_some_and(|hex| hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
        "invalid projection digest"
    );
    Ok(())
}

/// Count escaped serialized bytes without first allocating an oversized buffer.
fn check_size<T: Serialize>(value: &T) -> Result<()> {
    check_size_limit(value, MAX_BYTES - 128)
}

fn check_size_limit<T: Serialize>(value: &T, limit: usize) -> Result<()> {
    struct Budget(usize);
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.0 {
                return Err(io::Error::other("projection input byte limit exceeded"));
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(limit), value).context("projection input byte limit exceeded")
}

pub(super) fn routing(
    ctx: &crate::gateway::pipeline::context::PipelineContext,
) -> Result<Vec<EvidenceItem>> {
    let receipt = ctx.get_event::<crate::gateway::routing::preparation::Receipt>();
    let selection = ctx.get_event::<crate::gateway::routing::plan::Selection>();
    check_size(&(receipt, selection))?;
    crate::gateway::observability::decision::routing_evidence_from(
        ctx.request_id(),
        receipt,
        selection,
    )
}
