//! Content-based auto-routing: signal detection, complexity estimation,
//! and decision resolution.
//!
//! This module implements the keyword-matching pipeline that sits between
//! an incoming model name (a "trigger" like `"auto"`) and the final
//! concrete model name that gets routed through the normal
//! [`ConfigRoutingTable`](crate::routing::ConfigRoutingTable) pipeline.
//!
//! The pipeline is:
//! 1. **Signal detection** — scan the request text for keyword matches,
//!    pick the highest-scoring signal.
//! 2. **Complexity estimation** — combine heuristics (message length,
//!    turn count, code blocks, complexity keywords) into Low or High.
//! 3. **Decision resolution** — look up `"signal.complexity"` →
//!    `"signal"` → `"default"` in the rule's model map.

use std::collections::HashMap;

use bitrouter_core::routers::content::RouteContext;

use crate::config::{RoutingRuleConfig, SignalConfig};

// ── Compiled types ──────────────────────────────────────────────────

/// Complexity level estimated from request heuristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComplexityLevel {
    Low,
    High,
}

impl ComplexityLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
        }
    }
}

/// A signal definition compiled for fast matching.
///
/// Keywords are stored lowercased so matching against the already-lowercased
/// [`RouteContext::text`] requires no per-keyword allocation.
#[derive(Debug, Clone)]
struct CompiledSignal {
    name: String,
    keywords: Vec<String>,
}

/// A fully compiled routing rule ready for evaluation.
#[derive(Debug, Clone)]
struct CompiledRule {
    signals: Vec<CompiledSignal>,
    high_keywords: Vec<String>,
    message_length_threshold: usize,
    turn_count_threshold: usize,
    code_blocks_increase_complexity: bool,
    /// Maps `"signal.complexity"`, `"signal"`, or `"default"` → model name.
    models: HashMap<String, String>,
}

/// Holds all compiled content-routing rules keyed by trigger model name.
#[derive(Debug, Clone)]
pub struct ContentRoutingRules {
    rules: HashMap<String, CompiledRule>,
}

// ── Default built-in signals ────────────────────────────────────────

/// Parsed shape of `signals/builtin.yaml`.
#[derive(Debug, serde::Deserialize)]
struct BuiltinSignalsDef {
    #[serde(default)]
    signals: HashMap<String, SignalConfig>,
    #[serde(default)]
    complexity: BuiltinComplexityDef,
}

#[derive(Debug, Default, serde::Deserialize)]
struct BuiltinComplexityDef {
    #[serde(default)]
    high_keywords: Vec<String>,
    #[serde(default)]
    message_length_threshold: Option<usize>,
    #[serde(default)]
    turn_count_threshold: Option<usize>,
    #[serde(default)]
    code_blocks_increase_complexity: bool,
}

const BUILTIN_SIGNALS_YAML: &str = include_str!("../signals/builtin.yaml");

fn load_builtin_signals() -> BuiltinSignalsDef {
    serde_saphyr::from_str(BUILTIN_SIGNALS_YAML).unwrap_or_else(|e| {
        eprintln!("warning: failed to parse built-in signals YAML: {e}");
        BuiltinSignalsDef {
            signals: HashMap::new(),
            complexity: BuiltinComplexityDef::default(),
        }
    })
}

// ── Compilation ─────────────────────────────────────────────────────

impl ContentRoutingRules {
    /// Compiles routing rules from configuration.
    ///
    /// Each entry in `rules_config` maps a trigger model name to a
    /// [`RoutingRuleConfig`]. Built-in signals and complexity defaults
    /// are merged according to `inherit_defaults`.
    pub fn compile(rules_config: &HashMap<String, RoutingRuleConfig>) -> Self {
        let builtin = load_builtin_signals();
        let rules = rules_config
            .iter()
            .map(|(trigger, cfg)| {
                let rule = compile_rule(cfg, &builtin);
                (trigger.clone(), rule)
            })
            .collect();
        Self { rules }
    }

    /// Returns `true` if `model_name` is a trigger for content-based routing.
    pub fn is_trigger(&self, model_name: &str) -> bool {
        self.rules.contains_key(model_name)
    }

