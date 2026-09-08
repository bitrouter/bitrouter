//! Immutable observations of a coding workspace. A stable filesystem read is
//! an artifact checkpoint, not proof of exclusive authorship or task success.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use super::types::{Artifact, MAX_RECORD_BYTES, MAX_RECORDS};
use crate::eval::types::canonical_digest;

const MAX_CONTENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileContent {
    mode: String,
    base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Workspace {
    pub cwd: Option<PathBuf>,
    pub repository: Option<PathBuf>,
    pub head: Option<String>,
    pub exclusions: BTreeSet<PathBuf>,
    files: BTreeMap<String, FileContent>,
    pub gaps: BTreeSet<String>,
}

impl Workspace {
    fn empty(cwd: Option<PathBuf>) -> Self {
        Self {
            cwd,
            repository: None,
            head: None,
            exclusions: BTreeSet::new(),
            files: BTreeMap::new(),
            gaps: BTreeSet::new(),
        }
    }

    pub(crate) fn from_artifact(artifact: &Artifact) -> Result<Self> {
        ensure!(
            artifact.kind == "workspace_snapshot/1"
                && artifact.digest == canonical_digest(&artifact.content)?
                && artifact.content.len() <= MAX_RECORD_BYTES,
            "invalid workspace artifact"
        );
        let workspace: Self = serde_json::from_str(&artifact.content)?;
        ensure!(
            artifact.attributes.is_empty(),
            "unexpected workspace artifact attributes"
        );
        ensure!(
            workspace.cwd.as_ref().is_none_or(|path| path.is_absolute())
                && workspace
                    .repository
                    .as_ref()
                    .is_none_or(|path| path.is_absolute())
                && workspace.exclusions.iter().all(|path| path.is_absolute()),
            "invalid workspace scope"
        );
        if workspace.gaps.is_empty() {
            let cwd = workspace
                .cwd
                .as_ref()
                .context("complete workspace cwd missing")?;
            let repository = workspace
                .repository
                .as_ref()
                .context("complete workspace repository missing")?;
            ensure!(
                cwd.starts_with(repository) && workspace.head.is_some(),
                "incomplete workspace scope"
            );
        }
        if let Some(head) = &workspace.head {
            ensure!(
                (head.len() == 40 || head.len() == 64)
                    && head.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "invalid workspace commit"
            );
        }
        ensure!(workspace.files.len() <= MAX_RECORDS, "workspace file limit");
        let mut size = 0;
        for (path, file) in &workspace.files {
            relative_path(path)?;
            ensure!(
                matches!(file.mode.as_str(), "100644" | "100755" | "120000"),
                "unknown workspace file mode"
            );
            let bytes = STANDARD.decode(&file.base64)?;
            ensure!(
                STANDARD.encode(&bytes) == file.base64 && bytes.len() <= MAX_FILE_BYTES,
                "invalid workspace file content"
            );
            size += bytes.len();
            ensure!(size <= MAX_CONTENT_BYTES, "workspace content limit");
        }
        Ok(workspace)
    }

    pub(crate) fn artifact(mut self) -> Result<Artifact> {
        let mut content = serde_json::to_string(&self)?;
        if content.len() > MAX_RECORD_BYTES {
            self.files.clear();
            self.gaps.insert("workspace_artifact_limit".into());
            content = serde_json::to_string(&self)?;
        }
        ensure!(
            content.len() <= MAX_RECORD_BYTES,
            "workspace artifact limit"
        );
        Ok(Artifact {
            kind: "workspace_snapshot/1".into(),
            digest: canonical_digest(&content)?,
            content,
            attributes: BTreeMap::new(),
        })
    }
}

pub(crate) async fn capture(
    cwd: Option<PathBuf>,
    exclusions: BTreeSet<PathBuf>,
    gaps: BTreeSet<String>,
) -> Result<Artifact> {
    let mut unavailable = Workspace::empty(cwd.filter(|path| path.is_absolute()));
    unavailable.exclusions = exclusions.clone();
    unavailable.gaps = gaps.clone();
    let Some(cwd) = unavailable.cwd.clone() else {
        unavailable
            .gaps
            .insert("workspace_scope_unavailable".into());
        return unavailable.artifact();
    };
    let captured = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let first = capture_once(&cwd, &exclusions).await?;
        let mut second = capture_once(&cwd, &exclusions).await?;
        if first != second {
            second
                .gaps
                .insert("workspace_changed_during_capture".into());
        }
        second.gaps.extend(gaps);
        // Serialization is bounded work, but must run off the async worker so
        // its timeout can still fire. An oversized encoding is partial evidence.
        tokio::task::spawn_blocking(move || second.artifact()).await?
    })
    .await;
    match captured {
        Ok(Ok(artifact)) => Ok(artifact),
        Ok(Err(error)) => {
            tracing::warn!(%error, "workspace evidence could not be captured");
            unavailable.gaps.insert("workspace_capture_failed".into());
            unavailable.artifact()
        }
        Err(_) => {
            unavailable.gaps.insert("workspace_capture_timeout".into());
            unavailable.artifact()
        }
    }
}

