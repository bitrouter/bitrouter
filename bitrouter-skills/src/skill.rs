//! Runtime skill types following the [agentskills.io](https://agentskills.io) standard.

use std::collections::HashMap;
use std::fmt;

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A skill tracked in the bitrouter skills registry.
///
/// Combines fields from the agentskills.io standard (name, description,
/// license, compatibility, metadata, allowed_tools) with bitrouter-specific
/// tracking fields (source, required_apis, installed_by).
#[derive(Debug, Clone)]
pub struct Skill {
    /// Unique identifier.
    pub id: Uuid,
    /// Skill name (1–64 chars, lowercase alphanumeric + hyphens).
    pub name: String,
    /// What the skill does and when to use it.
    pub description: String,
    /// License name or reference.
    pub license: Option<String>,
    /// Environment requirements (e.g. "Requires Python 3.14+").
    pub compatibility: Option<String>,
    /// Arbitrary key-value metadata.
    pub metadata: HashMap<String, String>,
    /// Pre-approved tool names.
    pub allowed_tools: Option<Vec<String>>,
    /// How this skill was registered.
    pub source: SkillSource,
    /// Provider names this skill depends on for paid API access.
    pub required_apis: Vec<String>,
    /// Who installed this skill.
    pub installed_by: InstalledBy,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// How a skill was registered in the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    /// Loaded from `bitrouter.yaml` config.
    Config,
    /// Registered via the REST API.
    Manual,
}

impl fmt::Display for SkillSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Config => "config",
            Self::Manual => "manual",
        })
    }
}

/// Who installed a skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstalledBy {
    /// Pre-installed by a human operator.
    Human,
    /// Auto-installed by an agent during a session.
    Agent {
        /// The session in which the agent installed this skill.
        session_id: String,
    },
}

impl fmt::Display for InstalledBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Human => f.write_str("human"),
            Self::Agent { .. } => f.write_str("agent"),
        }
    }
}
