//! [`GuardrailsPlugin`] — legacy [`Plugin`] assembly for trusted custom hosts.
//!
//! The default OSS binary no longer installs this package. New router-bound
//! input checks register [`crate::checker::callback`] through
//! `bitrouter::extension::ExtensionApi`; they do not require the `sdk` feature.
//! This compatibility API retains its existing global/per-request rule deposits
//! and stream block/redact behavior. Input-only request checks cannot replace
//! those output or global protection guarantees and do not add request-check
//! receipts to these hooks.
//!
//! The current alpha SDK API retains this path. Removal requires an explicitly
//! announced breaking SDK release with migration notes; no removal date is
//! scheduled. Keep a compatible custom host when those semantics are required.
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

/// Legacy custom-host package for upstream and downstream guardrail hooks.
///
/// Retained for the current alpha SDK API; removal requires an explicitly
/// announced breaking SDK release with migration notes. Unlike new router-bound
/// request-check registration, installation activates global hooks immediately.
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
