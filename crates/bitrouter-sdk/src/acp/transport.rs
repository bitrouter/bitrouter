//! ACP upstream-transport descriptors.
//!
//! The ACP spec defines stdio as the canonical client-→-agent transport: the
//! client launches the agent as a child process and exchanges
//! newline-delimited JSON-RPC messages over its stdio pipes. Spec:
//! <https://agentclientprotocol.com/protocol/transports>.
//!
//! These types are always available (no `acp` feature required) so a
//! consumer can implement a custom [`super::Executor`] against them without
//! pulling in the bundled stdio executor.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// How to dial one upstream ACP agent.
///
/// v1.0 ships stdio only — the spec lists stdio as the primary transport for
/// agent processes spawned by an IDE / CLI client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AcpTransport {
    /// Stdio transport: launch `command` with `args` and exchange JSON-RPC
    /// over the child's stdin/stdout.
    Stdio {
        /// The program to spawn (resolved via `$PATH`).
        command: String,
        /// Arguments to pass to the child.
        #[serde(default)]
        args: Vec<String>,
        /// Extra environment variables for the child. Inherited env is kept.
        #[serde(default)]
        env: HashMap<String, String>,
    },
}

/// One configured upstream ACP agent, as written in `bitrouter.yaml` under
/// `agents:`.
///
/// The same `name` is what the [`super::RoutingTable`] resolves against;
/// future inbound entry points (the `agent-proxy` CLI) address agents by
/// this name.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AcpAgentConfig {
    /// Agent id. Non-empty; no `/`.
    pub name: String,
    /// Wire transport.
    pub transport: AcpTransport,
}

/// Errors returned by [`AcpAgentConfig::validate`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AcpConfigError {
    /// The `name` field violates one of the documented restrictions.
    #[error("invalid acp agent name '{name}': {reason}")]
    InvalidName {
        /// The offending name (or empty string).
        name: String,
        /// Human-readable explanation.
        reason: String,
    },
    /// Stdio transport with an empty `command`.
    #[error("acp agent '{name}': stdio command must not be empty")]
    EmptyStdioCommand {
        /// The agent whose command is missing.
        name: String,
    },
}

impl AcpAgentConfig {
    /// Verify the config is internally consistent. Called by
    /// [`super::config_routing::ConfigAcpRoutingTable`] at startup so a
    /// malformed `bitrouter.yaml` is rejected before the first request.
    pub fn validate(&self) -> Result<(), AcpConfigError> {
        if self.name.is_empty() {
            return Err(AcpConfigError::InvalidName {
                name: String::new(),
                reason: "must not be empty".into(),
            });
        }
        if self.name.contains('/') {
            return Err(AcpConfigError::InvalidName {
                name: self.name.clone(),
                reason: "must not contain '/'".into(),
            });
        }
        match &self.transport {
            AcpTransport::Stdio { command, .. } if command.is_empty() => {
                Err(AcpConfigError::EmptyStdioCommand {
                    name: self.name.clone(),
                })
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_round_trips_through_serde() {
        let cfg = AcpAgentConfig {
            name: "claude-acp".into(),
            transport: AcpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@zed-industries/claude-code-acp@latest".into()],
                env: Default::default(),
            },
        };
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["transport"]["type"], "stdio");
        assert_eq!(json["transport"]["command"], "npx");
        let back: AcpAgentConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back.name, "claude-acp");
    }

    #[test]
    fn validate_rejects_empty_name() {
        let cfg = AcpAgentConfig {
            name: String::new(),
            transport: AcpTransport::Stdio {
                command: "x".into(),
                args: vec![],
                env: Default::default(),
            },
        };
        assert!(matches!(
            cfg.validate(),
            Err(AcpConfigError::InvalidName { .. })
        ));
    }

    #[test]
    fn validate_rejects_slash_in_name() {
        let cfg = AcpAgentConfig {
            name: "a/b".into(),
            transport: AcpTransport::Stdio {
                command: "x".into(),
                args: vec![],
                env: Default::default(),
            },
        };
        assert!(matches!(
            cfg.validate(),
            Err(AcpConfigError::InvalidName { .. })
        ));
    }

    #[test]
    fn validate_rejects_empty_stdio_command() {
        let cfg = AcpAgentConfig {
            name: "x".into(),
            transport: AcpTransport::Stdio {
                command: String::new(),
                args: vec![],
                env: Default::default(),
            },
        };
        assert!(matches!(
            cfg.validate(),
            Err(AcpConfigError::EmptyStdioCommand { .. })
        ));
    }

    #[test]
    fn validate_accepts_well_formed_stdio() {
        let cfg = AcpAgentConfig {
            name: "x".into(),
            transport: AcpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into()],
                env: Default::default(),
            },
        };
        assert!(cfg.validate().is_ok());
    }
}
