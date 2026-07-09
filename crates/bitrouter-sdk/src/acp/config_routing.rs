//! Routing table backed by the `agents` map in `bitrouter.yaml`.

use std::collections::HashMap;

use async_trait::async_trait;

use super::transport::{AcpAgentConfig, AcpTransport};
use super::{AcpTarget, RoutingTable};
use crate::caller::CallerContext;
use crate::error::{BitrouterError, Result};

/// In-memory `agent → transport` map populated from config at startup.
#[derive(Debug, Default, Clone)]
pub struct ConfigAcpRoutingTable {
    agents: HashMap<String, AcpTransport>,
}

impl ConfigAcpRoutingTable {
    /// Build from a `name → AcpAgentConfig` map. Validates every entry up
    /// front; the first invalid entry stops the build so a startup
    /// misconfiguration is loud, not silent.
    pub fn from_configs<I>(entries: I) -> Result<Self>
    where
        I: IntoIterator<Item = (String, AcpAgentConfig)>,
    {
        let mut agents = HashMap::new();
        for (key, cfg) in entries {
            cfg.validate()
                .map_err(|e| BitrouterError::internal(e.to_string()))?;
            agents.insert(key, cfg.transport);
        }
        Ok(Self { agents })
    }

    /// True if no agents are configured.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Direct transport lookup, for callers that want to introspect
    /// configuration without going through a [`super::Pipeline`].
    pub fn lookup(&self, agent: &str) -> Option<&AcpTransport> {
        self.agents.get(agent)
    }
}

#[async_trait]
impl RoutingTable for ConfigAcpRoutingTable {
    async fn resolve(&self, agent: &str, _caller: &CallerContext) -> Result<AcpTarget> {
        let transport = self.agents.get(agent).cloned().ok_or_else(|| {
            BitrouterError::NotFound(format!("no acp agent configured for '{agent}'"))
        })?;
        Ok(AcpTarget {
            agent_name: agent.to_string(),
            transport,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, cmd: &str) -> (String, AcpAgentConfig) {
        (
            name.to_string(),
            AcpAgentConfig {
                name: name.to_string(),
                transport: AcpTransport::Stdio {
                    command: cmd.to_string(),
                    args: vec![],
                    env: Default::default(),
                },
            },
        )
    }

    fn caller() -> CallerContext {
        CallerContext::new("k", "u")
    }

    #[tokio::test]
    async fn resolves_configured_agent() {
        let table = ConfigAcpRoutingTable::from_configs([agent("claude", "npx")]).unwrap();
        let target = table.resolve("claude", &caller()).await.unwrap();
        assert_eq!(target.agent_name, "claude");
        match target.transport {
            AcpTransport::Stdio { command, .. } => assert_eq!(command, "npx"),
        }
    }

    #[tokio::test]
    async fn unknown_agent_returns_not_found() {
        let table = ConfigAcpRoutingTable::from_configs([agent("claude", "npx")]).unwrap();
        let err = table.resolve("missing", &caller()).await.unwrap_err();
        assert_eq!(err.status(), 404);
    }

    #[test]
    fn invalid_entry_fails_construction() {
        let bad = (
            "bad".to_string(),
            AcpAgentConfig {
                name: "bad".into(),
                transport: AcpTransport::Stdio {
                    command: String::new(),
                    args: vec![],
                    env: Default::default(),
                },
            },
        );
        assert!(ConfigAcpRoutingTable::from_configs([bad]).is_err());
    }

    #[test]
    fn lookup_returns_transport_for_introspection() {
        let table = ConfigAcpRoutingTable::from_configs([agent("claude", "npx")]).unwrap();
        assert!(matches!(
            table.lookup("claude"),
            Some(AcpTransport::Stdio { .. })
        ));
        assert!(table.lookup("missing").is_none());
    }
}
