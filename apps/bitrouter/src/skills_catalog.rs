//! The SEP-2640 skills catalog — the app side of the origin server's
//! `skills/list` and `skills/get`.
//!
//! Implements [`SkillCatalog`] over the installed-skills root, using
//! [`crate::skills::format`] for discovery and frontmatter. Read-only.
//!
//! Distinct from [`crate::skills_query`], which implements the older
//! tool-shaped `skills_search` / `skills_get` port over the same root. Both are
//! served; see `bitrouter_mcp::capabilities::skill_catalog` for why.
//!
//! ## URI derivation
//!
//! SEP-2640 requires the final `<skill-path>` segment of a skill's URI to equal
//! its frontmatter `name`. That does **not** hold for the installed directory
//! name, and BitRouter cannot make it hold: it no longer installs skills, so it
//! does not control how this directory is laid out. Whatever populated it —
//! `npx skills add`, a plugin marketplace, a human with `mkdir` — chose the
//! directory name, and nothing obliges that name to match the frontmatter.
//!
//! The URI is therefore derived from the frontmatter name, never the directory:
//!
//! | Case | URI |
//! |---|---|
//! | directory name == frontmatter name | `skill://<name>/SKILL.md` |
//! | they differ | `skill://<dir>/<name>/SKILL.md` |
//!
//! Both satisfy the invariant — in the second form `<dir>` is the
//! "server-chosen organizational prefix" the SEP permits — and the second form
//! also keeps two skills that share a frontmatter name distinguishable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::skills::format::{SkillFrontmatter, discover_all_skills};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bitrouter_mcp::capabilities::skill_catalog::{SkillCatalog, SkillFile, SkillFileBody};
use bitrouter_mcp::error::ToolError;
use bitrouter_sdk::mcp::skills::{
    GetSkillResult, ListSkillsResult, SKILL_SCHEME, SkillEntry, SkillResource,
};
use sha2::{Digest, Sha256};

/// Serves the skills installed under a root directory.
///
/// `discover_all_skills` searches `root`, `root/skills`, and
/// `root/.claude/skills`, so the project root covers the conventional layouts.
pub struct InstalledSkillCatalog {
    root: PathBuf,
}

impl InstalledSkillCatalog {
    /// Serve skills installed under `root` (typically the base repo).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Every installed skill, keyed by URI.
    ///
    /// A `BTreeMap` both dedupes and orders: discovery walks several
    /// conventional roots, so the same frontmatter name can legitimately be
    /// found twice (say `./skills/foo` and `./.claude/skills/foo`). That is a
    /// genuine local conflict rather than something to resolve silently — the
    /// first wins and the loser is logged, because dropping one without a word
    /// is how a shadowed skill goes unnoticed.
    fn collect(root: &Path) -> BTreeMap<String, SkillEntry> {
        let mut entries: BTreeMap<String, SkillEntry> = BTreeMap::new();
        for (skill_md, fm) in discover_all_skills(root) {
            let Some(skill_dir) = skill_md.parent() else {
                continue;
            };
            let uri = match skill_uri(skill_dir, &fm) {
                Some(uri) => uri,
                None => {
                    tracing::warn!(
                        path = %skill_md.display(),
                        "skills catalog: skill directory has no usable name; skipped",
                    );
                    continue;
                }
            };
            if let Some(existing) = entries.get(&uri) {
                tracing::warn!(
                    uri = %uri,
                    kept = %existing.uri,
                    dropped = %skill_md.display(),
                    "skills catalog: two installed skills resolve to the same URI; \
                     keeping the first. Rename one so both are addressable.",
                );
                continue;
            }
            let resources = match enumerate_resources(skill_dir, &uri) {
                Ok(resources) => resources,
                Err(e) => {
                    tracing::warn!(
                        path = %skill_dir.display(),
                        error = %e,
                        "skills catalog: could not enumerate skill files; skipped",
                    );
                    continue;
                }
            };
            entries.insert(
                uri.clone(),
                SkillEntry {
                    uri,
                    frontmatter: frontmatter_to_json(&fm),
                    resources: Some(resources),
                    // Nothing to carry: this catalog builds entries from the
                    // filesystem rather than forwarding an upstream's.
                    extra: serde_json::Map::new(),
                },
            );
        }
        entries
    }

