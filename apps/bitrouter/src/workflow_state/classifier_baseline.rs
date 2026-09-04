//! Frozen research-dataset manifest for semantic classifier comparisons.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::fixture::WorkflowTraceFixture;
use super::replay::ReplayEvaluator;
use crate::eval::types::canonical_digest;

pub const CLASSIFIER_BASELINE_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierEvaluationCase {
    pub fixture_id: String,
    pub input_projection_digest: String,
    pub task_family: String,
    pub next_step_role: String,
    pub progress_state: String,
    pub route_risk: String,
    pub research_slices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifierBaselineManifest {
    pub schema_version: u32,
    pub dataset_digest: String,
    pub fixture_count: usize,
    pub by_slice: BTreeMap<String, usize>,
    pub current_predictor_exact_count: usize,
    pub current_predictor_mismatch_count: usize,
    pub evaluation_cases: Vec<ClassifierEvaluationCase>,
}

#[derive(Serialize)]
struct FrozenInferenceInput<'a> {
    harness: &'a super::ir::HarnessId,
    protocol: &'a super::ir::ProtocolKind,
    headers: Vec<(String, String)>,
    raw_body: &'a serde_json::Value,
    canonical_prompt: &'a Option<serde_json::Value>,
}

#[derive(Serialize)]
struct FrozenFixtureInput<'a> {
    id: &'a str,
    inference_input: FrozenInferenceInput<'a>,
    expected: &'a super::fixture::ExpectedWorkflowState,
    research_slices: &'a std::collections::BTreeSet<String>,
}

impl ClassifierBaselineManifest {
    pub fn from_fixtures(fixtures: &[WorkflowTraceFixture]) -> anyhow::Result<Self> {
        let selected = selected_research_fixtures(fixtures);
        if selected.is_empty() {
            anyhow::bail!("classifier bake-off requires at least one research fixture")
        }
        let by_slice =
            selected
                .iter()
                .fold(BTreeMap::<String, usize>::new(), |mut counts, fixture| {
                    for slice in &fixture.research_slices {
                        *counts.entry(slice.clone()).or_default() += 1;
                    }
                    counts
                });
        let digest_input = selected
            .iter()
            .map(|fixture| frozen_fixture_input(fixture))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let dataset_digest =
            canonical_digest(&("bitrouter.classifier-baseline-dataset.v2", digest_input))?;
        let evaluation_cases = selected
            .iter()
            .map(|fixture| evaluation_case(fixture))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let replay = ReplayEvaluator.run(
            &selected
                .iter()
                .map(|fixture| (*fixture).clone())
                .collect::<Vec<_>>(),
        );
        Ok(Self {
            schema_version: CLASSIFIER_BASELINE_SCHEMA_VERSION,
            dataset_digest,
            fixture_count: selected.len(),
            by_slice,
            current_predictor_exact_count: replay.predictive_exact_count,
            current_predictor_mismatch_count: replay
                .predictive_expectation_count
                .saturating_sub(replay.predictive_exact_count),
            evaluation_cases,
        })
    }
}

fn frozen_fixture_input(fixture: &WorkflowTraceFixture) -> anyhow::Result<FrozenFixtureInput<'_>> {
    Ok(FrozenFixtureInput {
        id: fixture.id.as_str(),
        inference_input: frozen_inference_input(fixture)?,
        expected: &fixture.expected,
        research_slices: &fixture.research_slices,
    })
}

fn frozen_inference_input(
    fixture: &WorkflowTraceFixture,
) -> anyhow::Result<FrozenInferenceInput<'_>> {
    let mut headers = fixture
        .headers
        .iter()
        .map(|(name, value)| Ok((name.as_str().to_owned(), value.to_str()?.to_owned())))
        .collect::<anyhow::Result<Vec<_>>>()?;
    headers.sort();
    Ok(FrozenInferenceInput {
        harness: &fixture.harness,
        protocol: &fixture.protocol,
        headers,
        raw_body: &fixture.raw_body,
        canonical_prompt: &fixture.canonical_prompt,
    })
}

fn evaluation_case(fixture: &WorkflowTraceFixture) -> anyhow::Result<ClassifierEvaluationCase> {
    let expected = fixture.expected.prediction.as_ref().ok_or_else(|| {
        anyhow::anyhow!("research fixture {} lacks a prediction label", fixture.id)
    })?;
    let task_family = expected.task_family.ok_or_else(|| {
        anyhow::anyhow!("research fixture {} lacks a task-family label", fixture.id)
    })?;
    let progress_state = expected.progress_state.ok_or_else(|| {
        anyhow::anyhow!(
            "research fixture {} lacks a progress-state label",
            fixture.id
        )
    })?;
    Ok(ClassifierEvaluationCase {
        fixture_id: fixture.id.clone(),
        input_projection_digest: canonical_digest(&(
            "bitrouter.classifier-input-projection.v1",
            frozen_inference_input(fixture)?,
        ))?,
        task_family: task_family.key().to_owned(),
        next_step_role: expected.next_step_role.key().to_owned(),
        progress_state: progress_state.key().to_owned(),
        route_risk: expected.route_risk.to_string(),
        research_slices: fixture.research_slices.iter().cloned().collect(),
    })
}

pub(crate) fn selected_research_fixtures(
    fixtures: &[WorkflowTraceFixture],
) -> Vec<&WorkflowTraceFixture> {
    let mut selected = fixtures
        .iter()
        .filter(|fixture| !fixture.research_slices.is_empty())
        .collect::<Vec<_>>();
    selected.sort_by(|left, right| left.id.cmp(&right.id));
    selected
}