    /// Runs the classification pipeline and returns the resolved model name,
    /// or `None` if no rule matched or the decision map lacks an entry.
    pub fn resolve(&self, trigger: &str, ctx: &RouteContext) -> Option<String> {
        let rule = self.rules.get(trigger)?;

        // Detect the winning signal.
        let signal = detect_signal(&rule.signals, &ctx.text);

        // Estimate complexity.
        let complexity = estimate_complexity(rule, ctx);

        // Decision resolution: signal.complexity → signal → default.
        resolve_decision(&rule.models, signal.as_deref(), complexity)
    }
}

// ── Signal detection ────────────────────────────────────────────────

/// Scans `text` for keyword matches against each signal.
///
/// Returns the name of the signal with the highest hit count.
/// Ties are broken by definition order (first defined wins).
/// If no keywords match any signal, returns `None`.
fn detect_signal(signals: &[CompiledSignal], text: &str) -> Option<String> {
    let mut best: Option<(&str, usize)> = None;

    for signal in signals {
        let score = signal
            .keywords
            .iter()
            .filter(|kw| text.contains(kw.as_str()))
            .count();

        if score > 0 {
            if let Some((_, best_score)) = best {
                if score > best_score {
                    best = Some((&signal.name, score));
                }
            } else {
                best = Some((&signal.name, score));
            }
        }
    }

    best.map(|(name, _)| name.to_owned())
}

// ── Complexity estimation ───────────────────────────────────────────

/// Estimates complexity from request heuristics.
///
/// Scores ≥ 2 → High, otherwise Low.
fn estimate_complexity(rule: &CompiledRule, ctx: &RouteContext) -> ComplexityLevel {
    let mut score: u8 = 0;

    // High-complexity keywords in the text.
    if rule
        .high_keywords
        .iter()
        .any(|kw| ctx.text.contains(kw.as_str()))
    {
        score += 1;
    }

    // Message length.
    if ctx.char_count >= rule.message_length_threshold {
        score += 1;
    }

    // Turn count.
    if ctx.turn_count >= rule.turn_count_threshold {
        score += 1;
    }

    // Code blocks.
    if rule.code_blocks_increase_complexity && ctx.has_code_blocks {
        score += 1;
    }

    if score >= 2 {
        ComplexityLevel::High
    } else {
        ComplexityLevel::Low
    }
}

// ── Decision resolution ─────────────────────────────────────────────

/// Looks up the model map using the fallback chain:
/// `"{signal}.{complexity}"` → `"{signal}"` → `"default"`.
fn resolve_decision(
    models: &HashMap<String, String>,
    signal: Option<&str>,
    complexity: ComplexityLevel,
) -> Option<String> {
    if let Some(sig) = signal {
        // Try exact: "signal.complexity"
        let exact_key = format!("{}.{}", sig, complexity.as_str());
        if let Some(model) = models.get(&exact_key) {
            return Some(model.clone());
        }

        // Try signal-only: "signal"
        if let Some(model) = models.get(sig) {
            return Some(model.clone());
        }
    }

    // Final fallback: "default"
    models.get("default").cloned()
}

// ── Rule compilation helper ─────────────────────────────────────────

