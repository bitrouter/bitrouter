//! Preset and variant resolution.
//!
//! Parses model-field strings like `@careful:price`, validates preset/variant
//! definitions at config load time, and ships built-in variants
//! (`:price` only in v0).
//!
//! Grammar:
//! ```text
//! model_field   ::= callable_name [':' variant_name]
//! callable_name ::= real_model_name              ; e.g. "gpt-5"
//!                 | '@' preset_name              ; e.g. "@careful"
//! ```
//!
//! The two callable forms are mutually exclusive: when the field begins
//! with `@`, the bare model name comes from the preset's own `model`
//! field, not from the request string. So `@careful:price` parses as
//! preset=careful + variant=price (no caller-supplied model token).
//!
//! Resolution rules:
//! - Strip the `@preset_name` prefix (everything between `@` and the
//!   first `:` or end-of-string).
//! - Strip the `:variant_name` suffix (after the last `:` in what remains).
//! - `@name` lookup is mandatory — an unknown preset is a 400.
//! - `:name` lookup is passthrough — unknown variant names are silently
//!   preserved on the model string (some upstream IDs like HuggingFace
//!   revisions contain `:`).

use std::collections::HashMap;

use bitrouter_core::routers::routing_table::AppliedPreset;

use crate::config::{BitrouterConfig, PresetConfig, RoutingPrefs, SortKey, VariantConfig};
use crate::error::ConfigError;

// ── Parsing ─────────────────────────────────────────────────────────

/// Parsed components of a model-field string.
///
/// `model` is `None` when the request was `@preset` alone — in that case
/// the preset's own `model` field must supply the real model name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedModelField {
    pub preset: Option<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
}

/// Parses `[@preset_name][model_name][:variant_name]` into its three parts.
///
/// Pure string manipulation — does not consult any preset or variant maps.
/// Lookup happens in [`resolve`].
pub fn parse_model_field(raw: &str) -> ParsedModelField {
    let (preset, rest) = if let Some(after) = raw.strip_prefix('@') {
        // Preset name extends to the next ':' (which starts the variant),
        // or to end-of-string. Everything between `@` and the next ':' is the
        // preset name; the rest (if any) is `[model][:variant]`.
        match after.find(':') {
            Some(idx) => (Some(after[..idx].to_owned()), &after[idx..]),
            None => (Some(after.to_owned()), ""),
        }
    } else {
        (None, raw)
    };

    let (model, variant) = split_variant(rest);
    ParsedModelField {
        preset,
        model,
        variant,
    }
}

/// Splits `rest` on the LAST `:` to extract a variant suffix.
///
/// Returns `(Some(model), Some(variant))` when a colon is present,
/// `(Some(model), None)` for plain `model`, or `(None, None)` for an empty
/// `rest` (the `@preset` case with no further qualification).
fn split_variant(rest: &str) -> (Option<String>, Option<String>) {
    if rest.is_empty() {
        return (None, None);
    }
    match rest.rsplit_once(':') {
        Some((before, after)) => {
            let model = if before.is_empty() {
                None
            } else {
                Some(before.to_owned())
            };
            let variant = if after.is_empty() {
                None
            } else {
                Some(after.to_owned())
            };
            (model, variant)
        }
        None => (Some(rest.to_owned()), None),
    }
}

// ── Resolution ──────────────────────────────────────────────────────

/// Resolution output: the effective model name to route to, plus the
/// body-level preset overrides and merged routing preferences.
#[derive(Debug, Clone, Default)]
pub struct ResolvedPreset {
    /// The model name to feed into the routing table after preset/variant
    /// peeling — i.e. the original `request.model` minus any `@preset`
    /// prefix and `:variant` suffix, replaced by `preset.model` when the
    /// caller invoked a preset by name only.
    pub model: String,
    /// Body-level overrides — `None` when the request invoked no preset
    /// or the preset had no body fields set.
    pub preset: Option<AppliedPreset>,
    /// Routing preferences merged from preset and variant. Empty `RoutingPrefs`
    /// means "no opinion — let the model's strategy decide".
    pub routing: RoutingPrefs,
}

