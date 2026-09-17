//! Stage-0 model-name resolution: stripping `bitrouter/<slug>` / `@preset` /
//! `:variant` and deriving the clean model name + `RoutingPrefs` + prompt body
//! overrides.
//!
//! A request `model` string composes as `[bitrouter/slug|@preset]base[:variant]`:
//! - `bitrouter/auto` — the public spelling of a BitRouter-owned routing model;
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
//!
//! # The reserved `bitrouter/` namespace
//!
//! `bitrouter/<slug>` follows the `vendor/auto` convention the gateway
//! ecosystem already uses, so a caller can swap one segment of an existing
//! `.../auto` model id and keep the rest of their config. The vendor segment
//! names the *router being addressed*, not the token destination — a
//! `bitrouter/auto` request is still fulfilled by whichever upstream provider
//! the bound policy selects.
//!
//! Because the segment names BitRouter itself, this resolver claims the **whole
//! prefix**: an unrecognised slug is a 400 here rather than a 404 from a
//! provider lookup further down the pipeline. The registry is held to the other
//! side of that bargain — `dist-helper registry validate` rejects any catalog
//! model id under `bitrouter/`, so the namespace can never be shadowed by a
//! provider's physical model.

use std::collections::HashMap;

use crate::config::checker::CheckerConfig;
use crate::config::router::{EffectiveRouterDefinition, RouterConfig};
use crate::config::{PresetConfig, RoutingConfig, VariantConfig};
use crate::error::{BitrouterError, Result};
use crate::language_model::request_checks::RequestCheckBinding;
use crate::language_model::routing::{RouterRequestIdentity, RoutingPrefs};

// `PromptOverrides` is defined in `language_model::routing` because it is the
// return type of [`crate::language_model::RoutingTable::preset_overrides`],
// which must stay available without the `config_file` feature. Re-exported
// here so callers reading the config still find it under its old path.
pub use crate::language_model::routing::PromptOverrides;

/// The reserved BitRouter model namespace — the `bitrouter/` in
/// `bitrouter/auto`. Everything under it is resolved by BitRouter itself and
/// never reaches a provider lookup.
pub const RESERVED_NAMESPACE: &str = "bitrouter/";

/// The public model slug for policy-driven automatic routing.
pub const AUTO_SLUG: &str = "bitrouter/auto";

/// Whether `model` addresses the reserved BitRouter namespace. Callers that
/// classify a model string — "is this already an explicit route?", "is this a
/// concrete upstream model id?" — must treat the namespace like `@preset`: it
/// names a BitRouter routing feature, not something a provider can serve.
pub fn is_reserved(model: &str) -> bool {
    model.starts_with(RESERVED_NAMESPACE)
}

/// What a slug under the reserved namespace addresses.
enum ReservedSlug {
    /// Resolved here, through the named preset's model and policy binding.
    Preset(&'static str),
    /// Reserved, but rewritten by an ingress transform before Stage 0 runs —
    /// so reaching this resolver means the feature is unconfigured.
    IngressAlias { requires: &'static str },
}

/// Classify a slug under the reserved namespace. `None` is an unknown slug.
fn reserved_slug(slug: &str) -> Option<ReservedSlug> {
    match slug {
        "auto" => Some(ReservedSlug::Preset("auto")),
        "fusion" => Some(ReservedSlug::IngressAlias {
            requires: "server_tools.fusion",
        }),
        _ => None,
    }
}

/// The preset a reserved slug addresses, or a 400 naming what went wrong.
fn reserved_preset(slug: &str) -> Result<&'static str> {
    match reserved_slug(slug) {
        Some(ReservedSlug::Preset(preset)) => Ok(preset),
        Some(ReservedSlug::IngressAlias { requires }) => Err(BitrouterError::bad_request(format!(
            "'{RESERVED_NAMESPACE}{slug}' requires the `{requires}` config section"
        ))),
        None => Err(BitrouterError::bad_request(format!(
            "unknown BitRouter model '{RESERVED_NAMESPACE}{slug}'"
        ))),
    }
}

fn missing_reserved_binding(preset: &str) -> BitrouterError {
    BitrouterError::bad_request(format!(
        "'{RESERVED_NAMESPACE}{preset}' needs a preset named '{preset}' bound to a routing \
         policy; run `{cli} policy init {preset} --preset {preset} --economy provider:model`",
        cli = crate::invocation::name()
    ))
}

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
    /// App-owned policy bound to the selected preset. Bare models and presets
    /// without a binding return `None`.
    pub policy: Option<String>,
    /// Known variant selected by the caller, preserved for app-owned policy
    /// selection independently of routing preferences.
    pub variant: Option<String>,
    /// Stable router identity for canonical and legacy named addresses.
    pub router: Option<RouterRequestIdentity>,
    /// Ordered request-check bindings frozen with this router resolution.
    pub request_checks: Vec<RequestCheckBinding>,
}

