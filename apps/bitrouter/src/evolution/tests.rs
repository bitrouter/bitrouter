use anyhow::Result;
use bitrouter_sdk::acp::capture::{CaptureDirection, CaptureEvent, CaptureKind, CapturePort};
use serde_json::json;

use super::{evidence::EvidencePacket, rubric::*, scoring};
use crate::acp_trajectory::checkpoint::types::{AssessmentSource, CriterionScore};
use crate::acp_trajectory::{CanonicalStore, RecordingScope, SessionIdentity};

pub(super) async fn fixture() -> Result<(CanonicalStore, SessionIdentity, String)> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = CanonicalStore::new(db);
    let identity = SessionIdentity {
        owner: "local".into(),
        source: "fixture".into(),
        native_session_id: "session".into(),
    };
    let recorder = store
        .recorder(RecordingScope {
            owner: identity.owner.clone(),
            source: identity.source.clone(),
            controller_instance_id: None,
            route_scope_id: None,
        })
        .await?;
    for (direction, kind, call_id, method, payload) in [
        (
            CaptureDirection::Client,
            CaptureKind::Request,
            Some(1),
            "session/new",
            json!({"cwd":"/fixture"}),
        ),
        (
            CaptureDirection::Client,
            CaptureKind::Response,
            Some(1),
            "session/new",
            json!({"result":{"sessionId":"session"}}),
        ),
        (
            CaptureDirection::Client,
            CaptureKind::Request,
            Some(2),
            "session/prompt",
            json!({"sessionId":"session","prompt":[{"type":"text","text":"Fix the parser and run its tests. Do not create a PR."}]}),
        ),
        (
            CaptureDirection::Agent,
            CaptureKind::Notification,
            None,
            "session/update",
            json!({"sessionId":"session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Fixed it; tests pass."}}}),
        ),
    ] {
        recorder
            .record(CaptureEvent {
                direction,
                kind,
                call_id,
                method: method.into(),
                payload,
            })
            .await?;
    }
    let transcript = store.transcript(&identity).await?;
    let checkpoint = store
        .freeze_checkpoint(&identity, transcript.session.head)
        .await?;
    Ok((store, identity, checkpoint.checkpoint_id))
}

pub(super) fn unknown(packet: &EvidencePacket) -> RubricEvaluation {
    RubricEvaluation {
        rubric_version: RUBRIC_VERSION.into(),
        items: library().into_iter().map(|template| RubricItem {
            criterion_id: template.id.into(),
            applicability: if template.mandatory || template.id == "verification" { Applicability::Applicable } else { Applicability::NotApplicable },
            selection_reason: "Observed request requires delivery, constraints and verification; no other obligation is visible.".into(),
            score: if template.mandatory || template.id == "verification" { CriterionScore::Unknown } else { CriterionScore::NotApplicable },
            evidence: packet.items.first().map(|i| vec![i.citation.clone()]).unwrap_or_default(),
            explanation: "There is no observed artifact or executed verification result.".into(),
        }).collect(),
        diagnostics: vec![], severe_violation: false, violation_evidence: vec![],
        summary: "Execution evidence is missing; an assistant assertion is not verification.".into(),
    }
}

#[tokio::test]
async fn missing_verification_is_unknown_and_claims_cannot_be_scored_as_tests() -> Result<()> {
    let (store, identity, checkpoint) = fixture().await?;
    let input = scoring::prepare(&store, &identity, &checkpoint).await?;
    let mut evaluation = unknown(&input.evidence);
    let quality = evaluation.aggregate(&input.evidence)?;
    assert_eq!((quality.lower_ppm, quality.upper_ppm), (0, PPM));
    assert!(quality.complete_score().is_none());
    let verification = evaluation
        .items
        .iter_mut()
        .find(|i| i.criterion_id == "verification")
        .ok_or_else(|| anyhow::anyhow!("verification template missing"))?;
    verification.score = CriterionScore::Scored { value_ppm: PPM };
    verification.evidence = vec![
        input
            .evidence
            .items
            .last()
            .ok_or_else(|| anyhow::anyhow!("missing fixture claim"))?
            .citation
            .clone(),
    ];
    assert!(evaluation.aggregate(&input.evidence).is_err());
    Ok(())
}

