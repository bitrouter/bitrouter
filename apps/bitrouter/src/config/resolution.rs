//! Resolve named routers without preset or variant compatibility syntax.

use std::collections::HashMap;

use crate::config::{
    checker::CheckerConfig,
    router::{EffectiveRouterDefinition, RouterConfig},
};
use crate::gateway::{
    error::{BitrouterError, Result},
    pipeline::{
        request_checks::RequestCheckBinding,
        routing::{PromptOverrides, RoutingPrefs},
    },
};
use bitrouter_ai::routing::RouterRequestIdentity;

/// Namespace reserved for named application routers.
pub const RESERVED_NAMESPACE: &str = "bitrouter/";
/// Default model-policy router selector.
pub const AUTO_SLUG: &str = "bitrouter/auto";

/// Whether a selector addresses the application's router namespace.
pub fn is_reserved(model: &str) -> bool {
    model.starts_with(RESERVED_NAMESPACE)
}

/// One resolved named router or unchanged upstream selector.
#[derive(Debug, Clone)]
pub struct RouterResolution {
    /// Upstream model after explicit router selection.
    pub clean_model: String,
    /// Routing constraints supplied by the named router.
    pub prefs: RoutingPrefs,
    /// Request defaults supplied by the named router.
    pub overrides: PromptOverrides,
    /// Exact selected policy name, without implicit suffix interpretation.
    pub policy: Option<String>,

    /// Immutable identity of the selected router binding.
    pub router: Option<RouterRequestIdentity>,
    /// Ordered entry checks frozen with the selected router.
    pub request_checks: Vec<RequestCheckBinding>,
}

pub(super) fn resolve_routers(
    raw_model: &str,
    routers: &HashMap<String, RouterConfig>,
    checkers: &HashMap<String, CheckerConfig>,
) -> Result<RouterResolution> {
    if raw_model.starts_with('@')
        || matches!(
            raw_model.strip_prefix("bitrouter:"),
            Some("auto" | "fusion")
        )
    {
        return Err(BitrouterError::bad_request(
            "unsupported router selector; use bitrouter/<id>",
        ));
    }
    let Some(id) = raw_model.strip_prefix(RESERVED_NAMESPACE) else {
        return Ok(RouterResolution {
            clean_model: raw_model.to_owned(),
            prefs: RoutingPrefs::default(),
            overrides: PromptOverrides::default(),
            policy: None,
            router: None,
            request_checks: Vec::new(),
        });
    };
    let definition = routers.get(id).ok_or_else(|| {
        BitrouterError::bad_request(format!("unknown BitRouter router '{raw_model}'"))
    })?;
    let router = EffectiveRouterDefinition::from_router(definition);
    let clean_model = router
        .base_model()
        .ok_or_else(|| BitrouterError::bad_request("router model is missing"))?
        .to_owned();
    let prefs = RoutingPrefs {
        sort: router.routing.sort.unwrap_or_default(),
        require_tags: router.routing.require_tags.clone(),
        only: router.routing.only.clone(),
        ignore: router.routing.ignore.clone(),
        ..RoutingPrefs::default()
    };
    Ok(RouterResolution {
        clean_model,
        prefs,
        overrides: router.defaults.to_prompt_overrides(),
        policy: router.policy().map(ToOwned::to_owned),

        router: Some(RouterRequestIdentity {
            router_id: id.to_owned(),
            original_selector: raw_model.to_owned(),
            binding_digest: router.binding_digest(id, checkers)?,
        }),
        request_checks: router.request_checks(id, checkers)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::config;

    #[test]
    fn named_policy_binding_is_literal_and_upstream_colons_are_unchanged() -> anyhow::Result<()> {
        let config = config::parse(
            "inherit_defaults: false\nrouters:\n  cost:\n    selection:\n      kind: policy\n      policy: 'auto:cost'\n      base_model: 'provider:model'\n    defaults:\n      params: {temperature: 0.2}\n",
        )?;
        let resolved = config.resolve_router("bitrouter/cost")?;
        assert_eq!(resolved.policy.as_deref(), Some("auto:cost"));
        assert_eq!(resolved.clean_model, "provider:model");
        assert_eq!(
            resolved.overrides.params.get("temperature"),
            Some(&serde_json::json!(0.2))
        );
        for selector in ["provider:model", "provider:model:cost"] {
            assert_eq!(config.resolve_router(selector)?.clean_model, selector);
        }
        for selector in [
            "@cost",
            "@cost:cheap",
            "bitrouter/cost:cheap",
            "bitrouter/unknown",
            "bitrouter:auto",
        ] {
            assert!(config.resolve_router(selector).is_err(), "{selector}");
        }
        Ok(())
    }

    #[test]
    fn removed_router_configuration_is_rejected() {
        for source in [
            "presets: {}",
            "variants: {}",
            "policy: {mode: frozen}",
            "policy: {writeback: locked}",
        ] {
            assert!(config::parse(source).is_err(), "{source}");
        }
    }
}
