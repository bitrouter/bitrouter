//! Immutable, content-addressed Goal artifacts and checked workspace transfer.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use bitrouter_orchestrator::goal::workflow::ArtifactRef;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;
const MAX_FILES: usize = 4096;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceBundle {
    base_commit: String,
    patch: ArtifactRef,
    files: Vec<NewFile>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewFile {
    path: String,
    content: ArtifactRef,
    executable: bool,
}

pub(super) async fn put(root: &Path, bytes: &[u8]) -> Result<ArtifactRef> {
    anyhow::ensure!(
        bytes.len() <= MAX_BLOB_BYTES,
        "Goal artifact exceeds byte bound"
    );
    tokio::fs::create_dir_all(root).await?;
    let root = tokio::fs::canonicalize(root).await?;
    let sha256 = hex::encode(Sha256::digest(bytes));
    let reference = ArtifactRef {
        path: root.join(format!("{sha256}.blob")),
        sha256,
        bytes: u64::try_from(bytes.len())?,
    };
    if tokio::fs::try_exists(&reference.path).await? {
        read(&root, &reference).await?;
        return Ok(reference);
    }
    let temporary = root.join(format!(".{}", uuid::Uuid::new_v4()));
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary).await?;
    let result = async {
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, &reference.path).await?;
        Result::<()>::Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result?;
    Ok(reference)
}

pub(super) async fn read(root: &Path, reference: &ArtifactRef) -> Result<Vec<u8>> {
    reference.validate()?;
    let root = tokio::fs::canonicalize(root).await?;
    anyhow::ensure!(
        reference.path == root.join(format!("{}.blob", reference.sha256)),
        "artifact is outside its Goal store"
    );
    let metadata = tokio::fs::symlink_metadata(&reference.path).await?;
    anyhow::ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() == reference.bytes
            && metadata.len() <= u64::try_from(MAX_BLOB_BYTES)?,
        "artifact metadata does not match its identity"
    );
    let bytes = tokio::fs::read(&reference.path).await?;
    anyhow::ensure!(
        bytes.len() <= MAX_BLOB_BYTES
            && u64::try_from(bytes.len())? == reference.bytes
            && hex::encode(Sha256::digest(&bytes)) == reference.sha256,
        "artifact content changed"
    );
    Ok(bytes)
}

fn relative(value: &str) -> Result<&Path> {
    let path = Path::new(value);
    anyhow::ensure!(
        !value.is_empty()
            && !path.is_absolute()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "artifact path escaped workspace"
    );
    Ok(path)
}

fn executable(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

pub(super) async fn capture(
    root: &Path,
    workspace: &Path,
    base_commit: &str,
) -> Result<ArtifactRef> {
    let patch = crate::goal_runtime::git(
        workspace,
        &[
            "diff".as_ref(),
            "--binary".as_ref(),
            "--no-ext-diff".as_ref(),
            "--no-textconv".as_ref(),
            base_commit.as_ref(),
            "--".as_ref(),
        ],
    )
    .await?
    .stdout;
    let patch_ref = put(root, &patch).await?;
    let names = crate::goal_runtime::git(
        workspace,
        &[
            "ls-files".as_ref(),
            "--others".as_ref(),
            "--exclude-standard".as_ref(),
            "-z".as_ref(),
        ],
    )
    .await?
    .stdout;
    let mut files = Vec::new();
    let mut total = patch.len();
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        anyhow::ensure!(
            files.len() < MAX_FILES,
            "workspace transfer exceeds file bound"
        );
        let path = std::str::from_utf8(name)?.to_string();
        let file = workspace.join(relative(&path)?);
        let metadata = tokio::fs::symlink_metadata(&file).await?;
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "untracked transfer must be a regular file"
        );
        anyhow::ensure!(
            metadata.len() <= u64::try_from(MAX_BLOB_BYTES)?,
            "transfer file exceeds byte bound"
        );
        total = total
            .checked_add(usize::try_from(metadata.len())?)
            .context("transfer byte count overflow")?;
        anyhow::ensure!(
            total <= 256 * 1024 * 1024,
            "workspace transfer exceeds total byte bound"
        );
        let bytes = tokio::fs::read(&file).await?;
        files.push(NewFile {
            path,
            content: put(root, &bytes).await?,
            executable: executable(&metadata),
        });
    }
    let after = crate::goal_runtime::git(
        workspace,
        &[
            "diff".as_ref(),
            "--binary".as_ref(),
            "--no-ext-diff".as_ref(),
            "--no-textconv".as_ref(),
            base_commit.as_ref(),
            "--".as_ref(),
        ],
    )
    .await?
    .stdout;
    anyhow::ensure!(
        patch == after,
        "workspace changed while capturing its artifact"
    );
    let names_after = crate::goal_runtime::git(
        workspace,
        &[
            "ls-files".as_ref(),
            "--others".as_ref(),
            "--exclude-standard".as_ref(),
            "-z".as_ref(),
        ],
    )
    .await?
    .stdout;
    anyhow::ensure!(names == names_after, "workspace file membership changed");
    for file in &files {
        let path = workspace.join(relative(&file.path)?);
        let metadata = tokio::fs::symlink_metadata(&path).await?;
        anyhow::ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && executable(&metadata) == file.executable
                && tokio::fs::read(path).await? == read(root, &file.content).await?,
            "workspace file changed while capturing its artifact"
        );
    }
    let bundle = WorkspaceBundle {
        base_commit: base_commit.into(),
        patch: patch_ref,
        files,
    };
    put(root, &serde_json::to_vec(&bundle)?).await
}

