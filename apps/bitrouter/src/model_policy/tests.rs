use super::*;
use serde_json::{Value, json};

fn source() -> Value {
    json!({"spec_version":"policy/1.0.0-beta.1", "policies":{"auto":{
        "tiers":{"economy":{"model":"fixture:small"}, "strong":{"model":"fixture:large","effort":"high"}},
        "default_tier":"economy", "tool_use_tier":"strong", "tool_safe_tiers":["strong"]
    }}})
}

#[test]
fn public_source_round_trips_without_context_or_learned_evidence() -> Result<()> {
    let value = source();
    let compiled = parse(&value)?;
    assert_eq!(serde_json::to_value(super::source(&compiled)?)?, value);
    assert!(compiled.policies.values().all(|policy| {
        policy
            .tiers
            .values()
            .all(|target| target.context == ContextStrategy::Preserve)
    }));
    assert!(compiled.certificates.is_empty());
    Ok(())
}

#[test]
fn classification_requires_an_explicit_supported_classifier_and_valid_class() -> Result<()> {
    let mut value = source();
    value["policies"]["auto"]["routes"] = json!({"code:debugging|verify|normal":"strong"});
    assert!(parse(&value).is_err());
    value["policies"]["auto"]["classifier_digest"] =
        json!(compiled_predictor_contract().config_digest);
    let compiled = parse(&value)?;
    assert_eq!(serde_json::to_value(super::source(&compiled)?)?, value);
    let policy = &compiled.policies["auto"];
    assert_eq!(
        policy
            .routes
            .get("semantic_route/v1|code:debugging|verify|normal")
            .map(String::as_str),
        Some("strong")
    );
    let cert = &compiled.certificates["auto"]["semantic_route/v1|code:debugging|verify|normal"];
    assert_eq!(cert.owner, RouteOwner::Operator);
    assert_eq!(cert.eligible_episodes, 0);
    value["policies"]["auto"]["routes"] = json!({"code:debugging|future|normal":"strong"});
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
    let mut value = source();
    value["policies"]["auto"]["tiers"]["economy"]["context"] = json!("preserve");
    assert!(parse(&value).is_err());
    let mut value = source();
    value["policies"]["auto"]["default_tier"] = json!("missing");
    assert!(parse(&value).is_err());
}

#[test]
fn experimental_runtime_state_cannot_be_exported_as_service_policy() -> Result<()> {
    let mut compiled = parse(&source())?;
    compiled
        .policies
        .get_mut("auto")
        .context("auto")?
        .tiers
        .get_mut("economy")
        .context("tier")?
        .context = ContextStrategy::Evidence;
    assert!(super::source(&compiled).is_err());
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
        runtime.prepare_selection("auto", None, &mut ctx).await?;
        runtime.select_variant("auto", None, &mut ctx).await?;
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
