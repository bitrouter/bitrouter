use super::*;
use bitrouter_ai::routing::ContextStrategy;
use serde_json::{Value, json};

fn source() -> Value {
    json!({"spec_version":"policy/1.0.0-beta.1", "policies":{"auto":{
        "tiers":{"economy":{"model":"fixture:small"}, "strong":{"model":"fixture:large","effort":"high"}},
        "default_tier":"economy", "tool_use_tier":"strong", "tool_safe_tiers":["strong"]
    }}})
}

#[test]
fn public_source_compiles_without_context_or_learned_evidence() -> Result<()> {
    let value = source();
    let document = parse(&value)?;
    let compiled = compile(&document)?;
    assert_eq!(serde_json::to_value(compiled.source())?, value);
    assert_eq!(compiled.digest(), source_digest(&document)?);
    assert_eq!(
        compiled.selections()["auto"].tiers["economy"].model,
        "fixture:small"
    );
    assert!(compiled.selections()["auto"].routes.is_empty());
    Ok(())
}

#[test]
fn source_digest_matches_recursively_sorted_utf8_json_vector() -> Result<()> {
    // Object keys are sorted at every level; the model remains literal UTF-8.
    // Typed omission rules exclude empty routes, tool lists and absent options.
    const CANONICAL: &str = r#"{"policies":{"auto":{"default_tier":"strong","tiers":{"strong":{"effort":"high","model":"fixture:雪"}}}},"spec_version":"policy/1.0.0-beta.1"}"#;
    const EXPECTED: &str =
        "sha256:2e9697fd93ee9ddd5ddb9ceed480a658c964d72ab608e6bdba385b36a6679cae";
    let document = decode(CANONICAL)?;

    assert_eq!(source_digest(&document)?, EXPECTED);
    assert_eq!(compile(&document)?.digest(), EXPECTED);
    assert_eq!(crate::policy_lock::semantic_digest(&document)?, EXPECTED);
    Ok(())
}

#[test]
fn source_digest_ignores_field_order_but_preserves_array_order() -> Result<()> {
    let first = decode(
        r#"{"spec_version":"policy/1.0.0-beta.1","policies":{"auto":{"tiers":{"strong":{"model":"fixture:large","effort":"high"},"economy":{"model":"fixture:small"}},"default_tier":"economy","tool_use_tier":"strong","tool_safe_tiers":["strong","economy"]}}}"#,
    )?;
    let mut reordered = decode(
        r#"{"policies":{"auto":{"tool_safe_tiers":["strong","economy"],"tool_use_tier":"strong","default_tier":"economy","tiers":{"economy":{"model":"fixture:small"},"strong":{"effort":"high","model":"fixture:large"}}}},"spec_version":"policy/1.0.0-beta.1"}"#,
    )?;
    assert_eq!(first, reordered);
    assert_eq!(source_digest(&first)?, source_digest(&reordered)?);

    reordered
        .policies
        .get_mut("auto")
        .context("fixture auto missing")?
        .tool_safe_tiers
        .reverse();
    assert_ne!(source_digest(&first)?, source_digest(&reordered)?);
    Ok(())
}

#[test]
fn classification_requires_supported_classifier_valid_class_and_existing_target() -> Result<()> {
    let mut value = source();
    value["policies"]["auto"]["routes"] = json!({"code:debugging|verify|normal":"strong"});
    assert!(parse(&value).is_err());
    value["policies"]["auto"]["classifier_digest"] =
        json!(compiled_predictor_contract().config_digest);
    let compiled = compile(&parse(&value)?)?;
    assert_eq!(
        compiled.selections()["auto"]
            .routes
            .get("semantic_route/v1|code:debugging|verify|normal")
            .map(String::as_str),
        Some("strong")
    );
    value["policies"]["auto"]["routes"] = json!({"code:debugging|future|normal":"strong"});
    assert!(parse(&value).is_err());
    value["policies"]["auto"]["routes"] = json!({"code:debugging|verify|normal":"missing"});
    assert!(parse(&value).is_err());
    Ok(())
}

