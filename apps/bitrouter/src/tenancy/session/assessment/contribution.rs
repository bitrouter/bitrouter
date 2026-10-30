//! Only a current checked revision can add quality to operational native evidence.
use super::store;
use crate::{
    core::{
        eval::compiler::EvalEvidenceRecord,
        evolution::rubric::{PPM, RubricEvaluation},
    },
    harness::trajectory::owned::io,
    storage::{
        evaluation::{
            canonical_digest, evidence_digest, result_digest, validate_result_for_subject,
        },
        operation_budget::{Error, OperationBudget},
        scope::Scope,
    },
    tenancy::experiment::store::completed::Reader,
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::eval::{
    DecisionCredit, EVAL_SCHEMA_VERSION, EvalVerdict, EvaluationResult, EvaluatorIdentity,
    EvaluatorKind, EvidenceItem, MetricUnit, MetricValue,
};
use sea_orm::DatabaseTransaction;
use std::collections::{BTreeMap, BTreeSet};

pub(in crate::tenancy) struct Contribution {
    pub record: Option<EvalEvidenceRecord>,
    pub head_digest: String,
    pub conclusive: bool,
}

pub(in crate::tenancy) async fn acquire(
    tx: &DatabaseTransaction,
    scope: &Scope,
    unit: &str,
    reader: Reader<'_>,
    budget: &OperationBudget<'_>,
) -> Result<Contribution> {
    let Some(view) = store::selected(tx, scope, unit, reader, budget).await? else {
        return Ok(Contribution {
            record: None,
            head_digest: canonical_digest(&(
                "native-effective-head-v1",
                unit,
                Option::<String>::None,
            ))?,
            conclusive: false,
        });
    };
    let selected = view
        .selected
        .as_ref()
        .context("native assessment source has no current head")?;
    // Includes the immutable source, selected revision and normalized author;
    // a correction or explicit retraction invalidates outstanding computation.
    let head_digest =
        canonical_digest(&("native-effective-head-v1", view.completion.edge(), selected))?;
    let Some(content) = &selected.input.assessment else {
        return Ok(Contribution {
            record: None,
            head_digest,
            conclusive: false,
        });
    };
    let rubric: RubricEvaluation = serde_json::from_str(&content.explanation)?;
    let quality = rubric.aggregate(view.completion.packet())?;
    // Match the existing workflow reward adapter's full-credit pass semantics.
    // Missing evidence remains unknown; neither cost nor transport success is quality.
    let verdict = if quality.severe_violation {
        EvalVerdict::Fail
    } else if quality.complete_score().is_none() {
        EvalVerdict::Inconclusive
    } else if quality.lower_ppm == PPM {
        EvalVerdict::Pass
    } else {
        EvalVerdict::Fail
    };
    let conclusive = verdict != EvalVerdict::Inconclusive;
    let mut subject = view
        .completion
        .records()
        .first()
        .context("native complete unit has no publications")?
        .subject
        .clone();
    let mut decisions = BTreeMap::new();
    for record in view.completion.records() {
        ensure!(
            record.subject.subject_id == subject.subject_id
                && record.subject.policy_digest == subject.policy_digest,
            "native quality source spans units"
        );
        for decision in &record.subject.decisions {
            if let Some(previous) = decisions.insert(decision.decision_id.clone(), decision.clone())
            {
                ensure!(
                    previous == *decision,
                    "native quality decision identity changed"
                );
            }
        }
    }
    ensure!(
        !decisions.is_empty(),
        "native quality has no exact decision attribution"
    );
    subject.decisions = decisions.into_values().collect();
    subject.eval_id = format!("native-quality:{}", selected.revision_id);
    let attributes = BTreeMap::from([
        (
            "projection_version".into(),
            view.completion.packet().projection_version.clone(),
        ),
        (
            "checkpoint".into(),
            view.completion.edge().checkpoint.clone(),
        ),
        (
            "source_closure".into(),
            view.completion.edge().closure.clone(),
        ),
        ("assessment_revision".into(), selected.revision_id.clone()),
        ("effective_head".into(), head_digest.clone()),
    ]);
    subject.evidence = vec![EvidenceItem {
        evidence_id: "native-rubric-head".into(),
        kind: "native.rubric.revision.v1".into(),
        digest: canonical_digest(&attributes)?,
        redacted: true,
        attributes,
    }];
    subject.evidence_digest = evidence_digest(&subject.evidence)?;
    subject.requested_dimensions = BTreeSet::from(["quality.pass".into()]);
    let hard_violations = if quality.severe_violation {
        vec!["safety.native_rubric".into()]
    } else {
        Vec::new()
    };
    let mut metric_ids = BTreeSet::from(["quality.pass".into()]);
    metric_ids.extend(hard_violations.iter().cloned());
    let metrics = if conclusive {
        BTreeMap::from([(
            "quality.pass".into(),
            MetricValue {
                value: i64::from(verdict == EvalVerdict::Pass),
                unit: MetricUnit::Boolean,
            },
        )])
    } else {
        BTreeMap::new()
    };
    let decision_credit = subject
        .decisions
        .iter()
        .map(|decision| {
            (
                decision.decision_id.clone(),
                DecisionCredit {
                    weight_ppm: 1_000_000,
                    metric_ids: metric_ids.clone(),
                },
            )
        })
        .collect();
    let result = EvaluationResult {
        schema_version: EVAL_SCHEMA_VERSION,
        eval_id: subject.eval_id.clone(),
        evidence_digest: subject.evidence_digest.clone(),
        evaluator: EvaluatorIdentity {
            authority_id: view.completion.principal().into(),
            evaluator_id: selected.input.evaluator_id.clone(),
            kind: EvaluatorKind::Human,
            version: selected.input.evaluator_version.clone(),
            config_digest: canonical_digest(&(
                "native-rubric-full-credit-v1",
                &content.pipeline_config_digest,
            ))?,
        },
        verdict,
        metrics,
        hard_violations,
        confidence_ppm: None,
        evidence_refs: vec!["native-rubric-head".into()],
        decision_credit,
        idempotency_key: format!("native-quality:{}", selected.revision_id),
        submitted_at: selected.created_at.clone(),
    };
    validate_result_for_subject(&result, &subject)?;
    let record = EvalEvidenceRecord {
        result_id: format!("native-quality:{}", selected.revision_id),
        content_digest: result_digest(&result)?,
        subject,
        result,
    };
    budget.charge(
        0,
        io::json_len(&record, 1024 * 1024).map_err(|_| Error::Capacity)?,
    )?;
    Ok(Contribution {
        record: Some(record),
        head_digest,
        conclusive,
    })
}
