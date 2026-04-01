/// Errors produced by the TUI.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ACP error: {0}")]
    Acp(#[from] agent_client_protocol::Error),
}
