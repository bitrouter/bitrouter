//! App-layer routing glue for the Claude Code **subscription** provider
//! (`claude-code`).
//!
//! Two daemon-side responsibilities, both keyed on the `claude-code` provider
//! id and the Claude Code identity system prompt:
//!
//! - [`ClaudeCodeRouter`] — an ingress [`PromptTransform`] that detects genuine
//!   Claude Code traffic by its system prompt and routes it to the subscription
//!   provider by provider-prefixing the model (`claude-code:<model>`).
//! - [`enable_if_logged_in`] — auto-add the `claude-code` provider to the
//!   in-memory `providers:` map when the OAuth credential store holds a
//!   `claude-code` credential (mirrors [`crate::cloud::enable_in_zero_config`]).
//!
//! These live in the app layer (not `bitrouter-providers`) because the routing
//! decision reads the parsed canonical [`Prompt`](bitrouter_sdk::language_model::types::Prompt),
//! which only exists above the SDK ingress seam, and because the enable step
//! mutates the assembled [`Config`](bitrouter_sdk::config::Config).

use bitrouter_providers::oauth::credential_store::{CredentialStore, DEFAULT_LABEL};
use bitrouter_sdk::PromptTransform;
use bitrouter_sdk::config::{Config, ProviderConfig};
use bitrouter_sdk::language_model::types::Prompt;

/// Provider id of the Claude Pro/Max subscription provider. Must match the id
/// the [`bitrouter_providers::claude_code::ClaudeCodeAuthApplier`] is registered
/// under and the id used in the explicit-provider route prefix below.
const PROVIDER_ID: &str = "claude-code";

/// Ingress [`PromptTransform`] that routes genuine Claude Code traffic to the
/// Claude Pro/Max **subscription** provider.
///
/// Genuine Claude Code traffic carries the Claude Code identity as its first
/// system block: the SDK's Messages decoder flattens the inbound `system`
/// blocks into one `\n`-joined string (identity first), so the canonical
/// [`Prompt::system`] *starts with* [`CLAUDE_CODE_SYSTEM_PROMPT`]
/// (`bitrouter_providers::anthropic::headers::CLAUDE_CODE_SYSTEM_PROMPT`). Such
/// a request also targets a bare Claude model id (e.g.
/// `claude-sonnet-4-5-20250929`).
///
/// When both hold, the transform rewrites `prompt.model` to
/// `claude-code:<model>`, which sends the request to the subscription provider
/// via the explicit-provider route. Everything else is left untouched:
/// non-Claude-Code traffic, non-Claude models, and already-prefixed models all
/// route wherever they already pointed (the pay-as-you-go `anthropic` provider
/// for bare Claude models, or the explicit provider).
///
/// The transform **only reads** the identity — it never adds it. The
/// subscription applier separately *requires* the identity to be present and
/// refuses to fabricate it, so this transform cannot be used to spoof arbitrary
/// traffic as Claude Code.
///
/// [`CLAUDE_CODE_SYSTEM_PROMPT`]: bitrouter_providers::anthropic::headers::CLAUDE_CODE_SYSTEM_PROMPT
pub struct ClaudeCodeRouter;

impl PromptTransform for ClaudeCodeRouter {
    fn apply(&self, prompt: &mut Prompt) {
        let is_cc = prompt.system.as_deref().is_some_and(|s| {
            s.trim_start()
                .starts_with(bitrouter_providers::anthropic::headers::CLAUDE_CODE_SYSTEM_PROMPT)
        });
        if is_cc && prompt.model.starts_with("claude") && !prompt.model.starts_with("claude-code:")
        {
            prompt.model = format!("claude-code:{}", prompt.model);
        }
    }
}

/// Insert the `claude-code` provider into `config.providers` when the OAuth
/// credential store holds a `claude-code` credential (subscription marker or
/// stored OAuth token) and the entry is not already present.
///
/// The inserted entry is an empty [`ProviderConfig::default()`]; the registry
/// merge fills its `api_base` / `api_protocol` / auth from the fetched
/// `claude-code` registry entry, the
/// [`bitrouter_providers::claude_code::ClaudeCodeAuthApplier`] authenticates it,
/// and `claude-code`'s `access: local_oauth` keeps it active even though it
/// declares no canonical models.
///
/// Best-effort: a missing or unreadable store is a no-op — the user simply
/// hasn't signed in to their Claude subscription yet. Mirrors
/// [`crate::cloud::enable_in_zero_config`].
pub fn enable_if_logged_in(config: &mut Config) {
    let Ok(store) = CredentialStore::default_path() else {
        return;
    };
    enable_if_logged_in_with_store(config, &store);
}

