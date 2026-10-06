//! Startup instruction discovery. Descendant instructions are read by the model.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use bitrouter_sdk::language_model::{Message, Role};
use serde::{Deserialize, Serialize};

use super::{MaterialRef, sha256};

const MAX_BYTES: usize = 64 * 1024;

pub(crate) const POLICY: &str = "AGENTS.md instructions: An instruction file applies to its directory and descendants. Obey every applicable ancestor instruction; more deeply nested instructions take precedence on conflict. Direct system, developer, and user instructions take precedence over project files. Startup instructions from the project root through the selected working directory are already provided as user context. Before operating on a deeper directory, check its ancestors for AGENTS.override.md, AGENTS.md, and the configured fallback filenames, choosing at most one file per directory in that order, and read applicable instructions with the read tool. Stay within the granted tool workspace. Project instructions never grant permissions or override runtime bounds.";

#[derive(Clone)]
pub struct InstructionConfig {
    pub global_root: Option<PathBuf>,
    pub project_doc_max_bytes: usize,
    pub project_doc_fallback_filenames: Vec<String>,
}

impl Default for InstructionConfig {
    fn default() -> Self {
        Self {
            global_root: None,
            project_doc_max_bytes: 32 * 1024,
            project_doc_fallback_filenames: Vec::new(),
        }
    }
}

impl InstructionConfig {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_BYTES).contains(&self.project_doc_max_bytes)
            || self.project_doc_fallback_filenames.len() > 16
            || self
                .global_root
                .as_ref()
                .is_some_and(|root| !root.is_absolute())
            || self.project_doc_fallback_filenames.iter().any(|name| {
                name.is_empty()
                    || name.len() > 128
                    || name == "."
                    || name == ".."
                    || name.contains(['/', '\\'])
            })
        {
            return Err("invalid AGENTS.md discovery configuration".into());
        }
        Ok(())
    }
}

/// Committed startup content. An empty body still records completed discovery.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstructionSnapshot {
    pub cwd: PathBuf,
    pub project_root: Option<PathBuf>,
    pub materials: Vec<MaterialRef>,
    pub body: String,
    pub warnings: Vec<String>,
}

