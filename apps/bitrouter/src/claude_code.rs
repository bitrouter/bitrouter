//! App-layer routing glue for the Claude Code **subscription** provider
//! (`claude-code`).
//!
//! Two daemon-side responsibilities, both keyed on the `claude-code` provider
//! id and the Claude Code agent-profile beta:
//!
//! - [`ClaudeCodeRouter`] — an ingress [`PromptTransform`] that detects genuine
//!   Claude Code traffic by its `anthropic-beta: claude-code-…` header and
//!   routes it to the subscription provider by provider-prefixing the model
//!   (`claude-code:<model>`).
//! - [`enable_if_logged_in`] — auto-add the `claude-code` provider to the
//!   in-memory `providers:` map when the OAuth credential store holds a
//!   `claude-code` credential (mirrors [`crate::cloud::enable_in_zero_config`]).
//!
//! These live in the app layer (not `bitrouter-providers`) because the routing
//! decision needs the ingress request (the parsed [`Prompt`] plus the inbound
//! headers), which only exists above the SDK ingress seam, and because the
//! enable step mutates the assembled [`Config`].

use bitrouter_providers::oauth::credential_store::{Credential, CredentialStore, DEFAULT_LABEL};
use bitrouter_sdk::HeaderMap;
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
/// Genuine Claude Code traffic carries the Claude Code agent-profile beta —
/// `anthropic-beta: claude-code-…` — the same marker the Pro/Max subscription
/// endpoint keys on. It's sent identically by the CLI, the Agent SDK, and
/// `bitrouter spawn`, and is stable across releases (unlike the
/// version-dependent system-prompt text the older detection relied on). Such a
/// request also targets a bare Claude model id (e.g. `claude-opus-4-8`).
///
/// When both hold, the transform rewrites `prompt.model` to
/// `claude-code:<model>`, which sends the request to the subscription provider
/// via the explicit-provider route. Everything else is left untouched:
/// non-Claude-Code traffic, non-Claude models, and already-prefixed models all
/// route wherever they already pointed (the pay-as-you-go `anthropic` provider
/// for bare Claude models, or the explicit provider).
///
/// The transform **only reads** the marker — it never adds it. The subscription
/// applier separately *requires* the beta to be present and refuses to
/// fabricate it, so this transform cannot be used to spoof arbitrary traffic as
/// Claude Code.
pub struct ClaudeCodeRouter;

impl PromptTransform for ClaudeCodeRouter {
    fn apply(&self, _prompt: &mut Prompt) {
        // Detection needs the inbound `anthropic-beta` header, so all the work
        // is in `apply_with_headers` (which the HTTP server always calls). With
        // no headers there is nothing to decide, so this is a no-op.
    }

    fn apply_with_headers(&self, prompt: &mut Prompt, headers: &HeaderMap) {
        // Genuine Claude Code carries the agent-profile beta
        // (`anthropic-beta: claude-code-…`) — the same marker the subscription
        // endpoint keys on, stable across Claude Code's CLI / Agent-SDK /
        // `bitrouter spawn` shapes (unlike the version-dependent system prompt
        // text). When it's present and the request targets a bare `claude-*`
        // model, route to the subscription provider by prefixing the model.
        // Non-Claude-Code traffic is left untouched (it falls to the
        // pay-as-you-go `anthropic` provider). The transform only READS the
        // marker — it never adds it, so it can't be used to spoof.
        let is_cc = headers_indicate_claude_code(headers);
        if is_cc && prompt.model.starts_with("claude") && !prompt.model.starts_with("claude-code:")
        {
            prompt.model = format!("claude-code:{}", prompt.model);
        }
    }
}

/// Whether the inbound headers carry the Claude Code agent-profile beta. The
/// `anthropic-beta` header is a comma-joined list; any token whose name starts
/// with `claude-code` counts, so the match is stable across the dated suffix.
fn headers_indicate_claude_code(headers: &HeaderMap) -> bool {
    headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.split(',').any(|b| b.trim().starts_with("claude-code")))
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
    // Adopt any legacy #590 subscription marker (stored under `anthropic`)
    // before reading the store, so a running daemon picks up the move to
    // `claude-code` on a serve / reload config-build pass.
    migrate_legacy_anthropic_marker_default();
    let Ok(store) = CredentialStore::default_path() else {
        return;
    };
    enable_if_logged_in_with_store(config, &store);
}

/// Provider id of the legacy platform / pay-as-you-go Anthropic provider. A
/// pre-split (#590) user signed in to their Claude subscription stored the
/// [`Credential::ClaudeCodeCli`] marker here; the migration moves it to
/// [`PROVIDER_ID`].
const LEGACY_PROVIDER_ID: &str = "anthropic";

