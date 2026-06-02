//! Parsing the YAML frontmatter of a `SKILL.md` and discovering skills under a
//! directory tree.
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

use crate::{Error, Result};

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

/// Parse the frontmatter from `SKILL.md` content.
pub fn parse_frontmatter(content: &str) -> Result<SkillFrontmatter> {
    let block = extract_frontmatter_block(content).ok_or(Error::MissingFrontmatter)?;
    serde_saphyr::from_str::<SkillFrontmatter>(block).map_err(|e| Error::Frontmatter(e.to_string()))
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

/// Discover every `SKILL.md` reachable under `root`: a `SKILL.md` directly in
/// `root`, or one in any immediate subdirectory of the conventional skills
/// directories. Entries that fail to parse are skipped.
pub fn discover_all_skills(root: &Path) -> Vec<(PathBuf, SkillFrontmatter)> {
    let mut found = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut push = |path: PathBuf, found: &mut Vec<(PathBuf, SkillFrontmatter)>| {
        // The conventional search roots overlap (e.g. `<root>/skills/SKILL.md`
        // is both a child of `<root>` and the direct file of `<root>/skills`);
        // dedup by path so a skill is never discovered twice.
        if !seen.insert(path.clone()) {
            return;
        }
        if let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(fm) = parse_frontmatter(&content)
        {
            found.push((path, fm));
        }
    };
    for base in skill_search_roots(root) {
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
        let (path, fm) = discover_all_skills(&dir).into_iter().next().expect("found");
        assert_eq!(fm.name, "root-skill");
        assert!(path.ends_with("SKILL.md"));
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
        let (_, fm) = discover_all_skills(&dir).into_iter().next().expect("found");
        assert_eq!(fm.name, "alpha");
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
        let mut names: Vec<_> = all.into_iter().map(|(_, fm)| fm.name).collect();
        names.sort();
        assert_eq!(names, vec!["one".to_string(), "two".to_string()]);
        cleanup(&dir);
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
