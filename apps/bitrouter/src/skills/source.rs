//! Explicit local/Git acquisition. Git objects are read without checkout or filters.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use super::bundle::{self, Builder, Bundle};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Source {
    Local {
        path: PathBuf,
        skill_path: String,
    },
    Git {
        repository: String,
        requested_ref: Option<String>,
        resolved_ref: String,
        commit: String,
        tracking: bool,
        skill_path: String,
    },
}

impl Source {
    pub fn validate(&self) -> Result<()> {
        let skill_path = match self {
            Self::Local { path, skill_path } => {
                ensure!(
                    path.is_absolute(),
                    "local provenance must use an absolute path"
                );
                skill_path
            }
            Self::Git {
                repository,
                requested_ref,
                resolved_ref,
                commit,
                tracking,
                skill_path,
            } => {
                ensure!(
                    !repository.is_empty() && !repository.contains(['\r', '\n', '\0']),
                    "invalid Git provenance"
                );
                if repository.contains("://") {
                    validate_url(repository)?;
                } else {
                    ensure!(
                        Path::new(repository).is_absolute()
                            || (repository.starts_with("git@") && repository.contains(':')),
                        "Git provenance must use a URL or absolute repository path"
                    );
                }
                for reference in requested_ref.iter().chain(std::iter::once(resolved_ref)) {
                    ensure!(
                        !reference.is_empty()
                            && !reference.starts_with('-')
                            && !reference.contains(['\r', '\n', '\0']),
                        "invalid Git provenance ref"
                    );
                }
                ensure!(
                    is_oid(commit) && *tracking == resolved_ref.starts_with("refs/heads/"),
                    "invalid Git provenance commit/tracking policy"
                );
                skill_path
            }
        };
        if skill_path != "." {
            bundle::relative(skill_path)?;
        }
        Ok(())
    }
}

pub(super) struct Prepared {
    pub bundle: Bundle,
    pub source: Source,
}

async fn git(directory: Option<&Path>, args: &[OsString], limit: usize) -> Result<Vec<u8>> {
    let empty = tempfile::tempdir()?;
    let mut command = tokio::process::Command::new("git");
    command
        .args([
            "--no-pager",
            "--no-replace-objects",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.file.allow=always",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.http.allow=always",
            "-c",
            "protocol.ssh.allow=always",
            "-c",
            "protocol.git.allow=always",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "fetch.fsckObjects=true",
            "-c",
            "transfer.fsckObjects=true",
            "-c",
            "uploadpack.packObjectsHook=",
        ])
        .arg("-c")
        .arg(format!("core.hooksPath={}", empty.path().display()))
        .arg("-c")
        .arg(format!("init.templateDir={}", empty.path().display()));
    if let Some(directory) = directory {
        command.arg("-C").arg(directory);
    }
    for key in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_REPLACE_REF_BASE",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_TEMPLATE_DIR",
    ] {
        command.env_remove(key);
    }
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("Git is required for Git sources and Git-root resolution")?;
    let stdout = child.stdout.take().context("Git stdout")?;
    let operation = async {
        let mut bytes = Vec::new();
        stdout
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(bytes.len() <= limit, "Git output exceeds its limit");
        ensure!(
            child.wait().await?.success(),
            "Git operation failed; check repository access/ref (remote diagnostics omitted)"
        );
        Ok(bytes)
    };
    tokio::time::timeout(Duration::from_secs(120), operation)
        .await
        .context("Git operation timed out")?
}

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

pub(super) async fn project_root(cwd: &Path) -> Result<PathBuf> {
    if !cwd.ancestors().any(|path| path.join(".git").exists()) {
        return Ok(cwd.to_path_buf());
    }
    let output = git(
        Some(cwd),
        &args(&["rev-parse", "--show-toplevel"]),
        64 * 1024,
    )
    .await?;
    let root = PathBuf::from(std::str::from_utf8(&output)?.trim_end()).canonicalize()?;
    ensure!(
        cwd.starts_with(&root),
        "Git root lies outside the selected workspace"
    );
    Ok(root)
}

fn validate_url(source: &str) -> Result<()> {
    let url = url::Url::parse(source).context("invalid Git URL")?;
    ensure!(
        matches!(url.scheme(), "https" | "http" | "ssh" | "git" | "file"),
        "unsupported Git transport"
    );
    ensure!(
        url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && (!matches!(url.scheme(), "http" | "https") || url.username().is_empty()),
        "Git source URLs must not contain credentials/query/fragment; use Git authentication configuration"
    );
    Ok(())
}

