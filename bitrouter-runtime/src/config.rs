use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BitrouterConfig {
    pub listen_addr: SocketAddr,
    pub control: ControlEndpoint,
    pub log_level: String,
}

impl Default for BitrouterConfig {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787),
            control: ControlEndpoint::default(),
            log_level: String::from("info"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlEndpoint {
    pub path: PathBuf,
}

impl Default for ControlEndpoint {
    fn default() -> Self {
        Self {
            path: PathBuf::from("bitrouter.sock"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub config_file: PathBuf,
    pub runtime_dir: PathBuf,
    pub log_dir: PathBuf,
}

impl RuntimePaths {
    pub fn from_config_path(config_file: impl Into<PathBuf>) -> Self {
        let config_file = config_file.into();
        let runtime_dir = config_file
            .parent()
            .map(|parent| parent.join("run"))
            .unwrap_or_else(|| PathBuf::from("run"));
        let log_dir = config_file
            .parent()
            .map(|parent| parent.join("logs"))
            .unwrap_or_else(|| PathBuf::from("logs"));

        Self {
            config_file,
            runtime_dir,
            log_dir,
        }
    }
}
