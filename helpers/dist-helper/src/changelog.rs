//! Per-PR change files (`.changes/*.md`) and folding them into `CHANGELOG.md`.
//!
//! A change file states, in prose, what a pull request means for someone
//! upgrading. It is written while the author still has the context — not
//! reconstructed from commit subjects at release time. release-plz still owns
//! versioning, tagging, and publishing; the generated commit list it writes is
//! kept, but demoted below the curated prose.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Keep a Changelog categories, in the order they are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ChangeType {
    Added,
    Changed,
    Deprecated,
    Removed,
    Fixed,
    Security,
}

impl ChangeType {
    const ALL: [Self; 6] = [
        Self::Added,
        Self::Changed,
        Self::Deprecated,
        Self::Removed,
        Self::Fixed,
        Self::Security,
    ];

    fn heading(self) -> &'static str {
        match self {
            Self::Added => "Added",
            Self::Changed => "Changed",
            Self::Deprecated => "Deprecated",
            Self::Removed => "Removed",
            Self::Fixed => "Fixed",
            Self::Security => "Security",
        }
    }
}

/// The YAML front matter of a change file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrontMatter {
    #[serde(rename = "type")]
    kind: ChangeType,
    /// One line, written for a reader upgrading — not a commit subject.
    title: String,
    /// Renders under "Breaking changes" instead of the type's own section.
    #[serde(default)]
    breaking: bool,
    /// Pull request number, linked from the rendered heading.
    #[serde(default)]
    pr: Option<u64>,
}

#[derive(Debug)]
struct Entry {
    front: FrontMatter,
    body: String,
}

/// Validate every pending change file. Wired into `dist-helper check`.
pub fn check(root: &Path) -> Result<()> {
    let entries = load(root)?;
    println!("change files valid: {} pending", entries.len());
    Ok(())
}

/// Fold pending change files into the release section `CHANGELOG.md` opens
/// with, and delete them. Runs inside the release-plz release PR, after
/// release-plz has bumped the workspace version and written that section.
pub fn fold(root: &Path) -> Result<()> {
    let entries = load(root)?;
    if entries.is_empty() {
        println!("no pending change files - CHANGELOG.md left untouched");
        return Ok(());
    }
    let manifest = workspace_manifest(root)?;
    // The release being prepared is not tagged yet. Refusing once it is keeps a
    // stray run on `main` from rewriting an already-published section.
    let tag = format!("v{}", manifest.version);
    if tag_exists(root, &tag)? {
        bail!(
            "{tag} is already tagged - `changelog fold` runs in the release PR, before the tag exists"
        );
    }
    let path = changelog_path(root);
    let current =
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let folded = fold_into(&current, &entries, &manifest)?;
    fs::write(&path, folded).with_context(|| format!("writing {}", path.display()))?;

    let dir = changes_dir(root);
    for name in change_file_names(&dir)? {
        let file = dir.join(&name);
        fs::remove_file(&file).with_context(|| format!("removing {}", file.display()))?;
    }
    println!(
        "folded {} change file(s) into {}",
        entries.len(),
        path.display()
    );
    Ok(())
}

fn load(root: &Path) -> Result<Vec<Entry>> {
    let dir = changes_dir(root);
    let mut entries = Vec::new();
    for name in change_file_names(&dir)? {
        let path = dir.join(&name);
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let entry = parse(&raw).with_context(|| format!("parsing {}", path.display()))?;
        entries.push(entry);
    }
    Ok(entries)
}

