//! Filesystem-backed skill registry.
//!
//! Implements [`SkillService`] and [`ToolRegistry`] against an in-memory
//! catalog populated from `SKILL.md` files on disk. Skills can also be
//! installed at runtime from remote registries via HTTP.

use std::path::PathBuf;
use std::sync::Arc;

use bitrouter_core::routers::registry::{SkillEntry, SkillService, ToolEntry, ToolRegistry};
use tokio::sync::RwLock;

use super::catalog::SkillCatalogEntry;
use super::installer;
use super::scanner;

/// A filesystem-backed skill registry.
///
/// Follows the [agentskills.io](https://agentskills.io) client-side model:
/// - **Startup**: scans `skills_dir` for `SKILL.md` files, builds in-memory catalog.
/// - **Runtime**: install/uninstall write to/remove from disk and update the catalog.
/// - **No database**: all persistence is the filesystem.
pub struct FilesystemSkillRegistry {
    catalog: Arc<RwLock<Vec<SkillCatalogEntry>>>,
    skills_dir: PathBuf,
    http_client: reqwest::Client,
}

impl Default for FilesystemSkillRegistry {
    fn default() -> Self {
        Self {
            catalog: Arc::new(RwLock::new(Vec::new())),
            skills_dir: PathBuf::from("skills"),
            http_client: reqwest::Client::new(),
        }
    }
}

impl FilesystemSkillRegistry {
    /// Create a registry by scanning an existing skills directory.
    pub async fn from_dir(skills_dir: PathBuf) -> Result<Self, String> {
        let entries = scanner::scan_skills_dir(&skills_dir).await?;
        Ok(Self {
            catalog: Arc::new(RwLock::new(entries)),
            skills_dir,
            http_client: reqwest::Client::new(),
        })
    }

    /// Create a registry from config-declared skills merged with filesystem scan.
    ///
    /// Config skills that don't have a `SKILL.md` on disk get a minimal one
    /// generated and written. Filesystem skills not in config are also included.
    pub async fn from_config_and_dir(
        configs: Vec<bitrouter_config::skill::SkillConfig>,
        skills_dir: PathBuf,
    ) -> Result<Self, String> {
        // Scan existing filesystem skills first.
        let mut entries = scanner::scan_skills_dir(&skills_dir).await?;

        // Merge config-declared skills: write SKILL.md for any not yet on disk.
        for cfg in configs {
            let already_exists = entries.iter().any(|e| e.name == cfg.name);
            if already_exists {
                continue;
            }

            let skill_dir = skills_dir.join(&cfg.name);
            let skill_md = skill_dir.join("SKILL.md");

            // Write a minimal SKILL.md from config.
            let required_apis: Vec<String> = cfg
                .required_apis
                .iter()
                .map(|a| a.provider.clone())
                .collect();

            let mut yaml_parts = vec![
                format!("name: \"{}\"", cfg.name),
                format!("description: \"{}\"", cfg.description),
            ];
            if !required_apis.is_empty() {
                yaml_parts.push("required_apis:".to_string());
                for api in &required_apis {
                    yaml_parts.push(format!("  - \"{api}\""));
                }
            }

            let content = format!("---\n{}\n---\n", yaml_parts.join("\n"),);

            tokio::fs::create_dir_all(&skill_dir)
                .await
                .map_err(|e| format!("failed to create {}: {e}", skill_dir.display()))?;
            tokio::fs::write(&skill_md, &content)
                .await
                .map_err(|e| format!("failed to write {}: {e}", skill_md.display()))?;

            let now = chrono::Utc::now().to_rfc3339();
            entries.push(SkillCatalogEntry {
                id: uuid::Uuid::new_v4().to_string(),
                name: cfg.name,
                description: cfg.description,
                source: cfg.source.unwrap_or_else(|| "config".to_string()),
                required_apis,
                path: skill_md,
                created_at: now.clone(),
                updated_at: now,
            });
        }

        Ok(Self {
            catalog: Arc::new(RwLock::new(entries)),
            skills_dir,
            http_client: reqwest::Client::new(),
        })
    }
}