#[test]
fn unsupported_contracts_and_experimental_fields_fail_closed() {
    for version in [
        "1.0.0-beta.1",
        "registry/1.0.0-beta.1",
        "policy/1.0.0-beta.2",
        "policy/1.0.0",
        "policy/4",
    ] {
        let mut value = source();
        value["spec_version"] = json!(version);
        assert!(parse(&value).is_err(), "accepted {version}");
    }
    for field in ["artifact", "certificates", "lockfileVersion"] {
        let mut value = source();
        value[field] = json!(4);
        assert!(parse(&value).is_err());
    }
    for field in ["progress_guard", "optimization", "adequacy", "predictor"] {
        let mut value = source();
        value["policies"]["auto"][field] = json!({});
        assert!(parse(&value).is_err());
    }
    for context in ["preserve", "evidence", "summary"] {
        let mut value = source();
        value["policies"]["auto"]["tiers"]["economy"]["context"] = json!(context);
        assert!(parse(&value).is_err());
    }
}

#[test]
fn invalid_references_duplicate_targets_and_recursive_selectors_are_rejected() {
    let mut missing_default = source();
    missing_default["policies"]["auto"]["default_tier"] = json!("missing");
    let mut missing_tool = source();
    missing_tool["policies"]["auto"]["tool_use_tier"] = json!("missing");
    let mut unsafe_tool = source();
    unsafe_tool["policies"]["auto"]["tool_safe_tiers"] = json!(["economy"]);
    let mut missing_safe = source();
    missing_safe["policies"]["auto"]["tool_safe_tiers"] = json!(["strong", "missing"]);
    let mut duplicate = source();
    duplicate["policies"]["auto"]["tiers"]["clone"] =
        duplicate["policies"]["auto"]["tiers"]["economy"].clone();
    for value in [
        missing_default,
        missing_tool,
        unsafe_tool,
        missing_safe,
        duplicate,
    ] {
        assert!(parse(&value).is_err());
    }
    for model in [
        "@auto",
        "bitrouter/auto",
        "bitrouter/other",
        "bitrouter:auto",
        "bitrouter:fusion",
    ] {
        let mut value = source();
        value["policies"]["auto"]["tiers"]["economy"]["model"] = json!(model);
        assert!(parse(&value).is_err());
    }
}

#[test]
fn raw_duplicate_mappings_are_rejected_before_value_conversion() {
    for raw in [
        r#"{"spec_version":"policy/1.0.0-beta.1","policies":{"auto":{"tiers":{"one":{"model":"fixture:one"},"one":{"model":"fixture:two"}},"default_tier":"one"}}}"#,
        "spec_version: policy/1.0.0-beta.1\npolicies:\n  auto: {tiers: {one: {model: 'fixture:one'}}, default_tier: one}\n  auto: {tiers: {two: {model: 'fixture:two'}}, default_tier: two}\n",
        "spec_version: policy/1.0.0-beta.1\npolicies: {auto: {tiers: {one: {model: 'fixture:one', model: 'fixture:two'}}, default_tier: one}}\n",
        "spec_version: policy/1.0.0-beta.1\npolicies: {auto: {tiers: {one: {model: 'fixture:one'}}, default_tier: one, default_tier: one}}\n",
    ] {
        assert!(decode(raw).is_err(), "accepted duplicate mapping: {raw}");
    }
}