/// Resolves a raw model field into a `ResolvedPreset`.
///
/// Returns `Err` only when the preset name is unknown. Unknown variant names
/// pass through unchanged (the suffix is glued back onto the model string).
pub fn resolve(
    raw: &str,
    presets: &HashMap<String, PresetConfig>,
    variants: &HashMap<String, VariantConfig>,
) -> Result<ResolvedPreset, bitrouter_core::errors::BitrouterError> {
    let parsed = parse_model_field(raw);

    let preset_cfg: Option<&PresetConfig> = match &parsed.preset {
        Some(name) => match presets.get(name) {
            Some(p) => Some(p),
            None => {
                return Err(bitrouter_core::errors::BitrouterError::invalid_request(
                    None,
                    format!("unknown preset '@{name}'"),
                    None,
                ));
            }
        },
        None => None,
    };

    // Variant lookup is passthrough on miss.
    let (variant_cfg, variant_recognized): (Option<&VariantConfig>, bool) = match &parsed.variant {
        Some(name) => match variants.get(name) {
            Some(v) => (Some(v), true),
            None => (None, false),
        },
        None => (None, false),
    };

    // Compose the model name to hand back to routing.
    //
    // - If a preset is invoked, prefer the preset's `model` field; fall back
    //   to its name (so `@careful` can be its own routing key).
    // - Otherwise, use the parsed model name as-is.
    // - If the variant was unrecognized, re-attach it to the name so the
    //   routing table sees the original string (passthrough).
    let mut model_name = match (&parsed.preset, preset_cfg, &parsed.model) {
        (Some(name), Some(p), _) => p.model.clone().unwrap_or_else(|| name.clone()),
        (None, _, Some(m)) => m.clone(),
        (None, _, None) => String::new(),
        (Some(name), None, _) => name.clone(), // unreachable: handled above
    };
    if !variant_recognized && let Some(suffix) = &parsed.variant {
        model_name.push(':');
        model_name.push_str(suffix);
    }

    // Build the AppliedPreset bundle for body application.
    let preset = preset_cfg.and_then(|p| {
        let params = &p.params;
        let bundle = AppliedPreset {
            system: p.system_prompt.clone(),
            temperature: params.temperature,
            top_p: params.top_p,
            top_k: params.top_k,
            max_tokens: params.max_tokens,
            stop_sequences: params.stop.clone(),
            presence_penalty: params.presence_penalty,
            frequency_penalty: params.frequency_penalty,
            reasoning_effort: params.reasoning_effort,
        };
        if bundle.is_empty() {
            None
        } else {
            Some(bundle)
        }
    });

    // Merge routing prefs: preset is base, variant overrides on overlapping fields.
    let mut routing = preset_cfg.map(|p| p.routing.clone()).unwrap_or_default();
    if let Some(v) = variant_cfg {
        merge_routing_prefs(&mut routing, &v.routing);
    }

    Ok(ResolvedPreset {
        model: model_name,
        preset,
        routing,
    })
}

/// Merges `overlay` on top of `base`. Scalar fields use overlay-when-set;
/// list fields are replaced wholesale when overlay is non-empty (the issue
/// spec says arrays replace, not concat).
fn merge_routing_prefs(base: &mut RoutingPrefs, overlay: &RoutingPrefs) {
    if overlay.sort.is_some() {
        base.sort = overlay.sort;
    }
    if !overlay.require_tags.is_empty() {
        base.require_tags = overlay.require_tags.clone();
    }
    if !overlay.only.is_empty() {
        base.only = overlay.only.clone();
    }
    if !overlay.ignore.is_empty() {
        base.ignore = overlay.ignore.clone();
    }
}

// ── Validation ──────────────────────────────────────────────────────

