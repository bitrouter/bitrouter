//! Skill configuration — parallel to [`crate::tool`] for MCP servers and [`crate::agent`] for A2A.

use serde::{Deserialize, Serialize};

/// Configuration for a single skill in the skills registry.
///
/// Follows the [agentskills.io](https://agentskills.io) standard for naming
/// and description conventions. The `required_apis` field declares which
/// upstream providers a skill depends on so bitrouter can route payment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillConfig {
    /// Skill name (1–64 chars, lowercase alphanumeric + hyphens).
    pub name: String,

    /// What the skill does and when to use it (1–1024 chars).
    pub description: String,

    /// Provenance URL or local path (for human reference).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    /// Upstream providers this skill depends on for paid API access.
    ///
    /// Each entry must match a key in the top-level `providers` section.
    /// bitrouter handles payment (402/MPP) transparently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_apis: Vec<SkillRequiredApi>,
}

/// A paid API dependency declared by a skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRequiredApi {
    /// Provider name — must match a key in the `providers` config section.
    pub provider: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_skill_config_round_trip() {
        let yaml = r#"
name: "code-review"
description: "Reviews code for quality and security issues"
"#;
        let config: SkillConfig = serde_yaml::from_str(yaml).expect("deserialize");
        assert_eq!(config.name, "code-review");
        assert_eq!(config.description, "Reviews code for quality and security issues");
        assert!(config.source.is_none());
        assert!(config.required_apis.is_empty());

        let serialized = serde_yaml::to_string(&config).expect("serialize");
        let parsed: SkillConfig = serde_yaml::from_str(&serialized).expect("re-deserialize");
        assert_eq!(parsed.name, config.name);
        assert_eq!(parsed.description, config.description);
    }

    #[test]
    fn full_skill_config_round_trip() {
        let yaml = r#"
name: "translate"
description: "Translates text between languages using DeepL"
source: "https://github.com/example/translate-skill"
required_apis:
  - provider: deepl
  - provider: openai
"#;
        let config: SkillConfig = serde_yaml::from_str(yaml).expect("deserialize");
        assert_eq!(config.name, "translate");
        assert_eq!(config.source.as_deref(), Some("https://github.com/example/translate-skill"));
        assert_eq!(config.required_apis.len(), 2);
        assert_eq!(config.required_apis[0].provider, "deepl");
        assert_eq!(config.required_apis[1].provider, "openai");

        let serialized = serde_yaml::to_string(&config).expect("serialize");
        let parsed: SkillConfig = serde_yaml::from_str(&serialized).expect("re-deserialize");
        assert_eq!(parsed.required_apis.len(), 2);
    }

    #[test]
    fn skill_required_api_round_trip() {
        let yaml = r#"provider: "anthropic""#;
        let api: SkillRequiredApi = serde_yaml::from_str(yaml).expect("deserialize");
        assert_eq!(api.provider, "anthropic");

        let serialized = serde_yaml::to_string(&api).expect("serialize");
        let parsed: SkillRequiredApi = serde_yaml::from_str(&serialized).expect("re-deserialize");
        assert_eq!(parsed.provider, "anthropic");
    }

    #[test]
    fn empty_required_apis_omitted_in_yaml() {
        let config = SkillConfig {
            name: "test".to_string(),
            description: "A test skill".to_string(),
            source: None,
            required_apis: Vec::new(),
        };
        let yaml = serde_yaml::to_string(&config).expect("serialize");
        assert!(!yaml.contains("required_apis"));
        assert!(!yaml.contains("source"));
    }
}
