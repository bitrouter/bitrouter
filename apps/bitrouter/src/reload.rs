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
    /// Concrete upstream HTTP executor. Timeout knobs are client-level, so a
    /// config reload must rebuild the live executor's client set too.
    upstream_executor: Arc<bitrouter_sdk::language_model::HttpExecutor>,
    policy_runtime: Option<Arc<crate::policy_lock::PolicyRuntime>>,
    /// Serializes the complete multi-subsystem reload transaction. The routing
    /// table's own lock is narrower and cannot protect policy prepare/commit.
    reload_lock: tokio::sync::Mutex<()>,
    source: ReloadSource,
}

impl AppReloader {
    /// Build a reloader over the daemon's reloadable subsystems.
    pub fn new(
        policy_store: Arc<PolicyStore>,
        routing_table: Arc<bitrouter_sdk::config::ConfigRoutingTable>,
        upstream_executor: Arc<bitrouter_sdk::language_model::HttpExecutor>,
        source: ReloadSource,
    ) -> Self {
        Self {
            policy_store,
            routing_table,
            upstream_executor,
            policy_runtime: None,
            reload_lock: tokio::sync::Mutex::new(()),
            source,
        }
    }

    /// Attach the app's named routing-policy registry to the same reload fanout.
    pub fn with_policy_runtime(mut self, runtime: Arc<crate::policy_lock::PolicyRuntime>) -> Self {
        self.policy_runtime = Some(runtime);
        self
    }

    async fn replace_routing_and_timeouts(
        &self,
        fresh: bitrouter_sdk::config::Config,
    ) -> bitrouter_sdk::Result<()> {
        let (global_timeouts, provider_timeouts) =
            crate::assemble::resolved_upstream_timeouts(&fresh);
        self.routing_table.replace_config(fresh).await?;
        self.upstream_executor
            .reload_provider_timeouts(global_timeouts, provider_timeouts)?;
        Ok(())
    }

    async fn replace_file_backed(
        &self,
        fresh: bitrouter_sdk::config::Config,
        path: &std::path::Path,
    ) -> anyhow::Result<()> {
        let prepared = match &self.policy_runtime {
            Some(runtime) => Some(runtime.prepare_for_config(&fresh, Some(path)).await?),
            None => None,
        };
        self.replace_routing_and_timeouts(fresh).await?;
        if let (Some(runtime), Some(prepared)) = (&self.policy_runtime, prepared) {
            runtime.commit(prepared);
        }
        Ok(())
    }

