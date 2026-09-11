//! Typed candidate construction and review against the live routing snapshot.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use bitrouter_sdk::config::{AccountStrategy, ConfigRoutingTable, resolve_presets};
use bitrouter_sdk::language_model::RoutingTable;
use serde::{Deserialize, Serialize};

use super::EvolutionRuntime;
use crate::acp_trajectory::checkpoint::types::AssessmentSource;
use crate::evolution::bandit::BanditConfig;
use crate::evolution::catalog::{block_digest, route_contract};
use crate::evolution::control::{
    BlockDefinition, BlockRule, BlockStatus, ControlState, EvolutionMode,
};
use crate::evolution::judge::JUDGE_VERSION;
use crate::evolution::rubric::digest;
use crate::evolution::scoring::{TUI_EVALUATOR_ID, TUI_EVALUATOR_VERSION, measurement_contract};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FeedbackChoice {
    Manual,
    Judge { model: String },
}

impl FeedbackChoice {
    pub fn label(&self) -> String {
        match self {
            Self::Manual => "Manual rubric review".into(),
            Self::Judge { model } => format!("Automatic judge: {model}"),
        }
    }

    fn contract(&self) -> Result<String> {
        match self {
            Self::Manual => measurement_contract(
                AssessmentSource::Human,
                TUI_EVALUATOR_ID,
                TUI_EVALUATOR_VERSION,
            ),
            Self::Judge { model } => {
                ensure!(!model.trim().is_empty(), "judge model is required");
                measurement_contract(
                    AssessmentSource::Agentic,
                    &format!("checkpoint-judge:{model}"),
                    JUDGE_VERSION,
                )
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateSpec {
    pub block_id: String,
    #[serde(default)]
    pub predecessor: Option<String>,
    pub source: String,
    pub rules: Vec<BlockRule>,
    pub rationale: String,
    pub independence_rationale: String,
    pub feedback: FeedbackChoice,
}

impl CandidateSpec {
    fn definition(&self, state: &ControlState) -> Result<BlockDefinition> {
        let mut definition = BlockDefinition {
            block_id: self.block_id.clone(),
            source: self.source.clone(),
            rationale: self.rationale.clone(),
            rules: self.rules.clone(),
            independence_rationale: self.independence_rationale.clone(),
            dependencies: BTreeMap::new(),
            measurement_contract: self.feedback.contract()?,
            batch_sessions: 16,
            bandit: BanditConfig::default(),
        };
        if let Some(previous) = &self.predecessor {
            let block = state
                .experiment(&self.block_id, Some(previous))
                .context("previous experiment missing")?;
            definition.dependencies = block.definition.dependencies.clone();
            definition.batch_sessions = block.definition.batch_sessions;
            definition.bandit = block.definition.bandit.clone();
        }
        definition.validate()?;
        Ok(definition)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateRoute {
    pub selector: String,
    pub can_match: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReservedMatcher {
    pub block_id: String,
    pub source: String,
    pub selector: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateCatalog {
    pub routes: Vec<CandidateRoute>,
    pub reserved_matchers: Vec<ReservedMatcher>,
    pub mode: EvolutionMode,
    pub judge_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidatePreview {
    pub owner: String,
    pub spec: CandidateSpec,
    pub definition: BlockDefinition,
    pub routing_digest: String,
    pub control_generation: u64,
    pub mode_epoch: u64,
    pub mode: EvolutionMode,
    #[serde(default)]
    pub reset_to_configured: bool,
    pub route_descriptions: BTreeMap<String, String>,
    pub preview_digest: String,
}

impl CandidatePreview {
    fn content_digest(&self) -> Result<String> {
        digest(&(
            "candidate-preview-v1",
            &self.owner,
            &self.spec,
            &self.definition,
            &self.routing_digest,
            self.control_generation,
            self.mode_epoch,
            self.mode,
            self.reset_to_configured,
            &self.route_descriptions,
        ))
    }

    fn validate(&self, state: &ControlState) -> Result<()> {
        ensure!(
            self.preview_digest == self.content_digest()?
                && digest(&self.spec.definition(state)?)? == digest(&self.definition)?,
            "candidate preview was modified; review it again"
        );
        Ok(())
    }

    async fn commit(
        &self,
        service: &crate::evolution::service::EvolutionService,
        precondition: impl FnOnce() -> Result<()>,
    ) -> Result<ControlState> {
        let expected = (self.control_generation, self.mode_epoch);
        if let Some(previous) = &self.spec.predecessor {
            service
                .revise_checked(
                    crate::evolution::service::revisions::RevisionRegistration {
                        definition: self.definition.clone(),
                        routing_digest: self.routing_digest.clone(),
                        predecessor: previous.clone(),
                        reset_to_configured: self.reset_to_configured,
                        expected_control: expected,
                    },
                    precondition,
                )
                .await
        } else {
            service
                .register_checked(
                    self.definition.clone(),
                    self.routing_digest.clone(),
                    Some(expected),
                    precondition,
                )
                .await
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CandidateAction {
    Catalog,
    Revision { block_id: String },
    Preview { spec: Box<CandidateSpec> },
    Register { preview: Box<CandidatePreview> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "report", rename_all = "snake_case")]
pub enum CandidateReport {
    Catalog(CandidateCatalog),
    Revision(Box<CandidateSpec>),
    Preview(Box<CandidatePreview>),
    Registered {
        block_id: String,
        experiment_id: String,
        status: BlockStatus,
        mode: EvolutionMode,
    },
}

impl EvolutionRuntime {
    pub async fn candidate_action(
        &self,
        owner: &str,
        action: CandidateAction,
    ) -> Result<CandidateReport> {
        let service = self.service(owner)?;
        match action {
            CandidateAction::Revision { block_id } => {
                let state = service.state().await?;
                let block = state
                    .blocks
                    .get(&block_id)
                    .context("unknown policy block")?;
                let (generation, config) = self.routing.versioned_snapshot();
                let policies = self.policies.routing_snapshot();
                let reset = block_digest(&config, &policies, &block.definition)
                    .await
                    .as_ref()
                    .ok()
                    != Some(&block.routing_config_digest)
                    || !state.external_dependencies_match(block);
                let (baseline, _) = state.baseline_source(block)?;
                let adopted = !reset
                    && block.status == BlockStatus::Adopted
                    && state.dependencies_match(block);
                let mut rules = block.definition.rules.clone();
                for rule in &mut rules {
                    let old = baseline
                        .definition
                        .rules
                        .iter()
                        .find(|r| r.selector == rule.selector && r.fingerprint == rule.fingerprint)
                        .context("baseline matcher missing")?;
                    rule.baseline_route = if reset {
                        rule.selector.clone()
                    } else if adopted {
                        old.challenger_route.clone()
                    } else {
                        old.baseline_route.clone()
                    };
                }
                ensure!(
                    self.routing.generation() == generation
                        && self.policies.matches_routing_snapshot(&policies),
                    "routing changed while preparing revision; retry"
                );
                let feedback = if state.mode == EvolutionMode::Automatic {
                    state
                        .judge_model
                        .clone()
                        .map_or(FeedbackChoice::Manual, |model| FeedbackChoice::Judge {
                            model,
                        })
                } else {
                    FeedbackChoice::Manual
                };
                Ok(CandidateReport::Revision(Box::new(CandidateSpec {
                    block_id,
                    predecessor: Some(block.experiment_id.clone()),
                    source: block.definition.source.clone(),
                    rules,
                    rationale: String::new(),
                    independence_rationale: block.definition.independence_rationale.clone(),
                    feedback,
                })))
            }
            CandidateAction::Catalog => {
                let (_, config) = self.routing.versioned_snapshot();
                let state = service.state().await?;
                let table = ConfigRoutingTable::from_config(config.clone());
                let mut routes = BTreeMap::new();
                for model in table.list_models() {
                    let can_match = config.models.contains_key(&model.id)
                        || model.id == "bitrouter/auto"
                        || model.id.starts_with('@');
                    routes.insert(model.id, can_match);
                }
                for preset in config.presets.keys() {
                    routes.insert(format!("@{preset}"), true);
                    for variant in config.variants.keys() {
                        routes.insert(format!("@{preset}:{variant}"), true);
                    }
                }
                let reserved_matchers = state
                    .blocks
                    .values()
                    .flat_map(|block| {
                        block.definition.rules.iter().map(|rule| ReservedMatcher {
                            block_id: block.definition.block_id.clone(),
                            source: block.definition.source.clone(),
                            selector: rule.selector.clone(),
                        })
                    })
                    .collect();
                Ok(CandidateReport::Catalog(CandidateCatalog {
                    routes: routes
                        .into_iter()
                        .map(|(selector, can_match)| CandidateRoute {
                            selector,
                            can_match,
                        })
                        .collect(),
                    reserved_matchers,
                    mode: state.mode,
                    judge_model: state.judge_model,
                }))
            }
            CandidateAction::Preview { spec } => {
                let state = service.state().await?;
                if let FeedbackChoice::Judge { model } = &spec.feedback {
                    ensure!(
                        state.judge_model.as_ref() == Some(model),
                        "configured judge changed; select the current evaluator"
                    );
                }
                let definition = spec.definition(&state)?;
                let (generation, config) = self.routing.versioned_snapshot();
                let policies = self.policies.routing_snapshot();
                let routing_digest = block_digest(&config, &policies, &definition).await?;
                // Validate overlap and block identity before offering registration.
                let mut prospective = state.clone();
                let reset_to_configured = if let Some(previous) = &spec.predecessor {
                    let block = state
                        .experiment(&spec.block_id, Some(previous))
                        .context("previous experiment missing")?;
                    let reset = block_digest(&config, &policies, &block.definition)
                        .await
                        .as_ref()
                        .ok()
                        != Some(&block.routing_config_digest)
                        || !state.external_dependencies_match(block);
                    prospective.revise(
                        definition.clone(),
                        routing_digest.clone(),
                        previous,
                        reset,
                    )?;
                    reset
                } else {
                    prospective.register(definition.clone(), routing_digest.clone())?;
                    false
                };
                let mut descriptions = BTreeMap::new();
                let mut exhaustive = config.clone();
                for provider in exhaustive.providers.values_mut() {
                    provider.account_strategy = AccountStrategy::Failover;
                }
                let table = ConfigRoutingTable::from_config(exhaustive);
                for rule in &definition.rules {
                    for route in [&rule.baseline_route, &rule.challenger_route] {
                        if descriptions.contains_key(route) {
                            continue;
                        }
                        let contract = route_contract(&config, &table, &policies, route).await?;
                        let resolution = resolve_presets(route, &config.presets, &config.variants)?;
                        let mut lines = vec![format!("Route: {route}")];
                        if let Some(policy) = resolution.policy {
                            lines.push(format!("Policy: {policy}"));
                        }
                        if resolution.overrides.system_prompt.is_some()
                            || !resolution.overrides.params.is_empty()
                        {
                            lines.push("Includes configured prompt or generation defaults.".into());
                        }
                        if let Some(models) = contract.get("models").and_then(|v| v.as_object()) {
                            for (model, value) in models {
                                lines.push(format!("Model/tier: {model}"));
                                if let Some(protocols) =
                                    value.get("protocols").and_then(|v| v.as_array())
                                {
                                    let mut chains: BTreeMap<String, Vec<String>> = BTreeMap::new();
                                    for protocol in protocols {
                                        let targets = protocol
                                            .get("targets")
                                            .and_then(|v| v.as_array())
                                            .context("route preview targets are unavailable")?;
                                        let mut hops = Vec::new();
                                        if targets.iter().any(|t| {
                                            t.get("account_strategy").and_then(|v| v.as_str())
                                                == Some("Balance")
                                        }) {
                                            hops.push("Account order may rotate per request; shown in configured order.".into());
                                        }
                                        for (index, target) in targets.iter().enumerate() {
                                            let account = target
                                                .get("account")
                                                .and_then(|v| v.as_str())
                                                .map(|s| format!(" (account {s})"))
                                                .unwrap_or_default();
                                            hops.push(format!(
                                                "  Hop {}: {}/{}{}",
                                                index + 1,
                                                target
                                                    .get("provider")
                                                    .and_then(|v| v.as_str())
                                                    .context("route provider is unavailable")?,
                                                target
                                                    .get("model")
                                                    .and_then(|v| v.as_str())
                                                    .context("route model is unavailable")?,
                                                account
                                            ));
                                        }
                                        chains.entry(hops.join("\n")).or_default().push(
                                            protocol
                                                .get("inbound")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("default")
                                                .into(),
                                        );
                                    }
                                    for (chain, formats) in chains {
                                        lines.push(format!(
                                            "Request formats: {}",
                                            formats.join(", ")
                                        ));
                                        lines.push(chain);
                                    }
                                }
                            }
                        }
                        descriptions.insert(route.clone(), lines.join("\n"));
                    }
                }
                ensure!(
                    self.routing.generation() == generation
                        && self.policies.matches_routing_snapshot(&policies),
                    "routing changed during candidate preview; retry"
                );
                let mut preview = CandidatePreview {
                    owner: owner.into(),
                    spec: *spec,
                    definition,
                    routing_digest,
                    control_generation: state.generation,
                    mode_epoch: state.mode_epoch,
                    mode: state.mode,
                    reset_to_configured,
                    route_descriptions: descriptions,
                    preview_digest: String::new(),
                };
                preview.preview_digest = preview.content_digest()?;
                Ok(CandidateReport::Preview(Box::new(preview)))
            }
            CandidateAction::Register { preview } => {
                let state = service.state().await?;
                preview.validate(&state)?;
                ensure!(
                    preview.owner == owner,
                    "candidate preview belongs to another owner"
                );
                let id = preview.spec.block_id.clone();
                let registered = if state
                    .registration(
                        &preview.definition,
                        &preview.routing_digest,
                        preview.spec.predecessor.as_deref(),
                    )?
                    .is_some()
                {
                    // Lost replies remain idempotent after subsequent revisions.
                    preview
                        .commit(&service, || {
                            anyhow::bail!(
                                "previous registration disappeared; review the candidate again"
                            )
                        })
                        .await?
                } else {
                    let (generation, config) = self.routing.versioned_snapshot();
                    let policies = self.policies.routing_snapshot();
                    let dependency = block_digest(&config, &policies, &preview.definition).await?;
                    ensure!(
                        dependency == preview.routing_digest,
                        "routes changed since preview; review the candidate again"
                    );
                    if let Some(previous) = &preview.spec.predecessor {
                        let block = state
                            .experiment(&id, Some(previous))
                            .context("previous experiment missing")?;
                        let reset = block_digest(&config, &policies, &block.definition)
                            .await
                            .as_ref()
                            .ok()
                            != Some(&block.routing_config_digest)
                            || !state.external_dependencies_match(block);
                        ensure!(
                            reset == preview.reset_to_configured,
                            "baseline inheritance changed; review the candidate again"
                        );
                    } else {
                        ensure!(
                            !preview.reset_to_configured,
                            "initial registration cannot rebase"
                        );
                    }
                    preview
                        .commit(&service, || {
                            ensure!(
                                self.routing.generation() == generation
                                    && self.policies.matches_routing_snapshot(&policies),
                                "routing changed before registration; review the candidate again"
                            );
                            Ok(())
                        })
                        .await?
                };
                let block = registered
                    .registration(
                        &preview.definition,
                        &preview.routing_digest,
                        preview.spec.predecessor.as_deref(),
                    )?
                    .context("registered block is unavailable")?;
                Ok(CandidateReport::Registered {
                    block_id: id,
                    experiment_id: block.experiment_id.clone(),
                    status: block.status,
                    mode: registered.mode,
                })
            }
        }
    }
}
