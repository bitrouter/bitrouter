//! The agent-agnostic model-selection IR for `bitrouter spawn`.
//!
//! A [`ModelPlan`] is a partial map from a generic capability [`ModelTier`] to a
//! BitRouter model id. [`resolve`] builds one by merging, low-priority first:
//!
//! 1. the configured default plan (`spawn.model`),
//! 2. the `BITROUTER_SPAWN_PRESET` environment preset,
//! 3. the `BITROUTER_SPAWN_MODEL` environment model (a bare id → every tier),
//! 4. the `--preset` CLI preset, and
//! 5. the `--model` CLI flags (`<id>` → every tier, `<tier>=<id>` → one tier).
//!
//! Later layers override earlier ones **per tier**. The resulting plan is then
//! translated into a specific harness's model environment variables by that
//! agent's binding ([`AgentSpec::tier_env`](super::agent::AgentSpec::tier_env)).
//!
//! Keeping the tier vocabulary here — distinct from both the config layer (which
//! stores raw `label → model` strings) and the per-agent env-var mapping — is
//! what makes the override feature generic across harnesses even though only
//! Claude Code exists today.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, anyhow, bail};
use bitrouter_sdk::config::SpawnConfig;

use crate::spawn::agent::SpawnAgent;

/// A generic capability tier, ordered most→least capable. Agent-agnostic: each
/// agent maps a tier to its own concrete model env var (see
/// [`AgentSpec::tier_env`](super::agent::AgentSpec::tier_env)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelTier {
    /// Most capable / heaviest tier. Claude Code: the `opus` slot.
    High,
    /// Default working tier. Claude Code: the `sonnet` slot.
    Mid,
    /// Cheapest / background tier. Claude Code: the `haiku` slot.
    Low,
}

impl ModelTier {
    /// Every tier, high→low. Used when a single bare model id applies to all
    /// tiers.
    pub const ALL: [ModelTier; 3] = [ModelTier::High, ModelTier::Mid, ModelTier::Low];

    /// The canonical, agent-neutral label for this tier.
    pub fn canonical(self) -> &'static str {
        match self {
            ModelTier::High => "high",
            ModelTier::Mid => "mid",
            ModelTier::Low => "low",
        }
    }
}

impl std::fmt::Display for ModelTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.canonical())
    }
}

/// A resolved model selection: tier → BitRouter model id. Partial — unset tiers
/// fall through to the harness's own defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelPlan {
    tiers: BTreeMap<ModelTier, String>,
}

impl ModelPlan {
    /// True when no tier is set — the launcher then injects no model env vars,
    /// reproducing the harness's default model behaviour.
    pub fn is_empty(&self) -> bool {
        self.tiers.is_empty()
    }

    /// Iterate `(tier, model)` pairs, high→low.
    pub fn iter(&self) -> impl Iterator<Item = (ModelTier, &str)> {
        self.tiers.iter().map(|(t, m)| (*t, m.as_str()))
    }

    /// Set one tier, overwriting any previous value (later layers win).
    fn set(&mut self, tier: ModelTier, model: String) {
        self.tiers.insert(tier, model);
    }
}

