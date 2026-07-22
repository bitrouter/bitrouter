use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::workflow_state::extractors::{ExtractorInput, extract_workflow_state};
use crate::workflow_state::fixture::WorkflowTraceFixture;
use crate::workflow_state::ir::{
    CapabilityConstraints, EvidenceLevel, HarnessId, RequirementLevel, SessionConfidence,
    WorkflowStateKind,
};

#[derive(Debug, Default)]
pub struct ReplayEvaluator;

#[derive(Debug, Default, Serialize)]
pub struct ReplaySummary {
    pub total: usize,
    pub covered: usize,
    pub coverage: f32,
    pub baseline_bucket_count: usize,
    pub ir_bucket_count: usize,
    pub collision_count: usize,
    pub visibility_gap_count: usize,
    pub visibility_gaps_by_harness: BTreeMap<String, usize>,
    pub session_confidence_distribution: BTreeMap<String, usize>,
    pub baseline_midstream_count: usize,
    pub ir_unknown_count: usize,
    pub model_ladder: ModelLadderConstraintSummary,
}

#[derive(Debug, Default, Serialize)]
pub struct ModelLadderConstraintSummary {
    pub flagship: usize,
    pub open_source_flagship: usize,
    pub standard: usize,
    pub cheap_tool_safe: usize,
    pub cheap_fast: usize,
}

impl ReplayEvaluator {
    pub fn run(&self, fixtures: &[WorkflowTraceFixture]) -> ReplaySummary {
        let mut summary = ReplaySummary {
            total: fixtures.len(),
            ..ReplaySummary::default()
        };
        let mut baseline_buckets = BTreeSet::new();
        let mut ir_buckets = BTreeSet::new();
        let mut labels_by_ir_key: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        for fixture in fixtures {
            let baseline = fixture.baseline_fingerprint();
            if baseline == "midstream" {
                summary.baseline_midstream_count += 1;
            }
            baseline_buckets.insert(baseline);

            let ir = extract_fixture_ir(fixture);
            if ir.state_kind == WorkflowStateKind::Unknown {
                summary.ir_unknown_count += 1;
            }
            if ir.state_kind != WorkflowStateKind::Unknown
                && ir.confidence >= fixture.expected.confidence_min
            {
                summary.covered += 1;
            }

            let key = ir.routing_key();
            ir_buckets.insert(key.clone());
            labels_by_ir_key
                .entry(key)
                .or_default()
                .insert(fixture.expected.state_kind.to_string());

            if ir
                .evidence
                .iter()
                .any(|e| e.kind == "server_side_context_gap" && e.level == EvidenceLevel::Missing)
            {
                summary.visibility_gap_count += 1;
                *summary
                    .visibility_gaps_by_harness
                    .entry(harness_key(&fixture.harness))
                    .or_insert(0) += 1;
            }

            *summary
                .session_confidence_distribution
                .entry(session_key(&ir.session.confidence))
                .or_insert(0) += 1;

            summary.model_ladder.observe(&ir.capability_constraints);
        }

        summary.coverage = if summary.total == 0 {
            0.0
        } else {
            summary.covered as f32 / summary.total as f32
        };
        summary.baseline_bucket_count = baseline_buckets.len();
        summary.ir_bucket_count = ir_buckets.len();
        summary.collision_count = labels_by_ir_key
            .values()
            .filter(|labels| labels.len() > 1)
            .count();
        summary
    }
}

impl ModelLadderConstraintSummary {
    fn observe(&mut self, constraints: &CapabilityConstraints) {
        self.flagship += 1;
        if constraints.context_pressure != RequirementLevel::High {
            self.open_source_flagship += 1;
        }
        if constraints.context_pressure != RequirementLevel::High
            && constraints.expected_redo_penalty != RequirementLevel::High
        {
            self.standard += 1;
        }
        if constraints.context_pressure != RequirementLevel::High {
            self.cheap_tool_safe += 1;
        }
        if constraints.tool_reliability == RequirementLevel::Low
            && constraints.context_pressure == RequirementLevel::Low
            && constraints.expected_redo_penalty != RequirementLevel::High
        {
            self.cheap_fast += 1;
        }
    }
}

pub fn extract_fixture_ir(
    fixture: &WorkflowTraceFixture,
) -> crate::workflow_state::ir::WorkflowStateIR {
    let input = ExtractorInput {
        harness_hint: Some(fixture.harness.clone()),
        protocol_hint: fixture.protocol.clone(),
        headers: &fixture.headers,
        raw_body: &fixture.raw_body,
        prompt: &fixture.prompt,
    };
    extract_workflow_state(&input)
}

fn harness_key(harness: &HarnessId) -> String {
    match harness {
        HarnessId::Generic => "generic",
        HarnessId::Hermes => "hermes",
        HarnessId::ClaudeCode => "claude_code",
        HarnessId::Codex => "codex",
        HarnessId::Terminus2 => "terminus_2",
        HarnessId::OpenClaw => "openclaw",
        HarnessId::Unknown => "unknown",
    }
    .to_string()
}

fn session_key(confidence: &SessionConfidence) -> String {
    match confidence {
        SessionConfidence::None => "none",
        SessionConfidence::Low => "low",
        SessionConfidence::Medium => "medium",
        SessionConfidence::High => "high",
    }
    .to_string()
}
