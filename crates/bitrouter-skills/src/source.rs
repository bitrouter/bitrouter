//! Parsing a skill source string into a [`SkillSource`] and fetching it onto
//! disk.
//!
//! Accepted forms (auto-detected, mirroring the wider ecosystem):
//!
//! - `owner/repo` — GitHub shorthand → [`SkillSource::Github`]
//! - `https://github.com/owner/repo[.git]` → [`SkillSource::Github`]
//! - `https://github.com/owner/repo/tree/<ref>/<subdir>` → [`SkillSource::GitSubdir`]
//! - any other `https://…` / `git://…` / `git@…` URL → [`SkillSource::Url`]
//!
//! A trailing `#<ref>` fragment on a shorthand or plain URL selects a branch,
//! tag, or commit.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// A resolved, fetchable skill source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    /// A GitHub repository referenced by `owner/repo`.
    Github {
        owner: String,
        repo: String,
        git_ref: Option<String>,
    },
    /// Any git-cloneable URL that is not a recognised GitHub repo.
    Url {
        url: String,
        git_ref: Option<String>,
    },
    /// A sub-directory within a git repository.
    GitSubdir {
        url: String,
        path: String,
        git_ref: Option<String>,
    },
    /// A local directory on disk (the tree is copied, not cloned).
    Local { path: PathBuf },
}

/// Split a trailing `#<ref>` fragment off a shorthand/URL.
fn split_ref(input: &str) -> (&str, Option<String>) {
    match input.split_once('#') {
        Some((head, frag)) if !frag.is_empty() => (head, Some(frag.to_string())),
        _ => (input, None),
    }
}

/// Build the canonical HTTPS clone URL for a GitHub repo.
pub(crate) fn github_clone_url(owner: &str, repo: &str) -> String {
    format!("https://github.com/{owner}/{repo}.git")
}

/// Whether a subdirectory path is safe to join onto a clone root: no `..`
/// components and not absolute. Guards against a crafted source escaping the
/// temporary clone directory. Enforced both at parse time and again at the
/// join site in `install`.
pub(crate) fn subdir_is_safe(subdir: &str) -> bool {
    !subdir.is_empty()
        && !subdir.starts_with('/')
        && !subdir.split(['/', '\\']).any(|seg| seg == "..")
}

/// Interpret a `github.com` URL path as either a whole repo or a subdirectory
/// (`/owner/repo/tree/<ref>/<subdir…>`). An explicit `#<ref>` fragment (`frag`)
/// takes precedence over a `tree/<ref>` path segment.
fn github_from_path(segments: &[&str], frag: Option<String>) -> Option<SkillSource> {
    let owner = (*segments.first()?).to_string();
    let repo = segments.get(1)?.trim_end_matches(".git").to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    // /owner/repo/tree/<ref>/<subdir...>  or  /blob/...
    if matches!(segments.get(2), Some(&"tree") | Some(&"blob"))
        && let Some(tree_ref) = segments.get(3)
    {
        let git_ref = frag.or_else(|| Some((*tree_ref).to_string()));
        let subdir = segments[4..].join("/");
        if subdir.is_empty() {
            return Some(SkillSource::Github {
                owner,
                repo,
                git_ref,
            });
        }
        if !subdir_is_safe(&subdir) {
            return None;
        }
        return Some(SkillSource::GitSubdir {
            url: github_clone_url(&owner, &repo),
            path: subdir,
            git_ref,
        });
    }
    Some(SkillSource::Github {
        owner,
        repo,
        git_ref: frag,
    })
}

/// Expand a leading `~` / `~/` in a local path to the home directory.
fn expand_home(path: &str) -> Result<PathBuf> {
    if path == "~" {
        return crate::home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(crate::home_dir()?.join(rest));
    }
    Ok(PathBuf::from(path))
}