impl SkillService for FilesystemSkillRegistry {
    /// Install a skill.
    ///
    /// If `source` is a URL (starts with `http://` or `https://`), fetches the
    /// `SKILL.md` content from that URL. Otherwise generates a minimal `SKILL.md`
    /// from the provided name and description. Writes to disk and adds to catalog.
    async fn create(
        &self,
        name: String,
        description: String,
        source: Option<String>,
        required_apis: Vec<String>,
    ) -> Result<SkillEntry, String> {
        // Check for duplicates.
        {
            let catalog = self.catalog.read().await;
            if catalog.iter().any(|e| e.name == name) {
                return Err(format!("skill '{name}' already exists"));
            }
        }

        let content = match source.as_deref() {
            Some(url) if url.starts_with("http://") || url.starts_with("https://") => {
                installer::fetch_skill(&self.http_client, url).await?
            }
            _ => {
                // Generate a minimal SKILL.md.
                let mut yaml_parts = vec![
                    format!("name: \"{name}\""),
                    format!("description: \"{description}\""),
                ];
                if !required_apis.is_empty() {
                    yaml_parts.push("required_apis:".to_string());
                    for api in &required_apis {
                        yaml_parts.push(format!("  - \"{api}\""));
                    }
                }
                format!("---\n{}\n---\n", yaml_parts.join("\n"))
            }
        };

        // Write to disk.
        let skill_dir = self.skills_dir.join(&name);
        let skill_md = skill_dir.join("SKILL.md");
        tokio::fs::create_dir_all(&skill_dir)
            .await
            .map_err(|e| format!("failed to create {}: {e}", skill_dir.display()))?;
        tokio::fs::write(&skill_md, &content)
            .await
            .map_err(|e| format!("failed to write {}: {e}", skill_md.display()))?;

        let now = chrono::Utc::now().to_rfc3339();
        let entry = SkillCatalogEntry {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            description,
            source: source.unwrap_or_else(|| "manual".to_string()),
            required_apis,
            path: skill_md,
            created_at: now.clone(),
            updated_at: now,
        };

        let skill_entry = entry.to_skill_entry();
        self.catalog.write().await.push(entry);
        Ok(skill_entry)
    }

    async fn list(&self) -> Result<Vec<SkillEntry>, String> {
        let catalog = self.catalog.read().await;
        Ok(catalog
            .iter()
            .map(SkillCatalogEntry::to_skill_entry)
            .collect())
    }

    async fn get(&self, name: &str) -> Result<Option<SkillEntry>, String> {
        let catalog = self.catalog.read().await;
        Ok(catalog
            .iter()
            .find(|e| e.name == name)
            .map(SkillCatalogEntry::to_skill_entry))
    }

    /// Uninstall a skill: remove from catalog and delete from disk.
    async fn delete(&self, name: &str) -> Result<bool, String> {
        let removed = {
            let mut catalog = self.catalog.write().await;
            catalog
                .iter()
                .position(|e| e.name == name)
                .map(|idx| catalog.remove(idx))
        };

        let Some(entry) = removed else {
            return Ok(false);
        };

        // Remove the skill directory from disk.
        let skill_dir = entry
            .path
            .parent()
            .ok_or_else(|| format!("invalid skill path: {}", entry.path.display()))?;

        if skill_dir.exists() {
            tokio::fs::remove_dir_all(skill_dir)
                .await
                .map_err(|e| format!("failed to remove {}: {e}", skill_dir.display()))?;
        }

        Ok(true)
    }
}

