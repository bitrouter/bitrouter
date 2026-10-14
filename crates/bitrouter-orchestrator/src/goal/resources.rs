//! Task-selected resources and immutable material provided to an ACP execution.

use std::collections::BTreeSet;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use super::workflow::ArtifactRef;
use super::{GoalError, identifier, invalid};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillSelection {
    pub material_id: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    /// A BRO route selector; it is not an upstream provider model identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_route: Option<String>,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    /// Stable catalog material IDs, not potentially ambiguous skill names.
    #[serde(default)]
    pub skills: Vec<SkillSelection>,
    /// Relative files from the isolated workspace, after checked handoff.
    #[serde(default)]
    pub context_files: Vec<String>,
}

impl Selection {
    pub fn validate(&self) -> Result<(), GoalError> {
        if self.model_route.as_ref().is_some_and(|route| {
            route.is_empty()
                || route.len() > 256
                || route.chars().any(char::is_whitespace)
                || route.chars().any(char::is_control)
        }) {
            return Err(invalid("invalid selected model route"));
        }
        for ids in [&self.mcp_servers, &self.context_files] {
            if ids.len() > 16 || ids.iter().collect::<BTreeSet<_>>().len() != ids.len() {
                return Err(invalid(
                    "resource selection has duplicate or too many entries",
                ));
            }
        }
        for id in &self.mcp_servers {
            identifier(id)?;
        }
        if self.skills.len() > 16
            || self
                .skills
                .iter()
                .map(|skill| &skill.material_id)
                .collect::<BTreeSet<_>>()
                .len()
                != self.skills.len()
        {
            return Err(invalid("skill selection has duplicate or too many entries"));
        }
        for skill in &self.skills {
            identifier(&skill.material_id)?;
            if skill.sha256.len() != 64
                || !skill.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(invalid("skill selection must bind the advertised version"));
            }
        }
        for path in &self.context_files {
            if path.is_empty()
                || path.len() > 1024
                || path.contains('\\')
                || path.chars().any(char::is_control)
                || Path::new(path)
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_)))
            {
                return Err(invalid("context must select a relative workspace file"));
            }
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.model_route.is_none()
            && self.mcp_servers.is_empty()
            && self.skills.is_empty()
            && self.context_files.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Material {
    Context {
        path: String,
        content: ArtifactRef,
    },
    Skill {
        id: String,
        name: String,
        content: ArtifactRef,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepared {
    pub selection: Selection,
    pub prompt: ArtifactRef,
    pub materials: Vec<Material>,
    /// Private descriptor digest: credentials are never included in the head.
    pub mcp_configuration_sha256: String,
}

impl Prepared {
    pub fn validate(&self) -> Result<(), GoalError> {
        self.selection.validate()?;
        self.prompt.validate()?;
        if self.prompt.bytes > super::MAX_RECORD_BYTES as u64
            || self.mcp_configuration_sha256.len() != 64
            || !self
                .mcp_configuration_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid(
                "invalid prepared resource identity or prompt bound",
            ));
        }
        let mut contexts = BTreeSet::new();
        let mut skills = BTreeSet::new();
        let mut bytes = 0u64;
        for material in &self.materials {
            let content = match material {
                Material::Context { path, content } => {
                    if !self.selection.context_files.contains(path) || !contexts.insert(path) {
                        return Err(invalid("context material does not match selected files"));
                    }
                    content
                }
                Material::Skill { id, name, content } => {
                    identifier(name)?;
                    if !self
                        .selection
                        .skills
                        .iter()
                        .any(|skill| &skill.material_id == id && skill.sha256 == content.sha256)
                        || !skills.insert(id)
                    {
                        return Err(invalid(
                            "skill material does not match selected catalog IDs",
                        ));
                    }
                    content
                }
            };
            content.validate()?;
            bytes = bytes.saturating_add(content.bytes);
        }
        if contexts.len() != self.selection.context_files.len()
            || skills.len() != self.selection.skills.len()
            || bytes > 256 * 1024
        {
            return Err(invalid("selected material is missing or exceeds 256 KiB"));
        }
        Ok(())
    }
}