fn repository(source: &str) -> Result<String> {
    let source = source.strip_prefix("git+").unwrap_or(source);
    ensure!(
        !source.starts_with('-') && !source.contains(['\n', '\r', '\0']),
        "invalid Git repository source"
    );
    if source.contains("://") {
        validate_url(source)?;
        Ok(source.into())
    } else if source.starts_with("git@") && source.contains(':') {
        Ok(source.into())
    } else {
        Ok(std::fs::canonicalize(source)
            .context("local Git repository does not exist")?
            .to_str()
            .context("Git repository path must be UTF-8")?
            .into())
    }
}

#[derive(Clone)]
struct Candidate {
    path: String,
    name: String,
    problem: Option<String>,
}

fn candidate(path: String, content: &[u8], expected: Option<&str>) -> Candidate {
    let parsed = (|| -> Result<String> {
        let content = std::str::from_utf8(content)?;
        let fm = bitrouter_orchestrator::harness::skills::format::parse_frontmatter(content)
            .map_err(|_| anyhow::anyhow!("invalid standard frontmatter"))?;
        fm.validate()
            .map_err(|_| anyhow::anyhow!("invalid skill name or description"))?;
        if let Some(expected) = expected {
            ensure!(fm.name == expected, "directory and skill name differ");
        }
        Ok(fm.name)
    })();
    match parsed {
        Ok(name) => Candidate {
            path,
            name,
            problem: None,
        },
        Err(_) => Candidate {
            name: expected.unwrap_or("root").into(),
            path,
            problem: Some(
                "invalid SKILL.md name/description/frontmatter or directory name mismatch".into(),
            ),
        },
    }
}

async fn choose(
    candidates: &[Candidate],
    selected: Option<&str>,
    interactive: bool,
) -> Result<Candidate> {
    ensure!(
        !candidates.is_empty(),
        "no regular SKILL.md found in the source"
    );
    if let Some(selected) = selected {
        let path = selected.strip_prefix("./");
        let matches: Vec<_> = candidates
            .iter()
            .filter(|c| {
                if let Some(path) = path {
                    c.path == path
                } else if selected == "." || selected.contains('/') {
                    c.path == selected
                } else {
                    c.name == selected
                }
            })
            .collect();
        ensure!(
            matches.len() == 1,
            "--skill is missing or ambiguous; use an explicit relative path such as --skill ./skills/name"
        );
        let found = matches[0];
        ensure!(
            found.problem.is_none(),
            "selected skill has invalid frontmatter or a directory/name mismatch"
        );
        return Ok(found.clone());
    }
    let index = if candidates.len() == 1 {
        0
    } else {
        ensure!(
            interactive && crate::prompt::terminal(),
            "multiple skills found; pass --skill NAME or ./relative/path in nonterminal use"
        );
        crate::prompt::select(
            "Skill bundle",
            "Choose exactly one bundle; no scripts are executed.",
            candidates
                .iter()
                .map(|c| {
                    crate::prompt::Item::new(
                        &c.name,
                        format!(
                            "{}{}",
                            c.path,
                            if c.problem.is_some() {
                                " (invalid)"
                            } else {
                                ""
                            }
                        ),
                    )
                })
                .collect(),
            0,
        )
        .await?
    };
    let selected = &candidates[index];
    ensure!(
        selected.problem.is_none(),
        "selected skill has invalid frontmatter or a directory/name mismatch"
    );
    Ok(selected.clone())
}

fn local_candidates(root: &Path, excluded: &Path) -> Result<Vec<Candidate>> {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut candidates = Vec::new();
    let mut seen = 0;
    while let Some((directory, depth)) = stack.pop() {
        seen += 1;
        ensure!(
            seen <= 4096 && depth <= 32,
            "skill discovery exceeds directory/depth limits"
        );
        let skill = directory.join("SKILL.md");
        if let Ok(metadata) = std::fs::symlink_metadata(&skill) {
            ensure!(
                metadata.is_file() && !metadata.is_symlink(),
                "SKILL.md must be a regular file, without symlinks"
            );
            let bytes = bundle::read_regular(root, &skill, 256 * 1024)?;
            let relative = directory
                .strip_prefix(root)?
                .components()
                .map(|part| {
                    part.as_os_str()
                        .to_str()
                        .context("source path must be UTF-8")
                })
                .collect::<Result<Vec<_>>>()?
                .join("/");
            candidates.push(candidate(
                if relative.is_empty() {
                    ".".into()
                } else {
                    relative
                },
                &bytes,
                directory.file_name().and_then(OsStr::to_str),
            ));
            ensure!(candidates.len() <= 256, "source exceeds 256 skills");
            continue;
        }
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_name() == ".git" || path == excluded {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_dir() && !metadata.is_symlink() {
                stack.push((path, depth + 1));
            }
        }
    }
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(candidates)
}

fn is_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|c| c.is_ascii_hexdigit())
}