    /// Run [`Self::collect`] off the async runtime — discovery, directory
    /// walking, and hashing are all blocking filesystem work.
    async fn collect_off_thread(&self) -> Result<BTreeMap<String, SkillEntry>, ToolError> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || Self::collect(&root))
            .await
            .map_err(|e| ToolError::new(format!("skills catalog task failed: {e}")))
    }
}

#[async_trait::async_trait]
impl SkillCatalog for InstalledSkillCatalog {
    async fn list(&self) -> Result<ListSkillsResult, ToolError> {
        Ok(ListSkillsResult {
            skills: self.collect_off_thread().await?.into_values().collect(),
        })
    }

    async fn get(&self, uri: &str) -> Result<GetSkillResult, ToolError> {
        self.collect_off_thread()
            .await?
            .remove(uri)
            .map(|skill| GetSkillResult { skill })
            .ok_or_else(|| ToolError::new(format!("no installed skill at '{uri}'")))
    }

    /// Resolve `uri` against the catalog and read the file it names.
    ///
    /// Resolution is by **lookup, not path arithmetic**: the URI must appear
    /// in some entry's `resources`, which is built by walking the skill
    /// directory. A traversal URI (`skill://x/../../etc/passwd`) is never in
    /// that set, so it cannot resolve — there is no string-munging step for it
    /// to defeat. This also keeps `resources/read` and the entry's `resources`
    /// enumeration in exact agreement, which SEP-2640 requires: a host must
    /// treat a read of an unlisted file as a verification failure.
    async fn read(&self, uri: &str) -> Result<SkillFile, ToolError> {
        let root = self.root.clone();
        let wanted = uri.to_string();
        tokio::task::spawn_blocking(move || {
            let path = Self::collect(&root)
                .values()
                .find_map(|entry| resource_path(&root, entry, &wanted))
                .ok_or_else(|| {
                    ToolError::new(format!("'{wanted}' is not a file of any installed skill"))
                })?;
            let bytes = std::fs::read(&path)
                .map_err(|e| ToolError::new(format!("reading {}: {e}", path.display())))?;
            Ok(SkillFile {
                mime_type: Some(mime_type_for(&path).to_string()),
                body: match String::from_utf8(bytes) {
                    Ok(text) => SkillFileBody::Text(text),
                    Err(e) => SkillFileBody::Blob(BASE64.encode(e.as_bytes())),
                },
                uri: wanted,
            })
        })
        .await
        .map_err(|e| ToolError::new(format!("skills read task failed: {e}")))?
    }
}

/// The on-disk path for `wanted`, if it is one of `entry`'s enumerated files.
///
/// Rebuilds each resource's path the same way [`enumerate_resources`] built its
/// URI — by joining the file's skill-relative suffix onto the skill directory
/// — so the two cannot disagree.
fn resource_path(root: &Path, entry: &SkillEntry, wanted: &str) -> Option<PathBuf> {
    let resources = entry.resources.as_ref()?;
    if !resources.iter().any(|r| r.uri == wanted) {
        return None;
    }
    let base = entry.uri.strip_suffix("SKILL.md")?;
    let relative = wanted.strip_prefix(base)?;
    let skill_dir = skill_dir_for(root, entry)?;
    Some(skill_dir.join(relative))
}

/// The directory holding `entry`'s `SKILL.md`, found by re-running discovery
/// rather than reversing the URI — the URI is derived from frontmatter, so it
/// is not invertible to a path.
fn skill_dir_for(root: &Path, entry: &SkillEntry) -> Option<PathBuf> {
    discover_all_skills(root).into_iter().find_map(|(md, fm)| {
        let dir = md.parent()?;
        (skill_uri(dir, &fm).as_deref() == Some(entry.uri.as_str())).then(|| dir.to_path_buf())
    })
}

/// Best-effort content type from a file extension. `text/markdown` for the
/// `SKILL.md` and its references, which is what the SEP asks for; everything
/// unknown falls back to `application/octet-stream`.
fn mime_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("md" | "markdown") => "text/markdown",
        Some("txt") => "text/plain",
        Some("json") => "application/json",
        Some("yaml" | "yml") => "application/yaml",
        Some("py") => "text/x-python",
        Some("sh") => "application/x-sh",
        Some("toml") => "application/toml",
        _ => "application/octet-stream",
    }
}