#[tokio::test]
async fn partial_scores_keep_unknown_weight_in_denominator() -> Result<()> {
    let (store, identity, checkpoint) = fixture().await?;
    let input = scoring::prepare(&store, &identity, &checkpoint).await?;
    let mut evaluation = unknown(&input.evidence);
    evaluation.items[0].score = CriterionScore::Scored { value_ppm: 500_000 };
    let quality = evaluation.aggregate(&input.evidence)?;
    assert_eq!(quality.applicable_weight, 10);
    assert_eq!(quality.unknown_weight, 6);
    assert_eq!(
        (quality.lower_ppm, quality.upper_ppm, quality.coverage_ppm),
        (200_000, 800_000, 400_000)
    );
    evaluation.items[0].applicability = Applicability::NotApplicable;
    evaluation.items[0].score = CriterionScore::NotApplicable;
    assert!(evaluation.aggregate(&input.evidence).is_err());
    Ok(())
}

#[tokio::test]
async fn legacy_labels_and_unversioned_packets_cannot_enter_current_aggregation() -> Result<()> {
    let (store, identity, checkpoint) = fixture().await?;
    let input = scoring::prepare(&store, &identity, &checkpoint).await?;
    let mut old_label = unknown(&input.evidence);
    old_label.rubric_version = "coding-checkpoint-rubric-v1".into();
    assert!(old_label.aggregate(&input.evidence).is_err());
    let mut encoded = serde_json::to_value(&input.evidence)?;
    encoded
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("packet is not an object"))?
        .remove("projection_version");
    let old_packet: EvidencePacket = serde_json::from_value(encoded)?;
    assert!(unknown(&input.evidence).aggregate(&old_packet).is_err());
    assert!(unknown(&input.evidence).aggregate(&input.evidence).is_ok());
    Ok(())
}

#[tokio::test]
async fn rubric_revisions_are_idempotent_and_preserve_model_provenance() -> Result<()> {
    let (store, identity, checkpoint) = fixture().await?;
    let input = scoring::prepare(&store, &identity, &checkpoint).await?;
    let submission = scoring::RubricSubmission {
        submission_id: "blind-reference-1".into(),
        checkpoint_id: checkpoint,
        expected_revision: None,
        source: AssessmentSource::Agentic,
        evaluator_id: "reference-model".into(),
        evaluator_version: "1".into(),
        evaluation: unknown(&input.evidence),
    };
    let receipt = scoring::submit(&store, &identity, submission.clone()).await?;
    let retry = scoring::submit(&store, &identity, submission).await?;
    assert_eq!(receipt.revision.revision_id, retry.revision.revision_id);
    assert_eq!(store.assessment_history(&identity).await?.len(), 1);
    assert_eq!(receipt.revision.input.source, AssessmentSource::Agentic);
    let mut correction = receipt.revision.input.clone();
    correction.submission_id = "operator-correction".into();
    correction.expected_revision = Some(receipt.revision.revision_id);
    correction.source = AssessmentSource::Human;
    let revised = store.submit_assessment(&identity, correction).await?;
    assert_eq!(
        store
            .effective_assessment(&identity)
            .await?
            .current_revision,
        Some(revised.revision_id)
    );
    Ok(())
}

