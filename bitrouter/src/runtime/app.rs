use std::path::{Path, PathBuf};
use std::sync::Arc;

use bitrouter_config::BitrouterConfig;
use sea_orm::DatabaseConnection;

use crate::runtime::{error::Result, paths::RuntimePaths, server::ServerTableBound};

pub struct AppRuntime<R> {
    pub config: BitrouterConfig,
    pub paths: RuntimePaths,
    pub routing_table: R,
    pub db: Option<Arc<DatabaseConnection>>,
}

impl<R: ServerTableBound + Send + Sync + 'static> AppRuntime<R> {
    pub fn status(&self) -> RuntimeStatus {
        let daemon_pid = crate::runtime::daemon::DaemonManager::new(self.paths.clone())
            .is_running()
            .ok()
            .flatten();
        RuntimeStatus {
            home_dir: self.paths.home_dir.clone(),
            config_file: self.paths.config_file.clone(),
            runtime_dir: self.paths.runtime_dir.clone(),
            listen_addr: self.config.server.listen,
            providers: self.config.providers.keys().cloned().collect(),
            models: self.config.models.keys().cloned().collect(),
            daemon_pid,
        }
    }

    pub async fn start(&self) -> Result<()> {
        let dm = crate::runtime::daemon::DaemonManager::new(self.paths.clone());
        let pid = dm.start().await?;
        println!("bitrouter daemon started (pid {pid})");
        Ok(())
    }

    pub async fn stop(&self) -> Result<()> {
        let dm = crate::runtime::daemon::DaemonManager::new(self.paths.clone());
        dm.stop().await?;
        println!("bitrouter daemon stopped");
        Ok(())
    }

    pub async fn restart(&self) -> Result<()> {
        let dm = crate::runtime::daemon::DaemonManager::new(self.paths.clone());
        let pid = dm.restart().await?;
        println!("bitrouter daemon restarted (pid {pid})");
        Ok(())
    }

    pub fn reload(&self) -> Result<()> {
        let dm = crate::runtime::daemon::DaemonManager::new(self.paths.clone());
        dm.reload()?;
        println!("bitrouter configuration reload signal sent");
        Ok(())
    }
}

