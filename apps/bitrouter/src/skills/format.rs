//! The `SKILL.md` format: YAML frontmatter parsing, skill-name rules, and
//! discovery of skills under a directory tree.
//!
//! Moved here from the former `bitrouter-skills` crate when the skills
//! *package manager* (`add` / `remove` / `find` / `update`) was cut. What
//! remains is format support, and its only consumers are in this binary: the
//! SEP-2640 catalog ([`crate::skills_catalog`]), the `skills_search` /
//! `skills_get` tools ([`crate::actions::skills`]), and the `skills list` /
//! `skills init` CLI verbs.
//!
//! A `SKILL.md` opens with a YAML frontmatter block fenced by `---` lines:
//!
//! ```text
//! ---
//! name: my-skill
//! description: What this skill does.
//! ---
//!
//! # My Skill
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{Error, Result};

/// Parsed frontmatter from a `SKILL.md` file.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SkillFrontmatter {
    /// Canonical skill name (filesystem slug).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Freeform metadata map (version, author, tags, …).
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
    /// The complete YAML mapping rendered as JSON, including fields this
    /// version of BitRouter does not model. The SEP catalog publishes this
    /// verbatim; typed fields above remain available to the CLI/tool surfaces.
    #[serde(skip)]
    pub raw: serde_json::Map<String, serde_json::Value>,
}

/// Extract the YAML frontmatter block (the text between the first pair of `---`
/// fence lines) from `SKILL.md` content. Returns `None` when there is no
/// opening fence or no closing fence.
///
/// The closing fence must be a line that is *exactly* `---` (ignoring a
/// trailing `\r`), so a `----` divider or a `---` appearing inside a YAML value
/// does not prematurely terminate the block.
fn extract_frontmatter_block(content: &str) -> Option<&str> {
    let rest = content.strip_prefix("---")?;
    // The opening fence line may carry a trailing `\r`; require it to be
    // followed by a newline so a leading `---word` isn't mistaken for a fence.
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let text = line.strip_suffix('\n').unwrap_or(line);
        let text = text.strip_suffix('\r').unwrap_or(text);
        if text == "---" {
            return Some(&rest[..offset]);
        }
        offset += line.len();
    }
    None
}

/// The markdown body of a `SKILL.md`: everything after the frontmatter fence.
///
/// Falls back to the whole file when there is no recognizable frontmatter — a
/// superset beats a lost body, and a skill whose frontmatter did not parse is
/// exactly the one whose text a reader most wants to see.
///
/// This lives beside `extract_frontmatter_block` on purpose: the split is one
/// rule, and the `skills_get` adapter used to carry a second, hand-rolled copy
/// of it that disagreed about `\r\n` and about `----`.
pub fn skill_body(content: &str) -> &str {
    let Some(block) = extract_frontmatter_block(content) else {
        return content;
    };
    // `block` is a subslice of `content`; everything from its end is the fence
    // line plus the body.
    let consumed = block.as_ptr() as usize - content.as_ptr() as usize + block.len();
    let after_block = &content[consumed..];
    let after_fence = after_block
        .split_inclusive('\n')
        .next()
        .map_or("", |fence| &after_block[fence.len()..]);
    after_fence.trim_start_matches(['\n', '\r'])
}

/// Parse the frontmatter from `SKILL.md` content.
pub fn parse_frontmatter(content: &str) -> Result<SkillFrontmatter> {
    let block = extract_frontmatter_block(content).ok_or(Error::MissingFrontmatter)?;
    let mut parsed = serde_saphyr::from_str::<SkillFrontmatter>(block)
        .map_err(|e| Error::Frontmatter(e.to_string()))?;
    let raw = serde_saphyr::from_str::<serde_json::Value>(block)
        .map_err(|e| Error::Frontmatter(e.to_string()))?;
    parsed.raw = raw
        .as_object()
        .cloned()
        .ok_or_else(|| Error::Frontmatter("frontmatter must be a YAML mapping".to_string()))?;
    Ok(parsed)
}

