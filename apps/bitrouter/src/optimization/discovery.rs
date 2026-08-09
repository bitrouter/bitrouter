//! Deterministic discovery of project-owned agent workflow entrypoints.
//!
//! Discovery proposes commands from repository facts. It never executes a
//! candidate, guesses a model/provider, or turns an ordinary unit-test target
//! into an optimization quality contract.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const EXECUTABLE_SEARCH_DIRS: &[&str] = &["", "scripts", "bin", "eval", "evals", "benchmarks"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowCandidateSource {
    PackageScript,
    Executable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowCandidate {
    pub id: String,
    pub label: String,
    pub command: Vec<String>,
    pub source: WorkflowCandidateSource,
    pub evidence: String,
    pub required_inputs: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuidedWorkflow {
    Resolved {
        command: Vec<String>,
        evidence: String,
    },
    Choose(Vec<WorkflowCandidate>),
    Missing,
}

/// Resolve an exact command when supplied; otherwise adopt a unique project
/// candidate and leave ambiguous/empty cases to the interactive CLI boundary.
pub fn resolve_guided_workflow(
    root: &Path,
    executable: Option<String>,
    arguments: Vec<String>,
) -> Result<GuidedWorkflow> {
    if let Some(executable) = executable {
        if executable.trim().is_empty() || executable.chars().any(char::is_control) {
            anyhow::bail!("--workflow-command must be a non-empty bounded executable");
        }
        let mut command = Vec::with_capacity(arguments.len() + 1);
        command.push(executable);
        command.extend(arguments);
        return Ok(GuidedWorkflow::Resolved {
            command,
            evidence: "explicit --workflow-command argv".into(),
        });
    }
    if !arguments.is_empty() {
        anyhow::bail!("--workflow-arg requires --workflow-command");
    }
    let candidates = discover_workflow_candidates(root)?;
    match candidates.as_slice() {
        [] => Ok(GuidedWorkflow::Missing),
        [candidate] => Ok(GuidedWorkflow::Resolved {
            command: candidate.command.clone(),
            evidence: candidate.evidence.clone(),
        }),
        _ => Ok(GuidedWorkflow::Choose(candidates)),
    }
}

/// Return bounded, deterministic workflow suggestions derived from `root`.
pub fn discover_workflow_candidates(root: &Path) -> Result<Vec<WorkflowCandidate>> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("resolving workflow project root {}", root.display()))?;
    if !root.is_dir() {
        anyhow::bail!("workflow project root must be a directory");
    }

    let mut candidates = discover_package_scripts(&root)?;
    candidates.extend(discover_executables(&root)?);
    candidates.sort_by(|left, right| left.id.cmp(&right.id));
    candidates.dedup_by(|left, right| left.id == right.id);
    Ok(candidates)
}

fn discover_package_scripts(root: &Path) -> Result<Vec<WorkflowCandidate>> {
    let package_path = root.join("package.json");
    let bytes = match fs::read(&package_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading workflow metadata {}", package_path.display()));
        }
    };
    if bytes.len() > 4 * 1024 * 1024 {
        anyhow::bail!("package.json exceeds the workflow discovery size limit");
    }
    let document: serde_json::Value =
        serde_json::from_slice(&bytes).context("parsing package.json for workflow discovery")?;
    let Some(scripts) = document
        .get("scripts")
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(Vec::new());
    };
    let manager = package_manager(root);
    let mut candidates = Vec::new();
    for (name, body) in scripts {
        if !semantic_workflow_name(name) || body.as_str().is_none_or(str::is_empty) {
            continue;
        }
        candidates.push(WorkflowCandidate {
            id: format!("package:{name}"),
            label: format!("package.json script `{name}`"),
            command: vec![manager.into(), "run".into(), name.clone()],
            source: WorkflowCandidateSource::PackageScript,
            evidence: format!("package.json declares the `{name}` script"),
            required_inputs: Vec::new(),
        });
    }
    Ok(candidates)
}

fn package_manager(root: &Path) -> &'static str {
    if root.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if root.join("yarn.lock").is_file() {
        "yarn"
    } else if root.join("bun.lock").is_file() || root.join("bun.lockb").is_file() {
        "bun"
    } else {
        "npm"
    }
}

fn discover_executables(root: &Path) -> Result<Vec<WorkflowCandidate>> {
    let mut candidates = Vec::new();
    for relative_dir in EXECUTABLE_SEARCH_DIRS {
        let directory = if relative_dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(relative_dir)
        };
        let directory_metadata = match fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "reading workflow directory metadata {}",
                        directory.display()
                    )
                });
            }
        };
        if !directory_metadata.file_type().is_dir() {
            continue;
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("reading workflow directory {}", directory.display())
                });
            }
        };
        for entry in entries {
            let entry = entry.with_context(|| {
                format!("enumerating workflow directory {}", directory.display())
            })?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if !metadata.file_type().is_file() || !is_executable(&metadata) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !semantic_workflow_name(&name) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(root)
                .context("workflow entrypoint escaped the project root")?
                .to_path_buf();
            let normalized = relative.to_string_lossy().replace('\\', "/");
            candidates.push(WorkflowCandidate {
                id: format!("executable:{normalized}"),
                label: format!("project executable `{normalized}`"),
                command: vec![format!("./{normalized}")],
                source: WorkflowCandidateSource::Executable,
                evidence: format!("{normalized} is an executable project file"),
                required_inputs: Vec::new(),
            });
        }
    }
    Ok(candidates)
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(windows)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    metadata.is_file()
}

fn semantic_workflow_name(name: &str) -> bool {
    name.split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .any(|part| {
            matches!(
                part.to_ascii_lowercase().as_str(),
                "eval" | "evaluation" | "benchmark" | "bench" | "quality" | "acceptance"
            )
        })
}