/// Validates preset/variant names and tag tokens, and asserts that each
/// `variants:` entry only sets routing fields. Called from
/// [`BitrouterConfig::load_from_str`] after deserialisation.
pub(crate) fn validate(config: &BitrouterConfig) -> Result<(), ConfigError> {
    for name in config.presets.keys() {
        validate_name(name).map_err(|msg| {
            ConfigError::ConfigParse(format!("invalid preset name '{name}': {msg}"))
        })?;
    }
    for name in config.variants.keys() {
        validate_name(name).map_err(|msg| {
            ConfigError::ConfigParse(format!("invalid variant name '{name}': {msg}"))
        })?;
    }
    for (name, preset) in &config.presets {
        for tag in &preset.routing.require_tags {
            validate_tag(tag).map_err(|msg| {
                ConfigError::ConfigParse(format!(
                    "preset '{name}' references invalid tag '{tag}': {msg}"
                ))
            })?;
        }
    }
    for (name, variant) in &config.variants {
        for tag in &variant.routing.require_tags {
            validate_tag(tag).map_err(|msg| {
                ConfigError::ConfigParse(format!(
                    "variant '{name}' references invalid tag '{tag}': {msg}"
                ))
            })?;
        }
    }
    for model in config.models.values() {
        for ep in &model.endpoints {
            for tag in &ep.tags {
                validate_tag(tag).map_err(|msg| {
                    ConfigError::ConfigParse(format!("invalid endpoint tag '{tag}': {msg}"))
                })?;
            }
        }
    }
    Ok(())
}

/// Preset/variant names: `^[a-z0-9][a-z0-9_-]{0,63}$`.
fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.len() > 64 {
        return Err("must be at most 64 characters");
    }
    let mut chars = name.chars();
    let first = chars.next().ok_or("must not be empty")?;
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err("first character must be lowercase ASCII letter or digit");
    }
    for c in chars {
        if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '_' && c != '-' {
            return Err("characters must be [a-z0-9_-]");
        }
    }
    Ok(())
}

/// Tag tokens: `^[a-z][a-z0-9_-]{0,31}$`.
fn validate_tag(tag: &str) -> Result<(), &'static str> {
    if tag.len() > 32 {
        return Err("must be at most 32 characters");
    }
    let mut chars = tag.chars();
    let first = chars.next().ok_or("must not be empty")?;
    if !first.is_ascii_lowercase() {
        return Err("first character must be lowercase ASCII letter");
    }
    for c in chars {
        if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '_' && c != '-' {
            return Err("characters must be [a-z0-9_-]");
        }
    }
    Ok(())
}

// ── Built-in variants ───────────────────────────────────────────────

