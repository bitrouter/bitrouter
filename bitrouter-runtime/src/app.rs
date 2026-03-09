use std::path::{Path, PathBuf};

use crate::{
    config::{BitrouterConfig, RuntimePaths},
    control::ControlClient,
    error::{Result, RuntimeError},
    routing::ConfigRoutingTable,
    server::ServerPlan,
};

#[derive(Debug, Clone)]
pub struct AppRuntime {
    config: BitrouterConfig,
    paths: RuntimePaths,
}

impl AppRuntime {
    pub fn load(config_path: impl AsRef<Path>) -> Result<Self> {
        let config_file = config_path.as_ref().to_path_buf();
        let config = BitrouterConfig::load_from_file(&config_file)?;
        let paths = RuntimePaths::from_config_path(config_file);
        Ok(Self { config, paths })
    }

    pub fn scaffold(config_path: impl Into<PathBuf>) -> Self {
        let config_file = config_path.into();
        Self {
            config: BitrouterConfig::default(),
            paths: RuntimePaths::from_config_path(config_file),
        }
    }

    pub fn paths(&self) -> &RuntimePaths {
        &self.paths
    }

    pub fn config(&self) -> &BitrouterConfig {
        &self.config
    }

    pub fn routing_table(&self) -> ConfigRoutingTable {
        ConfigRoutingTable::new(self.config.providers.clone(), self.config.models.clone())
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
        Err(RuntimeError::Unsupported(
            "service-manager integration is not scaffolded yet",
        ))
    }

    pub async fn stop(&self) -> Result<()> {
        Err(RuntimeError::Unsupported(
            "service-manager integration is not scaffolded yet",
        ))
    }

    pub async fn restart(&self) -> Result<()> {
        Err(RuntimeError::Unsupported(
            "service-manager integration is not scaffolded yet",
        ))
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
