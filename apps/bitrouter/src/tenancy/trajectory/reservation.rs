//! Bound exactly the terminal producer's vocabulary before committing Started.
//! Sizing strings are never stored or presented as observations.
use super::*;
use crate::{
    gateway::{
        observability::decision::routing_evidence_from,
        pipeline::context::PipelineContext,
        routing::{plan::Selection, preparation::Prepared},
    },
    storage::operation_budget::Error,
    trajectory::{
        store::owned::{headroom::Reservation, io::json_size},
        types::TrajectoryEvidence,
    },
};
use std::collections::BTreeMap;

pub(super) fn prepare(
    execution: &TenantExecution,
    ctx: &PipelineContext,
    observe: bool,
) -> Result<Reservation> {
    let prepared = ctx
        .extension::<Prepared>()
        .context("guard missing prepared context views")?;
    let retained = execution
        .route()
        .policy()
        .context("guard missing retained policy")?;
    let policy = retained
        .document()
        .policies
        .get(retained.name())
        .context("retained policy missing")?;
    let model = largest(
        policy
            .tiers
            .values()
            .map(|target| target.model())
            .chain(std::iter::once(ctx.model())),
    )?;
    let view = largest(prepared.views.iter().map(|view| view.id.as_str()))?;
    // Both strategies serialize as bounded enum strings. Both plan reasons are
    // fixed ASCII phrases <=64 bytes; the prompt digest is always SHA-256.
    let selection = Selection {
        requested_model: String::new(),
        selected_model: model,
        selected_context: view,
        prompt_digest: format!("sha256:{}", "0".repeat(64)),
        strategy: bitrouter_ai::routing::ContextStrategy::Preserve,
        reason: "0".repeat(64),
        candidates: Vec::new(),
    };
    let routing = routing_evidence_from(
        ctx.request_id(),
        prepared.receipt.as_ref(),
        Some(&selection),
    )?;
    Reservation::new(bound(&routing)?, observe)
}

fn largest<'a>(values: impl Iterator<Item = &'a str>) -> Result<String> {
    let mut winner = None;
    for value in values {
        let bytes = json_size(&value, 60 * 1024)?;
        if winner.as_ref().is_none_or(|(size, _)| bytes > *size) {
            winner = Some((bytes, value.to_owned()));
        }
    }
    winner
        .map(|(_, value)| value)
        .context("terminal reservation has no admitted candidate")
}

fn bound(routing: &[bitrouter_ai::eval::EvidenceItem]) -> Result<usize> {
    // Keep this complete finite vocabulary in sync with Facts::capture and
    // PredictionObservationSnapshot::write_namespaced. Maximum JSON escaping
    // is six bytes per input byte, represented by NUL sizing strings here.
    // https://www.rfc-editor.org/rfc/rfc8259#section-7
    let structural = [
        "settlement.duration_ms",
        "settlement.prompt_tokens",
        "settlement.completion_tokens",
        "settlement.reasoning_tokens",
        "settlement.cache_read_tokens",
        "settlement.cache_write_tokens",
        "routing.prediction_confidence_ppm",
        "routing.task_family_confidence_ppm",
        "routing.observation_confidence_ppm",
    ]
    .into_iter()
    .map(|key| (key.into(), u64::MAX))
    .collect();
    let categorical = [
        ("settlement.usage_origin", 128),
        ("settlement.outcome", 128),
        ("settlement.error_code", 128),
        ("routing.continuation_proposed_tier", 128),
        ("routing.continuation_proposed_model", 512),
        ("routing.continuation_proposed_effort", 128),
        ("routing.continuation_adjustment", 32),
        ("routing.predicted_role", 128),
        ("routing.predicted_task_family", 128),
        ("routing.predicted_action", 128),
        ("routing.task_family_reason_codes", 128),
        ("routing.predictor_contract_digest", 71),
        ("routing.prediction_confidence_kind", 32),
        ("routing.observed_action", 128),
        ("routing.observation_reason_code", 128),
        ("routing.action_match", 128),
    ]
    .into_iter()
    .map(|(key, bytes)| (key.into(), "\0".repeat(bytes)))
    .collect();
    let evidence = TrajectoryEvidence {
        structural,
        categorical,
        digests: BTreeMap::new(),
        route_measurement: None,
        routing_evidence: routing.to_vec(),
    };
    json_size(&evidence, 60 * 1024).map_err(|_| Error::Capacity.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reservation_counts_escaped_candidates_and_rejects_oversized_receipts() -> Result<()> {
        assert_eq!(largest(["abcdefgh", "\0\0"].into_iter())?, "\0\0");
        let empty = bound(&[])?;
        assert!(empty < 20 * 1024);
        let item = bitrouter_ai::eval::EvidenceItem {
            evidence_id: "receipt".into(),
            kind: "routing.assessment".into(),
            digest: format!("sha256:{}", "0".repeat(64)),
            redacted: true,
            attributes: BTreeMap::from([("judgments".into(), "\0".repeat(8192))]),
        };
        assert!(bound(&[item]).is_err());
        Ok(())
    }
}