/// Build the final [`ModelPlan`] for one `bitrouter spawn` invocation by merging
/// all override sources in precedence order (see the module docs). `env_preset`
/// / `env_model` are the values of `BITROUTER_SPAWN_PRESET` /
/// `BITROUTER_SPAWN_MODEL` (passed in rather than read here so the resolver is
/// pure and unit-testable); `cli_preset` / `cli_models` are the `--preset` and
/// `--model` flags.
pub fn resolve(
    agent: SpawnAgent,
    spawn_cfg: &SpawnConfig,
    env_preset: Option<String>,
    env_model: Option<String>,
    cli_preset: Option<&str>,
    cli_models: &[String],
) -> Result<ModelPlan> {
    let mut plan = ModelPlan::default();

    // 1. Config default plan (lowest priority).
    apply_label_map(&mut plan, agent, &spawn_cfg.model, "spawn.model")?;

    // 2. Environment preset.
    if let Some(name) = env_preset.as_deref() {
        apply_preset(&mut plan, agent, spawn_cfg, name, "BITROUTER_SPAWN_PRESET")?;
    }

    // 3. Environment bare model → every tier.
    if let Some(model) = env_model.as_deref() {
        let model = model.trim();
        if model.is_empty() {
            bail!("BITROUTER_SPAWN_MODEL is set but empty");
        }
        apply_bare(&mut plan, model);
    }

    // 4. CLI preset.
    if let Some(name) = cli_preset {
        apply_preset(&mut plan, agent, spawn_cfg, name, "--preset")?;
    }

    // 5. CLI `--model` flags (highest priority).
    for spec in cli_models {
        apply_model_spec(&mut plan, agent, spec)?;
    }

    Ok(plan)
}

/// Apply a raw `tier-label → model-id` map (a config default plan or a preset
/// body) onto `plan`. Labels are validated against the agent; an unknown label,
/// an empty model id, or two labels resolving to the same tier are all hard
/// errors. Entries are processed in sorted-label order so error messages are
/// deterministic.
fn apply_label_map(
    plan: &mut ModelPlan,
    agent: SpawnAgent,
    map: &HashMap<String, String>,
    source: &str,
) -> Result<()> {
    let mut entries: Vec<(&String, &String)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));

    // Track tiers set *within this source* to catch e.g. both `high:` and
    // `opus:` (which collide on the same tier) instead of silently picking one.
    let mut seen: BTreeMap<ModelTier, &str> = BTreeMap::new();
    for (label, model) in entries {
        let tier = agent.parse_tier(label).ok_or_else(|| {
            anyhow!(
                "unknown model tier '{label}' in {source}; valid tiers: {}",
                agent.tier_labels()
            )
        })?;
        let model = model.trim();
        if model.is_empty() {
            bail!("empty model id for tier '{label}' in {source}");
        }
        if let Some(prev) = seen.insert(tier, label) {
            bail!(
                "tier '{tier}' set twice in {source} (labels '{prev}' and '{label}' both map to it)"
            );
        }
        plan.set(tier, model.to_string());
    }
    Ok(())
}

/// Look up a named preset in `spawn_cfg.presets` and apply its body. An unknown
/// name is a hard error listing the defined presets.
fn apply_preset(
    plan: &mut ModelPlan,
    agent: SpawnAgent,
    spawn_cfg: &SpawnConfig,
    name: &str,
    source: &str,
) -> Result<()> {
    let body = spawn_cfg.presets.get(name).ok_or_else(|| {
        let mut names: Vec<&str> = spawn_cfg.presets.keys().map(String::as_str).collect();
        names.sort_unstable();
        let defined = if names.is_empty() {
            "(none defined under spawn.presets)".to_string()
        } else {
            names.join(", ")
        };
        anyhow!("unknown spawn preset '{name}' (from {source}); defined presets: {defined}")
    })?;
    apply_label_map(plan, agent, body, &format!("preset '{name}'"))
}

/// Apply one `--model` flag: `tier=id` sets a single tier; a bare `id` sets
/// every tier.
fn apply_model_spec(plan: &mut ModelPlan, agent: SpawnAgent, spec: &str) -> Result<()> {
    match spec.split_once('=') {
        Some((label, id)) => {
            let tier = agent.parse_tier(label).ok_or_else(|| {
                anyhow!(
                    "unknown model tier '{}' in --model '{spec}'; valid tiers: {}",
                    label.trim(),
                    agent.tier_labels()
                )
            })?;
            let id = id.trim();
            if id.is_empty() {
                bail!("empty model id in --model '{spec}'");
            }
            plan.set(tier, id.to_string());
        }
        None => {
            let id = spec.trim();
            if id.is_empty() {
                bail!("empty --model value");
            }
            apply_bare(plan, id);
        }
    }
    Ok(())
}