async fn git_bundle(
    repository: String,
    requested_ref: Option<String>,
    selected: Option<&str>,
    interactive: bool,
) -> Result<Prepared> {
    let advertised = git(
        None,
        &[
            "ls-remote".into(),
            "--symref".into(),
            "--".into(),
            repository.clone().into(),
        ],
        4 * 1024 * 1024,
    )
    .await?;
    let advertised = std::str::from_utf8(&advertised)?;
    let mut references = std::collections::BTreeMap::new();
    let mut head = None;
    for line in advertised.lines() {
        if let Some((oid, name)) = line.split_once('\t') {
            if let Some(reference) = oid.strip_prefix("ref: ") {
                if name == "HEAD" {
                    head = Some(reference.to_owned());
                }
            } else if is_oid(oid) {
                references.insert(name.to_owned(), oid.to_owned());
            }
        }
    }
    let requested = requested_ref.as_deref().unwrap_or("HEAD");
    ensure!(
        !requested.is_empty()
            && !requested.starts_with('-')
            && !requested.contains(['\r', '\n', '\0']),
        "invalid Git ref"
    );
    let resolved = if requested == "HEAD" {
        head.unwrap_or_else(|| "HEAD".into())
    } else if is_oid(requested)
        || requested.starts_with("refs/heads/")
        || requested.starts_with("refs/tags/")
    {
        requested.into()
    } else {
        let branch = format!("refs/heads/{requested}");
        let tag = format!("refs/tags/{requested}");
        ensure!(
            references.contains_key(&branch) != references.contains_key(&tag),
            "Git ref is missing or ambiguous between branch/tag; use a full ref"
        );
        if references.contains_key(&branch) {
            branch
        } else {
            tag
        }
    };
    let expected = if is_oid(&resolved) {
        Some(resolved.clone())
    } else {
        references
            .get(&format!("{resolved}^{{}}"))
            .or_else(|| references.get(&resolved))
            .cloned()
    };
    ensure!(expected.is_some(), "Git ref was not advertised");
    let sandbox = tempfile::tempdir()?;
    git(
        None,
        &[
            "init".into(),
            "--bare".into(),
            sandbox.path().as_os_str().to_owned(),
        ],
        64 * 1024,
    )
    .await?;
    git(
        Some(sandbox.path()),
        &[
            "fetch".into(),
            "--no-tags".into(),
            "--depth=1".into(),
            "--".into(),
            repository.clone().into(),
            resolved.clone().into(),
        ],
        64 * 1024,
    )
    .await?;
    let commit = String::from_utf8(
        git(
            Some(sandbox.path()),
            &args(&["rev-parse", "--verify", "FETCH_HEAD^{commit}"]),
            1024,
        )
        .await?,
    )?
    .trim()
    .to_owned();
    ensure!(
        is_oid(&commit) && expected.as_deref() == Some(commit.as_str()),
        "Git ref moved during acquisition; retry explicitly"
    );
    let tree = git(
        Some(sandbox.path()),
        &[
            "ls-tree".into(),
            "-r".into(),
            "-t".into(),
            "-z".into(),
            "--long".into(),
            commit.clone().into(),
        ],
        4 * 1024 * 1024,
    )
    .await?;
    let mut entries = Vec::new();
    for item in tree
        .split(|byte| *byte == 0)
        .filter(|item| !item.is_empty())
    {
        let item = std::str::from_utf8(item).context("Git paths must be UTF-8")?;
        let (header, path) = item.split_once('\t').context("invalid Git tree entry")?;
        let fields: Vec<_> = header.split_whitespace().collect();
        ensure!(fields.len() == 4, "invalid Git tree metadata");
        entries.push((
            path.to_string(),
            fields[0].to_string(),
            fields[2].to_string(),
            fields[3].parse::<u64>().ok(),
        ));
    }
    let mut candidates = Vec::new();
    let mut roots = Vec::<String>::new();
    let mut skill_files: Vec<_> = entries
        .iter()
        .filter(|(path, _, _, _)| path == "SKILL.md" || path.ends_with("/SKILL.md"))
        .collect();
    skill_files.sort_by_key(|(path, _, _, _)| path.matches('/').count());
    for (path, mode, oid, size) in skill_files {
        let parent = path
            .strip_suffix("SKILL.md")
            .unwrap_or_default()
            .trim_end_matches('/');
        if roots
            .iter()
            .any(|root| root.is_empty() || parent.starts_with(&format!("{root}/")))
        {
            continue;
        }
        ensure!(
            mode == "100644" || mode == "100755",
            "Git SKILL.md must be a regular blob"
        );
        ensure!(
            size.is_some_and(|size| size <= 256 * 1024),
            "Git SKILL.md exceeds 256 KiB"
        );
        let bytes = git(
            Some(sandbox.path()),
            &["cat-file".into(), "blob".into(), oid.clone().into()],
            256 * 1024,
        )
        .await?;
        candidates.push(candidate(
            if parent.is_empty() {
                ".".into()
            } else {
                parent.into()
            },
            &bytes,
            if parent.is_empty() {
                None
            } else {
                parent.rsplit('/').next()
            },
        ));
        roots.push(parent.into());
        ensure!(candidates.len() <= 256, "source exceeds 256 skills");
    }
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    let chosen = choose(&candidates, selected, interactive).await?;
    let prefix = if chosen.path == "." {
        String::new()
    } else {
        format!("{}/", chosen.path)
    };
    let selected: Vec<_> = entries
        .into_iter()
        .filter(|(path, _, _, _)| path.starts_with(&prefix))
        .collect();
    ensure!(
        selected
            .iter()
            .filter(|(_, mode, _, _)| mode != "040000")
            .count()
            <= bundle::MAX_FILES,
        "Git bundle exceeds 1024 files"
    );
    ensure!(
        selected
            .iter()
            .filter(|(_, mode, _, _)| mode == "040000")
            .count()
            <= bundle::MAX_DIRECTORIES,
        "Git bundle exceeds 4096 directories"
    );
    let mut total = 0u64;
    for (_, mode, _, size) in &selected {
        if mode == "040000" {
            continue;
        }
        ensure!(
            mode == "100644" || mode == "100755",
            "Git bundle contains a symlink, submodule or special entry"
        );
        let size = size.context("Git blob size missing")?;
        total = total
            .checked_add(size)
            .context("Git bundle size overflow")?;
        ensure!(total <= bundle::MAX_BYTES, "Git bundle exceeds 32 MiB");
    }
    let mut builder = Builder::new()?;
    for (path, mode, oid, size) in selected {
        let relative = path.strip_prefix(&prefix).context("Git bundle prefix")?;
        bundle::relative(relative)?;
        if mode == "040000" {
            builder.directory(relative)?;
            continue;
        }
        let bytes = git(
            Some(sandbox.path()),
            &["cat-file".into(), "blob".into(), oid.into()],
            size.context("blob size")? as usize,
        )
        .await?;
        builder.push(relative, &bytes, mode == "100755")?;
    }
    let bundle = builder.finish(Some(&chosen.name))?;
    Ok(Prepared {
        bundle,
        source: Source::Git {
            repository,
            requested_ref,
            resolved_ref: resolved.clone(),
            commit,
            tracking: resolved.starts_with("refs/heads/"),
            skill_path: chosen.path,
        },
    })
}

