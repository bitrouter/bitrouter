//! [`GuardrailsPlugin`] — a [`Plugin`] convenience package that wires the
//! guardrail hooks onto the `language_model` pipeline in one call, for both the
//! OSS binary and any downstream host.
//!
//! - [`GuardrailsPlugin::with_static`] — a fixed, process-global rule set. It
//!   installs a [`DepositRulesHook`] (which inserts the shared rule set into
//!   every request's extensions) ahead of the two guardrail hooks.
//! - [`GuardrailsPlugin::dynamic`] — no built-in rules. It installs only the
//!   two guardrail hooks; the host resolves a per-request (e.g. per-account)
//!   [`RuleSet`] in an earlier pre-request stage and deposits it via
//!   [`PipelineContext::insert_extension`](bitrouter_sdk::language_model::PipelineContext::insert_extension).
//!   With nothing deposited, the hooks no-op.

use std::sync::Arc;

use bitrouter_sdk::{AppBuilder, Plugin, PluginId};

use crate::hooks::{DepositRulesHook, GuardrailPreHook, GuardrailStreamHook};
use crate::rules::RuleSet;

/// A [`Plugin`] that registers the upstream + downstream guardrail hooks.
pub struct GuardrailsPlugin {
    id: PluginId,
    static_rules: Option<Arc<RuleSet>>,
}

impl GuardrailsPlugin {
    /// Build a plugin over a fixed, process-global rule set.
    pub fn with_static(rules: RuleSet) -> Self {
        Self {
            id: PluginId::new("bitrouter-guardrails"),
            static_rules: Some(Arc::new(rules)),
        }
    }

    /// Build a plugin with no built-in rules, for hosts that resolve and
    /// deposit a per-request [`RuleSet`] themselves.
    pub fn dynamic() -> Self {
        Self {
            id: PluginId::new("bitrouter-guardrails"),
            static_rules: None,
        }
    }
}

impl Plugin for GuardrailsPlugin {
    fn id(&self) -> &PluginId {
        &self.id
    }

    fn install(&self, app: &mut AppBuilder) {
        let lm = app.language_model_builder();
        if let Some(rules) = &self.static_rules {
            // Runs ahead of the guardrail hooks (registration order), depositing
            // the shared rule set the two hooks then read.
            lm.pre_request_hook(DepositRulesHook::new(rules.clone()));
        }
        lm.pre_request_hook(GuardrailPreHook::new());
        lm.stream_hook(GuardrailStreamHook::new());
    }
}
