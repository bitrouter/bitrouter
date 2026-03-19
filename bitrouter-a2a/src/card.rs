//! A2A v1.0 Agent Card types.
//!
//! Defines the complete Agent Card schema per the
//! [A2A v1.0 specification](https://a2a-protocol.org/latest/definitions/).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::security::{SecurityRequirement, SecurityScheme};

/// An Agent Card — the self-describing manifest for an A2A agent.
///
/// Published at `/.well-known/agent-card.json` for discovery. Contains the
/// agent's identity, capabilities, skills, supported interfaces, and security
/// requirements.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    /// Human-readable agent name.
    pub name: String,

    /// Purpose description for users and other agents.
    pub description: String,

    /// Agent version (e.g., `"1.0.0"`).
    pub version: String,

    /// Service provider information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProvider>,

    /// Ordered list of supported protocol interfaces.
    pub supported_interfaces: Vec<AgentInterface>,

    /// Supported A2A capabilities.
    pub capabilities: AgentCapabilities,

    /// Named authentication scheme definitions.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub security_schemes: HashMap<String, SecurityScheme>,

    /// Security requirements for accessing this agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub security_requirements: Vec<SecurityRequirement>,

    /// Supported input media types across all skills.
    pub default_input_modes: Vec<String>,

    /// Supported output media types across all skills.
    pub default_output_modes: Vec<String>,

    /// Agent abilities and functions.
    pub skills: Vec<AgentSkill>,

    /// JWS signatures for card integrity verification.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signatures: Vec<AgentCardSignature>,

    /// URL to the agent's icon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,

    /// URL for additional documentation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,
}

/// Service provider of an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentProvider {
    /// Organization name.
    pub organization: String,
    /// Provider website or documentation URL.
    pub url: String,
}

/// Declares target URL, transport, and protocol version for agent interaction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentInterface {
    /// Absolute HTTPS URL where the interface is available.
    pub url: String,

    /// Protocol binding type: `"json-rpc"`, `"grpc"`, or `"http-rest"`.
    pub protocol_binding: String,

    /// A2A protocol version (e.g., `"1.0"`).
    pub protocol_version: String,

    /// Tenant ID for multi-tenant deployments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

/// Optional capabilities supported by an agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    /// Supports streaming responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,

    /// Supports push notifications for async updates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_notifications: Option<bool>,

    /// Supports extended agent card when authenticated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extended_agent_card: Option<bool>,

    /// Supported protocol extensions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<AgentExtension>,
}

/// A protocol extension declaration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentExtension {
    /// Unique URI identifying the extension.
    pub uri: String,

    /// How the agent uses the extension.
    pub description: String,

    /// Whether the client must understand this extension.
    pub required: bool,

    /// Extension-specific configuration parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A distinct capability or function an agent performs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    /// Unique skill identifier.
    pub id: String,

    /// Human-readable skill name.
    pub name: String,

    /// Detailed capability description.
    pub description: String,

    /// Keywords describing capabilities.
    pub tags: Vec<String>,

    /// Example prompts or scenarios.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,

    /// Supported input media types (overrides agent defaults).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modes: Vec<String>,

    /// Supported output media types (overrides agent defaults).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modes: Vec<String>,

    /// Security requirements specific to this skill.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub security_requirements: Vec<SecurityRequirement>,
}

/// JWS signature of an Agent Card (RFC 7515 JSON Serialization format).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentCardSignature {
    /// Base64url-encoded JWS protected header.
    pub protected: String,

    /// Base64url-encoded computed signature.
    pub signature: String,

    /// Unprotected JWS header values.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<serde_json::Value>,
}

