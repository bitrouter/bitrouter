use std::path::PathBuf;

use bitrouter_config::BitrouterConfig;
use bitrouter_core::routers::routing_table::RoutingTable;

use crate::runtime::{error::Result, paths::RuntimePaths};

pub struct AppRuntime<R> {
    pub config: BitrouterConfig,
    pub paths: RuntimePaths,
    pub routing_table: R,
}

impl<R: RoutingTable + Send + Sync + 'static> AppRuntime<R> {
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

    pub async fn serve<M>(self, model_router: M) -> Result<()>
    where
        M: bitrouter_core::routers::model_router::LanguageModelRouter + Send + Sync + 'static,
    {
        use crate::runtime::server::ServerPlan;
        use std::sync::Arc;
        ServerPlan::new(
            self.config,
            Arc::new(self.routing_table),
            Arc::new(model_router),
        )
        .serve()
        .await
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
}

/// Convenience constructors for the default `ConfigRoutingTable`.
impl AppRuntime<bitrouter_config::ConfigRoutingTable> {
    /// Load config from resolved paths. The `.env` file (if it exists) is loaded
    /// automatically from `paths.env_file`.
    pub fn load(paths: RuntimePaths) -> Result<Self> {
        let env_file = paths.env_file.exists().then_some(paths.env_file.as_path());
        let config = BitrouterConfig::load_from_file(&paths.config_file, env_file)?;
        let routing_table = bitrouter_config::ConfigRoutingTable::new(
            config.providers.clone(),
            config.models.clone(),
        );
        Ok(Self {
            config,
            paths,
            routing_table,
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
        let routing_table = bitrouter_config::ConfigRoutingTable::new(
            config.providers.clone(),
            config.models.clone(),
        );
        Self {
            config,
            paths,
            routing_table,
        }
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
