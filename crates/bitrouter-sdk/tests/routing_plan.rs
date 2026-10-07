use std::collections::BTreeSet;

use bitrouter_sdk::language_model::{Message, Prompt, Role};
use bitrouter_sdk::routing::ContextCapability;
use bitrouter_sdk::routing::plan::Model;
use bitrouter_sdk::routing::plan::{self, AdmittedModel, CostPolicy, Options, View};

fn models() -> Vec<AdmittedModel> {
    [("baseline", 100_000, 1_000_000), ("economy", 1_024, 1_000)]
        .into_iter()
        .map(|(model, max_prompt_bytes, price)| AdmittedModel {
            model: Model {
                model: model.into(),
                max_prompt_bytes,
                input_microusd_per_million: Some(price),
                output_microusd_per_million: Some(price),
            },
            admitted: true,
        })
        .collect()
}

fn views() -> Vec<View> {
    [
        ("full", "original evidence ".repeat(300), BTreeSet::new()),
        (
            "routed",
            "retained relevant evidence".into(),
            BTreeSet::from([ContextCapability::OmitEvidence]),
        ),
    ]
    .into_iter()
    .map(|(id, text, requires)| View {
        id: id.into(),
        prompt: Prompt {
            model: "baseline".into(),
            messages: vec![Message::text(Role::User, text)],
            system: None,
            system_provider_metadata: Default::default(),
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        },
        requires,
    })
    .collect()
}

fn options(capabilities: &BTreeSet<ContextCapability>) -> Options<'_> {
    Options {
        strategy: Default::default(),
        policy: CostPolicy {
            minimum_savings_fraction: 0.1,
            model_switch_penalty_microusd: 1_000,
            prefix_loss_penalty_microusd_per_kib: 0,
        },
        previous: None,
        hard_limit_bytes: 100_000,
        capabilities,
    }
}

#[test]
fn view_authority_changes_feasibility_without_an_input_origin_mode()
-> Result<(), Box<dyn std::error::Error>> {
    let models = models();
    let views = views();
    let read_only = BTreeSet::new();
    let full = plan::select(&models, "baseline", &views, options(&read_only))?;
    assert_eq!(full.selection.selected_model, "baseline");
    assert_eq!(full.selection.selected_context, "full");
    assert!(full.selection.candidates.iter().all(|candidate| {
        !candidate.eligible || (candidate.model == "baseline" && candidate.context == "full")
    }));

    // Any admitted input adapter can offer omission; no HTTP/Core identity is
    // passed to the selector, and semantic confidence cannot grant this right.
    let writable = BTreeSet::from([ContextCapability::OmitEvidence]);
    let compact = plan::select(&models, "baseline", &views, options(&writable))?;
    assert_eq!(compact.selection.selected_model, "economy");
    assert_eq!(compact.selection.selected_context, "routed");
    assert_eq!(compact.view_index, 1);
    assert_eq!(compact.prompt.messages, views[1].prompt.messages);
    Ok(())
}

#[test]
fn policy_admission_and_context_authority_are_both_required()
-> Result<(), Box<dyn std::error::Error>> {
    let mut models = models();
    models[1].admitted = false;
    let capabilities = BTreeSet::new();
    let selected = plan::select(&models, "baseline", &views(), options(&capabilities))?;
    assert_eq!(selected.selection.selected_model, "baseline");
    assert_eq!(selected.selection.selected_context, "full");
    let unauthorized_only = vec![views().remove(1)];
    assert!(
        plan::select(
            &models,
            "baseline",
            &unauthorized_only,
            options(&capabilities)
        )
        .is_err()
    );
    models[0].admitted = false;
    assert!(plan::select(&models, "baseline", &views(), options(&capabilities)).is_err());
    Ok(())
}

#[test]
fn invalid_candidate_identity_and_bounds_reject_before_selection() {
    let capabilities = BTreeSet::new();
    let mut candidates = models();
    candidates[1].model.model = candidates[0].model.model.clone();
    assert!(plan::select(&candidates, "baseline", &views(), options(&capabilities)).is_err());
    let mut candidates = models();
    candidates[1].model.max_prompt_bytes = 0;
    assert!(plan::select(&candidates, "baseline", &views(), options(&capabilities)).is_err());
    let mut offered = views();
    offered[1].id = offered[0].id.clone();
    assert!(plan::select(&models(), "baseline", &offered, options(&capabilities)).is_err());
}

#[test]
fn preserve_strategy_does_not_spend_granted_context_authority()
-> Result<(), Box<dyn std::error::Error>> {
    let capabilities = BTreeSet::from([ContextCapability::OmitEvidence]);
    let mut policy = options(&capabilities);
    policy.strategy = bitrouter_sdk::routing::ContextStrategy::Preserve;
    let selection = plan::select(&models(), "baseline", &views(), policy)?;
    assert_eq!(selection.selection.selected_context, "full");
    Ok(())
}

#[test]
fn unknown_prices_are_never_compared_to_bytes_as_money() -> Result<(), Box<dyn std::error::Error>> {
    let mut candidates = models();
    candidates[0].model.input_microusd_per_million = None;
    candidates[0].model.output_microusd_per_million = None;
    candidates[1].model.max_prompt_bytes = 100_000;
    candidates[1].model.input_microusd_per_million = Some(1_000_000_000);
    candidates[1].model.output_microusd_per_million = Some(1_000_000_000);
    let capabilities = BTreeSet::new();
    let selection = plan::select(&candidates, "baseline", &views(), options(&capabilities))?;
    assert_eq!(selection.selection.selected_model, "economy");
    assert!(
        selection
            .selection
            .candidates
            .iter()
            .filter(|candidate| candidate.model == "baseline")
            .all(|candidate| candidate.estimated_cost_microusd.is_none())
    );
    Ok(())
}

#[test]
fn prompt_commitment_changes_even_when_view_and_model_names_match()
-> Result<(), Box<dyn std::error::Error>> {
    let capabilities = BTreeSet::new();
    let original = views();
    let mut changed = original.clone();
    changed[0]
        .prompt
        .messages
        .push(Message::text(Role::User, "New requirement"));
    let first = plan::select(&models(), "baseline", &original, options(&capabilities))?;
    let second = plan::select(&models(), "baseline", &changed, options(&capabilities))?;
    assert_eq!(
        first.selection.selected_context,
        second.selection.selected_context
    );
    assert_eq!(
        first.selection.selected_model,
        second.selection.selected_model
    );
    assert_ne!(
        first.selection.prompt_digest,
        second.selection.prompt_digest
    );
    Ok(())
}