fn compile_rule(cfg: &RoutingRuleConfig, builtin: &BuiltinSignalsDef) -> CompiledRule {
    // Merge signals: built-in first, then user overrides.
    let mut merged_signals: HashMap<String, Vec<String>> = HashMap::new();

    if cfg.inherit_defaults {
        for (name, signal_cfg) in &builtin.signals {
            merged_signals.insert(
                name.clone(),
                signal_cfg
                    .keywords
                    .iter()
                    .map(|k| k.to_lowercase())
                    .collect(),
            );
        }
    }

    // User signals override same-name built-ins.
    for (name, signal_cfg) in &cfg.signals {
        merged_signals.insert(
            name.clone(),
            signal_cfg
                .keywords
                .iter()
                .map(|k| k.to_lowercase())
                .collect(),
        );
    }

    // Preserve a stable ordering: sort by signal name so behaviour is
    // deterministic (built-in ordering is already alphabetical by HashMap
    // iteration, but we want guarantees).
    let mut signal_names: Vec<String> = merged_signals.keys().cloned().collect();
    signal_names.sort();

    let signals: Vec<CompiledSignal> = signal_names
        .into_iter()
        .filter_map(|name| {
            let keywords = merged_signals.remove(&name)?;
            if keywords.is_empty() {
                return None;
            }
            Some(CompiledSignal { name, keywords })
        })
        .collect();

    // Merge complexity config.
    let (high_keywords, msg_thresh, turn_thresh, code_blocks) = if cfg.inherit_defaults {
        // Start from built-in defaults, overlay user config on top.
        let high_kw = if cfg.complexity.high_keywords.is_empty() {
            builtin
                .complexity
                .high_keywords
                .iter()
                .map(|k| k.to_lowercase())
                .collect()
        } else {
            cfg.complexity
                .high_keywords
                .iter()
                .map(|k| k.to_lowercase())
                .collect()
        };
        let msg = cfg
            .complexity
            .message_length_threshold
            .or(builtin.complexity.message_length_threshold)
            .unwrap_or(usize::MAX);
        let turn = cfg
            .complexity
            .turn_count_threshold
            .or(builtin.complexity.turn_count_threshold)
            .unwrap_or(usize::MAX);
        let code = if cfg.complexity.code_blocks_increase_complexity {
            true
        } else {
            builtin.complexity.code_blocks_increase_complexity
        };
        (high_kw, msg, turn, code)
    } else {
        let high_kw = cfg
            .complexity
            .high_keywords
            .iter()
            .map(|k| k.to_lowercase())
            .collect();
        let msg = cfg
            .complexity
            .message_length_threshold
            .unwrap_or(usize::MAX);
        let turn = cfg.complexity.turn_count_threshold.unwrap_or(usize::MAX);
        let code = cfg.complexity.code_blocks_increase_complexity;
        (high_kw, msg, turn, code)
    };

    CompiledRule {
        signals,
        high_keywords,
        message_length_threshold: msg_thresh,
        turn_count_threshold: turn_thresh,
        code_blocks_increase_complexity: code_blocks,
        models: cfg.models.clone(),
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ComplexityConfig;

    /// Helper: build a minimal RoutingRuleConfig with the given models map.
    fn rule_with_models(models: Vec<(&str, &str)>) -> RoutingRuleConfig {
        RoutingRuleConfig {
            inherit_defaults: true,
            signals: HashMap::new(),
            complexity: ComplexityConfig::default(),
            models: models
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        }
    }

    fn ctx(text: &str) -> RouteContext {
        RouteContext {
            text: text.to_lowercase(),
            has_code_blocks: false,
            has_tools: false,
            turn_count: 1,
            char_count: text.len(),
        }
    }

    // ── Signal detection ────────────────────────────────────────────

    #[test]
    fn builtin_signals_parse() {
        let builtin = load_builtin_signals();
        assert!(builtin.signals.contains_key("coding"));
        assert!(builtin.signals.contains_key("math"));
        assert!(builtin.signals.contains_key("creative"));
        assert!(builtin.signals.contains_key("research"));
        assert!(!builtin.complexity.high_keywords.is_empty());
    }

    #[test]
    fn detect_coding_signal() {
        let rules = ContentRoutingRules::compile(&HashMap::from([(
            "auto".to_owned(),
            rule_with_models(vec![("coding", "code-model"), ("default", "general")]),
        )]));

        let result = rules.resolve("auto", &ctx("help me debug this function"));
        assert_eq!(result.as_deref(), Some("code-model"));
    }

    #[test]
    fn detect_math_signal() {
        let rules = ContentRoutingRules::compile(&HashMap::from([(
            "auto".to_owned(),
            rule_with_models(vec![("math", "math-model"), ("default", "general")]),
        )]));

        let result = rules.resolve("auto", &ctx("calculate the integral of x^2"));
        assert_eq!(result.as_deref(), Some("math-model"));
    }

    #[test]
    fn detect_creative_signal() {
        let rules = ContentRoutingRules::compile(&HashMap::from([(
            "auto".to_owned(),
            rule_with_models(vec![("creative", "writer"), ("default", "general")]),
        )]));

        let result = rules.resolve("auto", &ctx("write me a poem about the ocean"));
        assert_eq!(result.as_deref(), Some("writer"));
    }

    #[test]
    fn no_signal_falls_back_to_default() {
        let rules = ContentRoutingRules::compile(&HashMap::from([(
            "auto".to_owned(),
            rule_with_models(vec![("coding", "code-model"), ("default", "general")]),
        )]));

        let result = rules.resolve("auto", &ctx("hello, how are you?"));
        assert_eq!(result.as_deref(), Some("general"));
    }

    #[test]
    fn no_default_and_no_signal_returns_none() {
        let rules = ContentRoutingRules::compile(&HashMap::from([(
            "auto".to_owned(),
            rule_with_models(vec![("coding", "code-model")]),
        )]));

        let result = rules.resolve("auto", &ctx("hello, how are you?"));
        assert!(result.is_none());
    }

    #[test]
    fn non_trigger_returns_none() {
        let rules = ContentRoutingRules::compile(&HashMap::from([(
            "auto".to_owned(),
            rule_with_models(vec![("default", "general")]),
        )]));

        let result = rules.resolve("not-a-trigger", &ctx("anything"));
        assert!(result.is_none());
    }

    // ── Complexity ──────────────────────────────────────────────────

    #[test]
    fn complexity_low_by_default() {
        let rules = ContentRoutingRules::compile(&HashMap::from([(
            "auto".to_owned(),
            rule_with_models(vec![
                ("coding.low", "fast"),
                ("coding.high", "heavy"),
                ("default", "general"),
            ]),
        )]));

        let result = rules.resolve("auto", &ctx("fix this function bug"));
        assert_eq!(result.as_deref(), Some("fast"));
    }

    #[test]
    fn complexity_high_from_multiple_heuristics() {
        let rules = ContentRoutingRules::compile(&HashMap::from([(
            "auto".to_owned(),
            rule_with_models(vec![
                ("coding.low", "fast"),
                ("coding.high", "heavy"),
                ("default", "general"),
            ]),
        )]));

        // Long message + complexity keyword => score >= 2 => High
        let long_text = format!(
            "I need you to optimize this complex algorithm. {}",
            "x".repeat(1200)
        );
        let result = rules.resolve("auto", &ctx(&long_text));
        assert_eq!(result.as_deref(), Some("heavy"));
    }

    #[test]
    fn complexity_fallback_signal_without_complexity_suffix() {
        let rules = ContentRoutingRules::compile(&HashMap::from([(
            "auto".to_owned(),
            rule_with_models(vec![
                ("coding", "code-any"),
                ("coding.high", "code-heavy"),
                ("default", "general"),
            ]),
        )]));

        // Low complexity coding → "coding.low" not in map → fallback "coding"
        let result = rules.resolve("auto", &ctx("write a function"));
        assert_eq!(result.as_deref(), Some("code-any"));
    }

    // ── Custom signals ──────────────────────────────────────────────

    #[test]
    fn custom_signal_overrides_builtin() {
        let mut cfg = rule_with_models(vec![("coding", "code-model"), ("default", "general")]);
        // Override "coding" with a very specific keyword
        cfg.signals.insert(
            "coding".to_owned(),
            SignalConfig {
                keywords: vec!["my-custom-keyword".to_owned()],
            },
        );

        let rules = ContentRoutingRules::compile(&HashMap::from([("auto".to_owned(), cfg)]));

        // Built-in "code" keyword should no longer match
        let result = rules.resolve("auto", &ctx("help me code something"));
        assert_eq!(result.as_deref(), Some("general"));

        // Custom keyword should match
        let result = rules.resolve("auto", &ctx("my-custom-keyword is here"));
        assert_eq!(result.as_deref(), Some("code-model"));
    }

    #[test]
    fn custom_signal_added_alongside_builtins() {
        let mut cfg = rule_with_models(vec![
            ("coding", "code-model"),
            ("devops", "devops-model"),
            ("default", "general"),
        ]);
        cfg.signals.insert(
            "devops".to_owned(),
            SignalConfig {
                keywords: vec![
                    "kubernetes".to_owned(),
                    "docker".to_owned(),
                    "deploy".to_owned(),
                ],
            },
        );

        let rules = ContentRoutingRules::compile(&HashMap::from([("auto".to_owned(), cfg)]));

        let result = rules.resolve("auto", &ctx("deploy this to kubernetes with docker"));
        assert_eq!(result.as_deref(), Some("devops-model"));
    }

    #[test]
    fn inherit_defaults_false_uses_only_custom() {
        let mut cfg = rule_with_models(vec![("custom", "custom-model"), ("default", "general")]);
        cfg.inherit_defaults = false;
        cfg.signals.insert(
            "custom".to_owned(),
            SignalConfig {
                keywords: vec!["special".to_owned()],
            },
        );

        let rules = ContentRoutingRules::compile(&HashMap::from([("auto".to_owned(), cfg)]));

        // Built-in coding keywords should not trigger anything
        let result = rules.resolve("auto", &ctx("help me code this function"));
        assert_eq!(result.as_deref(), Some("general"));

        // Custom keyword should work
        let result = rules.resolve("auto", &ctx("special request"));
        assert_eq!(result.as_deref(), Some("custom-model"));
    }

    // ── Decision resolution ─────────────────────────────────────────

    #[test]
    fn decision_exact_match_preferred() {
        let models = HashMap::from([
            ("coding.high".to_owned(), "heavy".to_owned()),
            ("coding".to_owned(), "any".to_owned()),
            ("default".to_owned(), "fallback".to_owned()),
        ]);

        let result = resolve_decision(&models, Some("coding"), ComplexityLevel::High);
        assert_eq!(result.as_deref(), Some("heavy"));
    }

    #[test]
    fn decision_signal_fallback() {
        let models = HashMap::from([
            ("coding".to_owned(), "any".to_owned()),
            ("default".to_owned(), "fallback".to_owned()),
        ]);

        let result = resolve_decision(&models, Some("coding"), ComplexityLevel::High);
        assert_eq!(result.as_deref(), Some("any"));
    }

    #[test]
    fn decision_default_fallback() {
        let models = HashMap::from([("default".to_owned(), "fallback".to_owned())]);

        let result = resolve_decision(&models, Some("unknown"), ComplexityLevel::Low);
        assert_eq!(result.as_deref(), Some("fallback"));
    }

    #[test]
    fn decision_no_signal_goes_to_default() {
        let models = HashMap::from([("default".to_owned(), "fallback".to_owned())]);

        let result = resolve_decision(&models, None, ComplexityLevel::Low);
        assert_eq!(result.as_deref(), Some("fallback"));
    }

    #[test]
    fn decision_no_match_returns_none() {
        let models = HashMap::from([("coding".to_owned(), "code".to_owned())]);

        let result = resolve_decision(&models, Some("math"), ComplexityLevel::Low);
        assert!(result.is_none());
    }

    // ── Highest score wins ──────────────────────────────────────────

    #[test]
    fn highest_score_signal_wins() {
        let signals = vec![
            CompiledSignal {
                name: "coding".to_owned(),
                keywords: vec!["code".to_owned(), "function".to_owned()],
            },
            CompiledSignal {
                name: "math".to_owned(),
                keywords: vec![
                    "calculate".to_owned(),
                    "equation".to_owned(),
                    "formula".to_owned(),
                ],
            },
        ];

        // Text has 1 coding keyword, 2 math keywords → math wins
        let result = detect_signal(&signals, "calculate this code equation using the formula");
        assert_eq!(result.as_deref(), Some("math"));
    }

    #[test]
    fn tie_goes_to_first_defined() {
        let signals = vec![
            CompiledSignal {
                name: "aaa".to_owned(),
                keywords: vec!["word".to_owned()],
            },
            CompiledSignal {
                name: "bbb".to_owned(),
                keywords: vec!["word".to_owned()],
            },
        ];

        // Both match with score 1 → first defined wins
        let result = detect_signal(&signals, "word");
        assert_eq!(result.as_deref(), Some("aaa"));
    }

    // ── Empty / edge cases ──────────────────────────────────────────

    #[test]
    fn empty_text_returns_default() {
        let rules = ContentRoutingRules::compile(&HashMap::from([(
            "auto".to_owned(),
            rule_with_models(vec![("coding", "code-model"), ("default", "general")]),
        )]));

        let result = rules.resolve("auto", &ctx(""));
        assert_eq!(result.as_deref(), Some("general"));
    }

    #[test]
    fn case_insensitive_matching() {
        let rules = ContentRoutingRules::compile(&HashMap::from([(
            "auto".to_owned(),
            rule_with_models(vec![("coding", "code-model"), ("default", "general")]),
        )]));

        let result = rules.resolve("auto", &ctx("HELP ME DEBUG THIS FUNCTION"));
        assert_eq!(result.as_deref(), Some("code-model"));
    }

    // ── Compile empty ───────────────────────────────────────────────

    #[test]
    fn compile_empty_config() {
        let rules = ContentRoutingRules::compile(&HashMap::new());
        assert!(!rules.is_trigger("anything"));
    }
}