    async fn replace_default(&self, fresh: bitrouter_sdk::config::Config) -> anyhow::Result<()> {
        let prepared = match &self.policy_runtime {
            Some(runtime) => Some(runtime.prepare_for_config(&fresh, None).await?),
            None => None,
        };
        self.replace_routing_and_timeouts(fresh).await?;
        if let (Some(runtime), Some(prepared)) = (&self.policy_runtime, prepared) {
            runtime.commit(prepared);
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl DaemonReloader for AppReloader {
    async fn reload(&self) -> anyhow::Result<()> {
        let _reload_guard = self.reload_lock.lock().await;
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
                    // Auto-enable the `claude-code` subscription provider when a
                    // credential is in the OAuth store, before the registry merge
                    // fills its fields — so a sign-in survives a hot-reload.
                    crate::claude_code::enable_if_logged_in(&mut fresh);
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
                    self.replace_file_backed(fresh, path).await
                }
                Err(e) => Err(e.into()),
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
                // Auto-enable the `claude-code` subscription provider when a
                // credential is in the OAuth store, before the registry merge
                // fills its fields — so a sign-in survives a hot-reload.
                crate::claude_code::enable_if_logged_in(&mut fresh);
                // Merge the registry so the canonical catalog + every
                // credentialed BYOK provider is routable after a reload too.
                crate::assemble::merge_registry_into(&mut fresh).await;
                // Then re-activate any provider with a stored OAuth / subscription
                // credential (invisible to the registry's config/env credential
                // gate), after the merge re-applies builtin defaults.
                activate_stored_credential_providers(&mut fresh);
                self.replace_default(fresh).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::config::{self, ConfigRoutingTable};
    use bitrouter_sdk::language_model::{
        ApiProtocol, Executor, GenerationParams, HttpExecutor, HttpTimeouts, Message,
        PipelineContext, PipelineRequest, Prompt, Role, RoutingTarget,
    };
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn temp_config_path() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix("bitrouter-reload-")
            .tempdir()
            .expect("create temp config dir")
            .keep();
        (dir.join("bitrouter.yaml"), dir)
    }

    fn config_yaml(read_secs: u64) -> String {
        format!(
            r#"
inherit_defaults: false
upstream:
  timeouts:
    read_secs: {read_secs}
providers:
  slow:
    api_base: https://api.example.com/v1
    api_key: k
    api_protocol:
      - "*": chat_completions
    models:
      - {{ id: m }}
"#
        )
    }

    async fn stalled_json_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request_buf = [0_u8; 1024];
            let _ = socket.read(&mut request_buf).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      content-type: application/json\r\n\
                      content-length: 1024\r\n\
                      \r\n\
                      {\"id\":\"partial\"",
                )
                .await
                .expect("write partial response");
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        format!("http://{addr}/v1")
    }

    fn prompt() -> Prompt {
        Prompt {
            model: "m".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "hi")],
            tools: vec![],
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    #[tokio::test]
    async fn reload_updates_live_upstream_timeout_clients() {
        let (path, dir) = temp_config_path();
        std::fs::write(&path, config_yaml(30)).expect("write initial config");
        let initial = config::parse(&config_yaml(30)).expect("parse initial config");
        let routing_table = Arc::new(ConfigRoutingTable::from_config(initial));
        let executor = Arc::new(
            HttpExecutor::new(HttpTimeouts {
                read: Duration::from_secs(30),
                ..HttpTimeouts::default()
            })
            .expect("build executor"),
        );
        let reloader = AppReloader::new(
            Arc::new(PolicyStore::new()),
            routing_table,
            executor.clone(),
            ReloadSource::File(path.clone()),
        );

        std::fs::write(&path, config_yaml(1)).expect("write reloaded config");
        reloader.reload().await.expect("reload config");

        let api_base = stalled_json_server().await;
        let target = RoutingTarget {
            provider_name: "slow".into(),
            service_id: "m".into(),
            api_base,
            api_key: "k".into(),
            api_protocol: ApiProtocol::ChatCompletions,
            chat_token_limit_field: None,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
        };
        let prompt = prompt();
        let ctx = PipelineContext::new(PipelineRequest::new(
            "m",
            bitrouter_sdk::caller::CallerContext::local(),
            prompt.clone(),
        ));

        let err = tokio::time::timeout(
            Duration::from_secs(3),
            executor.execute(&target, &prompt, &ctx),
        )
        .await
        .expect("reloaded read_secs should bound the stalled body")
        .expect_err("stalled body should timeout");
        std::fs::remove_dir_all(dir).ok();

        match err {
            bitrouter_sdk::BitrouterError::UpstreamTimeout => {}
            other => panic!("expected UpstreamTimeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_policy_candidate_does_not_swap_routing_or_policy() {
        use crate::adequacy::settlement::PendingAdequacyStore;
        use crate::policy_lock::PolicyRuntime;
        use bitrouter_sdk::config::PolicyWriteback;

        let (path, dir) = temp_config_path();
        let config_yaml = |model: &str| {
            format!(
                r#"inherit_defaults: false
presets:
  coding:
    model: {model}
    policy: coding
"#
            )
        };
        std::fs::write(&path, config_yaml("vendor:old")).expect("write initial config");
        std::fs::write(
            dir.join("policy-lock.yaml"),
            r#"lockfileVersion: 1
policies:
  coding:
    key_strategy: legacy_fingerprint
    tiers: { strong: vendor:old }
    routes: {}
    default_tier: strong
    tool_use_tier: strong
    tool_safe_tiers: [strong]
"#,
        )
        .expect("write initial policy");
        let initial = config::load(&path).await.expect("load initial config");
        let routing_table = Arc::new(ConfigRoutingTable::from_config(initial.clone()));
        let db = crate::db::connect("sqlite::memory:")
            .await
            .expect("connect db");
        crate::db::run_migrations(&db).await.expect("migrate db");
        let runtime = PolicyRuntime::new(
            &initial,
            Some(&path),
            db,
            Arc::new(PendingAdequacyStore::default()),
            None,
        )
        .await
        .expect("build policy runtime");
        let initial_digest = runtime
            .status(PolicyWriteback::Locked)
            .digest
            .expect("initial digest");
        let executor = Arc::new(HttpExecutor::new(HttpTimeouts::default()).expect("executor"));
        let reloader = AppReloader::new(
            Arc::new(PolicyStore::new()),
            routing_table.clone(),
            executor,
            ReloadSource::File(path.clone()),
        )
        .with_policy_runtime(runtime.clone());

        std::fs::write(&path, config_yaml("vendor:new")).expect("write candidate config");
        std::fs::write(dir.join("policy-lock.yaml"), "lockfileVersion: broken\n")
            .expect("break candidate policy");

        assert!(reloader.reload().await.is_err());
        assert_eq!(
            routing_table.snapshot_config().presets["coding"]
                .model
                .as_deref(),
            Some("vendor:old")
        );
        assert_eq!(
            runtime.status(PolicyWriteback::Locked).digest.as_deref(),
            Some(initial_digest.as_str())
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn concurrent_reload_waits_for_the_full_transaction_lock() {
        let (path, dir) = temp_config_path();
        std::fs::write(&path, "inherit_defaults: false\n").expect("write config");
        let initial = config::load(&path).await.expect("load config");
        let routing_table = Arc::new(ConfigRoutingTable::from_config(initial));
        let executor = Arc::new(HttpExecutor::new(HttpTimeouts::default()).expect("executor"));
        let reloader = Arc::new(AppReloader::new(
            Arc::new(PolicyStore::new()),
            routing_table,
            executor,
            ReloadSource::File(path),
        ));
        let waiting = reloader.clone();
        let guard = reloader.reload_lock.lock().await;
        let mut task = tokio::spawn(async move { waiting.reload().await });

        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut task)
                .await
                .is_err(),
            "a second reload must wait until the first transaction releases"
        );

        task.abort();
        drop(guard);
        let _ = task.await;
        std::fs::remove_dir_all(dir).ok();
    }
}
