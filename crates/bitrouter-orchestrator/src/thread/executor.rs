//! Native configuration remains wire-compatible. ACP settings describe only
//! what BRO controls; they never stand in for the harness's private context.
use std::path::PathBuf;
use std::time::Duration;

use bitrouter_sdk::caller::CallerContext;
use serde::{Deserialize, Deserializer, Serialize};

use super::PermissionProfile;
use crate::agent::{AgentConfig, ToolMode};

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Config {
    Native(AgentConfig),
    Acp { acp: AcpConfig },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpConfig {
    pub agent_id: String,
    pub requested_model_route: Option<String>,
    pub max_duration: Duration,
}

pub struct AcpThreadRequest {
    pub caller: CallerContext,
    pub workspace: PathBuf,
    pub config: AcpConfig,
    pub permission_profile: PermissionProfile,
    pub idempotency_key: String,
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(acp) = value.get("acp") {
            if value.as_object().is_none_or(|object| object.len() != 1) {
                return Err(serde::de::Error::custom("ambiguous executor configuration"));
            }
            serde_json::from_value(acp.clone())
                .map(|acp| Self::Acp { acp })
                .map_err(serde::de::Error::custom)
        } else {
            serde_json::from_value(value)
                .map(Self::Native)
                .map_err(serde::de::Error::custom)
        }
    }
}

impl From<AgentConfig> for Config {
    fn from(config: AgentConfig) -> Self {
        Self::Native(config)
    }
}

impl Config {
    pub fn native(&self) -> Result<&AgentConfig, String> {
        match self {
            Self::Native(config) => Ok(config),
            Self::Acp { .. } => Err("ACP Thread has no native Agent configuration".into()),
        }
    }

    pub fn model(&self) -> &str {
        match self {
            Self::Native(config) => &config.model,
            Self::Acp { acp } => acp.requested_model_route.as_deref().unwrap_or_default(),
        }
    }

    pub fn tool_mode(&self) -> ToolMode {
        match self {
            Self::Native(config) => config.tool_mode(),
            Self::Acp { .. } => ToolMode::Coding,
        }
    }

    pub fn max_duration(&self) -> Duration {
        match self {
            Self::Native(config) => config.max_duration,
            Self::Acp { acp } => acp.max_duration,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_headers_keep_their_existing_json_and_reject_ambiguous_executors()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = AgentConfig::fixed("fixture-model", None).read_only();
        let original = serde_json::to_value(&config)?;
        let recovered: Config = serde_json::from_value(original.clone())?;
        assert_eq!(recovered.native()?.model, config.model);
        assert_eq!(serde_json::to_value(recovered)?, original);
        let mut ambiguous = original;
        ambiguous["acp"] = serde_json::json!({"agent_id":"fixture-peer", "requested_model_route":null, "max_duration":{"secs":600,"nanos":0}});
        assert!(serde_json::from_value::<Config>(ambiguous).is_err());
        Ok(())
    }

    #[test]
    fn acp_header_contains_no_native_model_or_context_configuration()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = Config::Acp {
            acp: AcpConfig {
                agent_id: "fixture-peer".into(),
                requested_model_route: None,
                max_duration: Duration::from_secs(600),
            },
        };
        let value = serde_json::to_value(&config)?;
        assert!(value.get("model").is_none());
        assert!(value.get("instructions").is_none());
        let recovered: Config = serde_json::from_value(value)?;
        assert!(recovered.native().is_err());
        assert_eq!(recovered.model(), "");
        Ok(())
    }
}
