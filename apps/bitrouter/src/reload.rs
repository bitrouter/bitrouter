//! `AppReloader` — the daemon's config hot-reload fan-out.
//!
//! Kept in the lib (not `main.rs`) so the reload behaviour is testable
//! without spawning the binary — the same reason `commands.rs` lives here.
//!
//! Both reload paths build a fresh `Config` **in the app layer** and swap
//! it into the routing table via `ConfigRoutingTable::replace_config`.
//! Building the config here — above `bitrouter-providers` — is what lets
//! [`bitrouter_providers::apply_builtin_defaults`] fill the empty fields
//! of a built-in provider (`openai: {}`). The SDK's own
//! `RoutingTable::reload` sits *below* `bitrouter-providers` and so cannot
//! apply the catalog; routing through it on reload would leave a built-in
//! provider with an empty `api_base`, and an `auto_discover` provider
//! would then silently drop every model.

use std::path::PathBuf;
use std::sync::Arc;

use crate::daemon::DaemonReloader;
use crate::policy::PolicyStore;

/// Re-activate providers that have a credential in the OAuth store, loading the
/// default store. Best-effort: an unreadable store is a no-op. Mirrors the
/// startup pass in `assemble.rs` so a subscription / Claude Code session
/// provider survives a hot-reload instead of dropping out of routing.
fn activate_stored_credential_providers(config: &mut bitrouter_sdk::config::Config) {
    if let Ok(store) = bitrouter_providers::oauth::credential_store::CredentialStore::default_path()
    {
        bitrouter_providers::activate_stored_credential_providers(config, &store);
    }
}

/// Whether the daemon is running against a `bitrouter.yaml` on disk
/// (re-readable on reload) or a zero-config in-memory default
/// (rebuilt by re-running [`bitrouter_providers::zero_config`]).
pub enum ReloadSource {
    /// File-backed; the reloader re-reads the `bitrouter.yaml` at this
    /// path (re-substituting `${VAR}` references), re-applies the
    /// built-in provider catalog, and swaps the result into the
    /// routing table.
    File(PathBuf),
    /// In-memory zero-config; the reloader rebuilds the Config from
    /// scratch and hands it to the routing table via `replace_config`.
    Default,
}

/// Fan out a daemon `Reload` (and SIGHUP) to every reloadable subsystem the
/// running daemon owns. Failures from any single subsystem are accumulated and
/// reported together so an unrelated subsystem (e.g. a missing policy dir)
/// doesn't mask a fixable routing-table reload.
pub struct AppReloader {
    policy_store: Arc<PolicyStore>,
    /// Concrete handle on the routing table. Both reload paths build a
    /// fresh `Config` in the app layer — so `bitrouter_providers`'
    /// built-in catalog can be applied above the SDK — and swap it in
    /// via `ConfigRoutingTable::replace_config`.
    routing_table: Arc<bitrouter_sdk::config::ConfigRoutingTable>,
    source: ReloadSource,
}

impl AppReloader {
    /// Build a reloader over the daemon's reloadable subsystems.
    pub fn new(
        policy_store: Arc<PolicyStore>,
        routing_table: Arc<bitrouter_sdk::config::ConfigRoutingTable>,
        source: ReloadSource,
    ) -> Self {
        Self {
            policy_store,
            routing_table,
            source,
        }
    }
}

#[async_trait::async_trait]
impl DaemonReloader for AppReloader {
    async fn reload(&self) -> anyhow::Result<()> {
        let mut errors: Vec<String> = Vec::new();
        let routing_outcome = match &self.source {
            // File source: re-read the YAML (so `${VAR}` substitution
            // picks up any env override the CLI just installed), then
            // apply the built-in catalog. This is done in the app layer
            // — not via the SDK's `RoutingTable::reload`, which sits
            // below `bitrouter-providers` and so can't fill empty
            // built-in fields — so a provider like `openai: {}` keeps
            // its `api_base` / `api_protocol` / `api_key`. Without it an
            // `auto_discover` provider would reload with an empty
            // `api_base` and silently drop every model. `replace_config`
            // then runs `discover_models` against the populated config.
            ReloadSource::File(path) => match bitrouter_sdk::config::load(path).await {
                Ok(mut fresh) => {
                    bitrouter_providers::apply_builtin_defaults(&mut fresh);
                    // Re-merge the registry too, so a reload picks up newly-set
                    // credentials (a `bitrouter reload --env` that exports a
                    // provider key activates that provider's canonical models).
                    crate::assemble::merge_registry_into(&mut fresh).await;
                    // Then re-activate any provider that has an OAuth /
                    // subscription credential in the store — those live outside
                    // the config, so the registry's credential gate can't see
                    // them and the merge's `apply_builtin_defaults` would leave a
                    // keyless provider inactive. Runs after the merge.
                    activate_stored_credential_providers(&mut fresh);
                    self.routing_table.replace_config(fresh).await
                }
                Err(e) => Err(e),
            },
            // Default source: no file to re-read. Rebuild a fresh
            // zero-config `Config` (which goes through `env_lookup`
            // too), then apply built-in catalog defaults so every
            // newly-auto-enabled provider has its `api_base` /
            // `api_protocol` / `api_key` filled — `replace_config`
            // calls `discover_models`, which needs `api_base` to talk
            // to `/models`.
            ReloadSource::Default => {
                let mut fresh = bitrouter_providers::zero_config();
                // Layered on top of the env-var-driven auto-enable: a
                // signed-in user gets the `bitrouter` provider even when no
                // `$BITROUTER_API_KEY` is in the daemon's env-override map.
                crate::cloud::enable_in_zero_config(&mut fresh);
                bitrouter_providers::apply_builtin_defaults(&mut fresh);
                // Merge the registry so the canonical catalog + every
                // credentialed BYOK provider is routable after a reload too.
                crate::assemble::merge_registry_into(&mut fresh).await;
                // Then re-activate any provider with a stored OAuth / subscription
                // credential (invisible to the registry's config/env credential
                // gate), after the merge re-applies builtin defaults.
                activate_stored_credential_providers(&mut fresh);
                self.routing_table.replace_config(fresh).await
            }
        };
        if let Err(e) = routing_outcome {
            errors.push(format!("routing table: {e}"));
        }
        if let Err(e) = self.policy_store.reload().await {
            errors.push(format!("policy store: {e}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(errors.join("; ")))
        }
    }
}