#[derive(Clone, Copy)]
enum LocatedRouter<'a> {
    Configured(&'a RouterConfig),
    Legacy(&'a PresetConfig),
}

impl<'a> LocatedRouter<'a> {
    fn effective(self) -> EffectiveRouterDefinition<'a> {
        match self {
            Self::Configured(router) => EffectiveRouterDefinition::from_router(router),
            Self::Legacy(preset) => EffectiveRouterDefinition::from_legacy_preset(preset),
        }
    }
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
    resolve_routers(
        raw_model,
        &HashMap::new(),
        presets,
        variants,
        &HashMap::new(),
    )
}

pub(super) fn resolve_routers(
    raw_model: &str,
    routers: &HashMap<String, RouterConfig>,
    presets: &HashMap<String, PresetConfig>,
    variants: &HashMap<String, VariantConfig>,
    checkers: &HashMap<String, CheckerConfig>,
) -> Result<PresetResolution> {
    // Reject a reserved colon spelling before variant parsing. Otherwise a
    // configured variant with the same name (for example `auto`) could consume
    // the suffix and disguise `bitrouter:auto` as the bare model `bitrouter`.
    if let Some(slug) = raw_model.strip_prefix("bitrouter:")
        && reserved_slug(slug).is_some()
    {
        return Err(BitrouterError::bad_request(format!(
            "'{raw_model}' is not a provider route; use '{RESERVED_NAMESPACE}{slug}'"
        )));
    }

    // Canonical named routers do not accept variants in this batch. `auto`
    // keeps its existing compatibility behavior, while `@name:variant`
    // remains available for migrated callers.
    if let Some(slug) = raw_model.strip_prefix(RESERVED_NAMESPACE)
        && let Some((router_id, _)) = slug.rsplit_once(':')
        && router_id != "auto"
        && routers.contains_key(router_id)
    {
        return Err(BitrouterError::bad_request(format!(
            "router '{RESERVED_NAMESPACE}{router_id}' does not support ':variant'; use '@{router_id}:variant' during the compatibility window"
        )));
    }

    // 1. Split off a trailing `:variant` — but only if it is a *known* variant.
    let (head, variant_name) = match raw_model.rsplit_once(':') {
        Some((left, right)) if variants.contains_key(right) => (left, Some(right)),
        _ => (raw_model, None),
    };

    // 2. A leading `@` marks the head as a preset reference; the reserved
    //    `bitrouter/` namespace is the public spelling of the same thing. The
    //    `bitrouter:` colon form is a near-miss worth catching here — it would
    //    otherwise reach Strategy 1 and be dispatched to the BitRouter Cloud
    //    provider as an upstream model id it does not serve.
    let (router_name, base_from_head, canonical) = match head.strip_prefix('@') {
        Some(name) => (Some(name), None, false),
        None => match head.strip_prefix(RESERVED_NAMESPACE) {
            Some("fusion") => (Some(reserved_preset("fusion")?), None, true),
            Some(slug) if slug == "auto" || routers.contains_key(slug) => (Some(slug), None, true),
            Some(slug) => {
                return Err(BitrouterError::bad_request(format!(
                    "unknown BitRouter model '{RESERVED_NAMESPACE}{slug}'"
                )));
            }
            None => match head.strip_prefix("bitrouter:") {
                Some(slug) if reserved_slug(slug).is_some() => {
                    return Err(BitrouterError::bad_request(format!(
                        "'{head}' is not a provider route; use '{RESERVED_NAMESPACE}{slug}'"
                    )));
                }
                _ => (None, Some(head), false),
            },
        },
    };

    // 3. An unknown preset is a hard error: 400. A reserved slug resolves to a
    //    preset the operator has to have configured, so its miss reports the
    //    missing binding rather than an unknown name the caller never typed.
    let router: Option<EffectiveRouterDefinition<'_>> = match router_name {
        Some(name) => {
            let located = routers
                .get(name)
                .map(LocatedRouter::Configured)
                .or_else(|| presets.get(name).map(LocatedRouter::Legacy))
                .ok_or_else(|| {
                    if canonical && name == "auto" {
                        missing_reserved_binding(name)
                    } else {
                        BitrouterError::bad_request(format!("unknown preset '@{name}'"))
                    }
                })?;
            let router = located.effective();
            if canonical && name == "auto" && router.policy().is_none() {
                return Err(missing_reserved_binding(name));
            }
            if canonical && !routers.contains_key(name) && name != "auto" {
                return Err(BitrouterError::bad_request(format!(
                    "unknown BitRouter model '{RESERVED_NAMESPACE}{name}'"
                )));
            }
            Some(router)
        }
        None => None,
    };

    let router_identity = match (router_name, router) {
        (Some(name), Some(router)) => Some(RouterRequestIdentity {
            router_id: name.to_owned(),
            original_selector: raw_model.to_owned(),
            binding_digest: router.binding_digest(name, checkers)?,
        }),
        _ => None,
    };
    let request_checks = match (router_name, router) {
        (Some(name), Some(router)) => router.request_checks(name, checkers)?,
        _ => Vec::new(),
    };

    // 4. The clean model: a router's model/base_model wins, else the literal base.
    let clean_model = router
        .as_ref()
        .and_then(EffectiveRouterDefinition::base_model)
        .map(ToOwned::to_owned)
        .or_else(|| base_from_head.map(|s| s.to_string()))
        .ok_or_else(|| {
            BitrouterError::bad_request(format!(
                "preset '{}' defines no model: and the request gave none",
                router_name.unwrap_or_default()
            ))
        })?;
    if clean_model.is_empty() {
        return Err(BitrouterError::bad_request("empty model name"));
    }

    // 5. Routing prefs: router first, then a legacy-address variant refines.
    let mut prefs = RoutingPrefs::default();
    if let Some(router) = &router {
        apply_routing(&mut prefs, router.routing);
    }
    if let Some(name) = variant_name {
        apply_routing(&mut prefs, &variants[name].routing);
    }

    // 6. Prompt overrides — named definitions only.
    let overrides = router
        .as_ref()
        .map(|router| router.defaults.to_prompt_overrides())
        .unwrap_or_default();

    Ok(PresetResolution {
        clean_model,
        prefs,
        overrides,
        policy: router.and_then(|router| router.policy().map(ToOwned::to_owned)),
        variant: variant_name.map(ToString::to_string),
        router: router_identity,
        request_checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RoutingConfig;
    use crate::config::router::{RouterDefaults, RouterSelection};
    use crate::language_model::routing::SortOrder;

    fn presets() -> HashMap<String, PresetConfig> {
        let mut m = HashMap::new();
        m.insert(
            "careful".to_string(),
            PresetConfig {
                model: Some("gpt-5".to_string()),
                policy: None,
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
        m.insert(
            "cost".to_string(),
            VariantConfig {
                routing: RoutingConfig {
                    sort: Some(SortOrder::Cost),
                    ..Default::default()
                },
            },
        );
        m
    }

    fn routers() -> HashMap<String, RouterConfig> {
        HashMap::from([
            (
                "project".to_string(),
                RouterConfig {
                    selection: RouterSelection::Model {
                        model: "gpt-5".into(),
                        routing: RoutingConfig {
                            sort: Some(SortOrder::Latency),
                            require_tags: vec!["paid".into()],
                            ..RoutingConfig::default()
                        },
                    },
                    defaults: RouterDefaults {
                        system_prompt: Some("Be exact.".into()),
                        params: serde_json::Map::new(),
                    },
                    checks: Default::default(),
                },
            ),
            (
                "auto".to_string(),
                RouterConfig {
                    selection: RouterSelection::Policy {
                        policy: "auto".into(),
                        base_model: "openai-codex:gpt-5.6-sol".into(),
                        routing: RoutingConfig::default(),
                    },
                    defaults: RouterDefaults::default(),
                    checks: Default::default(),
                },
            ),
        ])
    }

    #[test]
    fn bare_model_passes_through() {
        let r = resolve_presets("gpt-5", &presets(), &variants()).unwrap();
        assert_eq!(r.clean_model, "gpt-5");
        assert!(r.prefs.require_tags.is_empty());
        assert!(r.overrides.is_empty());
    }

    #[test]
    fn preset_supplies_model_and_prefs_and_overrides() -> Result<()> {
        let r = resolve_presets("@careful", &presets(), &variants())?;
        assert_eq!(r.clean_model, "gpt-5");
        assert_eq!(r.prefs.sort, SortOrder::Latency);
        assert_eq!(r.prefs.require_tags, vec!["paid"]);
        assert_eq!(
            r.overrides.system_prompt.as_deref(),
            Some("Reason carefully.")
        );
        let identity = r
            .router
            .as_ref()
            .ok_or_else(|| BitrouterError::internal("legacy preset identity was not recorded"))?;
        assert_eq!(identity.router_id, "careful");
        assert_eq!(identity.original_selector, "@careful");
        assert!(identity.binding_digest.starts_with("router-v1:sha256:"));
        Ok(())
    }

    #[test]
    fn canonical_router_resolves_without_a_same_named_preset() -> Result<()> {
        let resolved = resolve_routers(
            "bitrouter/project",
            &routers(),
            &presets(),
            &variants(),
            &HashMap::new(),
        )?;
        assert_eq!(resolved.clean_model, "gpt-5");
        assert_eq!(resolved.prefs.sort, SortOrder::Latency);
        assert_eq!(resolved.prefs.require_tags, vec!["paid"]);
        assert_eq!(
            resolved.overrides.system_prompt.as_deref(),
            Some("Be exact.")
        );
        let identity = resolved
            .router
            .ok_or_else(|| BitrouterError::internal("router identity was not recorded"))?;
        assert_eq!(identity.router_id, "project");
        assert_eq!(identity.original_selector, "bitrouter/project");
        Ok(())
    }

    #[test]
    fn legacy_router_address_keeps_known_variant_compatibility() -> Result<()> {
        let resolved = resolve_routers(
            "@project:free",
            &routers(),
            &presets(),
            &variants(),
            &HashMap::new(),
        )?;
        assert_eq!(resolved.clean_model, "gpt-5");
        assert_eq!(resolved.variant.as_deref(), Some("free"));
        assert_eq!(resolved.prefs.require_tags, vec!["paid", "free"]);
        Ok(())
    }

    #[test]
    fn canonical_router_rejects_variant_suffixes() -> Result<()> {
        for selector in ["bitrouter/project:free", "bitrouter/project:unknown"] {
            let error = resolve_routers(
                selector,
                &routers(),
                &presets(),
                &variants(),
                &HashMap::new(),
            )
            .err()
            .ok_or_else(|| BitrouterError::internal("canonical router variant was accepted"))?;
            assert!(error.to_string().contains("does not support ':variant'"));
        }
        Ok(())
    }

    #[test]
    fn canonical_auto_keeps_policy_and_variant_compatibility() -> Result<()> {
        let resolved = resolve_routers(
            "bitrouter/auto:cost",
            &routers(),
            &presets(),
            &variants(),
            &HashMap::new(),
        )?;
        assert_eq!(resolved.clean_model, "openai-codex:gpt-5.6-sol");
        assert_eq!(resolved.policy.as_deref(), Some("auto"));
        assert_eq!(resolved.variant.as_deref(), Some("cost"));
        Ok(())
    }

    #[test]
    fn canonical_auto_preserves_an_explicit_custom_policy_binding() -> Result<()> {
        let mut configured = routers();
        configured.insert(
            "auto".into(),
            RouterConfig {
                selection: RouterSelection::Policy {
                    policy: "custom-policy".into(),
                    base_model: "vendor:base".into(),
                    routing: RoutingConfig::default(),
                },
                defaults: RouterDefaults::default(),
                checks: Default::default(),
            },
        );

        for selector in ["bitrouter/auto", "bitrouter/auto:cost"] {
            let resolved = resolve_routers(
                selector,
                &configured,
                &presets(),
                &variants(),
                &HashMap::new(),
            )?;
            assert_eq!(resolved.clean_model, "vendor:base");
            assert_eq!(resolved.policy.as_deref(), Some("custom-policy"));
        }
        Ok(())
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
        assert_eq!(r.variant.as_deref(), Some("free"));
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

    /// `presets()` plus the `auto` preset the reserved `bitrouter/auto` slug
    /// resolves through.
    fn presets_with_auto() -> HashMap<String, PresetConfig> {
        let mut m = presets();
        m.insert(
            "auto".to_string(),
            PresetConfig {
                model: Some("openai-codex:gpt-5.6-sol".to_string()),
                policy: Some("auto".to_string()),
                system_prompt: None,
                params: serde_json::Map::new(),
                routing: RoutingConfig::default(),
            },
        );
        m
    }

    #[test]
    fn reserved_auto_slug_resolves_through_the_auto_preset() {
        let r = resolve_presets(AUTO_SLUG, &presets_with_auto(), &variants()).unwrap();
        assert_eq!(r.clean_model, "openai-codex:gpt-5.6-sol");
        assert_eq!(r.policy.as_deref(), Some("auto"));
    }

    #[test]
    fn reserved_auto_slug_composes_with_a_variant() {
        // The variant split runs before the namespace is stripped, so the
        // public slug keeps the `:cost` composition `@auto:cost` has.
        let r = resolve_presets("bitrouter/auto:cost", &presets_with_auto(), &variants()).unwrap();
        assert_eq!(r.clean_model, "openai-codex:gpt-5.6-sol");
        assert_eq!(r.policy.as_deref(), Some("auto"));
        assert_eq!(r.variant.as_deref(), Some("cost"));
    }

    #[test]
    fn at_preset_spelling_still_resolves_identically() {
        let public = resolve_presets(AUTO_SLUG, &presets_with_auto(), &variants()).unwrap();
        let legacy = resolve_presets("@auto", &presets_with_auto(), &variants()).unwrap();
        assert_eq!(public.clean_model, legacy.clean_model);
        assert_eq!(public.policy, legacy.policy);
    }

    #[test]
    fn reserved_auto_slug_without_a_bound_preset_names_the_setup_step() {
        // The drop-in substitution moment: a caller pastes the slug before
        // running setup. The 400 has to say what is missing, not 404 as an
        // unknown provider model.
        let err = resolve_presets(AUTO_SLUG, &presets(), &variants()).unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(
            err.to_string()
                .contains("bro policy init auto --preset auto --economy")
        );
    }

    #[test]
    fn reserved_auto_slug_rejects_an_existing_preset_without_a_policy_binding() {
        let mut unbound = presets();
        unbound.insert(
            "auto".to_string(),
            PresetConfig {
                model: Some("openai-codex:gpt-5.6-sol".to_string()),
                policy: None,
                system_prompt: None,
                params: serde_json::Map::new(),
                routing: RoutingConfig::default(),
            },
        );

        let result = resolve_presets(AUTO_SLUG, &unbound, &variants());
        assert_eq!(
            result.as_ref().map(|_| ()).map_err(|err| err.status()),
            Err(400)
        );
        assert!(result.as_ref().err().is_some_and(|err| {
            err.to_string()
                .contains("bro policy init auto --preset auto --economy")
        }));
    }

    #[test]
    fn unknown_reserved_slug_is_400_not_a_provider_lookup() {
        let err = resolve_presets("bitrouter/nonexistent", &presets(), &variants()).unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(err.to_string().contains("unknown BitRouter model"));
    }

    #[test]
    fn reserved_fusion_slug_reaching_stage_0_reports_it_is_unconfigured() {
        // The Fusion ingress alias rewrites this before Stage 0, so arriving
        // here means `server_tools.fusion` is absent.
        let err = resolve_presets("bitrouter/fusion", &presets(), &variants()).unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(err.to_string().contains("server_tools.fusion"));
    }

    #[test]
    fn reserved_slug_in_colon_form_points_at_the_slash_spelling() {
        // `bitrouter:auto` would otherwise reach Strategy 1 and be dispatched
        // to the BitRouter Cloud provider as a model it does not serve.
        let err = resolve_presets("bitrouter:auto", &presets_with_auto(), &variants()).unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(err.to_string().contains("bitrouter/auto"));
    }

    #[test]
    fn reserved_colon_form_cannot_be_shadowed_by_an_auto_variant() {
        let mut shadowing_variants = variants();
        shadowing_variants.insert(
            "auto".to_string(),
            VariantConfig {
                routing: RoutingConfig::default(),
            },
        );

        let result = resolve_presets("bitrouter:auto", &presets_with_auto(), &shadowing_variants);
        assert_eq!(
            result.as_ref().map(|_| ()).map_err(|err| err.status()),
            Err(400)
        );
        assert!(
            result
                .as_ref()
                .err()
                .is_some_and(|err| err.to_string().contains("bitrouter/auto"))
        );
    }

    #[test]
    fn ordinary_bitrouter_provider_routes_are_untouched() {
        // Reserving the namespace must not capture Strategy-1 routes to the
        // BitRouter Cloud provider's real catalog models.
        let r = resolve_presets(
            "bitrouter:anthropic/claude-opus-5",
            &presets_with_auto(),
            &variants(),
        )
        .unwrap();
        assert_eq!(r.clean_model, "bitrouter:anthropic/claude-opus-5");
        assert!(r.policy.is_none());
    }

    #[test]
    fn provider_prefix_is_not_mistaken_for_a_variant() {
        // `openai:gpt-5` — `gpt-5` is not a known variant; the whole string is
        // left intact for Strategy-1 provider-prefix routing.
        let r = resolve_presets("openai:gpt-5", &presets(), &variants()).unwrap();
        assert_eq!(r.clean_model, "openai:gpt-5");
    }
}
