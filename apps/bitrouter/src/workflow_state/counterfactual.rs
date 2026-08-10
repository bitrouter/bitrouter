use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::policy_lock::PolicyDefinition;
use crate::workflow_state::archive::CloudUsageRecord;
use crate::workflow_state::fixture::WorkflowTraceFixture;
use crate::workflow_state::real_trace::{CapturedIngressTrace, TraceSanitizer};
use crate::workflow_state::replay::extract_fixture_ir;
use bitrouter_sdk::{BitrouterError, Result};

const PPM: u64 = 1_000_000;

/// Cost-only replay of one immutable policy over a settled baseline trace.
///
/// The factor is the candidate's effective cost divided by baseline cost and
/// may already include expected token, retry, and turn inflation. This is an
/// upper-bound prioritization tool: it never claims that replacing a request
/// leaves the remainder of the live trajectory unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyCounterfactualReport {
    pub effective_cost_factor_ppm: u32,
    pub trace_count: usize,
    pub known_cost_request_count: usize,
    pub unknown_cost_request_count: usize,
    pub eligible_request_count: usize,
    pub baseline_cost_micro_usd: u64,
    pub eligible_cost_micro_usd: u64,
    pub cost_weighted_coverage_ppm: u32,
    pub projected_savings_micro_usd: u64,
    pub projected_savings_ppm: u32,
    pub complete_cost_evidence: bool,
    pub routes: Vec<CounterfactualRouteSummary>,
    pub uncovered_routes: Vec<CounterfactualRouteSummary>,
    pub ranked_requests: Vec<CounterfactualRequest>,
    pub ranked_uncovered_requests: Vec<CounterfactualRequest>,
    pub targets: Vec<CounterfactualTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CounterfactualRouteSummary {
    pub request_key: String,
    pub selected_tier: String,
    pub request_count: usize,
    pub baseline_cost_micro_usd: u64,
    pub cost_share_ppm: u32,
    pub projected_savings_micro_usd: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CounterfactualRequest {
    pub trace_id: String,
    pub request_id: String,
    pub request_key: String,
    pub selected_tier: String,
    pub baseline_cost_micro_usd: u64,
    pub projected_savings_micro_usd: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CounterfactualTarget {
    pub target_savings_ppm: u32,
    pub required_coverage_ppm: u32,
    pub attainable: bool,
    pub minimum_request_count: Option<usize>,
}

#[derive(Default)]
struct RouteAccumulator {
    request_count: usize,
    baseline_cost_micro_usd: u64,
    projected_savings_micro_usd: u64,
}

/// Replay `policy` over protocol-native baseline traces and exact request
/// settlement. Route eligibility is derived from the lock rather than from a
/// benchmark- or agent-specific label.
pub fn analyze_policy_counterfactual(
    traces: &[CapturedIngressTrace],
    usage: &[CloudUsageRecord],
    policy: &PolicyDefinition,
    effective_cost_factor_ppm: u32,
    target_savings_ppm: &[u32],
) -> Result<PolicyCounterfactualReport> {
    if effective_cost_factor_ppm > PPM as u32 {
        return Err(BitrouterError::bad_request(
            "counterfactual effective cost factor must be between 0 and 1000000 ppm",
        ));
    }
    if target_savings_ppm.iter().any(|target| *target > PPM as u32) {
        return Err(BitrouterError::bad_request(
            "counterfactual savings targets must be between 0 and 1000000 ppm",
        ));
    }
    let default_tier = policy.default_tier.as_deref().ok_or_else(|| {
        BitrouterError::bad_request("counterfactual policy requires a default_tier")
    })?;
    let usage_by_request = strict_usage_join(traces, usage)?;
    let mut baseline_cost_micro_usd = 0_u64;
    let mut eligible_cost_micro_usd = 0_u64;
    let mut projected_savings_micro_usd = 0_u64;
    let mut known_cost_request_count = 0_usize;
    let mut unknown_cost_request_count = 0_usize;
    let mut routes = BTreeMap::<(String, String), RouteAccumulator>::new();
    let mut uncovered_routes = BTreeMap::<(String, String), RouteAccumulator>::new();
    let mut ranked_requests = Vec::new();
    let mut ranked_uncovered_requests = Vec::new();

    for trace in traces {
        let fixture = WorkflowTraceFixture::from_value(
            trace.to_replay_fixture_json(&TraceSanitizer::default())?,
        )?;
        let ir = extract_fixture_ir(&fixture);
        let projection = ir.route_projection();
        let primary_request_key = projection.key();
        let compatibility_request_key = ir.compatibility_route_projection_v1().key();
        let (selected_tier, request_key) = resolve_tier(
            policy,
            &primary_request_key,
            &compatibility_request_key,
            !fixture.prompt.tools.is_empty(),
        )
        .ok_or_else(|| {
            BitrouterError::bad_request("counterfactual policy has no resolvable tier")
        })?;
        let Some(cost) = usage_by_request
            .get(trace.id.as_str())
            .and_then(|record| record.final_charge_micro_usd)
        else {
            unknown_cost_request_count = unknown_cost_request_count.saturating_add(1);
            continue;
        };
        known_cost_request_count = known_cost_request_count.saturating_add(1);
        baseline_cost_micro_usd = baseline_cost_micro_usd.saturating_add(cost);
        if selected_tier == default_tier {
            let route = uncovered_routes
                .entry((request_key.to_string(), selected_tier.to_string()))
                .or_default();
            route.request_count = route.request_count.saturating_add(1);
            route.baseline_cost_micro_usd = route.baseline_cost_micro_usd.saturating_add(cost);
            ranked_uncovered_requests.push(CounterfactualRequest {
                trace_id: trace.id.clone(),
                request_id: trace.id.clone(),
                request_key: request_key.to_string(),
                selected_tier: selected_tier.to_string(),
                baseline_cost_micro_usd: cost,
                projected_savings_micro_usd: 0,
            });
            continue;
        }
        let savings = scaled_savings(cost, effective_cost_factor_ppm);
        eligible_cost_micro_usd = eligible_cost_micro_usd.saturating_add(cost);
        projected_savings_micro_usd = projected_savings_micro_usd.saturating_add(savings);
        let route = routes
            .entry((request_key.to_string(), selected_tier.to_string()))
            .or_default();
        route.request_count = route.request_count.saturating_add(1);
        route.baseline_cost_micro_usd = route.baseline_cost_micro_usd.saturating_add(cost);
        route.projected_savings_micro_usd =
            route.projected_savings_micro_usd.saturating_add(savings);
        ranked_requests.push(CounterfactualRequest {
            trace_id: trace.id.clone(),
            request_id: trace.id.clone(),
            request_key: request_key.to_string(),
            selected_tier: selected_tier.to_string(),
            baseline_cost_micro_usd: cost,
            projected_savings_micro_usd: savings,
        });
    }

    ranked_requests.sort_by(|left, right| {
        right
            .baseline_cost_micro_usd
            .cmp(&left.baseline_cost_micro_usd)
            .then_with(|| left.request_id.cmp(&right.request_id))
    });
    ranked_uncovered_requests.sort_by(|left, right| {
        right
            .baseline_cost_micro_usd
            .cmp(&left.baseline_cost_micro_usd)
            .then_with(|| left.request_id.cmp(&right.request_id))
    });
    let complete_cost_evidence = unknown_cost_request_count == 0;
    let routes = route_summaries(routes, baseline_cost_micro_usd);
    let uncovered_routes = route_summaries(uncovered_routes, baseline_cost_micro_usd);
    let projected_savings_ppm = ratio_ppm(projected_savings_micro_usd, baseline_cost_micro_usd);
    let targets = target_savings_ppm
        .iter()
        .map(|target| {
            target_report(
                *target,
                effective_cost_factor_ppm,
                baseline_cost_micro_usd,
                projected_savings_ppm,
                complete_cost_evidence,
                &ranked_requests,
            )
        })
        .collect();

    Ok(PolicyCounterfactualReport {
        effective_cost_factor_ppm,
        trace_count: traces.len(),
        known_cost_request_count,
        unknown_cost_request_count,
        eligible_request_count: ranked_requests.len(),
        baseline_cost_micro_usd,
        eligible_cost_micro_usd,
        cost_weighted_coverage_ppm: ratio_ppm(eligible_cost_micro_usd, baseline_cost_micro_usd),
        projected_savings_micro_usd,
        projected_savings_ppm,
        complete_cost_evidence,
        routes,
        uncovered_routes,
        ranked_requests,
        ranked_uncovered_requests,
        targets,
    })
}

fn route_summaries(
    routes: BTreeMap<(String, String), RouteAccumulator>,
    baseline_cost_micro_usd: u64,
) -> Vec<CounterfactualRouteSummary> {
    routes
        .into_iter()
        .map(
            |((request_key, selected_tier), route)| CounterfactualRouteSummary {
                request_key,
                selected_tier,
                request_count: route.request_count,
                baseline_cost_micro_usd: route.baseline_cost_micro_usd,
                cost_share_ppm: ratio_ppm(route.baseline_cost_micro_usd, baseline_cost_micro_usd),
                projected_savings_micro_usd: route.projected_savings_micro_usd,
            },
        )
        .collect()
}

fn strict_usage_join<'a>(
    traces: &[CapturedIngressTrace],
    usage: &'a [CloudUsageRecord],
) -> Result<BTreeMap<&'a str, &'a CloudUsageRecord>> {
    let mut trace_ids = BTreeSet::new();
    for trace in traces {
        if trace.id.trim().is_empty() || !trace_ids.insert(trace.id.as_str()) {
            return Err(BitrouterError::bad_request(
                "counterfactual traces contain an empty or duplicate request id",
            ));
        }
    }
    let mut usage_by_request = BTreeMap::new();
    for record in usage {
        let Some(request_id) = record.request_id.as_deref() else {
            return Err(BitrouterError::bad_request(
                "counterfactual usage row has no request id",
            ));
        };
        if usage_by_request.insert(request_id, record).is_some() {
            return Err(BitrouterError::bad_request(format!(
                "counterfactual usage contains duplicate request id {request_id}"
            )));
        }
    }
    let usage_ids = usage_by_request.keys().copied().collect::<BTreeSet<_>>();
    if trace_ids != usage_ids {
        return Err(BitrouterError::bad_request(
            "counterfactual trace/usage request ids differ",
        ));
    }
    Ok(usage_by_request)
}

fn resolve_tier<'policy, 'key>(
    policy: &'policy PolicyDefinition,
    primary_request_key: &'key str,
    compatibility_request_key: &'key str,
    carries_tools: bool,
) -> Option<(&'policy str, &'key str)> {
    let (raw, matched_request_key) = if let Some(tier) = policy.routes.get(primary_request_key) {
        (tier.as_str(), primary_request_key)
    } else if let Some(tier) = policy.routes.get(compatibility_request_key) {
        (tier.as_str(), compatibility_request_key)
    } else {
        (policy.default_tier.as_deref()?, primary_request_key)
    };
    if carries_tools
        && !policy.tool_safe_tiers.iter().any(|tier| tier == raw)
        && let Some(floor) = policy.tool_use_tier.as_deref()
    {
        return Some((floor, matched_request_key));
    }
    Some((raw, matched_request_key))
}

fn scaled_savings(cost: u64, effective_cost_factor_ppm: u32) -> u64 {
    let savings_factor = PPM.saturating_sub(u64::from(effective_cost_factor_ppm));
    mul_div_floor(cost, savings_factor, PPM)
}

fn ratio_ppm(numerator: u64, denominator: u64) -> u32 {
    if denominator == 0 {
        return 0;
    }
    let ratio = mul_div_floor(numerator, PPM, denominator);
    u32::try_from(ratio.min(PPM)).unwrap_or(PPM as u32)
}

fn target_report(
    target_savings_ppm: u32,
    effective_cost_factor_ppm: u32,
    baseline_cost_micro_usd: u64,
    projected_savings_ppm: u32,
    complete_cost_evidence: bool,
    ranked_requests: &[CounterfactualRequest],
) -> CounterfactualTarget {
    let savings_factor = PPM.saturating_sub(u64::from(effective_cost_factor_ppm));
    let required_coverage_ppm = if target_savings_ppm == 0 {
        0
    } else if savings_factor == 0 {
        u32::MAX
    } else {
        let required = mul_div_ceil(u64::from(target_savings_ppm), PPM, savings_factor);
        u32::try_from(required).unwrap_or(u32::MAX)
    };
    let target_cost = mul_div_ceil(baseline_cost_micro_usd, u64::from(target_savings_ppm), PPM);
    let mut accumulated = 0_u64;
    let mut minimum_request_count = None;
    if target_cost == 0 {
        minimum_request_count = Some(0);
    } else {
        for (index, request) in ranked_requests.iter().enumerate() {
            accumulated = accumulated.saturating_add(request.projected_savings_micro_usd);
            if accumulated >= target_cost {
                minimum_request_count = Some(index.saturating_add(1));
                break;
            }
        }
    }
    CounterfactualTarget {
        target_savings_ppm,
        required_coverage_ppm,
        attainable: complete_cost_evidence && projected_savings_ppm >= target_savings_ppm,
        minimum_request_count,
    }
}

fn mul_div_floor(value: u64, multiplier: u64, divisor: u64) -> u64 {
    if divisor == 0 {
        return u64::MAX;
    }
    let result = u128::from(value).saturating_mul(u128::from(multiplier)) / u128::from(divisor);
    u64::try_from(result).unwrap_or(u64::MAX)
}

fn mul_div_ceil(value: u64, multiplier: u64, divisor: u64) -> u64 {
    if divisor == 0 {
        return u64::MAX;
    }
    let numerator = u128::from(value).saturating_mul(u128::from(multiplier));
    let result =
        numerator.saturating_add(u128::from(divisor).saturating_sub(1)) / u128::from(divisor);
    u64::try_from(result).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::metering::{ChargeStatus, ReconciliationStatus};
    use crate::policy_lock::PolicyDefinition;
    use crate::workflow_state::archive::CloudUsageRecord;
    use crate::workflow_state::ir::{HarnessId, ProtocolKind};
    use crate::workflow_state::real_trace::{CapturedIngressTrace, RealTraceOutcome};

    use super::analyze_policy_counterfactual;

    fn trace(id: &str, assistant_action: Option<&str>) -> CapturedIngressTrace {
        let mut messages = vec![
            serde_json::json!({
                "role": "system",
                "content": "You are an AI assistant tasked with solving command-line tasks in a Linux environment. Format your response as JSON with commands and task_complete."
            }),
            serde_json::json!({"role": "user", "content": "continue"}),
        ];
        if let Some(action) = assistant_action {
            messages.push(serde_json::json!({"role": "assistant", "content": action}));
            messages.push(serde_json::json!({"role": "user", "content": "command output"}));
        }
        CapturedIngressTrace {
            id: id.to_string(),
            captured_at: Some("2026-07-31T00:00:00Z".to_string()),
            harness: HarnessId::Terminus2,
            protocol: ProtocolKind::ChatCompletions,
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            headers: BTreeMap::new(),
            raw_body: serde_json::json!({
                "model": "@auto",
                "messages": messages
            }),
            outcome: RealTraceOutcome {
                http_status: 200,
                status: "completed".to_string(),
            },
        }
    }

    fn usage(id: &str, cost: u64) -> CloudUsageRecord {
        CloudUsageRecord {
            request_id: Some(id.to_string()),
            provider_id: "openai-codex".to_string(),
            model_id: "gpt-5.6-sol".to_string(),
            final_charge_micro_usd: Some(cost),
            charge_status: ChargeStatus::Computed,
            reconciliation_status: ReconciliationStatus::NotApplicable,
            ..CloudUsageRecord::default()
        }
    }

    fn policy() -> PolicyDefinition {
        PolicyDefinition {
            tiers: BTreeMap::from([
                ("economy".to_string(), "vendor:economy".into()),
                ("strong".to_string(), "vendor:strong".into()),
            ]),
            routes: BTreeMap::from([(
                "agent_trace/v1|edit|normal".to_string(),
                "economy".to_string(),
            )]),
            default_tier: Some("strong".to_string()),
            tool_use_tier: Some("strong".to_string()),
            tool_safe_tiers: vec!["strong".to_string(), "economy".to_string()],
            ..PolicyDefinition::default()
        }
    }

    #[test]
    fn oracle_reports_cost_weighted_coverage_and_target_steps() {
        let traces = vec![
            trace("opening", None),
            trace(
                "edit",
                Some(r#"{"commands":[{"keystrokes":"sed -i s/a/b/ file"}],"task_complete":false}"#),
            ),
        ];
        let usage = vec![usage("opening", 1_000), usage("edit", 3_000)];

        let report = analyze_policy_counterfactual(&traces, &usage, &policy(), 250_000, &[500_000])
            .expect("counterfactual report");

        assert_eq!(report.baseline_cost_micro_usd, 4_000);
        assert_eq!(report.eligible_request_count, 1);
        assert_eq!(report.eligible_cost_micro_usd, 3_000);
        assert_eq!(report.cost_weighted_coverage_ppm, 750_000);
        assert_eq!(report.projected_savings_micro_usd, 2_250);
        assert_eq!(report.projected_savings_ppm, 562_500);
        assert_eq!(report.targets[0].required_coverage_ppm, 666_667);
        assert!(report.targets[0].attainable);
        assert_eq!(report.targets[0].minimum_request_count, Some(1));
        assert_eq!(report.ranked_requests[0].request_id, "edit");
        assert_eq!(report.ranked_requests[0].baseline_cost_micro_usd, 3_000);
        assert_eq!(report.uncovered_routes.len(), 1);
        assert_eq!(
            report.uncovered_routes[0].request_key,
            "agent_trace/v2|opening|normal"
        );
        assert_eq!(report.uncovered_routes[0].baseline_cost_micro_usd, 1_000);
        assert_eq!(report.ranked_uncovered_requests[0].request_id, "opening");
    }

    #[test]
    fn oracle_rejects_an_incomplete_trace_usage_join() {
        let error = analyze_policy_counterfactual(
            &[trace("opening", None)],
            &[],
            &policy(),
            250_000,
            &[300_000],
        )
        .expect_err("missing usage must fail closed");

        assert!(error.to_string().contains("trace/usage request ids differ"));
    }
}