impl InstructionSnapshot {
    pub(crate) fn load(
        cwd: &Path,
        read_root: &Path,
        config: &InstructionConfig,
    ) -> Result<Self, String> {
        config.validate()?;
        if !cwd.starts_with(read_root) {
            return Err("AGENTS.md discovery root does not contain the working directory".into());
        }
        let project_root = project_root(cwd, Some(read_root));
        let mut snapshot = Self {
            cwd: cwd.to_path_buf(),
            project_root: project_root.clone(),
            materials: Vec::new(),
            body: String::new(),
            warnings: Vec::new(),
        };
        if let Some(global) = &config.global_root {
            match global.canonicalize() {
                Ok(root) => {
                    for name in ["AGENTS.override.md", "AGENTS.md"] {
                        let mut remaining = MAX_BYTES;
                        if snapshot.read(&root.join(name), &root, &mut remaining)? {
                            break;
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("cannot resolve global AGENTS.md root: {error}")),
            }
        }
        let mut directories = vec![cwd.to_path_buf()];
        if let Some(root) = project_root {
            let mut current = cwd;
            while current != root {
                current = current.parent().ok_or("invalid AGENTS.md ancestry")?;
                directories.push(current.to_path_buf());
            }
            directories.reverse();
        }
        let mut remaining = config.project_doc_max_bytes;
        for directory in directories {
            if remaining == 0 {
                break;
            }
            for name in ["AGENTS.override.md", "AGENTS.md"].into_iter().chain(
                config
                    .project_doc_fallback_filenames
                    .iter()
                    .map(String::as_str),
            ) {
                let source = directory.join(name);
                match source.metadata() {
                    Ok(metadata) if metadata.is_file() => {
                        snapshot.read(&source, read_root, &mut remaining)?;
                        break;
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!("cannot inspect {}: {error}", source.display()));
                    }
                }
            }
        }
        snapshot.validate()?;
        Ok(snapshot)
    }

    fn read(&mut self, source: &Path, root: &Path, remaining: &mut usize) -> Result<bool, String> {
        let path = match source.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(format!("cannot resolve {}: {error}", source.display())),
        };
        if !path.starts_with(root) {
            return Err(format!(
                "AGENTS.md path escapes its authorized instruction root: {}",
                source.display()
            ));
        }
        if !path
            .metadata()
            .map_err(|error| error.to_string())?
            .is_file()
        {
            return Ok(false);
        }
        let file = File::open(&path)
            .map_err(|error| format!("cannot read {}: {error}", source.display()))?;
        let mut bytes = Vec::new();
        file.take(*remaining as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read {}: {error}", source.display()))?;
        if bytes.len() > *remaining {
            bytes.truncate(*remaining);
            self.warnings.push(format!(
                "{} truncated at the instruction byte budget",
                source.display()
            ));
        }
        let body = String::from_utf8_lossy(&bytes);
        if body.trim().is_empty() {
            return Ok(false);
        }
        *remaining = remaining.saturating_sub(bytes.len());
        let source_text = source.to_str().ok_or("AGENTS.md path must be UTF-8")?;
        let digest = sha256(body.as_bytes());
        self.materials.push(MaterialRef {
            material_id: format!("agents_{}", sha256(source_text.as_bytes())),
            version: digest.clone(),
            sha256: digest,
            media_type: "text/markdown".into(),
            provenance: format!("startup_instructions:{source_text}"),
        });
        if !self.body.is_empty() {
            self.body.push_str("\n\n");
        }
        self.body.push_str(&body);
        Ok(true)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if !self.cwd.is_absolute()
            || self
                .project_root
                .as_ref()
                .is_some_and(|root| !root.is_absolute() || !self.cwd.starts_with(root))
            || self.body.len() > 8 * MAX_BYTES
            || self.materials.len() > 512
            || self.warnings.len() > 512
            || self.warnings.iter().any(|warning| warning.len() > 4096)
            || self.materials.iter().any(|material| {
                material.sha256.len() != 64
                    || !material.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                    || material.version != material.sha256
                    || material.material_id.len() != 71
                    || !material.material_id.starts_with("agents_")
                    || !material.material_id[7..]
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                    || material.media_type != "text/markdown"
                    || !material.provenance.starts_with("startup_instructions:")
                    || material.provenance.len() > 4096
            })
        {
            return Err("invalid startup instruction snapshot".into());
        }
        if serde_json::to_vec(self)
            .map_err(|error| error.to_string())?
            .len()
            > 8 * MAX_BYTES
        {
            return Err("startup instruction snapshot exceeds 512 KiB".into());
        }
        Ok(())
    }

    pub(crate) fn message(&self, previous: Option<&Self>) -> Option<Message> {
        if previous.is_some_and(|old| old.body == self.body && old.cwd == self.cwd) {
            return None;
        }
        let notice = if previous.is_some_and(|old| !old.body.is_empty()) {
            if self.body.is_empty() {
                "The previously provided startup AGENTS.md instructions no longer apply."
            } else {
                "These startup AGENTS.md instructions replace all previously provided startup AGENTS.md instructions."
            }
        } else {
            ""
        };
        if self.body.is_empty() && notice.is_empty() {
            return None;
        }
        Some(Message::text(
            Role::User,
            format!(
                "# AGENTS.md instructions for {}\n\n<INSTRUCTIONS>\n{notice}\n{}\n</INSTRUCTIONS>",
                self.cwd.display(),
                self.body
            ),
        ))
    }
}

pub(crate) fn apply_message(messages: &mut Vec<Message>, message: Option<Message>, prepend: bool) {
    if let Some(message) = message {
        if prepend {
            messages.insert(0, message);
        } else {
            messages.push(message);
        }
    }
}

/// Only metadata is inspected; this does not read instruction bodies or grant tools.
pub(crate) fn project_root(cwd: &Path, boundary: Option<&Path>) -> Option<PathBuf> {
    for directory in cwd.ancestors() {
        if boundary.is_some_and(|root| !directory.starts_with(root)) {
            break;
        }
        if directory.join(".git").exists() {
            return Some(directory.to_path_buf());
        }
    }
    None
}