/// The candidate directories searched for a `SKILL.md`, relative to a fetched
/// source root. Mirrors the conventional layout used by the wider skills
/// ecosystem (root, then `skills/`, then `.claude/skills/`).
fn skill_search_roots(root: &Path) -> Vec<PathBuf> {
    vec![
        root.to_path_buf(),
        root.join("skills"),
        root.join(".claude").join("skills"),
    ]
}

/// Whether `candidate` resolves inside `root` without traversing a symlink
/// below that configured root.
///
/// The root itself may intentionally be a symlink selected by the operator,
/// so it is the canonical containment anchor. Every component beneath it must
/// be a real directory/file: otherwise discovery could read a `SKILL.md`
/// outside the configured workspace before the catalog has a chance to reject
/// it.
pub(crate) fn is_safe_installed_path(root: &Path, candidate: &Path) -> bool {
    let Ok(relative) = candidate.strip_prefix(root) else {
        return false;
    };
    let (Ok(canonical_root), Ok(canonical_candidate)) =
        (root.canonicalize(), candidate.canonicalize())
    else {
        return false;
    };
    if !canonical_candidate.starts_with(&canonical_root) {
        return false;
    }

    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(segment) = component else {
            return false;
        };
        cursor.push(segment);
        let Ok(metadata) = std::fs::symlink_metadata(&cursor) else {
            return false;
        };
        if metadata.file_type().is_symlink() {
            return false;
        }
    }
    true
}

/// One `SKILL.md` found on disk, whether or not it can be used.
///
/// Discovery deliberately does **not** drop a skill whose frontmatter failed to
/// parse. It used to, and that was one half of the drift phase 4 removes: the
/// CLI listed such a skill (it never read frontmatter) while the agent could not
/// see it at all. Reporting the failure is what lets both surfaces say the same
/// thing about the same directory.
#[derive(Debug)]
pub struct DiscoveredSkill {
    /// The directory holding the `SKILL.md`.
    pub dir: PathBuf,
    /// The `SKILL.md` itself.
    pub skill_md: PathBuf,
    /// Its parsed frontmatter, or the error that stopped it.
    pub frontmatter: Result<SkillFrontmatter>,
}

impl DiscoveredSkill {
    /// The skill's name: `frontmatter.name` where it parsed, the directory name
    /// otherwise — so an unusable skill is still nameable in the message that
    /// explains why.
    pub fn name(&self) -> String {
        match &self.frontmatter {
            Ok(fm) => fm.name.clone(),
            Err(_) => self
                .dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
        }
    }

    /// `frontmatter.description`, or empty when it did not parse.
    pub fn description(&self) -> &str {
        match &self.frontmatter {
            Ok(fm) => fm.description.as_str(),
            Err(_) => "",
        }
    }

    /// Why this skill cannot be served, or `None` when it can.
    ///
    /// **The one validation policy.** Every surface asks this function and none
    /// re-derives it: the SEP-2640 catalog skips a skill with a problem (it has
    /// no conforming entry to publish), while `bitrouter skills list` and
    /// `skills_search` show it marked with the string this returns.
    ///
    /// The rules are the Agent Skills format's, which SEP-2640 delegates to
    /// wholesale: parseable frontmatter, a directory name equal to
    /// `frontmatter.name`, and a name and description inside the format's
    /// bounds.
    pub fn problem(&self) -> Option<String> {
        let fm = match &self.frontmatter {
            Ok(fm) => fm,
            Err(e) => return Some(e.to_string()),
        };
        let dir_name = self.dir.file_name().and_then(|n| n.to_str());
        if dir_name != Some(fm.name.as_str()) {
            return Some(format!(
                "directory is named {:?} but frontmatter declares name {:?}; \
                 they must match",
                dir_name.unwrap_or_default(),
                fm.name
            ));
        }
        if !super::is_valid_skill_name(&fm.name) {
            return Some(super::Error::InvalidSkillName(fm.name.clone()).to_string());
        }
        if !super::is_valid_skill_description(&fm.description) {
            return Some("description must be 1-1024 characters".to_string());
        }
        None
    }
}

