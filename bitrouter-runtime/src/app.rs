use std::path::{Path, PathBuf};

use bitrouter_config::BitrouterConfig;
use bitrouter_core::routers::routing_table::RoutingTable;

use crate::{
    control::ControlClient,
    error::Result,
    paths::RuntimePaths,
    server::ServerPlan,
};

pub struct AppRuntime<R> {
    config: BitrouterConfig,
    paths: RuntimePaths,
    routing_table: R,
}

impl<R: RoutingTable + Send + Sync> AppRuntime<R> {
    pub fn new(config: BitrouterConfig, paths: RuntimePaths, routing_table: R) -> Self {
        Self {
            config,
            paths,
            routing_table,
        }
    }

    pub fn paths(&self) -> &RuntimePaths {
        &self.paths
    }

    pub fn config(&self) -> &BitrouterConfig {
        &self.config
    }

    pub fn routing_table(&self) -> &R {
        &self.routing_table
    }

    pub fn control_client(&self) -> ControlClient {
        ControlClient::new(self.paths.clone())
    }

    pub fn status(&self) -> RuntimeStatus {
        RuntimeStatus {
            config_file: self.paths.config_file.clone(),
            runtime_dir: self.paths.runtime_dir.clone(),
            listen_addr: self.config.server.listen,
            providers: self.config.providers.keys().cloned().collect(),
            models: self.config.models.keys().cloned().collect(),
        }
    }

    pub async fn serve(self) -> Result<()> {
        ServerPlan::new(self.config).serve().await
    }

    pub async fn start(&self) -> Result<()> {
        let dm = crate::daemon::DaemonManager::new(self.paths.clone());
        let pid = dm.start().await?;
        println!("bitrouter daemon started (pid {pid})");
        Ok(())
    }

    pub async fn stop(&self) -> Result<()> {
        let dm = crate::daemon::DaemonManager::new(self.paths.clone());
        dm.stop().await?;
        println!("bitrouter daemon stopped");
        Ok(())
    }

    pub async fn restart(&self) -> Result<()> {
        let dm = crate::daemon::DaemonManager::new(self.paths.clone());
        let pid = dm.restart().await?;
        println!("bitrouter daemon restarted (pid {pid})");
        Ok(())
    }
}

/// Convenience constructors for the default `ConfigRoutingTable`.
impl AppRuntime<bitrouter_config::ConfigRoutingTable> {
    pub fn load(config_path: impl AsRef<Path>) -> Result<Self> {
        let config_file = config_path.as_ref().to_path_buf();
        let config = BitrouterConfig::load_from_file(&config_file)?;
        let paths = RuntimePaths::from_config_path(config_file);
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

    pub fn scaffold(config_path: impl Into<PathBuf>) -> Self {
        let config_file = config_path.into();
        let config = BitrouterConfig::default();
        let routing_table = bitrouter_config::ConfigRoutingTable::new(
            config.providers.clone(),
            config.models.clone(),
        );
        Self {
            paths: RuntimePaths::from_config_path(config_file),
            config,
            routing_table,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeStatus {
    pub config_file: PathBuf,
    pub runtime_dir: PathBuf,
    pub listen_addr: std::net::SocketAddr,
    pub providers: Vec<String>,
    pub models: Vec<String>,
}
