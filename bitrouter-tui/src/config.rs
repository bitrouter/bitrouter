use std::net::SocketAddr;

/// Configuration passed from the binary to the TUI.
#[derive(Debug, Clone)]
pub struct TuiConfig {
    pub listen_addr: SocketAddr,
    pub providers: Vec<String>,
    pub route_count: usize,
    pub daemon_pid: Option<u32>,
}