impl ToolRegistry for FilesystemSkillRegistry {
    async fn list_tools(&self) -> Vec<ToolEntry> {
        let catalog = self.catalog.read().await;
        catalog
            .iter()
            .map(SkillCatalogEntry::to_tool_entry)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn from_dir_empty() {
        let tmp = TempDir::new().expect("tempdir");
        let reg = FilesystemSkillRegistry::from_dir(tmp.path().to_path_buf())
            .await
            .expect("from_dir");
        let list = reg.list().await.expect("list");
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn create_and_list() {
        let tmp = TempDir::new().expect("tempdir");
        let reg = FilesystemSkillRegistry::from_dir(tmp.path().to_path_buf())
            .await
            .expect("from_dir");

        let entry = reg
            .create(
                "test-skill".to_string(),
                "A test skill".to_string(),
                None,
                vec!["openai".to_string()],
            )
            .await
            .expect("create");

        assert_eq!(entry.name, "test-skill");
        assert_eq!(entry.description, "A test skill");
        assert_eq!(entry.required_apis, vec!["openai"]);

        // Verify file was written.
        let skill_md = tmp.path().join("test-skill").join("SKILL.md");
        assert!(skill_md.exists());

        // Verify it appears in list.
        let list = reg.list().await.expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "test-skill");
    }

    #[tokio::test]
    async fn get_existing_and_missing() {
        let tmp = TempDir::new().expect("tempdir");
        let reg = FilesystemSkillRegistry::from_dir(tmp.path().to_path_buf())
            .await
            .expect("from_dir");

        reg.create(
            "my-skill".to_string(),
            "Does things".to_string(),
            None,
            vec![],
        )
        .await
        .expect("create");

        let found = reg.get("my-skill").await.expect("get");
        assert!(found.is_some());
        assert_eq!(found.as_ref().map(|e| e.name.as_str()), Some("my-skill"));

        let missing = reg.get("nonexistent").await.expect("get");
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn delete_existing() {
        let tmp = TempDir::new().expect("tempdir");
        let reg = FilesystemSkillRegistry::from_dir(tmp.path().to_path_buf())
            .await
            .expect("from_dir");

        reg.create(
            "doomed".to_string(),
            "Will be deleted".to_string(),
            None,
            vec![],
        )
        .await
        .expect("create");

        let skill_dir = tmp.path().join("doomed");
        assert!(skill_dir.exists());

        let deleted = reg.delete("doomed").await.expect("delete");
        assert!(deleted);
        assert!(!skill_dir.exists());

        let list = reg.list().await.expect("list");
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn delete_nonexistent() {
        let tmp = TempDir::new().expect("tempdir");
        let reg = FilesystemSkillRegistry::from_dir(tmp.path().to_path_buf())
            .await
            .expect("from_dir");

        let deleted = reg.delete("nope").await.expect("delete");
        assert!(!deleted);
    }

    #[tokio::test]
    async fn create_duplicate_rejected() {
        let tmp = TempDir::new().expect("tempdir");
        let reg = FilesystemSkillRegistry::from_dir(tmp.path().to_path_buf())
            .await
            .expect("from_dir");

        reg.create("dup".to_string(), "first".to_string(), None, vec![])
            .await
            .expect("create");

        let err = reg
            .create("dup".to_string(), "second".to_string(), None, vec![])
            .await;
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("already exists"));
    }

    #[tokio::test]
    async fn tool_registry_integration() {
        let tmp = TempDir::new().expect("tempdir");
        let reg = FilesystemSkillRegistry::from_dir(tmp.path().to_path_buf())
            .await
            .expect("from_dir");

        reg.create("my-tool".to_string(), "A tool".to_string(), None, vec![])
            .await
            .expect("create");

        let tools = reg.list_tools().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].id, "skill/my-tool");
        assert_eq!(tools[0].provider, "skill");
        assert_eq!(tools[0].name.as_deref(), Some("my-tool"));
    }

    #[tokio::test]
    async fn from_config_and_dir_merges() {
        let tmp = TempDir::new().expect("tempdir");

        // Pre-create a filesystem skill.
        let fs_skill_dir = tmp.path().join("fs-skill");
        std::fs::create_dir(&fs_skill_dir).expect("mkdir");
        std::fs::write(
            fs_skill_dir.join("SKILL.md"),
            "---\nname: fs-skill\ndescription: From filesystem\n---\n",
        )
        .expect("write");

        // Config-declared skill not yet on disk.
        let configs = vec![bitrouter_config::skill::SkillConfig {
            name: "cfg-skill".to_string(),
            description: "From config".to_string(),
            source: None,
            required_apis: vec![],
        }];

        let reg = FilesystemSkillRegistry::from_config_and_dir(configs, tmp.path().to_path_buf())
            .await
            .expect("from_config_and_dir");

        let list = reg.list().await.expect("list");
        assert_eq!(list.len(), 2);

        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"fs-skill"));
        assert!(names.contains(&"cfg-skill"));

        // Config skill should have been written to disk.
        assert!(tmp.path().join("cfg-skill").join("SKILL.md").exists());
    }
}