pub(super) async fn prepare(
    source: &str,
    selected: Option<&str>,
    reference: Option<String>,
    excluded: &Path,
    interactive: bool,
) -> Result<Prepared> {
    let git_source = source.starts_with("git+")
        || source.contains("://")
        || source.starts_with("git@")
        || reference.is_some();
    if git_source {
        return git_bundle(repository(source)?, reference, selected, interactive).await;
    }
    let root = PathBuf::from(source)
        .canonicalize()
        .context("local skill source does not exist; use a directory or Git URL")?;
    ensure!(root.is_dir(), "local skill source must be a directory");
    let candidates = local_candidates(&root, excluded)?;
    let chosen = choose(&candidates, selected, interactive).await?;
    let selected_root = if chosen.path == "." {
        root.clone()
    } else {
        root.join(bundle::relative(&chosen.path)?)
    };
    let bundle = bundle::snapshot(&selected_root, excluded)?;
    Ok(Prepared {
        bundle,
        source: Source::Local {
            path: root,
            skill_path: chosen.path,
        },
    })
}

pub(super) async fn refresh(
    source: &Source,
    reference: Option<String>,
    excluded: &Path,
) -> Result<Option<Prepared>> {
    match source {
        Source::Local { path, skill_path } => {
            ensure!(
                reference.is_none(),
                "--ref is only supported for Git sources"
            );
            let selector = format!("./{skill_path}");
            prepare(
                path.to_str().context("source path must be UTF-8")?,
                Some(&selector),
                None,
                excluded,
                false,
            )
            .await
            .map(Some)
        }
        Source::Git {
            repository,
            requested_ref,
            resolved_ref,
            tracking,
            skill_path,
            ..
        } => {
            if !tracking
                && reference
                    .as_ref()
                    .is_none_or(|r| Some(r) == requested_ref.as_ref() || r == resolved_ref)
            {
                return Ok(None);
            }
            let selector = format!("./{skill_path}");
            let explicit = reference.clone();
            let mut prepared = git_bundle(
                repository.clone(),
                reference.or_else(|| Some(resolved_ref.clone())),
                Some(&selector),
                false,
            )
            .await?;
            if explicit.is_none()
                && let Source::Git {
                    requested_ref: ref mut next,
                    ..
                } = prepared.source
            {
                *next = requested_ref.clone();
            }
            Ok(Some(prepared))
        }
    }
}
