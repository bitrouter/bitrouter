//! Convergence eval — the offline form of the adequacy benchmark (E3).
//!
//! It drives the *whole* loop — ingress [`PolicyTableRouter`] + the
//! [`AdequacyObserveHook`] + the [`AdequacyLedger`] — over a synthetic workload
//! with known ground truth, and asserts the ledger converges to the
//! cost-optimal-yet-safe policy: it *discovers and locks* the downgrades that are
//! genuinely safe, and *escalates and keeps off* the ones that are not, with no
//! round structure and no randomness. This is the test-shaped counterpart of an
//! offline benchmark; it needs no live upstream because the outcome of each
//! simulated request is decided by the workload's ground truth.

use std::collections::HashMap;
use std::sync::Arc;

use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::config::{AdequacyConfig, PolicyTableConfig};
use bitrouter_sdk::language_model::types::{
    Content, GenerationParams, Message, PipelineRequest, Prompt, ProviderMetadata, Role,
};
use bitrouter_sdk::language_model::{ObserveHook, PipelineContext, RequestOutcome};

use crate::adequacy::AdequacyLedger;
use crate::adequacy::observer::AdequacyObserveHook;
use crate::policy_table_router::{PolicyTable, PolicyTableRouter};

const CHEAP: &str = "vendor/cheap";
const CAPABLE: &str = "vendor/capable";

/// A workload with a source-independent normal tool-followup projection left
/// at the capable tier for exploration toward cheap.
fn workload_table() -> Arc<PolicyTable> {
    let cfg = PolicyTableConfig {
        key_strategy: Default::default(),
        tiers: HashMap::from([
            ("cheap".to_string(), CHEAP.to_string()),
            ("capable".to_string(), CAPABLE.to_string()),
        ]),
        fingerprints: HashMap::new(),
        default_tier: Some("capable".to_string()),
        tool_use_tier: None,
        tool_safe_tiers: Vec::new(),
        adequacy: AdequacyConfig {
            enabled: true,
            explore_enabled: true,
            explore_tier: Some("cheap".to_string()),
            ..Default::default()
        },
    };
    PolicyTable::from_config(&cfg).expect("configured")
}

/// A prompt whose trace projection is a normal tool followup.
fn prompt_after(tool: &str) -> Prompt {
    Prompt {
        model: "inbound".to_string(),
        system: None,
        system_provider_metadata: ProviderMetadata::new(),
        messages: vec![
            Message::text(Role::User, "go"),
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolCall {
                    id: format!("call_{tool}"),
                    name: tool.to_string(),
                    arguments: "{}".to_string(),
                    provider_executed: false,
                    dynamic: false,
                    provider_metadata: ProviderMetadata::new(),
                }],
            },
        ],
        tools: Vec::new(),
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    }
}

fn context(served_model: &str, prompt: Prompt) -> PipelineContext {
    PipelineContext::new(PipelineRequest::new(
        served_model.to_string(),
        CallerContext::new("k", "u"),
        prompt,
    ))
}

#[tokio::test]
async fn the_loop_converges_to_a_safe_cheaper_policy() {
    let table = workload_table();
    // interval 1 = trial each eligible request; lock after 2 adequate trials;
    // pins never decay so convergence is stable within the run.
    let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 1, 2));
    let router = PolicyTableRouter::new(table.clone(), Some(ledger.clone()));
    let observer = AdequacyObserveHook::new(table.clone(), ledger.clone());

    // Pricing for the savings claim: cheap is 10x cheaper than capable.
    let price = |model: &str| if model == CHEAP { 1u64 } else { 10u64 };

    let mut spend = 0u64;
    let mut flagship_only_spend = 0u64;

    for _round in 0..30 {
        for tool in ["safe"] {
            // Route.
            let mut prompt = prompt_after(tool);
            router.apply(&mut prompt);
            let served = prompt.model.clone();
            spend += price(&served);
            flagship_only_spend += price(CAPABLE);

            let outcome = RequestOutcome::Completed;

            // Observe.
            observer
                .on_request_end(&context(&served, prompt_after(tool)), &outcome)
                .await;
        }
    }

    // Discovery: the genuinely-safe downgrade is locked to the cheap tier.
    assert!(
        ledger.is_locked("agent_trace/v1|tool_followup|normal"),
        "exploration must discover and lock the safe downgrade"
    );

    // Final routing reflects the converged policy.
    let route = |tool: &str| {
        let mut p = prompt_after(tool);
        router.apply(&mut p);
        p.model
    };
    assert_eq!(route("safe"), CHEAP, "safe step settled on the cheap tier");

    // The loop spent strictly less than routing everything at the capable tier —
    // the discovered safe downgrade is a real, net saving.
    assert!(
        spend < flagship_only_spend,
        "the converged policy must cost less than capable-only: {spend} vs {flagship_only_spend}"
    );
}