/// Whether a source string denotes a local filesystem path rather than a git
/// source or shorthand. Only explicit path forms qualify, so `owner/repo` is
/// never mistaken for a path.
fn looks_local(s: &str) -> bool {
    s == "."
        || s == ".."
        || s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with('/')
        || s.starts_with("~/")
        || s == "~"
        || (cfg!(windows) && s.starts_with(".\\"))
}

/// Parse a source string into a [`SkillSource`].
pub fn parse_source(input: &str) -> Result<SkillSource> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidSource("empty source".to_string()));
    }

    // Local filesystem paths (explicit `./`, `../`, `/`, `~/` forms).
    if looks_local(trimmed) {
        return Ok(SkillSource::Local {
            path: expand_home(trimmed)?,
        });
    }

    // Plain-HTTP sources are refused: a skill is executable content, so an
    // on-path attacker must not be able to swap it via an unauthenticated
    // channel. Require `https://`.
    if trimmed.starts_with("http://") {
        return Err(Error::InvalidSource(format!(
            "plain-HTTP sources are not allowed (use https://): {trimmed}"
        )));
    }

    // Split a trailing `#<ref>` once, up-front, so every branch handles it
    // uniformly (the fragment was previously dropped for github subdir URLs).
    let (head, frag) = split_ref(trimmed);

    // Explicit HTTPS URLs.
    if let Some(rest) = head.strip_prefix("https://") {
        let mut host_and_path = rest.splitn(2, '/');
        let host = host_and_path.next().unwrap_or_default();
        let path = host_and_path.next().unwrap_or_default();
        if host.eq_ignore_ascii_case("github.com") {
            let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
            return github_from_path(&segments, frag)
                .ok_or_else(|| Error::InvalidSource(format!("not a GitHub repo URL: {trimmed}")));
        }
        return Ok(SkillSource::Url {
            url: head.to_string(),
            git_ref: frag,
        });
    }

    // scp-style git remotes (`git@host:owner/repo.git`) and `git://` URLs.
    if head.starts_with("git@") || head.starts_with("git://") {
        return Ok(SkillSource::Url {
            url: head.to_string(),
            git_ref: frag,
        });
    }

    // Reject any other explicit scheme (e.g. `file://`, `ssh://`-less typos).
    if head.contains("://") {
        return Err(Error::InvalidSource(format!(
            "unsupported scheme: {trimmed}"
        )));
    }

    // Bare `github.com/owner/repo[/tree/<ref>/<subdir>]` (no scheme).
    if let Some(path) = head.strip_prefix("github.com/") {
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        return github_from_path(&segments, frag)
            .ok_or_else(|| Error::InvalidSource(format!("not a GitHub repo: {trimmed}")));
    }

    // GitHub shorthand `owner/repo` (exactly two non-empty segments).
    let segments: Vec<&str> = head.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() == 2 {
        return Ok(SkillSource::Github {
            owner: segments[0].to_string(),
            repo: segments[1].trim_end_matches(".git").to_string(),
            git_ref: frag,
        });
    }

    Err(Error::InvalidSource(format!(
        "unrecognised source {trimmed:?}; expected owner/repo, a git URL, or a github.com URL"
    )))
}

impl SkillSource {
    /// The branch/tag/commit selected, if any.
    fn git_ref(&self) -> Option<&str> {
        match self {
            SkillSource::Github { git_ref, .. }
            | SkillSource::Url { git_ref, .. }
            | SkillSource::GitSubdir { git_ref, .. } => git_ref.as_deref(),
            SkillSource::Local { .. } => None,
        }
    }

    /// The clone URL for git sources.
    fn clone_url(&self) -> Option<String> {
        match self {
            SkillSource::Github { owner, repo, .. } => Some(github_clone_url(owner, repo)),
            SkillSource::Url { url, .. } | SkillSource::GitSubdir { url, .. } => Some(url.clone()),
            SkillSource::Local { .. } => None,
        }
    }

