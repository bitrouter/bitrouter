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

use bitrouter_sdk::BitrouterError;
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

/// A workload with both halves to exercise: an operator-configured downgrade
/// (`after_routine` → cheap) that is unsafe and must escalate; and two
/// exploration candidates (left at the capable tier) — one safe to downgrade,
/// one not.
fn workload_table() -> Arc<PolicyTable> {
    let cfg = PolicyTableConfig {
        key_strategy: Default::default(),
        tiers: HashMap::from([
            ("cheap".to_string(), CHEAP.to_string()),
            ("capable".to_string(), CAPABLE.to_string()),
        ]),
        fingerprints: HashMap::from([("after_routine".to_string(), "cheap".to_string())]),
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

/// Ground truth: whether the cheap tier is actually adequate for a fingerprint.
fn cheap_is_adequate(fingerprint: &str) -> bool {
    match fingerprint {
        // A safe downgrade — exploration should discover and lock it.
        "after_safe" => true,
        // Unsafe steps — cheap always fails; the ledger must escalate.
        "after_risky" | "after_routine" => false,
        _ => true,
    }
}

/// A prompt whose fingerprint is `after_<tool>`.
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
        for tool in ["safe", "risky", "routine"] {
            let fingerprint = format!("after_{tool}");
            // Route.
            let mut prompt = prompt_after(tool);
            router.apply(&mut prompt);
            let served = prompt.model.clone();
            spend += price(&served);
            flagship_only_spend += price(CAPABLE);

            // Simulate the outcome from ground truth: a cheap route to an
            // inadequate step hard-fails; everything else completes.
            let served_cheap = served == CHEAP;
            let inadequate = served_cheap && !cheap_is_adequate(&fingerprint);
            let outcome = if inadequate {
                RequestOutcome::Failed(BitrouterError::internal("cheap inadequate"))
            } else {
                RequestOutcome::Completed
            };

            // Observe.
            observer
                .on_request_end(&context(&served, prompt_after(tool)), &outcome)
                .await;
        }
    }

    // Discovery: the genuinely-safe downgrade is locked to the cheap tier.
    assert!(
        ledger.is_locked("after_safe"),
        "exploration must discover and lock the safe downgrade"
    );
    // Safety: the unsafe exploration candidate is never locked, and is escalated.
    assert!(
        !ledger.is_locked("after_risky"),
        "the unsafe downgrade must not be locked"
    );
    assert!(
        ledger.is_pinned("after_risky"),
        "the unsafe exploration candidate must be escalated"
    );
    // Safety: the unsafe *operator* downgrade self-corrects (escalated).
    assert!(
        ledger.is_pinned("after_routine"),
        "the unsafe operator downgrade must self-correct"
    );

    // Final routing reflects the converged policy.
    let route = |tool: &str| {
        let mut p = prompt_after(tool);
        router.apply(&mut p);
        p.model
    };
    assert_eq!(route("safe"), CHEAP, "safe step settled on the cheap tier");
    assert_eq!(route("risky"), CAPABLE, "risky step escalated to capable");
    assert_eq!(
        route("routine"),
        CAPABLE,
        "routine (operator) downgrade escalated to capable"
    );

    // The loop spent strictly less than routing everything at the capable tier —
    // the discovered safe downgrade is a real, net saving.
    assert!(
        spend < flagship_only_spend,
        "the converged policy must cost less than capable-only: {spend} vs {flagship_only_spend}"
    );
}
