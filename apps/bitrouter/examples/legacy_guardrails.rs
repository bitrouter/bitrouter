//! Run a loopback custom host with explicit legacy global guardrail hooks.
//! Usage: cargo run -p bitrouter --features guardrails --example legacy_guardrails -- API_BASE MODEL
//! Set UPSTREAM_API_KEY for the Chat Completions provider; request model `guarded`.
//! Output hooks cover streamed text/reasoning only. Redaction is per delta.

use std::sync::Arc;

use anyhow::{Context, Result};
use bitrouter::gateway::assembly::App;
use bitrouter::gateway::extensions::guardrails::plugin::GuardrailsPlugin;
use bitrouter::gateway::pipeline::executor::HttpExecutor;
use bitrouter::gateway::pipeline::routing::StaticRoutingTable;
use bitrouter::gateway::pipeline::types::RoutingTarget;
use bitrouter_ai::types::ApiProtocol;
use bitrouter_guardrails::rules::{Action, GuardrailRule, RuleSet};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let api_base = args.next().context("expected upstream API_BASE")?;
    let service_id = args.next().context("expected upstream MODEL")?;
    anyhow::ensure!(args.next().is_none(), "expected only API_BASE and MODEL");
    let api_key = std::env::var("UPSTREAM_API_KEY")
        .context("set UPSTREAM_API_KEY for upstream authentication")?;

    // These illustrative rules are global to this host's language-model pipeline.
    // Block denies matching input and aborts matching streamed output. Redact
    // replaces matches in each output delta; it does not rewrite request input.
    let rules = RuleSet::from_rules([
        GuardrailRule::new("restricted-text", r"(?i)forbidden", Action::Block)?,
        GuardrailRule::new("ssn", r"\d{3}-\d{2}-\d{4}", Action::Redact)?,
    ]);
    let routing = StaticRoutingTable::new();
    routing.insert(
        "guarded",
        vec![RoutingTarget {
            registry: None,
            provider_name: "legacy-example".into(),
            service_id,
            api_base,
            api_key,
            api_protocol: ApiProtocol::ChatCompletions,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            chat_google_extensions: false,
            reasoning_effort: None,
            model_constraints: Default::default(),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
            headers: Vec::new(),
        }],
    );
    let executor = HttpExecutor::with_defaults()?;
    let app = App::builder()
        // This local example has no inbound authentication and binds loopback.
        .skip_auth(true)
        .language_model(|lm| {
            lm.routing_table(Arc::new(routing))
                .executor(Arc::new(executor));
        })
        // Enabling the feature alone never installs these hooks.
        .plugin(GuardrailsPlugin::with_static(rules))
        .build()?;

    eprintln!("Legacy guardrails host listening on http://127.0.0.1:4356; model: guarded");
    app.serve("127.0.0.1:4356").await?;
    Ok(())
}