/// Discover every `SKILL.md` reachable under `root`: a `SKILL.md` directly in
/// `root`, or one in any immediate subdirectory of the conventional skills
/// directories (`<root>`, `<root>/skills`, `<root>/.claude/skills`).
///
/// **The one discovery function.** `list_installed`'s single `read_dir` of
/// `<root>/.claude/skills` was the second one, and it is now a call to this;
/// the SEP catalog's extra validation was the third, and it is now
/// [`DiscoveredSkill::problem`].
///
/// A path that escapes `root` or traverses a symlink beneath it is skipped
/// silently — that containment is a security property, not a user-visible
/// problem to report back.
pub fn discover_all_skills(root: &Path) -> Vec<DiscoveredSkill> {
    let mut found = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut push = |path: PathBuf, found: &mut Vec<DiscoveredSkill>| {
        if !is_safe_installed_path(root, &path) {
            return;
        }
        // The conventional search roots overlap (e.g. `<root>/skills/SKILL.md`
        // is both a child of `<root>` and the direct file of `<root>/skills`);
        // dedup by path so a skill is never discovered twice.
        if !seen.insert(path.clone()) {
            return;
        }
        let Some(dir) = path.parent().map(Path::to_path_buf) else {
            return;
        };
        let frontmatter = match std::fs::read_to_string(&path) {
            Ok(content) => parse_frontmatter(&content),
            // Unreadable is a problem worth reporting, not an absence: the
            // directory has a SKILL.md and the user cannot use it.
            Err(e) => Err(Error::Io(format!("reading {}: {e}", path.display()))),
        };
        found.push(DiscoveredSkill {
            dir,
            skill_md: path,
            frontmatter,
        });
    };
    for base in skill_search_roots(root) {
        if !is_safe_installed_path(root, &base) {
            continue;
        }
        // A SKILL.md directly inside this base directory.
        push(base.join("SKILL.md"), &mut found);
        // A SKILL.md one level down: base/<child>/SKILL.md.
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            push(entry.path().join("SKILL.md"), &mut found);
        }
    }
    found.sort_by(|a, b| a.skill_md.cmp(&b.skill_md));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_and_description() {
        let content = "---\nname: my-skill\ndescription: Does a thing.\n---\n\n# Body\n";
        let fm = parse_frontmatter(content).expect("should parse");
        assert_eq!(fm.name, "my-skill");
        assert_eq!(fm.description, "Does a thing.");
        assert!(fm.metadata.is_empty());
    }

    #[test]
    fn parses_metadata_map() {
        let content = "---\nname: s\ndescription: d\nmetadata:\n  version: \"1.2.0\"\n  internal: true\n---\nbody";
        let fm = parse_frontmatter(content).expect("should parse");
        assert_eq!(
            fm.metadata.get("version"),
            Some(&serde_json::Value::String("1.2.0".to_string()))
        );
        assert_eq!(
            fm.metadata.get("internal"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn four_dash_line_is_not_a_closing_fence() {
        // A `----` divider must not be mistaken for the `---` closing fence;
        // with no exact `---` closer the frontmatter is malformed.
        let content = "---\nname: s\ndescription: d\n----\nmore\n";
        let err = parse_frontmatter(content).expect_err("no exact --- fence");
        assert!(matches!(err, Error::MissingFrontmatter));
    }

    #[test]
    fn inner_dashes_do_not_truncate_block() {
        // A `----` value before the real fence must not cut the block short.
        let content = "---\nname: s\ndescription: d\nnote: \"----\"\n---\nbody\n";
        let fm = parse_frontmatter(content).expect("parses to the real fence");
        assert_eq!(fm.name, "s");
        assert_eq!(fm.description, "d");
    }

    #[test]
    fn missing_frontmatter_is_an_error() {
        let err = parse_frontmatter("# Just a heading\n").expect_err("no fence");
        assert!(matches!(err, Error::MissingFrontmatter));
    }

    #[test]
    fn unterminated_frontmatter_is_missing() {
        let err = parse_frontmatter("---\nname: s\n").expect_err("no closing fence");
        assert!(matches!(err, Error::MissingFrontmatter));
    }

    #[test]
    fn malformed_yaml_is_a_parse_error() {
        // Missing the required `description` field.
        let err = parse_frontmatter("---\nname: s\n---\n").expect_err("incomplete");
        assert!(matches!(err, Error::Frontmatter(_)));
    }

    #[test]
    fn discovers_skill_in_root() {
        let dir = tempdir("discover-root");
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: root-skill\ndescription: d\n---\n",
        )
        .unwrap();
        let found = discover_all_skills(&dir).into_iter().next().expect("found");
        assert_eq!(found.name(), "root-skill");
        assert!(found.skill_md.ends_with("SKILL.md"));
        assert_eq!(found.dir, dir);
        cleanup(&dir);
    }

    #[test]
    fn discovers_skill_in_skills_subdir() {
        let dir = tempdir("discover-subdir");
        let nested = dir.join("skills").join("alpha");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("SKILL.md"),
            "---\nname: alpha\ndescription: d\n---\n",
        )
        .unwrap();
        let found = discover_all_skills(&dir).into_iter().next().expect("found");
        assert_eq!(found.name(), "alpha");
        cleanup(&dir);
    }

    #[test]
    fn discover_all_finds_multiple() {
        let dir = tempdir("discover-all");
        let skills = dir.join("skills");
        for name in ["one", "two"] {
            let nested = skills.join(name);
            std::fs::create_dir_all(&nested).unwrap();
            std::fs::write(
                nested.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: d\n---\n"),
            )
            .unwrap();
        }
        let all = discover_all_skills(&dir);
        let mut names: Vec<_> = all.iter().map(DiscoveredSkill::name).collect();
        names.sort();
        assert_eq!(names, vec!["one".to_string(), "two".to_string()]);
        assert!(all.iter().all(|s| s.problem().is_none()));
        cleanup(&dir);
    }

    /// The half of the drift that used to make a skill invisible to the agent:
    /// broken YAML was dropped by discovery, so only the CLI (which never
    /// parsed frontmatter) listed it. It is now discovered *and* marked.
    #[test]
    fn a_skill_with_broken_frontmatter_is_discovered_and_carries_a_problem() {
        let dir = tempdir("discover-broken");
        let nested = dir.join(".claude").join("skills").join("broken");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("SKILL.md"), "---\nname: broken\n---\n").unwrap();

        let found = discover_all_skills(&dir).into_iter().next().expect("found");
        assert_eq!(found.name(), "broken", "falls back to the directory name");
        assert_eq!(found.description(), "");
        assert!(
            found.problem().is_some_and(|p| p.contains("frontmatter")),
            "{:?}",
            found.problem()
        );
        cleanup(&dir);
    }

    /// The SEP catalog's rule, now the shared one: a directory whose name does
    /// not equal `frontmatter.name` cannot be published, and both surfaces say
    /// so instead of one dropping it silently.
    #[test]
    fn a_directory_name_mismatch_is_a_problem_not_a_disappearance() {
        let dir = tempdir("discover-mismatch");
        let nested = dir.join(".claude").join("skills").join("on-disk");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("SKILL.md"),
            "---\nname: in-frontmatter\ndescription: d\n---\n",
        )
        .unwrap();

        let found = discover_all_skills(&dir).into_iter().next().expect("found");
        assert_eq!(found.name(), "in-frontmatter");
        let problem = found.problem().expect("mismatch is a problem");
        assert!(
            problem.contains("on-disk") && problem.contains("in-frontmatter"),
            "{problem}"
        );
        cleanup(&dir);
    }

    #[test]
    fn body_starts_after_the_frontmatter_fence() {
        assert_eq!(
            skill_body("---\nname: s\ndescription: d\n---\n\n# Alpha\n\nRun it.\n"),
            "# Alpha\n\nRun it.\n"
        );
        // CRLF fences split identically.
        assert_eq!(
            skill_body("---\r\nname: s\r\ndescription: d\r\n---\r\n# Alpha\r\n"),
            "# Alpha\r\n"
        );
        // No recognizable frontmatter: the whole file, rather than nothing.
        assert_eq!(skill_body("# Just a heading\n"), "# Just a heading\n");
        assert_eq!(skill_body("---\nname: s\n"), "---\nname: s\n");
    }

    #[test]
    fn discover_finds_nothing_when_empty() {
        let dir = tempdir("discover-empty");
        assert!(discover_all_skills(&dir).is_empty());
        cleanup(&dir);
    }

    fn tempdir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("brskills-fm-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }
}
