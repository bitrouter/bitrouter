//! Frozen research-dataset manifest for semantic classifier comparisons.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::fixture::WorkflowTraceFixture;
use super::replay::ReplayEvaluator;
use crate::eval::types::canonical_digest;

pub const CLASSIFIER_BASELINE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifierBaselineManifest {
    pub schema_version: u32,
    pub dataset_digest: String,
    pub fixture_count: usize,
    pub by_slice: BTreeMap<String, usize>,
    pub current_predictor_exact_count: usize,
    pub current_predictor_mismatch_count: usize,
}

#[derive(Serialize)]
struct FrozenFixtureInput<'a> {
    id: &'a str,
    harness: &'a super::ir::HarnessId,
    protocol: &'a super::ir::ProtocolKind,
    headers: Vec<(String, String)>,
    raw_body: &'a serde_json::Value,
    canonical_prompt: &'a Option<serde_json::Value>,
    expected: &'a super::fixture::ExpectedWorkflowState,
    research_slices: &'a std::collections::BTreeSet<String>,
}

impl ClassifierBaselineManifest {
    pub fn from_fixtures(fixtures: &[WorkflowTraceFixture]) -> anyhow::Result<Self> {
        let mut selected = fixtures
            .iter()
            .filter(|fixture| !fixture.research_slices.is_empty())
            .collect::<Vec<_>>();
        selected.sort_by(|left, right| left.id.cmp(&right.id));
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
            .map(|fixture| -> anyhow::Result<_> {
                let mut headers = fixture
                    .headers
                    .iter()
                    .map(|(name, value)| Ok((name.as_str().to_owned(), value.to_str()?.to_owned())))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                headers.sort();
                Ok(FrozenFixtureInput {
                    id: fixture.id.as_str(),
                    harness: &fixture.harness,
                    protocol: &fixture.protocol,
                    headers,
                    raw_body: &fixture.raw_body,
                    canonical_prompt: &fixture.canonical_prompt,
                    expected: &fixture.expected,
                    research_slices: &fixture.research_slices,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let dataset_digest =
            canonical_digest(&("bitrouter.classifier-baseline-dataset.v1", digest_input))?;
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
        })
    }
}