/// Move a pre-split (#590) Claude subscription marker from the legacy
/// `anthropic` provider to the dedicated `claude-code` provider, using the
/// default credential-store path. Best-effort: an unresolvable / unreadable
/// store is a silent no-op. See [`migrate_legacy_anthropic_marker`].
pub fn migrate_legacy_anthropic_marker_default() {
    let Ok(store) = CredentialStore::default_path() else {
        return;
    };
    let _ = migrate_legacy_anthropic_marker(store.path().to_path_buf());
}

/// Move a pre-split (#590) Claude subscription marker from the legacy
/// `anthropic` provider to the dedicated `claude-code` provider.
///
/// #590 stored the [`Credential::ClaudeCodeCli`] subscription marker under
/// `anthropic`. Now that `anthropic` is platform / pay-as-you-go (`x-api-key`)
/// and the subscription is its own `claude-code` provider, an existing user
/// would otherwise be stranded. For every label under `anthropic` whose
/// credential is the `ClaudeCodeCli` marker, this re-keys it to `claude-code`
/// (set under `claude-code`, removed from `anthropic`).
///
/// Strictly scoped to the marker:
/// - A pasted [`Credential::ApiKey`] (or any OAuth credential) under `anthropic`
///   is **left untouched** — that is `anthropic`'s own platform key.
/// - If `claude-code` already holds a credential for that label, the `anthropic`
///   marker is left as-is (no clobber).
///
/// Best-effort: an unreadable store is a silent no-op. Takes the store path so
/// callers / tests can inject it.
pub fn migrate_legacy_anthropic_marker(store_path: std::path::PathBuf) -> bool {
    let Ok(mut store) = CredentialStore::load(store_path) else {
        return false;
    };
    // Snapshot the labels first — the move mutates the store as it goes.
    let labels: Vec<String> = store
        .labels(LEGACY_PROVIDER_ID)
        .into_iter()
        .map(str::to_string)
        .collect();
    let mut moved_any = false;
    for label in labels {
        // Only the tokenless subscription marker migrates; an `ApiKey` or any
        // other credential is anthropic's platform credential and stays put.
        if !matches!(
            store.get_any(LEGACY_PROVIDER_ID, &label),
            Some(Credential::ClaudeCodeCli)
        ) {
            continue;
        }
        // Don't clobber an existing `claude-code` credential for this label.
        if store.get_any(PROVIDER_ID, &label).is_some() {
            continue;
        }
        if store
            .set(PROVIDER_ID, &label, Credential::ClaudeCodeCli)
            .is_err()
        {
            // A failed write leaves the legacy marker in place; try again next
            // time rather than removing the only copy.
            continue;
        }
        let _ = store.remove(LEGACY_PROVIDER_ID, &label);
        moved_any = true;
    }
    moved_any
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
    use bitrouter_providers::oauth::credential_store::Credential;
    use bitrouter_sdk::language_model::types::{GenerationParams, ProviderMetadata};

    /// A representative `anthropic-beta` value as genuine Claude Code sends it:
    /// the agent-profile beta first, then feature betas.
    const CC_BETA: &str = "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07";

    fn prompt(model: &str) -> Prompt {
        Prompt {
            model: model.to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    /// Drive the router with an optional inbound `anthropic-beta` header value,
    /// returning the (possibly rewritten) model.
    fn route(model: &str, anthropic_beta: Option<&str>) -> String {
        let mut p = prompt(model);
        let mut headers = HeaderMap::new();
        if let Some(beta) = anthropic_beta {
            headers.insert("anthropic-beta", beta.parse().unwrap());
        }
        ClaudeCodeRouter.apply_with_headers(&mut p, &headers);
        p.model
    }

    #[test]
    fn claude_code_beta_and_bare_claude_model_is_prefixed() {
        assert_eq!(
            route("claude-opus-4-8", Some(CC_BETA)),
            "claude-code:claude-opus-4-8"
        );
    }

    #[test]
    fn claude_code_beta_and_non_claude_model_is_untouched() {
        assert_eq!(route("gpt-5", Some(CC_BETA)), "gpt-5");
    }

    #[test]
    fn no_claude_code_beta_leaves_claude_model_for_payg() {
        // The key guarantee: non-Claude-Code traffic for a Claude model is NOT
        // routed to the subscription — it falls to the pay-as-you-go provider.
        assert_eq!(route("claude-opus-4-8", None), "claude-opus-4-8");
        // A beta header that lacks the claude-code agent profile also doesn't route.
        assert_eq!(
            route(
                "claude-opus-4-8",
                Some("oauth-2025-04-20,context-1m-2025-08-07")
            ),
            "claude-opus-4-8"
        );
    }

    #[test]
    fn already_prefixed_claude_model_is_untouched() {
        assert_eq!(
            route("claude-code:claude-opus-4-8", Some(CC_BETA)),
            "claude-code:claude-opus-4-8"
        );
    }

    #[test]
    fn idempotent_on_already_claude_code_prefixed_model() {
        // Running the transform twice must not double-prefix.
        let mut p = prompt("claude-opus-4-8");
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-beta", CC_BETA.parse().unwrap());
        ClaudeCodeRouter.apply_with_headers(&mut p, &headers);
        ClaudeCodeRouter.apply_with_headers(&mut p, &headers);
        assert_eq!(p.model, "claude-code:claude-opus-4-8");
    }

    #[test]
    fn apply_without_headers_is_noop() {
        // The header-less path can't detect Claude Code, so it must not route.
        let mut p = prompt("claude-opus-4-8");
        ClaudeCodeRouter.apply(&mut p);
        assert_eq!(p.model, "claude-opus-4-8");
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
    fn migration_moves_marker_from_anthropic_to_claude_code() {
        let mut store = fresh_tmp_store();
        let path = store.path().to_path_buf();
        // A pre-split (#590) user: the subscription marker lives under
        // `anthropic`, and `claude-code` has nothing yet.
        store
            .set(LEGACY_PROVIDER_ID, DEFAULT_LABEL, Credential::ClaudeCodeCli)
            .unwrap();
        drop(store);

        assert!(migrate_legacy_anthropic_marker(path.clone()));

        let reloaded = CredentialStore::load(&path).unwrap();
        assert!(
            matches!(
                reloaded.get_any(PROVIDER_ID, DEFAULT_LABEL),
                Some(Credential::ClaudeCodeCli)
            ),
            "the marker must now live under `claude-code`"
        );
        assert!(
            reloaded
                .get_any(LEGACY_PROVIDER_ID, DEFAULT_LABEL)
                .is_none(),
            "the legacy `anthropic` marker must be removed"
        );
    }

    #[test]
    fn migration_leaves_anthropic_api_key_untouched() {
        let mut store = fresh_tmp_store();
        let path = store.path().to_path_buf();
        // An `anthropic` platform key must stay put — it is not a subscription
        // marker.
        store
            .set(
                LEGACY_PROVIDER_ID,
                DEFAULT_LABEL,
                Credential::api_key("sk-ant-api03-platform"),
            )
            .unwrap();
        drop(store);

        assert!(!migrate_legacy_anthropic_marker(path.clone()));

        let reloaded = CredentialStore::load(&path).unwrap();
        assert_eq!(
            reloaded
                .get_any(LEGACY_PROVIDER_ID, DEFAULT_LABEL)
                .and_then(Credential::as_api_key),
            Some("sk-ant-api03-platform"),
            "the anthropic platform API key must be left in place"
        );
        assert!(
            reloaded.get_any(PROVIDER_ID, DEFAULT_LABEL).is_none(),
            "no claude-code credential should be created for an API key"
        );
    }

    #[test]
    fn migration_does_not_clobber_existing_claude_code_credential() {
        let mut store = fresh_tmp_store();
        let path = store.path().to_path_buf();
        // Both providers already hold a marker for this label (e.g. the user
        // already logged in to `claude-code`). The legacy entry must be left
        // as-is rather than overwriting the existing `claude-code` credential.
        store
            .set(LEGACY_PROVIDER_ID, DEFAULT_LABEL, Credential::ClaudeCodeCli)
            .unwrap();
        store
            .set(
                PROVIDER_ID,
                DEFAULT_LABEL,
                Credential::api_key("sk-ant-oat-existing"),
            )
            .unwrap();
        drop(store);

        assert!(!migrate_legacy_anthropic_marker(path.clone()));

        let reloaded = CredentialStore::load(&path).unwrap();
        assert!(
            matches!(
                reloaded.get_any(LEGACY_PROVIDER_ID, DEFAULT_LABEL),
                Some(Credential::ClaudeCodeCli)
            ),
            "the anthropic marker must be left as-is when claude-code already has a credential"
        );
        assert_eq!(
            reloaded
                .get_any(PROVIDER_ID, DEFAULT_LABEL)
                .and_then(Credential::as_api_key),
            Some("sk-ant-oat-existing"),
            "the pre-existing claude-code credential must not be clobbered"
        );
    }

    #[test]
    fn migration_noop_on_unreadable_store() {
        // A path that cannot be loaded as a store (it's a directory) is a
        // silent no-op, never a panic.
        let dir =
            std::env::temp_dir().join(format!("bitrouter-cc-migrate-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!migrate_legacy_anthropic_marker(dir.clone()));
        let _ = std::fs::remove_dir_all(&dir);
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
