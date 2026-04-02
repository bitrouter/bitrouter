/// Errors produced by the TUI.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
