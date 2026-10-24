//! Frozen MCP-origin skill metadata and verification. These values describe
//! resources; neither a held entry nor verified bytes grant execution consent.

use std::path::{Path, PathBuf};

use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::mcp::skills::{
    SkillEntry, SkillResource, SkillResources, decode_skill_uri_segment, skill_resource_path,
    skill_uri_root, validate_skill_resource_uri,
};
use bitrouter_sdk::skills::{McpSkillSourceGrant, SkillsConfig};
use serde::{Deserialize, Serialize};

use crate::harness::sha256;

pub(crate) const MAX_ENTRY_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillScope {
    pub workspace: PathBuf,
    pub api_key_id: String,
    pub user_id: String,
    pub local: bool,
}

impl SkillScope {
    pub(crate) fn authorize(
        grant: &McpSkillSourceGrant,
        caller: &CallerContext,
        workspace: &Path,
    ) -> Result<Self, String> {
        grant.validate()?;
        if caller.is_anonymous()
            || !(if caller.is_local() {
                grant.local
            } else {
                grant.api_key_ids.iter().any(|id| id == caller.api_key_id())
                    || grant.user_ids.iter().any(|id| id == caller.user_id())
            })
        {
            return Err("MCP skill source is not allowed for this caller".into());
        }
        let workspace = workspace
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if !grant.workspaces.iter().any(|path| {
            path.is_absolute()
                && path
                    .canonicalize()
                    .is_ok_and(|allowed| allowed == workspace)
        }) {
            return Err("MCP skill source is not allowed for this workspace".into());
        }
        Ok(Self {
            workspace,
            api_key_id: caller.api_key_id().into(),
            user_id: caller.user_id().into(),
            local: caller.is_local(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillOrigin {
    /// Host configuration key, never the upstream's self-declared serverInfo.
    pub server: String,
    pub configuration_digest: String,
    pub scope: SkillScope,
}

/// Persist this complete entry for as long as its instructions remain in
/// context. A later listing is not a replacement for this commitment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HeldMcpSkill {
    pub origin: SkillOrigin,
    pub entry: SkillEntry,
    /// Covers the full entry, including every URI/digest/size and frontmatter.
    pub entry_digest: String,
}

impl HeldMcpSkill {
    pub(crate) fn new(origin: SkillOrigin, entry: SkillEntry) -> Result<Self, String> {
        let entry_digest = entry_digest(&entry)?;
        let held = Self {
            origin,
            entry,
            entry_digest,
        };
        held.validate()?;
        Ok(held)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.entry.validate_metadata()?;
        let frontmatter: super::format::SkillFrontmatter =
            serde_json::from_value(serde_json::Value::Object(self.entry.frontmatter.clone()))
                .map_err(|error| format!("invalid skill frontmatter metadata: {error}"))?;
        frontmatter.validate().map_err(|error| error.to_string())?;
        if entry_digest(&self.entry)? != self.entry_digest {
            return Err("held MCP skill entry digest mismatch".into());
        }
        let root = self.root()?;
        let SkillResources::Enumerated(files) = &self.entry.resources else {
            return Err("MCP skill resources=dynamic is unsupported by this native host".into());
        };
        for file in files {
            validate_uri(&file.uri)?;
            if !file
                .uri
                .strip_prefix(root)
                .is_some_and(|suffix| suffix.starts_with('/') && suffix.len() > 1)
            {
                return Err("MCP skill manifest file is outside its skill root".into());
            }
        }
        Ok(())
    }

    fn root(&self) -> Result<&str, String> {
        validate_uri(&self.entry.uri)?;
        let name = self
            .entry
            .frontmatter
            .get("name")
            .and_then(|name| name.as_str())
            .ok_or("skill name is missing")?;
        skill_uri_root(&self.entry.uri, name)
    }

    /// Resolve against this skill root, never the host filesystem or DNS.
    /// A directory view or a read of a nested SKILL.md does not add entries.
    pub fn resource(&self, reference: &str) -> Result<&SkillResource, String> {
        self.validate()?;
        let SkillResources::Enumerated(files) = &self.entry.resources else {
            return Err("dynamic skill has no frozen manifest".into());
        };
        if validate_skill_resource_uri(reference).is_ok() {
            return files
                .iter()
                .find(|file| file.uri == reference)
                .ok_or_else(|| "skill reference is not in the held manifest".into());
        }
        if reference.is_empty() || reference.starts_with('/') || reference.len() > MAX_ENTRY_BYTES {
            return Err("skill reference must be a bounded relative file path".into());
        }
        let path = relative_components(reference);
        let prefix = format!("{}/", self.root()?);
        let exact_relative = format!("{prefix}{reference}");
        let mut found = None;
        for file in files {
            let suffix = file
                .uri
                .strip_prefix(&prefix)
                .ok_or("file is outside its skill root")?;
            let file_path = suffix
                .split(['?', '#'])
                .next()
                .ok_or("file path is missing")?;
            if file.uri == exact_relative
                || path.as_ref().is_ok_and(|path| {
                    relative_components(file_path).is_ok_and(|candidate| candidate == *path)
                })
            {
                if found.is_some() {
                    return Err(
                        "relative skill reference matches multiple held URIs; use an exact URI"
                            .into(),
                    );
                }
                found = Some(file);
            }
        }
        match (found, path) {
            (Some(file), _) => Ok(file),
            (None, Err(error)) => Err(error),
            (None, Ok(_)) => Err("skill reference is not in the held manifest".into()),
        }
    }

    /// Used equally for network results and every cache hit. Stored digest
    /// labels, mtimes, permissions and prior verification are not evidence.
    pub fn verify(&self, file: &SkillResource, bytes: &[u8]) -> Result<(), String> {
        if self.resource(&file.uri)? != file {
            return Err("file is not the exact held manifest entry".into());
        }
        if bytes.len() as u64 != file.size {
            return Err("MCP skill raw byte size mismatch".into());
        }
        if format!("sha256:{}", sha256(bytes)) != file.digest {
            return Err("MCP skill raw byte digest mismatch".into());
        }
        if file.uri == self.entry.uri {
            let text =
                std::str::from_utf8(bytes).map_err(|_| "MCP SKILL.md requires UTF-8 encoding")?;
            let frontmatter =
                super::format::parse_frontmatter(text).map_err(|error| error.to_string())?;
            frontmatter.validate().map_err(|error| error.to_string())?;
            if frontmatter.raw != self.entry.frontmatter {
                return Err("MCP SKILL.md complete frontmatter mismatch".into());
            }
        }
        Ok(())
    }
}

fn entry_digest(entry: &SkillEntry) -> Result<String, String> {
    let mut entry = entry.clone();
    // Approval binds the set of files, not an incidental listing order.
    if let SkillResources::Enumerated(files) = &mut entry.resources {
        files.sort_by(|a, b| a.uri.cmp(&b.uri));
    }
    let bytes = serde_json::to_vec(&crate::harness::canonical(
        serde_json::to_value(&entry).map_err(|error| error.to_string())?,
    ))
    .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_ENTRY_BYTES {
        return Err("MCP skill entry exceeds the 2 MiB metadata budget".into());
    }
    Ok(sha256(&bytes))
}

fn validate_uri(uri: &str) -> Result<(), String> {
    for segment in skill_resource_path(uri)?.split('/') {
        let decoded = decode_skill_uri_segment(segment)?;
        if matches!(decoded.as_str(), "." | "..") || decoded.contains(['\\', '\0', '/']) {
            return Err("MCP skill URI contains an unsafe path segment".into());
        }
    }
    Ok(())
}

/// The extension defines filesystem-relative references, not URL-relative
/// authority or scheme replacement. Match decoded paths only within the held
/// manifest, then return its exact URI without rewriting source identity.
fn relative_components(reference: &str) -> Result<Vec<String>, String> {
    if reference.is_empty() || reference.starts_with('/') || reference.len() > MAX_ENTRY_BYTES {
        return Err("skill reference must be a bounded relative file path".into());
    }
    let mut components = Vec::new();
    for segment in reference.split('/') {
        let decoded = decode_skill_uri_segment(segment)?;
        if decoded.contains(['\\', '\0', '/']) {
            return Err("skill reference contains an unsafe path segment".into());
        }
        match decoded.as_str() {
            "" | "." => {}
            ".." => {
                components.pop().ok_or("skill reference escapes its root")?;
            }
            _ => components.push(decoded),
        }
    }
    if components.is_empty() {
        return Err("skill reference must identify a file".into());
    }
    Ok(components)
}

/// An optional verified-byte cache is owned by the caller's durable artifact
/// store, not by filesystem skill discovery or an additional database.
pub struct SkillRead<'a> {
    pub reference: &'a str,
    pub max_bytes: u64,
    pub cached_bytes: Option<&'a [u8]>,
}

/// Supplied afresh by the admitted execution owner, not serialized from model
/// output or inferred from an upstream annotation.
pub struct SkillAccess<'a> {
    pub caller: &'a CallerContext,
    pub policy: &'a SkillsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpSkillProblem {
    pub server: String,
    pub uri: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpSkillCatalog {
    pub skills: Vec<HeldMcpSkill>,
    pub excluded: Vec<McpSkillProblem>,
}

pub struct VerifiedSkillFile {
    pub uri: String,
    pub bytes: Vec<u8>,
}

impl VerifiedSkillFile {
    pub fn text(&self) -> Result<&str, String> {
        let text = std::str::from_utf8(&self.bytes)
            .map_err(|_| "binary or non-UTF-8 skill content is unsupported in model context")?;
        if text.contains('\0') {
            return Err("binary skill content is unsupported in model context".into());
        }
        Ok(text)
    }
}