/// The `skill://` URI for a skill's `SKILL.md`.
///
/// Returns `None` when the directory has no usable final component, which
/// would leave nothing to build a path from.
fn skill_uri(skill_dir: &Path, fm: &SkillFrontmatter) -> Option<String> {
    let dir_name = skill_dir.file_name()?.to_str()?;
    if dir_name == fm.name {
        Some(format!("{SKILL_SCHEME}{}/SKILL.md", fm.name))
    } else {
        Some(format!("{SKILL_SCHEME}{dir_name}/{}/SKILL.md", fm.name))
    }
}

/// Render parsed frontmatter back to the JSON object the SEP calls for.
///
/// The SEP wants the frontmatter "verbatim", but `SkillFrontmatter` only
/// models `name`, `description`, and a freeform `metadata` map, so anything
/// else the author wrote is already lost upstream of here. Emitting the three
/// modelled fields keeps the required ones (`name`, `description`) exact; see
/// the module-level note in the spec about widening this later.
fn frontmatter_to_json(fm: &SkillFrontmatter) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert("name".to_string(), fm.name.clone().into());
    map.insert("description".to_string(), fm.description.clone().into());
    if !fm.metadata.is_empty() {
        let metadata: serde_json::Map<String, serde_json::Value> = fm
            .metadata
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        map.insert("metadata".to_string(), metadata.into());
    }
    map
}

/// Every file in `skill_dir`, paired with the digest of its bytes.
///
/// `skill_uri` is the entry's own URI; supporting files are addressed relative
/// to the skill root by stripping its trailing `SKILL.md`, so
/// `references/GUIDE.md` becomes `<root>/references/GUIDE.md` exactly as a
/// relative filesystem reference would resolve.
///
/// Symlinks are skipped rather than followed: a link inside a skill directory
/// can point anywhere, and serving its target would place bytes from outside
/// the skill under the skill's URI space. `install.rs` already skips them when
/// installing, so an installed skill has none.
fn enumerate_resources(skill_dir: &Path, skill_uri: &str) -> std::io::Result<Vec<SkillResource>> {
    let base = skill_uri.strip_suffix("SKILL.md").unwrap_or(skill_uri);
    let mut files = Vec::new();
    collect_files(skill_dir, &mut files)?;
    // Deterministic order: the listing and its digests must not depend on
    // filesystem iteration order.
    files.sort();
    let mut resources = Vec::with_capacity(files.len());
    for path in files {
        let Ok(relative) = path.strip_prefix(skill_dir) else {
            continue;
        };
        let Some(relative) = relative.to_str() else {
            continue;
        };
        // Resource URIs use `/` regardless of host path separator.
        let relative = relative.replace('\\', "/");
        resources.push(SkillResource {
            uri: format!("{base}{relative}"),
            digest: digest_file(&path)?,
        });
    }
    Ok(resources)
}

/// Recursively collect regular files under `dir`, skipping symlinks.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        // `symlink_metadata` does not follow the link, so a symlinked
        // directory cannot walk us out of the skill.
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            tracing::debug!(path = %path.display(), "skills catalog: symlink skipped");
            continue;
        }
        if meta.is_dir() {
            collect_files(&path, out)?;
        } else if meta.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

/// SHA-256 of a file's raw bytes, formatted as the SEP's
/// `sha256:{64 lowercase hex}`.
fn digest_file(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(digest_bytes(&bytes))
}