async fn capture_once(cwd: &Path, exclusions: &BTreeSet<PathBuf>) -> Result<Workspace> {
    ensure!(cwd.is_absolute(), "workspace cwd must be absolute");
    let cwd = tokio::fs::canonicalize(cwd).await?;
    // Read-only Git plumbing defines the tracked/unignored file boundary. No
    // checkout, index update, external diff driver or user hook is invoked.
    // https://git-scm.com/docs/git-rev-parse
    // https://git-scm.com/docs/git-ls-files
    let root = git(&cwd, &["rev-parse", "--show-toplevel"]).await?;
    let root = String::from_utf8(root).context("non-UTF8 repository root")?;
    let root = PathBuf::from(
        root.strip_suffix('\n')
            .context("repository root terminator")?,
    );
    let root = tokio::fs::canonicalize(root).await?;
    ensure!(
        cwd.starts_with(&root),
        "workspace cwd is outside repository"
    );
    let mut workspace = Workspace::empty(Some(cwd));
    workspace.repository = Some(root.clone());
    workspace.exclusions = exclusions.clone();
    let tracked = git(&root, &["ls-files", "-v", "-z", "--cached"]).await?;
    if tracked
        .split(|byte| *byte == 0)
        .any(|entry| matches!(entry.first(), Some(b'S' | b's')))
    {
        // Absent skip-worktree entries are not evidence of a deletion.
        workspace
            .gaps
            .insert("workspace_sparse_checkout_unavailable".into());
    }
    // With core.symlinks=false, a tracked symlink may be materialized as an
    // ordinary file. Filesystem modes alone cannot reconstruct its Git mode.
    // https://git-scm.com/docs/git-config#Documentation/git-config.txt-coresymlinks
    if git(
        &root,
        &[
            "config",
            "--type=bool",
            "--default",
            "true",
            "--get",
            "core.symlinks",
        ],
    )
    .await?
        == b"false\n"
    {
        workspace
            .gaps
            .insert("workspace_git_symlinks_unavailable".into());
    }
    #[cfg(not(unix))]
    workspace
        .gaps
        .insert("workspace_file_modes_unavailable".into());
    match git(&root, &["rev-parse", "--verify", "HEAD"]).await {
        Ok(head) => {
            let head = String::from_utf8(head)?;
            let head = head.trim_end_matches('\n');
            ensure!(
                (head.len() == 40 || head.len() == 64)
                    && head.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid Git object id"
            );
            workspace.head = Some(head.into());
        }
        Err(_) => {
            workspace.gaps.insert("workspace_head_unavailable".into());
        }
    }
    let paths = git(
        &root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )
    .await?;
    let mut names = BTreeSet::new();
    for path in paths
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
    {
        let path = std::str::from_utf8(path).context("non-UTF8 workspace path")?;
        relative_path(path)?;
        names.insert(path.to_owned());
        ensure!(names.len() <= MAX_RECORDS, "workspace file count limit");
    }
    let mut size = 0;
    for name in names {
        if exclusions
            .iter()
            .any(|excluded| root.join(&name).starts_with(excluded))
        {
            // Never recursively collect the evidence database or native logs.
            // An explicitly listed exclusion keeps coverage partial.
            workspace
                .gaps
                .insert("workspace_runtime_data_excluded".into());
            continue;
        }
        match read_file(&root, &name).await {
            Ok(Some((file, bytes))) if size + bytes <= MAX_CONTENT_BYTES => {
                size += bytes;
                workspace.files.insert(name, file);
            }
            Ok(None) => {} // A tracked deletion is represented by absence.
            Ok(Some(_)) => {
                workspace.gaps.insert("workspace_content_limit".into());
            }
            Err(error) => {
                tracing::debug!(%error, "workspace file could not be captured");
                workspace.gaps.insert("workspace_file_unavailable".into());
            }
        }
    }
    Ok(workspace)
}

fn relative_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 8192
            && Path::new(path)
                .components()
                .all(|part| matches!(part, Component::Normal(value) if value != ".git")),
        "invalid relative workspace path"
    );
    Ok(())
}

async fn read_file(root: &Path, relative: &str) -> Result<Option<(FileContent, usize)>> {
    let path = root.join(relative);
    let before = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let parent = tokio::fs::canonicalize(path.parent().context("workspace file parent")?).await?;
    ensure!(
        parent.starts_with(root) && path.parent() == Some(parent.as_path()),
        "workspace path traverses a symlink or leaves repository"
    );
    let (mode, bytes) = if before.file_type().is_symlink() {
        let target = tokio::fs::read_link(&path).await?;
        let target = target.to_str().context("non-UTF8 symlink target")?;
        ("120000", target.as_bytes().to_vec())
    } else {
        ensure!(
            before.is_file() && before.len() <= MAX_FILE_BYTES as u64,
            "unsupported workspace entry"
        );
        let mut file = tokio::fs::File::open(&path).await?;
        ensure!(
            same_file(&before, &file.metadata().await?),
            "workspace entry changed before open"
        );
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() <= MAX_FILE_BYTES && same_file(&before, &file.metadata().await?),
            "workspace file changed during read"
        );
        (file_mode(&before), bytes)
    };
    ensure!(
        same_file(&before, &tokio::fs::symlink_metadata(&path).await?)
            && tokio::fs::canonicalize(path.parent().context("workspace file parent")?).await?
                == parent,
        "workspace entry changed after read"
    );
    let size = bytes.len();
    ensure!(size <= MAX_FILE_BYTES, "workspace file size limit");
    Ok(Some((
        FileContent {
            mode: mode.into(),
            base64: STANDARD.encode(bytes),
        },
        size,
    )))
}

fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if left.dev() != right.dev() || left.ino() != right.ino() || left.mode() != right.mode() {
            return false;
        }
    }
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.file_type() == right.file_type()
}

fn file_mode(metadata: &std::fs::Metadata) -> &'static str {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o111 != 0 {
            return "100755";
        }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    "100644"
}

async fn git(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(cwd)
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(key);
    }
    let mut child = command.spawn()?;
    let mut stdout = child.stdout.take().context("Git stdout")?;
    let mut bytes = Vec::new();
    (&mut stdout)
        .take(MAX_COMMAND_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= MAX_COMMAND_BYTES, "Git output exceeds limit");
    ensure!(child.wait().await?.success(), "Git evidence command failed");
    Ok(bytes)
}

#[cfg(test)]
mod tests;
