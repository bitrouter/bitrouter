//! Direct handoff from the frozen policy, independent of the pending cache.

use super::*;
use crate::gateway::observability::decision::PendingEvalDecision;
use crate::policy_lock::PolicyRoutingSnapshot;

impl Capture {
    pub(crate) fn policy(
        &self,
        name: &str,
        guarded: bool,
        pending: Option<&PendingEvalDecision>,
        snapshot: &PolicyRoutingSnapshot,
    ) -> Result<()> {
        identifier(name)?;
        let definition = snapshot
            .document()
            .and_then(|document| document.policies.get(name))
            .context("captured policy is absent from frozen document")?;
        ensure!(
            definition.progress_guard.is_some() == guarded,
            "captured guard differs from frozen policy"
        );
        let digest = snapshot
            .digest()
            .context("frozen policy digest is missing")?;
        validate_digest(digest)?;
        let mut treatment = pending
            .map(|pending| {
                ensure!(
                    pending.policy == name && pending.policy_digest == digest,
                    "captured policy differs from frozen selection"
                );
                // Bound values before cloning; normalization below retains only the
                // existing categorical prediction/continuation evidence.
                for value in [
                    &pending.request_id,
                    &pending.decision_id,
                    &pending.policy,
                    &pending.route_projection,
                    &pending.request_key,
                    &pending.selected_tier,
                ] {
                    identifier(value)?;
                }
                for value in [pending.baseline_tier.as_deref(), pending.preset.as_deref()]
                    .into_iter()
                    .flatten()
                {
                    identifier(value)?;
                }
                check_size(&(&pending.experiment, &pending.route_measurement))?;
                let captured = Treatment {
                    decision: EvalDecisionRef {
                        decision_id: pending.decision_id.clone(),
                        policy: pending.policy.clone(),
                        policy_digest: pending.policy_digest.clone(),
                        route_projection: pending.route_projection.clone(),
                        request_key: pending.request_key.clone(),
                        selected_tier: pending.selected_tier.clone(),
                        selected_effort: pending.selected_effort,
                        baseline_tier: pending.baseline_tier.clone(),
                        baseline_effort: pending.baseline_effort,
                        experiment: pending.experiment.clone(),
                        route_measurement: pending.route_measurement.clone(),
                    },
                    preset: pending.preset.clone(),
                    holdout: pending.holdout,
                    attributes: pending.observation_snapshot().attributes(),
                    observed_at: pending.observed_at.clone(),
                };
                check_size(&captured)?;
                Ok(Box::new(captured))
            })
            .transpose()?;
        self.update(|input| {
            if guarded && let (Some(treatment), Some(pending)) = (&mut treatment, pending) {
                // Guarded evaluation refers to the durable route event, whose
                // identity was derived with the episode's retained key. The
                // optional local cache uses a separate request/policy ID.
                treatment.decision.decision_id = crate::trajectory::correlation::stable_id(
                    "route-intent",
                    &input.owner,
                    &pending.request_id,
                );
            }
            input.policy = Policy::Evaluated {
                policy: name.into(),
                digest: digest.into(),
                guarded,
                treatment,
            };
            Ok(())
        })
    }
}