/// Inner form taking the credential store explicitly so unit tests can drive
/// the logic without touching the user's real store.
fn enable_if_logged_in_with_store(config: &mut Config, store: &CredentialStore) {
    if config.providers.contains_key(PROVIDER_ID) {
        return;
    }
    let logged_in = !store.labels(PROVIDER_ID).is_empty()
        || store.get_any(PROVIDER_ID, DEFAULT_LABEL).is_some();
    if !logged_in {
        return;
    }
    config
        .providers
        .insert(PROVIDER_ID.to_string(), ProviderConfig::default());
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_providers::anthropic::headers::CLAUDE_CODE_SYSTEM_PROMPT;
    use bitrouter_providers::oauth::credential_store::Credential;
    use bitrouter_sdk::language_model::types::{GenerationParams, ProviderMetadata};

    fn prompt(model: &str, system: Option<&str>) -> Prompt {
        Prompt {
            model: model.to_string(),
            system: system.map(str::to_string),
            system_provider_metadata: ProviderMetadata::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn route(model: &str, system: Option<&str>) -> String {
        let mut p = prompt(model, system);
        ClaudeCodeRouter.apply(&mut p);
        p.model
    }

    #[test]
    fn cc_system_and_bare_claude_model_is_prefixed() {
        let system = format!("{CLAUDE_CODE_SYSTEM_PROMPT}\nbe terse");
        assert_eq!(
            route("claude-sonnet-4-5-20250929", Some(&system)),
            "claude-code:claude-sonnet-4-5-20250929"
        );
    }

    #[test]
    fn cc_system_and_non_claude_model_is_untouched() {
        let system = format!("{CLAUDE_CODE_SYSTEM_PROMPT}\nbe terse");
        assert_eq!(route("gpt-5", Some(&system)), "gpt-5");
    }

    #[test]
    fn non_cc_system_with_claude_model_is_untouched() {
        // No system prompt at all.
        assert_eq!(
            route("claude-sonnet-4-5-20250929", None),
            "claude-sonnet-4-5-20250929"
        );
        // A system prompt that is not the Claude Code identity.
        assert_eq!(
            route("claude-sonnet-4-5-20250929", Some("be terse")),
            "claude-sonnet-4-5-20250929"
        );
    }

    #[test]
    fn already_prefixed_claude_model_is_untouched() {
        let system = format!("{CLAUDE_CODE_SYSTEM_PROMPT}\nbe terse");
        assert_eq!(
            route("claude-code:claude-sonnet-4-5-20250929", Some(&system)),
            "claude-code:claude-sonnet-4-5-20250929"
        );
    }

    #[test]
    fn idempotent_on_already_claude_code_prefixed_model() {
        // Running the transform twice must not double-prefix.
        let system = format!("{CLAUDE_CODE_SYSTEM_PROMPT}\nbe terse");
        let mut p = prompt("claude-sonnet-4-5-20250929", Some(&system));
        ClaudeCodeRouter.apply(&mut p);
        ClaudeCodeRouter.apply(&mut p);
        assert_eq!(p.model, "claude-code:claude-sonnet-4-5-20250929");
    }

    fn fresh_tmp_store() -> CredentialStore {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-cc-router-test-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        CredentialStore::load(dir.join("creds.json")).unwrap()
    }

    #[test]
    fn enable_inserts_when_marker_present() {
        let mut store = fresh_tmp_store();
        store
            .set(PROVIDER_ID, DEFAULT_LABEL, Credential::ClaudeCodeCli)
            .unwrap();
        let mut config = Config::default();
        enable_if_logged_in_with_store(&mut config, &store);
        assert!(
            config.providers.contains_key(PROVIDER_ID),
            "`claude-code` provider should be auto-enabled when a credential is stored"
        );
    }

    #[test]
    fn enable_noop_when_no_credential() {
        let store = fresh_tmp_store();
        let mut config = Config::default();
        enable_if_logged_in_with_store(&mut config, &store);
        assert!(
            !config.providers.contains_key(PROVIDER_ID),
            "no credential → no provider inserted"
        );
    }

    #[test]
    fn enable_noop_when_already_present() {
        let mut store = fresh_tmp_store();
        store
            .set(PROVIDER_ID, DEFAULT_LABEL, Credential::ClaudeCodeCli)
            .unwrap();
        let mut config = Config::default();
        // Pre-populate with a sentinel `api_base` to prove the existing entry
        // is not overwritten.
        config.providers.insert(
            PROVIDER_ID.to_string(),
            ProviderConfig {
                api_base: "https://example.invalid".to_string(),
                ..ProviderConfig::default()
            },
        );
        enable_if_logged_in_with_store(&mut config, &store);
        assert_eq!(
            config.providers.get(PROVIDER_ID).unwrap().api_base,
            "https://example.invalid",
            "existing entry must not be overwritten"
        );
    }
}