#[tokio::test]
async fn fabricated_or_cross_checkpoint_citations_are_rejected() -> Result<()> {
    let (store, identity, checkpoint) = fixture().await?;
    let input = scoring::prepare(&store, &identity, &checkpoint).await?;
    let mut evaluation = unknown(&input.evidence);
    evaluation.items[0].evidence[0].digest = "a".repeat(64);
    assert!(evaluation.aggregate(&input.evidence).is_err());
    let mut evaluation = unknown(&input.evidence);
    evaluation.severe_violation = true;
    assert!(evaluation.aggregate(&input.evidence).is_err());
    evaluation = unknown(&input.evidence);
    evaluation.items.pop();
    assert!(evaluation.aggregate(&input.evidence).is_err());
    Ok(())
}

#[tokio::test]
async fn projection_hides_boundary_usage_without_rewriting_tool_data() -> Result<()> {
    let (store, identity, _) = fixture().await?;
    let recorder = store
        .recorder(RecordingScope {
            owner: identity.owner.clone(),
            source: identity.source.clone(),
            controller_instance_id: None,
            route_scope_id: None,
        })
        .await?;
    let business_data = json!({"model":"customer-model","_meta":{"price":42}});
    for (kind, method, call_id, payload) in [
        (
            CaptureKind::Request,
            "session/load",
            Some(1),
            json!({"sessionId":"session","cwd":"/fixture"}),
        ),
        (
            CaptureKind::Response,
            "session/load",
            Some(1),
            json!({"result":{}}),
        ),
        (
            CaptureKind::Request,
            "session/prompt",
            Some(2),
            json!({"sessionId":"session","prompt":[{"type":"text","text":"Inspect this model field without changing it."}]}),
        ),
        (
            CaptureKind::Notification,
            "session/update",
            None,
            json!({"sessionId":"session","update":{"sessionUpdate":"tool_call","toolCallId":"inspect","status":"completed","rawOutput":business_data,"_meta":{"provider":"private-provider"}}}),
        ),
        (
            CaptureKind::Response,
            "session/prompt",
            Some(2),
            json!({"result":{"stopReason":"end_turn","usage":{"totalTokens":900},"_meta":{"quota":{"model_usage":[{"model":"private-model","cost":123}]}}},"_meta":{"routing":"private-route"}}),
        ),
    ] {
        recorder
            .record(CaptureEvent {
                direction: if kind == CaptureKind::Request {
                    CaptureDirection::Client
                } else {
                    CaptureDirection::Agent
                },
                kind,
                call_id,
                method: method.into(),
                payload,
            })
            .await?;
    }
    let transcript = store.transcript(&identity).await?;
    let checkpoint = store
        .freeze_checkpoint(&identity, transcript.session.head)
        .await?;
    let original = store
        .checkpoint_content(&identity, &checkpoint.checkpoint_id)
        .await?;
    let packet = EvidencePacket::from_checkpoint(&original)?;
    let boundary = packet
        .items
        .iter()
        .rev()
        .find(|item| item.kind == super::evidence::EvidenceKind::SessionBoundary)
        .ok_or_else(|| anyhow::anyhow!("boundary missing"))?;
    assert_eq!(
        boundary.content,
        json!({"result":{"stopReason":"end_turn"}})
    );
    let tool = packet
        .items
        .iter()
        .find(|item| item.content.get("toolCallId") == Some(&json!("inspect")))
        .ok_or_else(|| anyhow::anyhow!("tool result missing"))?;
    assert_eq!(tool.content["rawOutput"], business_data);
    assert!(tool.content.get("_meta").is_none());
    let raw = original
        .events
        .iter()
        .find(|event| event.node_id == boundary.citation.node_id)
        .ok_or_else(|| anyhow::anyhow!("original boundary missing"))?;
    assert_eq!(
        raw.event.payload["result"]["_meta"]["quota"]["model_usage"][0]["model"],
        "private-model"
    );
    let source_reference = checkpoint
        .segments
        .iter()
        .flat_map(|segment| &segment.events)
        .find(|reference| reference.node_id() == boundary.citation.node_id)
        .ok_or_else(|| anyhow::anyhow!("source reference missing"))?;
    assert_eq!(boundary.citation.digest, source_reference.digest);
    Ok(())
}
