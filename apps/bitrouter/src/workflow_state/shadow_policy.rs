use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::workflow_state::fixture::WorkflowTraceFixture;
use crate::workflow_state::ir::{HarnessId, RequirementLevel, ToolDensity, WorkflowStateKind};
use crate::workflow_state::replay::extract_fixture_ir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TierName {
    Flagship,
    OpenSourceFlagship,
    Standard,
    CheapToolSafe,
    CheapFast,
}

#[derive(Debug, Default)]
pub struct ShadowPolicyEvaluator;

#[derive(Debug, Default, Serialize)]
pub struct ShadowPolicySummary {
    pub total: usize,
    pub changed_count: usize,
    pub downgraded_count: usize,
    pub upgraded_count: usize,
    pub unsafe_cheap_fast_violations: usize,
    pub baseline_route_counts: BTreeMap<TierName, usize>,
    pub ir_route_counts: BTreeMap<TierName, usize>,
    pub by_state_kind: BTreeMap<String, usize>,
    pub by_harness: BTreeMap<String, usize>,
    pub decisions: Vec<ShadowPolicyDecision>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShadowPolicyDecision {
    pub fixture_id: String,
    pub harness: HarnessId,
    pub baseline_key: String,
    pub ir_key: String,
    pub ir_state_kind: WorkflowStateKind,
    pub baseline_tier: TierName,
    pub ir_tier: TierName,
    pub changed: bool,
    pub direction: TierDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TierDirection {
    Same,
    Downgrade,
    Upgrade,
}

impl ShadowPolicyEvaluator {
    pub fn run(&self, fixtures: &[WorkflowTraceFixture]) -> ShadowPolicySummary {
        let mut summary = ShadowPolicySummary {
            total: fixtures.len(),
            ..ShadowPolicySummary::default()
        };

        for fixture in fixtures {
            let baseline_key = fixture.baseline_fingerprint();
            let baseline_tier = tier_for_baseline_key(&baseline_key);
            let ir = extract_fixture_ir(fixture);
            let ir_tier = tier_for_ir(&ir.capability_constraints, &ir.tool_density);
            let direction = tier_direction(baseline_tier, ir_tier);

            *summary
                .baseline_route_counts
                .entry(baseline_tier)
                .or_insert(0) += 1;
            *summary.ir_route_counts.entry(ir_tier).or_insert(0) += 1;
            *summary
                .by_state_kind
                .entry(ir.state_kind.to_string())
                .or_insert(0) += 1;
            *summary
                .by_harness
                .entry(harness_key(&fixture.harness))
                .or_insert(0) += 1;

            if direction != TierDirection::Same {
                summary.changed_count += 1;
            }
            if direction == TierDirection::Downgrade {
                summary.downgraded_count += 1;
            }
            if direction == TierDirection::Upgrade {
                summary.upgraded_count += 1;
            }
            if ir_tier == TierName::CheapFast
                && !cheap_fast_compatible(&ir.capability_constraints, &ir.tool_density)
            {
                summary.unsafe_cheap_fast_violations += 1;
            }

            summary.decisions.push(ShadowPolicyDecision {
                fixture_id: fixture.id.clone(),
                harness: fixture.harness.clone(),
                baseline_key,
                ir_key: ir.routing_key(),
                ir_state_kind: ir.state_kind,
                baseline_tier,
                ir_tier,
                changed: direction != TierDirection::Same,
                direction,
            });
        }

        summary
    }
}

fn tier_for_baseline_key(key: &str) -> TierName {
    if key.starts_with("after_") {
        TierName::CheapToolSafe
    } else {
        TierName::Flagship
    }
}

fn tier_for_ir(
    constraints: &crate::workflow_state::ir::CapabilityConstraints,
    tool_density: &ToolDensity,
) -> TierName {
    if constraints.context_pressure == RequirementLevel::High {
        return TierName::Flagship;
    }
    if constraints.expected_redo_penalty == RequirementLevel::High
        || constraints.code_reasoning == RequirementLevel::High
        || constraints.output_precision == RequirementLevel::High
    {
        return TierName::OpenSourceFlagship;
    }
    if cheap_fast_compatible(constraints, tool_density) {
        return TierName::CheapFast;
    }
    if *tool_density != ToolDensity::None || constraints.tool_reliability == RequirementLevel::High
    {
        return TierName::CheapToolSafe;
    }
    if constraints.context_pressure == RequirementLevel::Medium
        || constraints.code_reasoning == RequirementLevel::Medium
        || constraints.expected_redo_penalty == RequirementLevel::Medium
        || constraints.output_precision == RequirementLevel::Medium
    {
        return TierName::Standard;
    }
    TierName::CheapToolSafe
}

fn cheap_fast_compatible(
    constraints: &crate::workflow_state::ir::CapabilityConstraints,
    tool_density: &ToolDensity,
) -> bool {
    *tool_density == ToolDensity::None
        && constraints.tool_reliability == RequirementLevel::Low
        && constraints.context_pressure == RequirementLevel::Low
        && constraints.expected_redo_penalty != RequirementLevel::High
}

fn tier_direction(from: TierName, to: TierName) -> TierDirection {
    match tier_rank(to).cmp(&tier_rank(from)) {
        std::cmp::Ordering::Less => TierDirection::Downgrade,
        std::cmp::Ordering::Greater => TierDirection::Upgrade,
        std::cmp::Ordering::Equal => TierDirection::Same,
    }
}

fn tier_rank(tier: TierName) -> usize {
    match tier {
        TierName::Flagship => 5,
        TierName::OpenSourceFlagship => 4,
        TierName::Standard => 3,
        TierName::CheapToolSafe => 2,
        TierName::CheapFast => 1,
    }
}

fn harness_key(harness: &HarnessId) -> String {
    match harness {
        HarnessId::Generic => "generic",
        HarnessId::Hermes => "hermes",
        HarnessId::ClaudeCode => "claude_code",
        HarnessId::Codex => "codex",
        HarnessId::OpenClaw => "openclaw",
        HarnessId::Unknown => "unknown",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::workflow_state::ir::{CapabilityConstraints, RequirementLevel};

    fn constraints(
        tool_reliability: RequirementLevel,
        code_reasoning: RequirementLevel,
        context_pressure: RequirementLevel,
        expected_redo_penalty: RequirementLevel,
        output_precision: RequirementLevel,
    ) -> CapabilityConstraints {
        CapabilityConstraints {
            tool_reliability,
            code_reasoning,
            context_pressure,
            latency_sensitivity: RequirementLevel::Low,
            expected_redo_penalty,
            output_precision,
            compatibility: Vec::new(),
        }
    }

    #[test]
    fn ir_tier_mapping_reaches_all_model_ladder_levels() {
        assert_eq!(
            tier_for_ir(
                &constraints(
                    RequirementLevel::Low,
                    RequirementLevel::Low,
                    RequirementLevel::Low,
                    RequirementLevel::Low,
                    RequirementLevel::Low,
                ),
                &ToolDensity::None,
            ),
            TierName::CheapFast
        );
        assert_eq!(
            tier_for_ir(
                &constraints(
                    RequirementLevel::High,
                    RequirementLevel::Low,
                    RequirementLevel::Low,
                    RequirementLevel::Medium,
                    RequirementLevel::Medium,
                ),
                &ToolDensity::High,
            ),
            TierName::CheapToolSafe
        );
        assert_eq!(
            tier_for_ir(
                &constraints(
                    RequirementLevel::Low,
                    RequirementLevel::Medium,
                    RequirementLevel::Medium,
                    RequirementLevel::Medium,
                    RequirementLevel::Medium,
                ),
                &ToolDensity::None,
            ),
            TierName::Standard
        );
        assert_eq!(
            tier_for_ir(
                &constraints(
                    RequirementLevel::High,
                    RequirementLevel::High,
                    RequirementLevel::Medium,
                    RequirementLevel::High,
                    RequirementLevel::High,
                ),
                &ToolDensity::High,
            ),
            TierName::OpenSourceFlagship
        );
        assert_eq!(
            tier_for_ir(
                &constraints(
                    RequirementLevel::High,
                    RequirementLevel::High,
                    RequirementLevel::High,
                    RequirementLevel::High,
                    RequirementLevel::High,
                ),
                &ToolDensity::High,
            ),
            TierName::Flagship
        );
    }
}