/// Every `*.md` under `.changes/` except the format contract itself, sorted so
/// a run is reproducible regardless of directory order.
fn change_file_names(dir: &Path) -> Result<Vec<String>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading an entry of {}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "README.md" || !name.ends_with(".md") {
            continue;
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

fn parse(raw: &str) -> Result<Entry> {
    let rest = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
        .ok_or_else(|| {
            anyhow::anyhow!("a change file must open with a `---` YAML front matter fence")
        })?;
    let (front, body) = rest
        .split_once("\n---\n")
        .or_else(|| rest.split_once("\r\n---\r\n"))
        .ok_or_else(|| anyhow::anyhow!("front matter is not closed by a `---` line"))?;
    // Titles carry `code spans` and colons, both of which YAML reads as syntax
    // in a plain scalar. The rule is to always double-quote `title`, so say so
    // here rather than leaving a bare YAML error to be decoded.
    let front: FrontMatter = serde_saphyr::from_str(front).context(
        "parsing front matter - `title` must be wrapped in double quotes, e.g. title: \"`foo` is gone\"",
    )?;
    if front.title.trim().is_empty() {
        bail!("`title` must not be empty");
    }
    let body = body.trim().to_string();
    if body.is_empty() {
        bail!("a change file needs a body - say what changed and how to migrate");
    }
    Ok(Entry { front, body })
}

/// Replace the release section's body with curated prose, keeping the
/// generated commit list in a collapsed block below it.
fn fold_into(changelog: &str, entries: &[Entry], manifest: &WorkspaceManifest) -> Result<String> {
    let heading = format!("## [{}]", manifest.version);
    let lines: Vec<&str> = changelog.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.starts_with("## [") && !line.starts_with("## [Unreleased]"))
        .filter(|&index| lines[index].starts_with(&heading))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "CHANGELOG.md does not open with a `{heading}` section - `changelog fold` runs after release-plz has written one for the version it just bumped to"
            )
        })?;
    let end = lines[start + 1..]
        .iter()
        .position(|line| line.starts_with("## ["))
        .map_or(lines.len(), |offset| start + 1 + offset);

    let generated = lines[start + 1..end].join("\n");
    let generated = generated.trim();

    let mut section = String::new();
    section.push_str(lines[start]);
    section.push_str("\n\n");
    section.push_str(&render_entries(entries, &manifest.repository));
    if !generated.is_empty() {
        section.push_str("<details>\n<summary>All commits</summary>\n\n");
        section.push_str(generated);
        section.push_str("\n\n</details>\n");
    }

    let mut out = String::new();
    if start > 0 {
        out.push_str(&lines[..start].join("\n"));
        out.push('\n');
    }
    out.push_str(&section);
    if end < lines.len() {
        out.push('\n');
        out.push_str(&lines[end..].join("\n"));
        out.push('\n');
    }
    Ok(out)
}

fn render_entries(entries: &[Entry], repository: &str) -> String {
    let mut out = String::new();
    let mut breaking: Vec<&Entry> = entries
        .iter()
        .filter(|entry| entry.front.breaking)
        .collect();
    breaking.sort_by(|a, b| {
        (a.front.kind, a.front.title.as_str()).cmp(&(b.front.kind, b.front.title.as_str()))
    });
    if !breaking.is_empty() {
        out.push_str("### Breaking changes\n\n");
        for entry in breaking {
            write_entry(&mut out, entry, repository);
        }
    }
    for kind in ChangeType::ALL {
        let mut group: Vec<&Entry> = entries
            .iter()
            .filter(|entry| !entry.front.breaking && entry.front.kind == kind)
            .collect();
        if group.is_empty() {
            continue;
        }
        group.sort_by(|a, b| a.front.title.cmp(&b.front.title));
        let _ = write!(out, "### {}\n\n", kind.heading());
        for entry in group {
            write_entry(&mut out, entry, repository);
        }
    }
    out
}

fn write_entry(out: &mut String, entry: &Entry, repository: &str) {
    match entry.front.pr {
        Some(pr) => {
            let _ = write!(
                out,
                "#### {} ([#{pr}]({repository}/pull/{pr}))\n\n",
                entry.front.title.trim()
            );
        }
        None => {
            let _ = write!(out, "#### {}\n\n", entry.front.title.trim());
        }
    }
    out.push_str(entry.body.trim());
    out.push_str("\n\n");
}

/// `[workspace.package]` facts the fold needs: the version release-plz just
/// bumped to, and the repository URL, so PR links stay correct in a fork
/// rather than being baked into this helper.
#[derive(Debug)]
struct WorkspaceManifest {
    version: String,
    repository: String,
}

fn workspace_manifest(root: &Path) -> Result<WorkspaceManifest> {
    let path = root.join("Cargo.toml");
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let manifest: toml::Value =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let package = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("package"))
        .ok_or_else(|| {
            anyhow::anyhow!("`[workspace.package]` is missing from {}", path.display())
        })?;
    let field = |key: &str| -> Result<String> {
        package
            .get(key)
            .and_then(toml::Value::as_str)
            .map(|value| value.trim_end_matches('/').to_string())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "`workspace.package.{key}` is missing from {}",
                    path.display()
                )
            })
    };
    Ok(WorkspaceManifest {
        version: field("version")?,
        repository: field("repository")?,
    })
}