#[tokio::test]
async fn selection_uses_literal_names_and_missing_suffix_never_falls_back() -> Result<()> {
    use crate::gateway::{
        caller::CallerContext,
        pipeline::{context::PipelineContext, routing::ModelSelector, types::PipelineRequest},
    };
    let mut value = source();
    let mut cost = value["policies"]["auto"].clone();
    cost["default_tier"] = json!("strong");
    value["policies"]["auto:cost"] = cost;
    let config = crate::config::Config {
        inherit_defaults: false,
        ..Default::default()
    };
    let runtime = crate::policy_lock::PolicyRuntime::from_document(
        &config,
        Some(parse(&value)?),
        Default::default(),
        None,
    )?;
    for (selected, expected) in [("auto", "fixture:small"), ("auto:cost", "fixture:large")] {
        let input = crate::gateway::http::parse_input(
            &bitrouter_ai::types::ApiProtocol::ChatCompletions,
            json!({"model":"bitrouter/auto","messages":[{"role":"user","content":"complete context"}]}),
        )?;
        let mut ctx = PipelineContext::new(PipelineRequest::new(
            "bitrouter/auto",
            CallerContext::local(),
            input.generation_prompt().context("prompt")?.clone(),
        ));
        runtime.prepare_selection(selected, &mut ctx).await?;
        runtime.select_policy(selected, &mut ctx).await?;
        assert_eq!(ctx.model(), expected);
        assert!(
            runtime
                .prepare_selection("auto:missing", &mut ctx)
                .await
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn default_tool_and_server_tool_bypass_preserve_admitted_content() -> Result<()> {
    use crate::gateway::{
        caller::CallerContext,
        pipeline::{context::PipelineContext, routing::ModelSelector, types::PipelineRequest},
    };
    use bitrouter_ai::types::{ApiProtocol, Tool};
    let config = crate::config::Config {
        inherit_defaults: false,
        ..Default::default()
    };
    let runtime = crate::policy_lock::PolicyRuntime::from_document(
        &config,
        Some(parse(&source())?),
        Default::default(),
        None,
    )?;
    for (tool, expected_model) in [
        (None, "fixture:small"),
        (
            Some(Tool::Function {
                name: "read_file".into(),
                description: None,
                parameters: json!({"type":"object"}),
                strict: None,
                provider_metadata: Default::default(),
            }),
            "fixture:large",
        ),
        (
            Some(Tool::ProviderDefined {
                id: "bitrouter.advisor".into(),
                name: "advisor".into(),
                args: json!({}),
                provider_metadata: Default::default(),
            }),
            "bitrouter/auto",
        ),
    ] {
        let input = crate::gateway::http::parse_input(
            &ApiProtocol::ChatCompletions,
            json!({"model":"bitrouter/auto","messages":[{"role":"system","content":"retain instructions"},{"role":"user","content":"retain complete history"}]}),
        )?;
        let mut prompt = input.generation_prompt().context("prompt")?.clone();
        prompt.tools = tool.into_iter().collect();
        let original = prompt.clone();
        let mut ctx = PipelineContext::new(PipelineRequest::new(
            "bitrouter/auto",
            CallerContext::local(),
            prompt,
        ));
        runtime.prepare_selection("auto", &mut ctx).await?;
        runtime.select_policy("auto", &mut ctx).await?;
        assert_eq!(ctx.model(), expected_model);
        assert_eq!(
            ctx.extension::<ContextStrategy>().as_deref(),
            Some(&ContextStrategy::Preserve)
        );
        let actual = ctx.require_generation_prompt()?;
        assert_eq!(actual.messages, original.messages);
        assert_eq!(actual.system, original.system);
        assert_eq!(actual.tools, original.tools);
    }
    Ok(())
}

struct FixedWithReducedView {
    plans: tokio::sync::Mutex<Vec<crate::gateway::pipeline::native::NativePlan>>,
    reject: bool,
    rebuild_calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl crate::gateway::pipeline::native::NativeExecutionControl for FixedWithReducedView {
    fn model_selection(&self) -> crate::gateway::pipeline::native::NativeModelSelection {
        crate::gateway::pipeline::native::NativeModelSelection::Fixed
    }

    async fn prepare_routing(
        &self,
        prompt: &bitrouter_ai::types::Prompt,
    ) -> crate::gateway::error::Result<Option<crate::gateway::routing::preparation::Prepared>> {
        use crate::gateway::routing::{ContextCapability, plan::View, preparation::Prepared};
        let mut prepared = Prepared::from_prompt(prompt, None, "fixed-model-context").await;
        let mut reduced = prompt.clone();
        reduced.messages.clear();
        prepared
            .capabilities
            .insert(ContextCapability::OmitEvidence);
        prepared.views.push(View {
            id: "reduced".into(),
            prompt: reduced,
            requires: [ContextCapability::OmitEvidence].into(),
        });
        Ok(Some(prepared))
    }

    async fn commit_routing(
        &self,
        plan: &crate::gateway::routing::plan::Plan,
    ) -> crate::gateway::error::Result<()> {
        assert_eq!(plan.selection.strategy, ContextStrategy::Preserve);
        assert_eq!(plan.selection.selected_context, "full");
        Ok(())
    }

    async fn plan(
        &self,
        plan: crate::gateway::pipeline::native::NativePlan,
    ) -> crate::gateway::error::Result<crate::gateway::pipeline::native::NativePlanAdmission> {
        let route_indices = (0..plan.routes.len()).map(|index| index as u32).collect();
        self.plans.lock().await.push(plan);
        if self.reject {
            return Err(crate::gateway::error::BitrouterError::bad_request(
                "context capacity rejected",
            ));
        }
        Ok(crate::gateway::pipeline::native::NativePlanAdmission { route_indices })
    }

    async fn rebuild_context(
        &self,
        _: &crate::gateway::pipeline::native::NativePlan,
    ) -> crate::gateway::error::Result<Option<Vec<bitrouter_ai::types::Message>>> {
        self.rebuild_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Some(Vec::new()))
    }

    async fn before_attempt(&self, _: &str, _: u32) -> crate::gateway::error::Result<()> {
        Ok(())
    }

    async fn after_attempt(&self, _: bitrouter_ai::core::evidence::NativeAttemptReport) {}
}

#[tokio::test]
async fn fixed_native_execution_preserves_context_even_when_reduced_views_are_authorized()
-> Result<()> {
    fixed_native_case(false).await?;
    fixed_native_case(true).await
}

async fn fixed_native_case(reject: bool) -> Result<()> {
    use crate::gateway::{
        caller::CallerContext,
        pipeline::{builder::PipelineBuilder, executor::MockExecutor, types::PipelineRequest},
    };
    use std::sync::Arc;
    let config: crate::config::Config = serde_json::from_value(json!({"inherit_defaults":false,
        "providers":{"fixture":{"api_base":"https://example.invalid/v1","models":[{"id":"base"},{"id":"small"},{"id":"large"}]}},
        "routers":{"auto":{"selection":{"kind":"policy","policy":"auto","base_model":"fixture:base"}}}
    }))?;
    let runtime = crate::policy_lock::PolicyRuntime::from_document(
        &config,
        Some(parse(&source())?),
        Default::default(),
        None,
    )?;
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(Arc::new(
            crate::config::routing_table::ConfigRoutingTable::from_config(config),
        ))
        .model_selector(runtime)
        .executor(Arc::new(MockExecutor::always_text("full context")));
    let input = crate::gateway::http::parse_input(
        &bitrouter_ai::types::ApiProtocol::ChatCompletions,
        json!({"model":"bitrouter/auto","messages":[{"role":"user","content":"retain the complete source history"}]}),
    )?;
    let prompt = input.generation_prompt().context("prompt")?.clone();
    let messages = prompt.messages.clone();
    let control = Arc::new(FixedWithReducedView {
        plans: Default::default(),
        reject,
        rebuild_calls: Default::default(),
    });
    let result = Arc::new(builder.build()?)
        .execute_native_controlled(
            PipelineRequest::new("bitrouter/auto", CallerContext::local(), prompt),
            control.clone(),
        )
        .await;
    assert_eq!(result.is_err(), reject);
    assert_eq!(
        control
            .rebuild_calls
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    let plans = control.plans.lock().await;
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].prompt.messages, messages);
    assert_eq!(plans[0].effective_model, "fixture:base");
    Ok(())
}
