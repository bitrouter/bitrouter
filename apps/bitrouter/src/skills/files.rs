//! Bounded directory manifests and verified reads; payload files are never executed.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(super) const MAX_BYTES: u64 = 32 * 1024 * 1024;
pub(super) const MAX_FILES: usize = 1024;
const MAX_SKILL_BYTES: u64 = 256 * 1024;
pub(super) const MAX_DIRECTORIES: usize = 4096;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct BundleFile {
    pub path: String,
    pub bytes: u64,
    pub digest: String,
    pub executable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Metadata {
    pub digest: String,
    pub bytes: u64,
    pub directories: Vec<String>,
    pub files: Vec<BundleFile>,
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

impl Metadata {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            valid_digest(&self.digest)
                && self.bytes <= MAX_BYTES
                && !self.files.is_empty()
                && self.files.len() <= MAX_FILES
                && self.directories.len() <= MAX_DIRECTORIES,
            "invalid bundle receipt limits"
        );
        ensure!(
            self.directories.windows(2).all(|pair| pair[0] < pair[1])
                && self
                    .files
                    .windows(2)
                    .all(|pair| pair[0].path < pair[1].path),
            "bundle receipt paths must be unique and sorted"
        );
        let mut names = BTreeSet::new();
        for directory in &self.directories {
            relative(directory)?;
            names.insert(directory.as_str());
        }
        let mut bytes = 0u64;
        let mut skill = false;
        for file in &self.files {
            relative(&file.path)?;
            ensure!(
                names.insert(&file.path) && valid_digest(&file.digest),
                "invalid file receipt"
            );
            bytes = bytes
                .checked_add(file.bytes)
                .context("bundle byte overflow")?;
            if file.path == "SKILL.md" {
                ensure!(file.bytes <= MAX_SKILL_BYTES, "SKILL.md exceeds 256 KiB");
                skill = true;
            }
        }
        ensure!(
            skill && bytes == self.bytes && digest(&self.directories, &self.files) == self.digest,
            "bundle receipt inventory or digest mismatch"
        );
        Ok(())
    }
}

pub(super) fn relative(path: &str) -> Result<PathBuf> {
    ensure!(
        !path.is_empty() && !path.contains(['\\', '\0', ':']),
        "invalid bundle path"
    );
    let path = PathBuf::from(path);
    ensure!(
        path.components()
            .all(|part| matches!(part, Component::Normal(name) if name != ".git")),
        "bundle paths must stay inside the bundle and exclude Git administrative data"
    );
    ensure!(
        path.to_string_lossy()
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."),
        "invalid bundle path component"
    );
    Ok(path)
}

fn digest(directories: &[String], files: &[BundleFile]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"bitrouter-skill-bundle-v1\0");
    for directory in directories {
        hash.update(b"directory\0");
        hash.update((directory.len() as u64).to_be_bytes());
        hash.update(directory.as_bytes());
    }
    for file in files {
        hash.update((file.path.len() as u64).to_be_bytes());
        hash.update(file.path.as_bytes());
        hash.update(file.bytes.to_be_bytes());
        hash.update([u8::from(file.executable)]);
        hash.update(file.digest.as_bytes());
    }
    hex::encode(hash.finalize())
}

pub(super) fn executable(metadata: &std::fs::Metadata) -> bool {
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

pub(super) fn read_regular(root: &Path, path: &Path, limit: u64) -> Result<Vec<u8>> {
    let before = std::fs::symlink_metadata(path)?;
    ensure!(
        before.is_file() && !before.is_symlink(),
        "bundle entries must be regular files, without symlinks"
    );
    ensure!(before.len() <= limit, "bundle file exceeds its byte limit");
    let canonical_root = root.canonicalize()?;
    ensure!(
        path.canonicalize()?.starts_with(&canonical_root),
        "bundle path escapes its source"
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A concurrent replacement with a FIFO must not block before fd validation.
        options.custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
        );
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            before.dev() == opened.dev() && before.ino() == opened.ino(),
            "bundle source changed while opening a file"
        );
    }
    ensure!(opened.is_file(), "bundle entries must be regular files");
    let mut data = Vec::new();
    file.take(limit + 1).read_to_end(&mut data)?;
    let after = std::fs::symlink_metadata(path)?;
    ensure!(
        data.len() as u64 <= limit
            && before.len() == after.len()
            && before.modified()? == after.modified()?
            && path.canonicalize()?.starts_with(canonical_root),
        "bundle source changed while reading"
    );
    Ok(data)
}

struct Inventory {
    directories: Vec<String>,
    files: Vec<(String, PathBuf, bool)>,
}

fn files(root: &Path) -> Result<Inventory> {
    let mut stack = vec![root.to_path_buf()];
    let mut output = Vec::new();
    let mut directories = Vec::new();
    while let Some(directory) = stack.pop() {
        let metadata = std::fs::symlink_metadata(&directory)?;
        ensure!(
            metadata.is_dir() && !metadata.is_symlink(),
            "bundle directories must not be symlinks"
        );
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_name() == ".git" {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            ensure!(!metadata.is_symlink(), "bundle symlinks are not supported");
            if metadata.is_dir() {
                let name = path
                    .strip_prefix(root)?
                    .components()
                    .map(|part| {
                        part.as_os_str()
                            .to_str()
                            .context("bundle directory names must be UTF-8")
                    })
                    .collect::<Result<Vec<_>>>()?
                    .join("/");
                relative(&name)?;
                ensure!(
                    directories.len() < MAX_DIRECTORIES,
                    "bundle exceeds 4096 directories"
                );
                directories.push(name);
                stack.push(path);
                continue;
            }
            ensure!(metadata.is_file(), "bundle entries must be regular files");
            ensure!(
                output.len() < MAX_FILES,
                "bundle exceeds 1024 regular files"
            );
            let name = path
                .strip_prefix(root)?
                .components()
                .map(|part| {
                    part.as_os_str()
                        .to_str()
                        .context("bundle filenames must be UTF-8")
                })
                .collect::<Result<Vec<_>>>()?
                .join("/");
            relative(&name)?;
            output.push((name, path, executable(&metadata)));
        }
    }
    output.sort_by(|a, b| a.0.cmp(&b.0));
    directories.sort();
    Ok(Inventory {
        directories,
        files: output,
    })
}

pub(super) fn inspect(root: &Path) -> Result<Metadata> {
    let mut bytes = 0u64;
    let mut entries = Vec::new();
    let Inventory { directories, files } = files(root)?;
    for (path, absolute, executable) in files {
        let limit = if path == "SKILL.md" {
            MAX_SKILL_BYTES.min(MAX_BYTES - bytes)
        } else {
            MAX_BYTES - bytes
        };
        let data = read_regular(root, &absolute, limit)?;
        bytes += data.len() as u64;
        entries.push(BundleFile {
            path,
            bytes: data.len() as u64,
            digest: hex::encode(Sha256::digest(&data)),
            executable,
        });
    }
    ensure!(
        entries.iter().any(|file| file.path == "SKILL.md"),
        "bundle has no SKILL.md"
    );
    Ok(Metadata {
        digest: digest(&directories, &entries),
        bytes,
        directories,
        files: entries,
    })
}
