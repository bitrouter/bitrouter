//! Assembly: turn a parsed [`Config`] into a running [`App`].
//!
//! This is the home of v0's `load_builtin_plugins` logic — it lives in the
//! `apps/bitrouter` **lib** (above the SDK and the plugins), wiring the builtin
//! hooks onto the `language_model` pipeline from config (002 §4.3).

use std::sync::Arc;

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use bitrouter_sdk::App;
use bitrouter_sdk::config::{Config, ConfigRoutingTable};
use bitrouter_sdk::language_model::HttpExecutor;

use bitrouter_auth::AuthHook;
use bitrouter_guardrails::{Action, GuardrailPreHook, GuardrailRule, GuardrailStreamHook, RuleSet};
use bitrouter_policy::{PolicyHook, PolicyStore};
use bitrouter_settlement::{ModelPricing, MppState, PricingTable, SettlementBundle};

/// A running application plus the database pool it was assembled over (the
/// caller keeps the pool for management commands — key creation, etc.).
pub struct Assembled {
    /// The fully wired application.
    pub app: App,
    /// The shared database pool.
    pub pool: SqlitePool,
}

/// Assemble an [`App`] from a parsed config: connect the database, run every
/// plugin's migrations, build the routing table + executor, and wire the
/// builtin hooks onto the `language_model` pipeline.
pub async fn build_app(config: &Config) -> Result<Assembled> {
    // ---- database + migrations (each plugin owns its own tables) ----
    let pool = SqlitePool::connect(&config.database.url)
        .await
        .with_context(|| format!("connecting to database {}", config.database.url))?;
    bitrouter_auth::migrate(&pool)
        .await
        .context("running bitrouter-auth migrations")?;
    bitrouter_settlement::migrate(&pool)
        .await
        .context("running bitrouter-settlement migrations")?;

    // ---- routing table + upstream executor ----
    let routing_table = Arc::new(ConfigRoutingTable::from_config(config.clone()));
    let executor =
        Arc::new(HttpExecutor::with_defaults().context("building the upstream HTTP executor")?);

    // ---- pricing, MPP, policy, guardrails — all derived from config ----
    let pricing = build_pricing_table(config);
    let mpp = build_mpp_state(config, &pool)?;
    let policy_store = Arc::new(load_policy_store(config).await?);
    let guardrail_rules = build_guardrail_rules(config)?;

    // The settlement bundle owns the MetricsStore; share it with PolicyHook so
    // spend ceilings can be enforced (003 §4.7).
    let settlement = SettlementBundle::new(pool.clone(), pricing, mpp);
    let metrics_store = settlement.metrics_store();

    let pool_for_hooks = pool.clone();
    let app = App::builder()
        .skip_auth(config.server.skip_auth)
        .metrics_store(metrics_store.clone())
        .language_model(move |lm| {
            lm.routing_table(routing_table).executor(executor);
            // Stage 1, in order: auth → policy → guardrail (upstream).
            lm.pre_request_hook(AuthHook::new(pool_for_hooks.clone()));
            lm.pre_request_hook(PolicyHook::new(policy_store, Some(metrics_store)));
            if !guardrail_rules.is_empty() {
                lm.pre_request_hook(GuardrailPreHook::new(guardrail_rules.clone()));
                // StreamHook stage: guardrail downstream redaction / abort.
                lm.stream_hook(GuardrailStreamHook::new(guardrail_rules));
            }
        })
        // The settlement bundle installs BalanceCheckHook, ByokRouteHook,
        // MppStreamHook, the ChargeStrategy chain and ReceiptRecorder.
        .plugin(settlement)
        .build()
        .context("building the App")?;

    Ok(Assembled { app, pool })
}

/// Build the settlement `PricingTable` from every provider's per-model pricing.
fn build_pricing_table(config: &Config) -> PricingTable {
    let mut table = PricingTable::new();
    for (provider_id, provider) in &config.providers {
        for model in &provider.models {
            if let Some(pricing) = model.pricing {
                table.insert(
                    provider_id.clone(),
                    model.id.clone(),
                    ModelPricing::new(
                        pricing.input_micro_usd_per_token,
                        pricing.output_micro_usd_per_token,
                    ),
                );
            }
        }
    }
    table
}

/// Build the MPP state from `plugins.bitrouter-settlement` config. v1.0 wires
/// the Tempo channel only; `solana` is rejected (008 §1.1).
fn build_mpp_state(config: &Config, pool: &SqlitePool) -> Result<Option<MppState>> {
    let Some(settlement_cfg) = config.plugins.get("bitrouter-settlement") else {
        return Ok(None);
    };
    let enabled = settlement_cfg
        .get("mpp_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !enabled {
        return Ok(None);
    }
    match settlement_cfg
        .get("mpp_channel")
        .and_then(|v| v.as_str())
        .unwrap_or("tempo")
    {
        "tempo" => Ok(Some(MppState::tempo(pool.clone()))),
        "solana" => {
            // Solana MPP is out of scope for v1.0 — fail loudly, do not
            // silently fall back (008 §1.1).
            MppState::solana(pool.clone())
                .map(Some)
                .context("MPP channel 'solana' is not supported in v1.0")
        }
        other => anyhow::bail!("unknown MPP channel '{other}' (expected 'tempo')"),
    }
}

/// Load the `PolicyStore` from `plugins.bitrouter-policy.policy_dir`, if set.
async fn load_policy_store(config: &Config) -> Result<PolicyStore> {
    let dir = config
        .plugins
        .get("bitrouter-policy")
        .and_then(|c| c.get("policy_dir"))
        .and_then(|v| v.as_str());
    match dir {
        Some(dir) => PolicyStore::load_dir(dir)
            .await
            .with_context(|| format!("loading policies from {dir}")),
        None => Ok(PolicyStore::new()),
    }
}

/// Build the guardrail `RuleSet` from `plugins.bitrouter-guardrails.custom_patterns`.
/// Each entry is `{ name, pattern, action: "block" | "redact" }`.
fn build_guardrail_rules(config: &Config) -> Result<RuleSet> {
    let Some(patterns) = config
        .plugins
        .get("bitrouter-guardrails")
        .and_then(|c| c.get("custom_patterns"))
        .and_then(|v| v.as_array())
    else {
        return Ok(RuleSet::new());
    };
    let mut set = RuleSet::new();
    for entry in patterns {
        let name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .context("guardrail pattern missing 'name'")?;
        let pattern = entry
            .get("pattern")
            .and_then(|v| v.as_str())
            .context("guardrail pattern missing 'pattern'")?;
        let action = match entry.get("action").and_then(|v| v.as_str()) {
            Some("block") | None => Action::Block,
            Some("redact") => Action::Redact,
            Some(other) => anyhow::bail!("unknown guardrail action '{other}'"),
        };
        set.push(
            GuardrailRule::new(name, pattern, action)
                .with_context(|| format!("compiling guardrail pattern '{name}'"))?,
        );
    }
    Ok(set)
}
