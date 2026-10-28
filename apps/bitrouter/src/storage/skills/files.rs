//! File commitments and data frames derived from the original selected source.

use crate::skills::mcp::HeldMcpSkillExt;
use bitrouter_ai::types::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[cfg(feature = "core")]
use super::SkillSelection;
use super::{SkillCandidate, SkillSource, invalid};
use crate::storage::checkpoint::sha256;
use crate::storage::checkpoint::snapshot::{AgentTurn, SessionSnapshot};
use crate::storage::skills::state as skills;
use bitrouter_ai::core::protocol::{CoreError, ErrorCode};

pub const TOOL: &str = "skill_read";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct File {
    pub reference: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Deserialize)]
struct Receipt {
    bundle: Bundle,
}
#[derive(Deserialize)]
struct Bundle {
    digest: String,
    files: Vec<ManagedFile>,
}
#[derive(Deserialize)]
struct ManagedFile {
    path: String,
    digest: String,
    bytes: u64,
}

fn local_path(reference: &str) -> Result<String, CoreError> {
    if reference.is_empty() || reference.starts_with('/') || reference.contains(['\\', '\0']) {
        return Err(invalid("skill file requires a relative bundle path"));
    }
    let mut path = Vec::new();
    for segment in reference.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                path.pop()
                    .ok_or_else(|| invalid("skill file escapes its bundle"))?;
            }
            _ => path.push(segment),
        }
    }
    if path.is_empty() {
        return Err(invalid("skill file path is empty"));
    }
    Ok(path.join("/"))
}

impl SkillSource {
    pub fn root_reference(&self) -> &str {
        match self {
            Self::Local(_) => "SKILL.md",
            Self::Mcp(held) => &held.entry.uri,
        }
    }
    pub fn file(&self, reference: &str) -> Result<File, CoreError> {
        match self {
            Self::Mcp(held) => {
                let file = held.resource(reference).map_err(invalid)?;
                Ok(File {
                    reference: file.uri.clone(),
                    sha256: file
                        .digest
                        .strip_prefix("sha256:")
                        .ok_or_else(|| invalid("invalid file digest"))?
                        .into(),
                    bytes: file.size,
                })
            }
            Self::Local(binding) => {
                let reference = local_path(reference)?;
                if reference == "SKILL.md" {
                    return Ok(File {
                        reference,
                        sha256: binding.instruction_sha256.clone(),
                        bytes: binding.instruction_bytes,
                    });
                }
                let receipt: Receipt = serde_json::from_value(binding.receipt.clone())
                    .map_err(|_| invalid("bound skill has no supported file manifest"))?;
                if receipt.bundle.digest != binding.version || receipt.bundle.files.len() > 1024 {
                    return Err(invalid(
                        "bound skill file manifest differs from its version",
                    ));
                }
                let mut matches = receipt
                    .bundle
                    .files
                    .into_iter()
                    .filter(|file| file.path == reference);
                let file = matches
                    .next()
                    .ok_or_else(|| invalid("file is outside the original bundle manifest"))?;
                if matches.next().is_some() {
                    return Err(invalid("duplicate bound file path"));
                }
                Ok(File {
                    reference,
                    sha256: file.digest,
                    bytes: file.bytes,
                })
            }
        }
    }

    pub fn root_file(&self, reference: &str) -> bool {
        reference == self.root_reference()
    }

    pub fn verify_file(&self, file: &File, bytes: &[u8]) -> Result<(), CoreError> {
        if self.file(&file.reference)? != *file
            || bytes.len() as u64 != file.bytes
            || sha256(bytes) != file.sha256
        {
            return Err(invalid("bound skill file raw byte size/digest mismatch"));
        }
        if self.root_file(&file.reference) {
            self.verify(bytes)?;
        }
        Ok(())
    }
}

pub fn text(
    candidate: &SkillCandidate,
    reference: &str,
    content: &str,
) -> Result<String, CoreError> {
    let file = candidate.source.file(reference)?;
    candidate.source.verify_file(&file, content.as_bytes())?;
    Ok(format!("{}{content}", prefix(candidate, &file)?))
}

pub(crate) fn prefix(candidate: &SkillCandidate, file: &File) -> Result<String, CoreError> {
    let source = match &candidate.source {
        SkillSource::Mcp(held) => {
            json!({"kind":"mcp","origin":held.origin,"skill_uri":held.entry.uri,"entry_digest":held.entry_digest})
        }
        SkillSource::Local(binding) => {
            json!({"kind":"local","scope":binding.scope,"directory_id":binding.directory_id,"version":binding.version,"directory":binding.directory,"workspace":binding.workspace})
        }
    };
    Ok(format!(
        "Skill supporting file data. This read does not execute scripts or activate nested skills. The parent binding and existing permissions remain authoritative.\n{}\nBEGIN VERIFIED FILE DATA\n",
        json!({"parent_source_id":candidate.source.identity()?,"source":source,"file":file})
    ))
}

#[cfg(feature = "core")]
pub(crate) fn cached_bytes(selection: &SkillSelection) -> Result<u64, CoreError> {
    let mut total = 0_u64;
    for candidate in &selection.candidates {
        for (reference, content) in &candidate.files {
            total = total.saturating_add(text(candidate, reference, content)?.len() as u64);
        }
    }
    Ok(total)
}

pub fn declarations(state: &SessionSnapshot, turn: &AgentTurn) -> Result<Vec<Tool>, CoreError> {
    if !state
        .manifest
        .required_features
        .iter()
        .any(|feature| feature == super::FILES_FEATURE)
    {
        return Ok(Vec::new());
    }
    let Some(selection) = state
        .skill_selection(&turn.agent_turn_id)
        .filter(|selection| selection.file_limit_bytes.is_some())
    else {
        return Ok(Vec::new());
    };
    let mut ids = Vec::new();
    let mut sources = Vec::new();
    for candidate in &selection.candidates {
        if skills::candidate_for_model(state, selection, candidate)? && candidate.content.is_some()
        {
            let id = candidate.source.identity()?;
            sources.push(json!({"id":id,"name":candidate.source.name(),"source":candidate.source.selector()}));
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![Tool::Function {
        name: TOOL.into(),
        description: Some(format!(
            "Load one file from a selected skill's original manifest using its ID and a relative path or exact MCP URI. Returns a receipt; verified file data is supplied in the next model context. Does not execute files, activate nested skills, change versions or grant tools. Selected sources: {}",
            json!(sources)
        )),
        parameters: json!({"type":"object","properties":{"skill_id":{"type":"string","enum":ids},"reference":{"type":"string","maxLength":selection.file_limit_bytes}},"required":["skill_id","reference"],"additionalProperties":false}),
        strict: None,
        provider_metadata: Default::default(),
    }])
}

pub fn enabled(state: &SessionSnapshot) -> Result<(), CoreError> {
    if state
        .manifest
        .required_features
        .iter()
        .any(|feature| feature == super::FILES_FEATURE)
    {
        Ok(())
    } else {
        Err(CoreError::rejected(
            ErrorCode::UnsupportedCapability,
            "native skill files were not negotiated",
        ))
    }
}