/// Build a minimal Agent Card with required fields only.
///
/// Sets reasonable defaults: empty skills, empty security, `text/plain`
/// input/output modes, and a single `http-rest` interface.
pub fn minimal_card(name: &str, description: &str, version: &str, url: &str) -> AgentCard {
    AgentCard {
        name: name.to_string(),
        description: description.to_string(),
        version: version.to_string(),
        provider: None,
        supported_interfaces: vec![AgentInterface {
            url: url.to_string(),
            protocol_binding: "http-rest".to_string(),
            protocol_version: "1.0".to_string(),
            tenant: None,
        }],
        capabilities: AgentCapabilities::default(),
        security_schemes: HashMap::new(),
        security_requirements: Vec::new(),
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["text/plain".to_string()],
        skills: Vec::new(),
        signatures: Vec::new(),
        icon_url: None,
        documentation_url: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_card_round_trip() {
        let card = minimal_card(
            "test-agent",
            "A test agent",
            "1.0.0",
            "https://agent.example.com",
        );

        let json = serde_json::to_string_pretty(&card).expect("serialize");
        let parsed: AgentCard = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(card, parsed);
    }

    #[test]
    fn full_agent_card_round_trip() {
        let card = AgentCard {
            name: "smart-assistant".to_string(),
            description: "A smart assistant agent".to_string(),
            version: "2.1.0".to_string(),
            provider: Some(AgentProvider {
                organization: "Acme Corp".to_string(),
                url: "https://acme.example.com".to_string(),
            }),
            supported_interfaces: vec![
                AgentInterface {
                    url: "https://agent.acme.example.com/a2a".to_string(),
                    protocol_binding: "json-rpc".to_string(),
                    protocol_version: "1.0".to_string(),
                    tenant: Some("tenant-1".to_string()),
                },
                AgentInterface {
                    url: "https://agent.acme.example.com/rest".to_string(),
                    protocol_binding: "http-rest".to_string(),
                    protocol_version: "1.0".to_string(),
                    tenant: None,
                },
            ],
            capabilities: AgentCapabilities {
                streaming: Some(true),
                push_notifications: Some(false),
                extended_agent_card: None,
                extensions: vec![AgentExtension {
                    uri: "https://a2a.example.com/ext/logging".to_string(),
                    description: "Structured logging extension".to_string(),
                    required: false,
                    params: Some(serde_json::json!({"level": "info"})),
                }],
            },
            security_schemes: HashMap::from([(
                "bearer".to_string(),
                crate::security::SecurityScheme::Http(crate::security::HttpAuthSecurityScheme {
                    scheme: "Bearer".to_string(),
                    bearer_format: Some("JWT".to_string()),
                    description: None,
                }),
            )]),
            security_requirements: vec![crate::security::SecurityRequirement {
                schemes: HashMap::from([(
                    "bearer".to_string(),
                    crate::security::StringList {
                        list: vec!["agent:invoke".to_string()],
                    },
                )]),
            }],
            default_input_modes: vec!["text/plain".to_string(), "application/json".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
            skills: vec![AgentSkill {
                id: "text-gen".to_string(),
                name: "Text Generation".to_string(),
                description: "Generate text from prompts".to_string(),
                tags: vec!["llm".to_string(), "text".to_string()],
                examples: vec!["Write a poem about Rust".to_string()],
                input_modes: Vec::new(),
                output_modes: Vec::new(),
                security_requirements: Vec::new(),
            }],
            signatures: vec![AgentCardSignature {
                protected: "eyJhbGciOiJFZERTQSJ9".to_string(),
                signature: "abc123".to_string(),
                header: None,
            }],
            icon_url: Some("https://acme.example.com/icon.png".to_string()),
            documentation_url: Some("https://docs.acme.example.com".to_string()),
        };

        let json = serde_json::to_string_pretty(&card).expect("serialize");
        let parsed: AgentCard = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(card, parsed);
    }

    #[test]
    fn minimal_card_has_defaults() {
        let card = minimal_card("test", "desc", "0.1.0", "http://localhost:8787");
        assert!(card.skills.is_empty());
        assert!(card.security_schemes.is_empty());
        assert_eq!(card.supported_interfaces.len(), 1);
        assert_eq!(card.supported_interfaces[0].protocol_binding, "http-rest");
        assert_eq!(card.supported_interfaces[0].protocol_version, "1.0");
    }
}