    /// The local path, when this is a [`SkillSource::Local`].
    pub(crate) fn local_path(&self) -> Option<&Path> {
        match self {
            SkillSource::Local { path } => Some(path.as_path()),
            _ => None,
        }
    }

    /// The subdirectory within the fetched tree that holds the skill, if the
    /// source narrows to one.
    pub(crate) fn subdir(&self) -> Option<&str> {
        match self {
            SkillSource::GitSubdir { path, .. } => Some(path.as_str()),
            _ => None,
        }
    }

    /// Clone this source into `dest_dir` via the system `git` binary. A shallow
    /// clone is used; a `git_ref` selects the branch/tag. Errors for a
    /// [`SkillSource::Local`] source, which is copied rather than cloned.
    pub async fn clone_into(&self, dest_dir: &Path) -> Result<()> {
        let url = self
            .clone_url()
            .ok_or_else(|| Error::Git("local sources are copied, not cloned".to_string()))?;
        // Reject any ref or URL that would be parsed as a git option (e.g.
        // `#--upload-pack=…`), which would otherwise be an argument-injection
        // path to arbitrary command execution.
        if url.starts_with('-') {
            return Err(Error::Git(format!(
                "refusing clone URL starting with '-': {url}"
            )));
        }
        let mut cmd = tokio::process::Command::new("git");
        cmd.arg("clone").arg("--depth").arg("1");
        if let Some(git_ref) = self.git_ref() {
            if git_ref.starts_with('-') {
                return Err(Error::Git(format!(
                    "refusing git ref starting with '-': {git_ref}"
                )));
            }
            cmd.arg("--branch").arg(git_ref);
        }
        // `--` ends option parsing so the URL and destination can never be
        // interpreted as flags.
        cmd.arg("--");
        cmd.arg(url);
        cmd.arg(dest_dir);
        let output = cmd
            .output()
            .await
            .map_err(|e| Error::Git(format!("running git clone: {e}")))?;
        if !output.status.success() {
            return Err(Error::Git(format!(
                "git clone failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorthand_owner_repo() {
        let s = parse_source("vercel-labs/skills").unwrap();
        assert_eq!(
            s,
            SkillSource::Github {
                owner: "vercel-labs".to_string(),
                repo: "skills".to_string(),
                git_ref: None,
            }
        );
    }

    #[test]
    fn shorthand_with_ref() {
        let s = parse_source("owner/repo#v1.2.0").unwrap();
        assert_eq!(
            s,
            SkillSource::Github {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                git_ref: Some("v1.2.0".to_string()),
            }
        );
    }

    #[test]
    fn shorthand_strips_git_suffix() {
        let s = parse_source("owner/repo.git").unwrap();
        assert_eq!(
            s,
            SkillSource::Github {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                git_ref: None,
            }
        );
    }

    #[test]
    fn full_github_url() {
        let s = parse_source("https://github.com/owner/repo").unwrap();
        assert_eq!(
            s,
            SkillSource::Github {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                git_ref: None,
            }
        );
    }

    #[test]
    fn full_github_url_with_dot_git() {
        let s = parse_source("https://github.com/owner/repo.git").unwrap();
        assert!(matches!(s, SkillSource::Github { repo, .. } if repo == "repo"));
    }

    #[test]
    fn github_tree_subdir() {
        let s = parse_source("https://github.com/owner/repo/tree/main/skills/foo").unwrap();
        assert_eq!(
            s,
            SkillSource::GitSubdir {
                url: "https://github.com/owner/repo.git".to_string(),
                path: "skills/foo".to_string(),
                git_ref: Some("main".to_string()),
            }
        );
    }

    #[test]
    fn github_tree_no_subdir_is_repo_on_branch() {
        let s = parse_source("https://github.com/owner/repo/tree/dev").unwrap();
        assert_eq!(
            s,
            SkillSource::Github {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                git_ref: Some("dev".to_string()),
            }
        );
    }

    #[test]
    fn bare_github_host_path() {
        let s = parse_source("github.com/owner/repo").unwrap();
        assert!(matches!(s, SkillSource::Github { owner, .. } if owner == "owner"));
    }

    #[test]
    fn non_github_url_is_plain_url() {
        let s = parse_source("https://gitlab.com/org/repo").unwrap();
        assert_eq!(
            s,
            SkillSource::Url {
                url: "https://gitlab.com/org/repo".to_string(),
                git_ref: None,
            }
        );
    }

    #[test]
    fn scp_git_remote_is_url() {
        let s = parse_source("git@github.com:owner/repo.git").unwrap();
        assert!(matches!(s, SkillSource::Url { .. }));
    }

    #[test]
    fn empty_is_rejected() {
        assert!(matches!(parse_source("   "), Err(Error::InvalidSource(_))));
    }

    #[test]
    fn github_subdir_url_keeps_explicit_ref() {
        // An explicit `#<ref>` fragment must override the tree ref and not be
        // dropped for a subdir source.
        let s = parse_source("https://github.com/o/r/tree/main/skills/foo#v2.0").unwrap();
        assert_eq!(
            s,
            SkillSource::GitSubdir {
                url: "https://github.com/o/r.git".to_string(),
                path: "skills/foo".to_string(),
                git_ref: Some("v2.0".to_string()),
            }
        );
    }

    #[test]
    fn bare_github_subdir_keeps_tree_ref() {
        let s = parse_source("github.com/o/r/tree/dev/sub").unwrap();
        assert_eq!(
            s,
            SkillSource::GitSubdir {
                url: "https://github.com/o/r.git".to_string(),
                path: "sub".to_string(),
                git_ref: Some("dev".to_string()),
            }
        );
    }

    #[test]
    fn http_scheme_is_rejected() {
        assert!(matches!(
            parse_source("http://github.com/o/r"),
            Err(Error::InvalidSource(_))
        ));
    }

    #[test]
    fn github_subdir_traversal_is_rejected() {
        assert!(matches!(
            parse_source("https://github.com/o/r/tree/main/../../etc"),
            Err(Error::InvalidSource(_))
        ));
    }

    #[tokio::test]
    async fn clone_into_rejects_option_like_ref() {
        let source = SkillSource::Github {
            owner: "o".to_string(),
            repo: "r".to_string(),
            git_ref: Some("--upload-pack=/tmp/evil".to_string()),
        };
        let dir = std::env::temp_dir().join("brskills-inject-test");
        let err = source.clone_into(&dir).await.expect_err("must refuse");
        assert!(matches!(err, Error::Git(_)));
    }

    #[test]
    fn file_scheme_is_rejected() {
        assert!(matches!(
            parse_source("file:///etc/passwd"),
            Err(Error::InvalidSource(_))
        ));
    }

    #[test]
    fn single_segment_is_rejected() {
        assert!(matches!(
            parse_source("justaname"),
            Err(Error::InvalidSource(_))
        ));
    }

    #[test]
    fn relative_paths_are_local() {
        for s in ["./local", "../up", "/abs/path", "."] {
            assert!(
                matches!(parse_source(s), Ok(SkillSource::Local { .. })),
                "{s:?} should be local"
            );
        }
    }

    #[test]
    fn owner_repo_is_not_local() {
        assert!(matches!(
            parse_source("owner/repo"),
            Ok(SkillSource::Github { .. })
        ));
    }

    #[test]
    fn local_path_accessor() {
        let s = parse_source("./skills").unwrap();
        assert_eq!(s.local_path(), Some(Path::new("./skills")));
        assert_eq!(parse_source("o/r").unwrap().local_path(), None);
    }

    #[test]
    fn subdir_accessor() {
        let s = parse_source("https://github.com/o/r/tree/main/a/b").unwrap();
        assert_eq!(s.subdir(), Some("a/b"));
        let s2 = parse_source("o/r").unwrap();
        assert_eq!(s2.subdir(), None);
    }
}
