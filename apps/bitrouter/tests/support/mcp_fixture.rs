//! Process transport and journal I/O only; each suite owns its wire expectations.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use bitrouter::config::mcp::McpTransport;
use serde_json::Value;

use super::example_binary;

pub fn transport(config: &Path, journal: &Path) -> std::io::Result<McpTransport> {
    static BINARY: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    let binary = BINARY
        .get_or_init(|| example_binary::build("bitrouter", "mcp_skills_fixture"))
        .as_ref()
        .map_err(|error| std::io::Error::other(error.clone()))?;
    Ok(McpTransport::Stdio {
        command: binary.to_string_lossy().into_owned(),
        args: vec![
            config.to_string_lossy().into_owned(),
            journal.to_string_lossy().into_owned(),
        ],
        env: HashMap::new(),
    })
}

pub fn requests(journal: &Path) -> std::io::Result<Vec<Value>> {
    std::fs::read_to_string(journal)?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(std::io::Error::other))
        .collect()
}