/// Set every tier to `model` (the bare-single-model override).
fn apply_bare(plan: &mut ModelPlan, model: &str) {
    for tier in ModelTier::ALL {
        plan.set(tier, model.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: SpawnAgent = SpawnAgent::Claude;

    /// Build a `SpawnConfig` from `(name, [(tier, model)])` preset specs and an
    /// optional default-plan spec.
    fn cfg(default_plan: &[(&str, &str)], presets: &[(&str, &[(&str, &str)])]) -> SpawnConfig {
        let mut c = SpawnConfig::default();
        for (k, v) in default_plan {
            c.model.insert((*k).to_string(), (*v).to_string());
        }
        for (name, body) in presets {
            let map = body
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            c.presets.insert((*name).to_string(), map);
        }
        c
    }

    /// Collect a plan into a sorted `Vec<(tier, model)>` for easy assertions.
    fn pairs(plan: &ModelPlan) -> Vec<(ModelTier, String)> {
        plan.iter().map(|(t, m)| (t, m.to_string())).collect()
    }

    fn resolve_ok(
        c: &SpawnConfig,
        env_preset: Option<&str>,
        env_model: Option<&str>,
        cli_preset: Option<&str>,
        cli_models: &[&str],
    ) -> ModelPlan {
        let cli: Vec<String> = cli_models.iter().map(|s| s.to_string()).collect();
        resolve(
            CLAUDE,
            c,
            env_preset.map(str::to_string),
            env_model.map(str::to_string),
            cli_preset,
            &cli,
        )
        .unwrap()
    }

    #[test]
    fn empty_when_nothing_set() {
        let plan = resolve_ok(&SpawnConfig::default(), None, None, None, &[]);
        assert!(plan.is_empty());
    }

    #[test]
    fn bare_cli_model_sets_all_tiers() {
        let plan = resolve_ok(&SpawnConfig::default(), None, None, None, &["prov/glm"]);
        assert_eq!(
            pairs(&plan),
            vec![
                (ModelTier::High, "prov/glm".into()),
                (ModelTier::Mid, "prov/glm".into()),
                (ModelTier::Low, "prov/glm".into()),
            ]
        );
    }

    #[test]
    fn tier_cli_model_sets_one_tier() {
        let plan = resolve_ok(&SpawnConfig::default(), None, None, None, &["low=prov/air"]);
        assert_eq!(pairs(&plan), vec![(ModelTier::Low, "prov/air".into())]);
    }

    #[test]
    fn cli_model_overrides_cli_preset_per_tier() {
        let c = cfg(
            &[],
            &[("cheap", &[("high", "p/a"), ("mid", "p/a"), ("low", "p/b")])],
        );
        // Preset sets all three; the explicit --model overrides only `high`.
        let plan = resolve_ok(&c, None, None, Some("cheap"), &["high=p/premium"]);
        assert_eq!(
            pairs(&plan),
            vec![
                (ModelTier::High, "p/premium".into()),
                (ModelTier::Mid, "p/a".into()),
                (ModelTier::Low, "p/b".into()),
            ]
        );
    }

    #[test]
    fn full_precedence_chain() {
        // Each layer touches `mid` so we can see which one wins, plus a unique
        // tier so we can confirm earlier layers still contribute.
        let c = cfg(
            &[("high", "cfg/high"), ("mid", "cfg/mid")],
            &[
                ("envp", &[("mid", "envp/mid")]),
                ("clip", &[("mid", "clip/mid")]),
            ],
        );
        let plan = resolve_ok(
            &c,
            Some("envp"),      // env preset
            Some("env/model"), // env bare model → all tiers
            Some("clip"),      // cli preset
            &["mid=cli/mid"],  // cli flag (highest)
        );
        // `mid` resolves to the highest layer that set it: the CLI flag.
        let mids: Vec<_> = plan
            .iter()
            .filter(|(t, _)| *t == ModelTier::Mid)
            .map(|(_, m)| m.to_string())
            .collect();
        assert_eq!(mids, vec!["cli/mid".to_string()]);
        // `high`/`low` were last set by the env bare model (it set all tiers,
        // overriding cfg/high, and nothing later touched them).
        let high: Vec<_> = plan
            .iter()
            .filter(|(t, _)| *t == ModelTier::High)
            .map(|(_, m)| m.to_string())
            .collect();
        assert_eq!(high, vec!["env/model".to_string()]);
        let low: Vec<_> = plan
            .iter()
            .filter(|(t, _)| *t == ModelTier::Low)
            .map(|(_, m)| m.to_string())
            .collect();
        assert_eq!(low, vec!["env/model".to_string()]);
    }

    #[test]
    fn env_model_overrides_env_preset() {
        let c = cfg(&[], &[("envp", &[("mid", "envp/mid")])]);
        let plan = resolve_ok(&c, Some("envp"), Some("env/all"), None, &[]);
        // env bare model (layer 3) beats env preset (layer 2).
        assert!(plan.iter().all(|(_, m)| m == "env/all"));
    }

    #[test]
    fn config_default_is_lowest_priority() {
        let c = cfg(&[("mid", "cfg/mid")], &[]);
        let plan = resolve_ok(&c, None, None, None, &["mid=cli/mid"]);
        assert_eq!(pairs(&plan), vec![(ModelTier::Mid, "cli/mid".into())]);
    }

    #[test]
    fn partial_preset_leaves_other_tiers_unset() {
        let c = cfg(&[], &[("hi", &[("high", "p/h")])]);
        let plan = resolve_ok(&c, None, None, Some("hi"), &[]);
        assert_eq!(pairs(&plan), vec![(ModelTier::High, "p/h".into())]);
    }

    #[test]
    fn unknown_preset_errors_with_available_list() {
        let c = cfg(&[], &[("cheap", &[("mid", "p/a")])]);
        let err = resolve(CLAUDE, &c, None, None, Some("nope"), &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown spawn preset 'nope'"), "{msg}");
        assert!(msg.contains("cheap"), "should list defined presets: {msg}");
    }

    #[test]
    fn unknown_env_preset_errors() {
        let err = resolve(
            CLAUDE,
            &SpawnConfig::default(),
            Some("ghost".to_string()),
            None,
            None,
            &[],
        )
        .unwrap_err();
        assert!(err.to_string().contains("BITROUTER_SPAWN_PRESET"));
    }

    #[test]
    fn unknown_tier_in_model_spec_errors() {
        let err = resolve(
            CLAUDE,
            &SpawnConfig::default(),
            None,
            None,
            None,
            &["turbo=p/x".to_string()],
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown model tier 'turbo'"), "{msg}");
        assert!(msg.contains("valid tiers"), "{msg}");
    }

    #[test]
    fn unknown_tier_in_config_errors() {
        let c = cfg(&[("ultra", "p/x")], &[]);
        let err = resolve(CLAUDE, &c, None, None, None, &[]).unwrap_err();
        assert!(err.to_string().contains("spawn.model"));
    }

    #[test]
    fn empty_model_id_errors() {
        let err = resolve(
            CLAUDE,
            &SpawnConfig::default(),
            None,
            None,
            None,
            &["mid=".to_string()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("empty model id"));
    }

    #[test]
    fn duplicate_tier_via_alias_in_one_source_errors() {
        // `high` and `opus` both map to the High tier — ambiguous within one map.
        let c = cfg(&[("high", "p/a"), ("opus", "p/b")], &[]);
        let err = resolve(CLAUDE, &c, None, None, None, &[]).unwrap_err();
        assert!(err.to_string().contains("set twice"), "{}", err);
    }
}
