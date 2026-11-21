//! Build-time provenance for BitRouter product packages and release archives.
#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Revision embedded in a binary and the Cargo inputs that determine it.
pub struct SourceRevision {
    pub value: Option<String>,
    pub rerun_paths: Vec<PathBuf>,
}

impl SourceRevision {
    fn watch(&mut self, path: &Path) -> std::io::Result<()> {
        // Cargo recursively watches directories. For an absent input, watch its
        // nearest existing parent to detect creation without a permanently dirty
        // missing-file watch. The next run switches to the exact input path.
        // https://doc.rust-lang.org/cargo/reference/build-scripts.html#rerun-if-changed
        for candidate in path.ancestors() {
            match std::fs::metadata(candidate) {
                Ok(metadata) if candidate == path || metadata.is_dir() => {
                    if !self.rerun_paths.iter().any(|watched| watched == candidate) {
                        self.rerun_paths.push(candidate.to_owned());
                    }
                    return Ok(());
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "build metadata input has no existing parent directory",
        ))
    }
}

/// An explicit release override wins over packaged VCS data and local Git.
pub fn source_revision(
    manifest: &Path,
    explicit: Option<&str>,
) -> Result<SourceRevision, Box<dyn Error>> {
    let vcs_file = manifest.join(".cargo_vcs_info.json");
    let mut revision = SourceRevision {
        value: None,
        rerun_paths: Vec::new(),
    };
    revision.watch(&vcs_file)?;
    if let Some(explicit) = explicit {
        revision.value = Some(validate_revision(explicit)?);
        return Ok(revision);
    }

    // cargo package verification builds in an extracted package, which may
    // still be inside another checkout. Its package-local VCS data wins.
    // https://doc.rust-lang.org/cargo/commands/cargo-package.html#cargo_vcs_infojson-format
    match std::fs::read_to_string(vcs_file) {
        Ok(contents) => {
            let vcs: serde_json::Value = serde_json::from_str(&contents)?;
            let sha = vcs["git"]["sha1"]
                .as_str()
                .ok_or("packaged VCS information has no Git revision")?;
            revision.value = Some(validate_revision(sha)?);
            return Ok(revision);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    // Do not attribute an untracked archive/package to an enclosing Git repo.
    // https://git-scm.com/docs/git-ls-files#Documentation/git-ls-files.txt---error-unmatch
    if git(
        manifest,
        &["ls-files", "--error-unmatch", "--", "Cargo.toml"],
    )
    .is_none()
    {
        return Ok(revision);
    }
    if let Some(root) = git(manifest, &["rev-parse", "--show-toplevel"]) {
        let dot_git = Path::new(&root).join(".git");
        if dot_git.is_file() {
            revision.watch(&dot_git)?;
        }
    }
    // Let Git resolve worktree/common-dir indirection. Track HEAD, each symbolic
    // ref in its chain and packed-refs. Absent refs watch their nearest existing
    // parent so a future loose ref invalidates a previously packed HEAD.
    // https://git-scm.com/docs/git-rev-parse#Documentation/git-rev-parse.txt---git-pathltpathgt
    // https://git-scm.com/docs/gitrepository-layout
    // https://doc.rust-lang.org/cargo/reference/build-scripts.html#rerun-if-changed
    for name in ["commondir", "packed-refs"] {
        if let Some(path) = git_path(manifest, name)
            && (name == "packed-refs" || path.exists())
        {
            revision.watch(&path)?;
        }
    }
    let mut reference = "HEAD".to_owned();
    let mut visited = BTreeSet::new();
    while visited.insert(reference.clone()) {
        if let Some(path) = git_path(manifest, &reference) {
            revision.watch(&path)?;
        }
        match git(
            manifest,
            &["symbolic-ref", "--quiet", "--no-recurse", &reference],
        ) {
            Some(next) => reference = next,
            None => break,
        }
    }
    if let Some(sha) = git(manifest, &["rev-parse", "--verify", "HEAD^{commit}"]) {
        revision.value = Some(validate_revision(&sha)?);
    }
    Ok(revision)
}

fn validate_revision(revision: &str) -> Result<String, Box<dyn Error>> {
    // Accept conventional short commit IDs as well as complete SHA-1/SHA-256
    // IDs. Reject labels, whitespace, empty overrides and output injection.
    if !(7..=64).contains(&revision.len()) || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("build revision must contain 7 to 64 hexadecimal characters".into());
    }
    Ok(revision.to_ascii_lowercase())
}

fn git_path(manifest: &Path, name: &str) -> Option<PathBuf> {
    git(
        manifest,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    )
    .map(PathBuf::from)
}

fn git(manifest: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(manifest)
        // Identity must describe this checkout even when invoked by a Git hook.
        // https://git-scm.com/docs/git#Documentation/git.txt-codeGITDIRcode
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
}