fn tag_exists(root: &Path, tag: &str) -> Result<bool> {
    let output = ProcessCommand::new("git")
        .current_dir(root)
        .args(["tag", "--list", tag])
        .output()
        .context("running `git tag --list`")?;
    if !output.status.success() {
        bail!(
            "`git tag --list {tag}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

fn changes_dir(root: &Path) -> PathBuf {
    root.join(".changes")
}

fn changelog_path(root: &Path) -> PathBuf {
    root.join("CHANGELOG.md")
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "https://github.com/bitrouter/bitrouter";

    fn entry(raw: &str) -> Entry {
        parse(raw).expect("valid change file")
    }

    fn manifest(version: &str) -> WorkspaceManifest {
        WorkspaceManifest {
            version: version.to_string(),
            repository: REPO.to_string(),
        }
    }

    #[test]
    fn parses_front_matter_and_body() {
        let parsed = entry("---\ntype: added\ntitle: Skills over MCP\npr: 770\n---\n\nProse.\n");
        assert_eq!(parsed.front.kind, ChangeType::Added);
        assert_eq!(parsed.front.title, "Skills over MCP");
        assert_eq!(parsed.front.pr, Some(770));
        assert!(!parsed.front.breaking);
        assert_eq!(parsed.body, "Prose.");
    }

    #[test]
    fn rejects_an_unknown_front_matter_key() {
        let err = parse("---\ntype: added\ntitle: T\nscope: cli\n---\n\nProse.\n")
            .expect_err("unknown keys are a typo, not an extension point");
        assert!(format!("{err:#}").contains("front matter"));
    }

    #[test]
    fn rejects_a_bodyless_change_file() {
        let err = parse("---\ntype: fixed\ntitle: T\n---\n\n")
            .expect_err("a title alone says nothing a commit subject would not");
        assert!(format!("{err:#}").contains("body"));
    }

    #[test]
    fn rejects_a_missing_front_matter_fence() {
        let err = parse("type: fixed\ntitle: T\n").expect_err("front matter is required");
        assert!(format!("{err:#}").contains("front matter"));
    }

    #[test]
    fn breaking_entries_render_first_and_link_their_pr() {
        let entries = vec![
            entry("---\ntype: added\ntitle: New surface\n---\n\nAdded prose.\n"),
            entry(
                "---\ntype: removed\ntitle: Old surface\nbreaking: true\npr: 770\n---\n\nMigrate like so.\n",
            ),
        ];
        let rendered = render_entries(&entries, REPO);
        let breaking = rendered
            .find("### Breaking changes")
            .expect("breaking section");
        let added = rendered.find("### Added").expect("added section");
        assert!(breaking < added);
        assert!(rendered.contains(
            "#### Old surface ([#770](https://github.com/bitrouter/bitrouter/pull/770))"
        ));
        assert!(rendered.contains("#### New surface\n\nAdded prose."));
    }

    #[test]
    fn folds_curated_prose_above_a_collapsed_commit_list() {
        let changelog = "# Changelog\n\n## [Unreleased]\n\n## [1.0.0-alpha.28](https://example.invalid/compare)\n\n\n### ⛰️ Features\n\n- *(cli)* Something - ([abc1234](https://example.invalid))\n\n\n## [1.0.0-alpha.27](https://example.invalid/older)\n\n### 🐛 Bug Fixes\n\n- Older - ([def5678](https://example.invalid))\n";
        let entries = vec![entry(
            "---\ntype: changed\ntitle: Routing keys are canonical\n---\n\nReplace the legacy routes.\n",
        )];
        let folded =
            fold_into(changelog, &entries, &manifest("1.0.0-alpha.28")).expect("fold succeeds");
        let heading = folded.find("## [1.0.0-alpha.28]").expect("release heading");
        let curated = folded.find("### Changed").expect("curated section");
        let details = folded.find("<details>").expect("collapsed commit list");
        let alpha_27 = folded
            .find("## [1.0.0-alpha.27]")
            .expect("older release kept");
        assert!(heading < curated);
        assert!(curated < details);
        assert!(details < alpha_27);
        assert!(folded.contains("Replace the legacy routes."));
        assert!(folded.contains("- *(cli)* Something"));
        assert!(folded.contains("## [Unreleased]"));
        assert!(folded.contains("- Older - ([def5678](https://example.invalid))"));
    }

    #[test]
    fn fold_errors_when_no_release_section_exists_yet() {
        let entries = vec![entry("---\ntype: fixed\ntitle: T\n---\n\nProse.\n")];
        let err = fold_into(
            "# Changelog\n\n## [Unreleased]\n",
            &entries,
            &manifest("1.0.0-alpha.28"),
        )
        .expect_err("fold runs after release-plz has written a section");
        assert!(format!("{err:#}").contains("1.0.0-alpha.28"));
    }

    /// Without this, a stray run on `main` would rewrite the section of the
    /// release that is already out.
    #[test]
    fn fold_refuses_a_section_that_is_not_the_version_being_released() {
        let changelog = "# Changelog\n\n## [Unreleased]\n\n## [1.0.0-alpha.27](https://example.invalid)\n\n### ⛰️ Features\n\n- Shipped - ([abc1234](https://example.invalid))\n";
        let entries = vec![entry("---\ntype: fixed\ntitle: T\n---\n\nProse.\n")];
        let err = fold_into(changelog, &entries, &manifest("1.0.0-alpha.28"))
            .expect_err("alpha.27 is already released");
        assert!(format!("{err:#}").contains("1.0.0-alpha.28"));
    }
}