/// Convenience constructors using `DynamicRoutingTable<ConfigRoutingTable>`.
impl
    AppRuntime<
        bitrouter_core::routers::dynamic::DynamicRoutingTable<bitrouter_config::ConfigRoutingTable>,
    >
{
    /// Load config from resolved paths. The `.env` file (if it exists) is loaded
    /// automatically from `paths.env_file`.
    pub fn load(paths: RuntimePaths) -> Result<Self> {
        let env_file = paths.env_file.exists().then_some(paths.env_file.as_path());
        let config = BitrouterConfig::load_from_file(&paths.config_file, env_file)?;
        let config_table = bitrouter_config::ConfigRoutingTable::new(
            config.providers.clone(),
            config.models.clone(),
        );
        let routing_table =
            bitrouter_core::routers::dynamic::DynamicRoutingTable::new(config_table);
        Ok(Self {
            config,
            paths,
            routing_table,
            db: None,
        })
    }

    /// Build a runtime with default config (no file on disk).
    ///
    /// Loads builtin providers with env_prefix resolution so that
    /// environment-provided API keys (e.g. `OPENAI_API_KEY`) are
    /// automatically picked up even without a config file.
    pub fn scaffold(paths: RuntimePaths) -> Self {
        let env_file = paths.env_file.exists().then_some(paths.env_file.as_path());
        let config = BitrouterConfig::load_from_str("{}", env_file).unwrap_or_default();
        let config_table = bitrouter_config::ConfigRoutingTable::new(
            config.providers.clone(),
            config.models.clone(),
        );
        let routing_table =
            bitrouter_core::routers::dynamic::DynamicRoutingTable::new(config_table);
        Self {
            config,
            paths,
            routing_table,
            db: None,
        }
    }

    /// Start the server with configuration hot-reload enabled.
    ///
    /// When the server receives a reload signal (SIGHUP on Unix, flag file on
    /// Windows), it re-reads the configuration file from disk and replaces the
    /// inner routing and tool tables without dropping in-flight requests or
    /// dynamic routes.
    pub async fn serve_with_reload<M>(self, model_router: M) -> Result<()>
    where
        M: bitrouter_core::routers::router::LanguageModelRouter + Send + Sync + 'static,
    {
        use bitrouter_core::routers::reload::ReloadableRoutingTable;

        let paths = self.paths.clone();
        let table = Arc::new(self.routing_table);

        // Build config-authoritative tool registry.
        let tool_table = bitrouter_config::ConfigToolRoutingTable::new(
            self.config.providers.clone(),
            self.config.tools.clone(),
        );
        let (initial_filters, initial_restrictions) = {
            let mut filters = std::collections::HashMap::new();
            let mut restrictions = std::collections::HashMap::new();
            for (name, provider) in &self.config.providers {
                if let Some(ref f) = provider.tool_filter {
                    filters.insert(name.clone(), f.clone());
                }
                if let Some(ref r) = provider.param_restrictions
                    && !r.rules.is_empty()
                {
                    restrictions.insert(name.clone(), r.clone());
                }
            }
            (filters, restrictions)
        };
        let tool_registry = Arc::new(
            bitrouter_core::routers::dynamic_tool::DynamicToolRegistry::new(
                tool_table,
                initial_filters,
                initial_restrictions,
            ),
        );

        // Build the reload callback — captures both routing table and tool
        // registry so it can re-read config and swap both inner tables.
        let reload_table = Arc::clone(&table);
        let reload_tool_registry = Arc::clone(&tool_registry);
        let reload_paths = paths.clone();
        let reload_fn = move || {
            let env_file = reload_paths
                .env_file
                .exists()
                .then_some(reload_paths.env_file.as_path());
            let config = BitrouterConfig::load_from_file(&reload_paths.config_file, env_file)
                .map_err(|e| e.to_string())?;

            // Reload model routing table.
            let new_table = bitrouter_config::ConfigRoutingTable::new(
                config.providers.clone(),
                config.models.clone(),
            );
            reload_table.reload(new_table).map_err(|e| e.to_string())?;

            // Reload tool routing table.
            let new_tool_table = bitrouter_config::ConfigToolRoutingTable::new(
                config.providers.clone(),
                config.tools.clone(),
            );
            reload_tool_registry
                .reload(new_tool_table)
                .map_err(|e| e.to_string())?;

            tracing::info!("model and tool routing tables reloaded");
            Ok(())
        };

        let mut plan =
            crate::runtime::server::ServerPlan::new(self.config, table, Arc::new(model_router))
                .with_paths(paths)
                .with_tool_registry(tool_registry)
                .with_reload(reload_fn);

        if let Some(db) = self.db {
            plan = plan.with_db(db);
        }
        plan.serve().await
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeStatus {
    pub home_dir: PathBuf,
    pub config_file: PathBuf,
    pub runtime_dir: PathBuf,
    pub listen_addr: std::net::SocketAddr,
    pub providers: Vec<String>,
    pub models: Vec<String>,
    /// PID of the running daemon, or `None` if no daemon is active.
    pub daemon_pid: Option<u32>,
}

/// Resolve the database URL.
///
/// Priority (highest wins):
/// 1. Explicit `--db` CLI argument
/// 2. `BITROUTER_DATABASE_URL` environment variable (system env + `.env` file)
/// 3. `database.url` from configuration file (already env-substituted)
/// 4. Default: `sqlite://<home_dir>/bitrouter.db?mode=rwc`
pub fn resolve_database_url(
    cli_url: Option<&str>,
    config: &BitrouterConfig,
    home_dir: &Path,
    env_file: Option<&Path>,
) -> String {
    // 1. CLI argument
    if let Some(url) = cli_url {
        return url.to_owned();
    }

    // 2. Environment variable (.env + system env)
    let env = bitrouter_config::env::load_env(env_file);
    if let Some(url) = env.get("BITROUTER_DATABASE_URL")
        && !url.is_empty()
    {
        return url.clone();
    }

    // 3. Configuration file
    if let Some(url) = &config.database.url
        && !url.is_empty()
    {
        return url.clone();
    }

    // 4. Default: sqlite at BITROUTER_HOME
    let db_path = home_dir.join("bitrouter.db");
    format!("sqlite://{}?mode=rwc", db_path.display())
}