/// SHA-256 of `bytes`, formatted as `sha256:{64 lowercase hex}`.
fn digest_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_skill(root: &Path, dir: &str, name: &str, files: &[(&str, &str)]) {
        let skill_dir = root.join(".claude").join("skills").join(dir);
        std::fs::create_dir_all(&skill_dir).expect("mkdir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d\n---\n\n# Body\n"),
        )
        .expect("write SKILL.md");
        for (rel, content) in files {
            let path = skill_dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir parent");
            }
            std::fs::write(path, content).expect("write file");
        }
    }

    /// Hand-derived: SHA-256 of the empty string is a well-known constant.
    #[test]
    fn digest_matches_a_known_sha256() {
        assert_eq!(
            digest_bytes(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // 64 lowercase hex characters after the prefix, per the SEP.
        let hex_part = digest_bytes(b"anything")
            .strip_prefix("sha256:")
            .expect("prefix")
            .to_string();
        assert_eq!(hex_part.len(), 64);
        assert!(
            hex_part
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
    }

    #[tokio::test]
    async fn lists_every_file_including_skill_md() {
        let dir = tempfile::tempdir().expect("tempdir");
        install_skill(
            dir.path(),
            "pdf",
            "pdf",
            &[
                ("references/FORMS.md", "forms"),
                ("scripts/x.py", "print()"),
            ],
        );
        let catalog = InstalledSkillCatalog::new(dir.path().to_path_buf());

        let listed = catalog.list().await.expect("list");
        assert_eq!(listed.skills.len(), 1);
        let entry = &listed.skills[0];
        assert_eq!(entry.uri, "skill://pdf/SKILL.md");

        let resources = entry.resources.as_ref().expect("resources present");
        let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
        assert_eq!(
            uris,
            vec![
                "skill://pdf/SKILL.md",
                "skill://pdf/references/FORMS.md",
                "skill://pdf/scripts/x.py",
            ],
            "complete, deterministic, and includes SKILL.md itself"
        );
        // The supporting file's digest is the digest of its bytes.
        let forms = resources
            .iter()
            .find(|r| r.uri.ends_with("FORMS.md"))
            .expect("forms");
        assert_eq!(forms.digest, digest_bytes(b"forms"));
    }

    #[tokio::test]
    async fn final_uri_segment_always_equals_the_frontmatter_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Directory name matches.
        install_skill(dir.path(), "alpha", "alpha", &[]);
        // Directory name differs — what `skills update` produces when the
        // upstream frontmatter name has drifted.
        install_skill(dir.path(), "pinned-dir", "upstream-name", &[]);
        let catalog = InstalledSkillCatalog::new(dir.path().to_path_buf());

        let listed = catalog.list().await.expect("list");
        assert_eq!(listed.skills.len(), 2);
        for entry in &listed.skills {
            let name = entry.frontmatter["name"].as_str().expect("name");
            let final_segment = entry
                .uri
                .strip_suffix("/SKILL.md")
                .and_then(|p| p.rsplit('/').next())
                .expect("final segment");
            assert_eq!(
                final_segment, name,
                "SEP-2640 requires the final path segment to be the name: {}",
                entry.uri
            );
        }
        let uris: Vec<&str> = listed.skills.iter().map(|s| s.uri.as_str()).collect();
        assert!(uris.contains(&"skill://alpha/SKILL.md"));
        assert!(
            uris.contains(&"skill://pinned-dir/upstream-name/SKILL.md"),
            "install_as keeps the directory as an organizational prefix: {uris:?}"
        );
    }

    #[tokio::test]
    async fn two_skills_sharing_a_name_stay_distinguishable() {
        let dir = tempfile::tempdir().expect("tempdir");
        install_skill(dir.path(), "refunds", "refunds", &[]);
        install_skill(dir.path(), "billing-refunds", "refunds", &[]);
        let catalog = InstalledSkillCatalog::new(dir.path().to_path_buf());

        let listed = catalog.list().await.expect("list");
        assert_eq!(listed.skills.len(), 2, "neither is dropped");
        let uris: std::collections::BTreeSet<&str> =
            listed.skills.iter().map(|s| s.uri.as_str()).collect();
        assert_eq!(uris.len(), 2, "distinct URIs: {uris:?}");
    }

    #[tokio::test]
    async fn get_returns_the_entry_and_errors_on_an_unknown_uri() {
        let dir = tempfile::tempdir().expect("tempdir");
        install_skill(dir.path(), "alpha", "alpha", &[]);
        let catalog = InstalledSkillCatalog::new(dir.path().to_path_buf());

        let got = catalog
            .get("skill://alpha/SKILL.md")
            .await
            .expect("known uri");
        assert_eq!(got.skill.frontmatter["name"], "alpha");

        let err = catalog
            .get("skill://ghost/SKILL.md")
            .await
            .expect_err("unknown uri");
        assert!(err.0.contains("ghost"), "names the uri: {}", err.0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_are_not_followed_out_of_the_skill() {
        let dir = tempfile::tempdir().expect("tempdir");
        install_skill(dir.path(), "alpha", "alpha", &[]);
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "secret").expect("write outside");
        let link = dir
            .path()
            .join(".claude")
            .join("skills")
            .join("alpha")
            .join("leak.txt");
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");

        let catalog = InstalledSkillCatalog::new(dir.path().to_path_buf());
        let listed = catalog.list().await.expect("list");
        let resources = listed.skills[0]
            .resources
            .as_ref()
            .expect("resources present");
        assert!(
            resources.iter().all(|r| !r.uri.ends_with("leak.txt")),
            "symlink is not enumerated: {resources:?}"
        );
    }

    #[tokio::test]
    async fn read_serves_skill_md_and_supporting_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        install_skill(
            dir.path(),
            "pdf",
            "pdf",
            &[("references/FORMS.md", "# Forms")],
        );
        let catalog = InstalledSkillCatalog::new(dir.path().to_path_buf());

        let skill_md = catalog
            .read("skill://pdf/SKILL.md")
            .await
            .expect("SKILL.md");
        assert_eq!(skill_md.mime_type.as_deref(), Some("text/markdown"));
        match skill_md.body {
            SkillFileBody::Text(text) => assert!(text.contains("name: pdf"), "{text}"),
            SkillFileBody::Blob(_) => panic!("markdown must be served as text"),
        }

        let forms = catalog
            .read("skill://pdf/references/FORMS.md")
            .await
            .expect("supporting file");
        match forms.body {
            SkillFileBody::Text(text) => assert_eq!(text, "# Forms"),
            SkillFileBody::Blob(_) => panic!("markdown must be served as text"),
        }
    }

    #[tokio::test]
    async fn read_refuses_traversal_and_unlisted_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        install_skill(dir.path(), "pdf", "pdf", &[]);
        std::fs::write(dir.path().join("secret.txt"), "secret").expect("write secret");
        let catalog = InstalledSkillCatalog::new(dir.path().to_path_buf());

        for hostile in [
            "skill://pdf/../../../etc/passwd",
            "skill://pdf/../../secret.txt",
            "skill://pdf/never-created.md",
            "file:///etc/passwd",
        ] {
            let err = catalog.read(hostile).await.expect_err(hostile);
            assert!(
                err.0.contains("not a file of any installed skill"),
                "{hostile} must not resolve: {}",
                err.0
            );
        }
    }

    /// Every URI a listing advertises must be readable, and nothing else may
    /// be — SEP-2640 makes a read of an unlisted file a verification failure,
    /// so the two surfaces have to agree exactly.
    #[tokio::test]
    async fn every_enumerated_resource_is_readable() {
        let dir = tempfile::tempdir().expect("tempdir");
        install_skill(
            dir.path(),
            "pinned",
            "drifted",
            &[("a.md", "a"), ("nested/b.json", "{}")],
        );
        let catalog = InstalledSkillCatalog::new(dir.path().to_path_buf());

        let listed = catalog.list().await.expect("list");
        let resources = listed.skills[0]
            .resources
            .as_ref()
            .expect("resources present");
        assert_eq!(resources.len(), 3);
        for resource in resources {
            let file = catalog
                .read(&resource.uri)
                .await
                .unwrap_or_else(|e| panic!("{} must be readable: {}", resource.uri, e.0));
            // And the bytes served match the digest advertised for them.
            let served = match file.body {
                SkillFileBody::Text(text) => digest_bytes(text.as_bytes()),
                SkillFileBody::Blob(b64) => {
                    digest_bytes(&BASE64.decode(b64).expect("valid base64"))
                }
            };
            assert_eq!(
                served, resource.digest,
                "digest matches for {}",
                resource.uri
            );
        }
    }

    #[tokio::test]
    async fn non_utf8_files_are_served_as_base64_blobs() {
        let dir = tempfile::tempdir().expect("tempdir");
        install_skill(dir.path(), "assets", "assets", &[]);
        let binary = dir
            .path()
            .join(".claude")
            .join("skills")
            .join("assets")
            .join("logo.bin");
        std::fs::write(&binary, [0xff, 0xfe, 0x00, 0x01]).expect("write binary");
        let catalog = InstalledSkillCatalog::new(dir.path().to_path_buf());

        let file = catalog
            .read("skill://assets/logo.bin")
            .await
            .expect("binary file");
        match file.body {
            SkillFileBody::Blob(b64) => {
                assert_eq!(
                    BASE64.decode(b64).expect("valid base64"),
                    [0xff, 0xfe, 0x00, 0x01]
                );
            }
            SkillFileBody::Text(_) => panic!("invalid UTF-8 must not be served as text"),
        }
    }

    #[tokio::test]
    async fn empty_root_lists_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog = InstalledSkillCatalog::new(dir.path().to_path_buf());
        assert!(catalog.list().await.expect("list").skills.is_empty());
    }
}