pub(super) async fn material(root: &Path, reference: &ArtifactRef) -> Result<serde_json::Value> {
    let bundle: WorkspaceBundle = serde_json::from_slice(&read(root, reference).await?)?;
    let patch = read(root, &bundle.patch).await?;
    anyhow::ensure!(
        patch.len() <= 256 * 1024,
        "workspace review requires a bounded patch"
    );
    let patch = String::from_utf8(patch).context("binary patch needs a capable reviewer")?;
    anyhow::ensure!(
        !patch.lines().any(|line| line == "GIT binary patch"
            || line.starts_with("Binary files ") && line.ends_with(" differ")),
        "binary changes require a capable reviewer"
    );
    let mut files = Vec::new();
    let mut total = patch.len();
    for file in &bundle.files {
        relative(&file.path)?;
        let content = read(root, &file.content).await?;
        total = total
            .checked_add(content.len())
            .context("review byte count overflow")?;
        anyhow::ensure!(total <= 512 * 1024, "workspace review exceeds byte bound");
        files.push(serde_json::json!({ "path": file.path, "sha256": file.content.sha256, "content": String::from_utf8(content).context("binary file needs a capable reviewer")? }));
    }
    Ok(serde_json::json!({ "base_commit": bundle.base_commit, "patch": patch, "new_files": files }))
}

pub(super) async fn apply(
    root: &Path,
    reference: &ArtifactRef,
    workspace: &Path,
    base_commit: &str,
) -> Result<()> {
    let bundle: WorkspaceBundle = serde_json::from_slice(&read(root, reference).await?)?;
    anyhow::ensure!(
        bundle.base_commit == base_commit,
        "dependency used a different workspace input version"
    );
    let patch = read(root, &bundle.patch).await?;
    if !patch.is_empty() {
        // Each bundle includes its dependency closure. Three-way application
        // merges common ancestors instead of replaying their hunks verbatim.
        // A conflict fails this private execution workspace before dispatch.
        // Send the hash-verified bytes, avoiding Git for Windows' handling of
        // canonical verbatim paths and a second read of the artifact file.
        crate::goal_runtime::git_with_input(
            workspace,
            &[
                "apply".as_ref(),
                "--3way".as_ref(),
                "--index".as_ref(),
                "--binary".as_ref(),
                "-".as_ref(),
            ],
            Some(&patch),
        )
        .await?;
    }
    anyhow::ensure!(
        bundle.files.len() <= MAX_FILES,
        "dependency exceeds file bound"
    );
    for file in &bundle.files {
        let relative = relative(&file.path)?;
        let parent = relative
            .parent()
            .context("missing dependency file parent")?;
        let mut directory = tokio::fs::canonicalize(workspace).await?;
        for component in parent.components() {
            directory.push(component.as_os_str());
            match tokio::fs::symlink_metadata(&directory).await {
                Ok(metadata) => anyhow::ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "dependency parent must be a real workspace directory"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::fs::create_dir(&directory).await?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        let name = relative
            .file_name()
            .context("missing dependency file name")?;
        let to = directory.join(name);
        let content = read(root, &file.content).await?;
        if let Ok(metadata) = tokio::fs::symlink_metadata(&to).await {
            anyhow::ensure!(
                metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && tokio::fs::read(&to).await? == content
                    && executable(&metadata) == file.executable,
                "dependency introduced conflicting file content"
            );
            continue;
        }
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        let mut output = options.open(&to).await?;
        output.write_all(&content).await?;
        output.sync_all().await?;
        #[cfg(unix)]
        if file.executable {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(to, std::fs::Permissions::from_mode(0o755)).await?;
        }
    }
    // Keep the index aligned for the next dependency's three-way merge.
    crate::goal_runtime::git(workspace, &["add".as_ref(), "-A".as_ref()]).await?;
    Ok(())
}

pub(super) fn root(workspaces: &Path, goal_id: &str) -> PathBuf {
    workspaces.join(goal_id).join("artifacts")
}

#[cfg(test)]
mod tests;