/// Built-in variant definitions merged via `inherit_defaults`.
///
/// v0 ships `:price` only — `:throughput` and `:latency` are deferred
/// until BitRouter has a metric source (see issue #449).
pub fn builtin_variants() -> HashMap<String, VariantConfig> {
    let mut m = HashMap::new();
    m.insert(
        "price".to_owned(),
        VariantConfig {
            routing: RoutingPrefs {
                sort: Some(SortKey::Price),
                ..Default::default()
            },
        },
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bare_model() {
        let p = parse_model_field("gpt-5");
        assert_eq!(p.preset, None);
        assert_eq!(p.model.as_deref(), Some("gpt-5"));
        assert_eq!(p.variant, None);
    }

    #[test]
    fn parse_preset_only() {
        let p = parse_model_field("@careful");
        assert_eq!(p.preset.as_deref(), Some("careful"));
        assert_eq!(p.model, None);
        assert_eq!(p.variant, None);
    }

    #[test]
    fn parse_model_with_variant() {
        let p = parse_model_field("gpt-5:price");
        assert_eq!(p.preset, None);
        assert_eq!(p.model.as_deref(), Some("gpt-5"));
        assert_eq!(p.variant.as_deref(), Some("price"));
    }

    #[test]
    fn parse_preset_with_variant() {
        let p = parse_model_field("@careful:price");
        assert_eq!(p.preset.as_deref(), Some("careful"));
        assert_eq!(p.model, None);
        assert_eq!(p.variant.as_deref(), Some("price"));
    }

    #[test]
    fn parse_provider_direct_with_variant() {
        // "openai:gpt-5" is the provider-direct route. With a trailing
        // ":price", the last `:` is the variant boundary, so the model
        // becomes "openai:gpt-5". The routing table's direct-route check
        // (provider name in providers map) still kicks in downstream.
        let p = parse_model_field("openai:gpt-5:price");
        assert_eq!(p.preset, None);
        assert_eq!(p.model.as_deref(), Some("openai:gpt-5"));
        assert_eq!(p.variant.as_deref(), Some("price"));
    }

    #[test]
    fn resolve_unknown_preset_errors() {
        let presets = HashMap::new();
        let variants = HashMap::new();
        let err = resolve("@nope", &presets, &variants).unwrap_err();
        assert!(matches!(
            err,
            bitrouter_core::errors::BitrouterError::InvalidRequest { .. }
        ));
    }

    #[test]
    fn resolve_unknown_variant_passes_through() {
        let presets = HashMap::new();
        let variants = HashMap::new();
        let r = resolve("gpt-5:hf-revision-1.2", &presets, &variants).unwrap();
        // Unknown variant suffix stays on the model name so the routing
        // table sees the original string.
        assert_eq!(r.model, "gpt-5:hf-revision-1.2");
        assert!(r.preset.is_none());
        assert!(r.routing.is_empty());
    }

    #[test]
    fn resolve_preset_applies_body_overrides() {
        let mut presets = HashMap::new();
        presets.insert(
            "careful".to_owned(),
            PresetConfig {
                model: Some("gpt-5".to_owned()),
                system_prompt: Some("Reason carefully.".to_owned()),
                params: crate::config::GenerationParams {
                    temperature: Some(0.2),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let variants = HashMap::new();
        let r = resolve("@careful", &presets, &variants).unwrap();
        assert_eq!(r.model, "gpt-5");
        let p = r.preset.expect("preset bundle");
        assert_eq!(p.temperature, Some(0.2));
        assert_eq!(p.system.as_deref(), Some("Reason carefully."));
    }

    #[test]
    fn resolve_propagates_reasoning_effort_into_bundle() {
        use bitrouter_core::models::language::call_options::ReasoningEffort;

        let mut presets = HashMap::new();
        presets.insert(
            "deep".to_owned(),
            PresetConfig {
                model: Some("gpt-5".to_owned()),
                params: crate::config::GenerationParams {
                    reasoning_effort: Some(ReasoningEffort::High),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let variants = HashMap::new();
        let r = resolve("@deep", &presets, &variants).unwrap();
        let bundle = r.preset.expect("preset bundle");
        assert_eq!(bundle.reasoning_effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn resolve_variant_overrides_preset_routing() {
        let mut presets = HashMap::new();
        presets.insert(
            "careful".to_owned(),
            PresetConfig {
                model: Some("gpt-5".to_owned()),
                routing: RoutingPrefs {
                    sort: None,
                    require_tags: vec!["paid".to_owned()],
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let mut variants = HashMap::new();
        variants.insert(
            "price".to_owned(),
            VariantConfig {
                routing: RoutingPrefs {
                    sort: Some(SortKey::Price),
                    ..Default::default()
                },
            },
        );
        let r = resolve("@careful:price", &presets, &variants).unwrap();
        assert_eq!(r.routing.sort, Some(SortKey::Price));
        assert_eq!(r.routing.require_tags, vec!["paid".to_owned()]);
    }

    #[test]
    fn validate_rejects_bad_preset_name() {
        let mut cfg = BitrouterConfig::default();
        cfg.presets
            .insert("Has Spaces".to_owned(), PresetConfig::default());
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, ConfigError::ConfigParse(_)));
    }

    #[test]
    fn validate_rejects_bad_tag() {
        let mut cfg = BitrouterConfig::default();
        cfg.variants.insert(
            "v".to_owned(),
            VariantConfig {
                routing: RoutingPrefs {
                    require_tags: vec!["Has Spaces".to_owned()],
                    ..Default::default()
                },
            },
        );
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, ConfigError::ConfigParse(_)));
    }

    #[test]
    fn builtin_variants_includes_price() {
        let v = builtin_variants();
        assert!(v.contains_key("price"));
        assert_eq!(v["price"].routing.sort, Some(SortKey::Price));
    }
}
