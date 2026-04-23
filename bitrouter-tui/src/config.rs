use std::net::SocketAddr;
use std::path::PathBuf;

/// Configuration passed from the binary to the TUI.
#[derive(Debug, Clone)]
pub struct TuiConfig {
    pub listen_addr: SocketAddr,
    pub providers: Vec<String>,
    pub route_count: usize,
    pub daemon_pid: Option<u32>,
    /// Root directory where per-agent binary installs live
    /// (`<home>/agents/`).  Each agent writes into a flat subdir keyed
    /// by its id.
    pub agents_dir: PathBuf,
    /// Install-state ledger (`<home>/agents/state.json`).
    pub agent_state_file: PathBuf,
}
