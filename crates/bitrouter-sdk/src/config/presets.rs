//! Stage-0 model-name resolution: stripping `@preset` / `:variant` and deriving
//! the clean model name + `RoutingPrefs` + prompt body overrides.
//!
//! A request `model` string composes as `[@preset]base[:variant]`:
//! - `@careful` — a preset (its `model:` supplies the base);
//! - `gpt-5:free` — a base model + a variant;
//! - `@careful:free` — both.
//!
//! Disambiguation from the `provider:model` Strategy-1 form: a trailing
//! `:segment` is treated as a variant **only if it names a known variant** —
//! `openai:gpt-5` is left intact for Strategy 1.
//!
//! Validation: an unknown `@preset` is a hard 400; an unknown `:variant` is a
//! passthrough (not stripped).

use std::collections::HashMap;

use crate::config::{PresetConfig, RoutingConfig, VariantConfig};
use crate::error::{BitrouterError, Result};
use crate::language_model::routing::RoutingPrefs;

// `PromptOverrides` is defined in `language_model::routing` because it is the
// return type of [`crate::language_model::RoutingTable::preset_overrides`],
// which must stay available without the `config_file` feature. Re-exported
// here so callers reading the config still find it under its old path.
pub use crate::language_model::routing::PromptOverrides;

/// The result of Stage-0 resolution.
#[derive(Debug, Clone)]
pub struct PresetResolution {
    /// The clean model name, with `@preset` / `:variant` stripped and any
    /// preset `model:` substitution applied. Fed to Strategy 1/2/3.
    pub clean_model: String,
    /// Routing preferences distilled from the preset and/or variant.
    pub prefs: RoutingPrefs,
    /// Prompt body overrides from the preset.
    pub overrides: PromptOverrides,
}

fn apply_routing(prefs: &mut RoutingPrefs, routing: &RoutingConfig) {
    if let Some(sort) = routing.sort {
        prefs.sort = sort;
    }
    for tag in &routing.require_tags {
        if !prefs.require_tags.contains(tag) {
            prefs.require_tags.push(tag.clone());
        }
    }
    for p in &routing.only {
        if !prefs.only.contains(p) {
            prefs.only.push(p.clone());
        }
    }
    for p in &routing.ignore {
        if !prefs.ignore.contains(p) {
            prefs.ignore.push(p.clone());
        }
    }
}

/// Resolve a raw request `model` string into its clean model name, routing
/// preferences and prompt overrides.
pub fn resolve_presets(
    raw_model: &str,
    presets: &HashMap<String, PresetConfig>,
    variants: &HashMap<String, VariantConfig>,
) -> Result<PresetResolution> {
    // 1. Split off a trailing `:variant` — but only if it is a *known* variant.
    let (head, variant_name) = match raw_model.rsplit_once(':') {
        Some((left, right)) if variants.contains_key(right) => (left, Some(right)),
        _ => (raw_model, None),
    };

    // 2. A leading `@` marks the head as a preset reference.
    let (preset_name, base_from_head) = match head.strip_prefix('@') {
        Some(name) => (Some(name), None),
        None => (None, Some(head)),
    };

    // 3. An unknown preset is a hard error: 400.
    let preset: Option<&PresetConfig> = match preset_name {
        Some(name) => Some(
            presets
                .get(name)
                .ok_or_else(|| BitrouterError::bad_request(format!("unknown preset '@{name}'")))?,
        ),
        None => None,
    };

    // 4. The clean model: a preset's `model:` wins, else the literal base.
    let clean_model = preset
        .and_then(|p| p.model.clone())
        .or_else(|| base_from_head.map(|s| s.to_string()))
        .ok_or_else(|| {
            BitrouterError::bad_request(format!(
                "preset '{}' defines no model: and the request gave none",
                preset_name.unwrap_or_default()
            ))
        })?;
    if clean_model.is_empty() {
        return Err(BitrouterError::bad_request("empty model name"));
    }

    // 5. Routing prefs: preset first, then variant refines.
    let mut prefs = RoutingPrefs::default();
    if let Some(p) = preset {
        apply_routing(&mut prefs, &p.routing);
    }
    if let Some(name) = variant_name {
        apply_routing(&mut prefs, &variants[name].routing);
    }

    // 6. Prompt overrides — preset only.
    let overrides = preset
        .map(|p| PromptOverrides {
            system_prompt: p.system_prompt.clone(),
            params: p.params.clone(),
        })
        .unwrap_or_default();

    Ok(PresetResolution {
        clean_model,
        prefs,
        overrides,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RoutingConfig;
    use crate::language_model::routing::SortOrder;

    fn presets() -> HashMap<String, PresetConfig> {
        let mut m = HashMap::new();
        m.insert(
            "careful".to_string(),
            PresetConfig {
                model: Some("gpt-5".to_string()),
                system_prompt: Some("Reason carefully.".to_string()),
                params: serde_json::Map::new(),
                routing: RoutingConfig {
                    sort: Some(SortOrder::Latency),
                    require_tags: vec!["paid".to_string()],
                    ..Default::default()
                },
            },
        );
        m
    }

    fn variants() -> HashMap<String, VariantConfig> {
        let mut m = HashMap::new();
        m.insert(
            "free".to_string(),
            VariantConfig {
                routing: RoutingConfig {
                    require_tags: vec!["free".to_string()],
                    ..Default::default()
                },
            },
        );
        m
    }

    #[test]
    fn bare_model_passes_through() {
        let r = resolve_presets("gpt-5", &presets(), &variants()).unwrap();
        assert_eq!(r.clean_model, "gpt-5");
        assert!(r.prefs.require_tags.is_empty());
        assert!(r.overrides.is_empty());
    }

    #[test]
    fn preset_supplies_model_and_prefs_and_overrides() {
        let r = resolve_presets("@careful", &presets(), &variants()).unwrap();
        assert_eq!(r.clean_model, "gpt-5");
        assert_eq!(r.prefs.sort, SortOrder::Latency);
        assert_eq!(r.prefs.require_tags, vec!["paid"]);
        assert_eq!(
            r.overrides.system_prompt.as_deref(),
            Some("Reason carefully.")
        );
    }

    #[test]
    fn variant_refines_routing() {
        let r = resolve_presets("gpt-5:free", &presets(), &variants()).unwrap();
        assert_eq!(r.clean_model, "gpt-5");
        assert_eq!(r.prefs.require_tags, vec!["free"]);
    }

    #[test]
    fn preset_and_variant_compose() {
        let r = resolve_presets("@careful:free", &presets(), &variants()).unwrap();
        assert_eq!(r.clean_model, "gpt-5");
        // preset's `paid` + variant's `free`
        assert_eq!(r.prefs.require_tags, vec!["paid", "free"]);
    }

    #[test]
    fn unknown_preset_is_400() {
        let err = resolve_presets("@nonexistent", &presets(), &variants()).unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn unknown_variant_passes_through_untouched() {
        // `gpt-5:turbo` — `turbo` is not a known variant, so it is NOT stripped.
        let r = resolve_presets("gpt-5:turbo", &presets(), &variants()).unwrap();
        assert_eq!(r.clean_model, "gpt-5:turbo");
    }

    #[test]
    fn provider_prefix_is_not_mistaken_for_a_variant() {
        // `openai:gpt-5` — `gpt-5` is not a known variant; the whole string is
        // left intact for Strategy-1 provider-prefix routing.
        let r = resolve_presets("openai:gpt-5", &presets(), &variants()).unwrap();
        assert_eq!(r.clean_model, "openai:gpt-5");
    }
}
