//! Read existing skill directories without an installation index or version store.

#[cfg(feature = "harness")]
use crate::gateway::caller::CallerContext;
#[cfg(feature = "harness")]
use crate::skills::format;
#[cfg(feature = "harness")]
use crate::skills::library::SkillDirectory;
use anyhow::Result;
#[cfg(feature = "harness")]
use anyhow::{Context, ensure};
#[cfg(feature = "harness")]
use async_trait::async_trait;
#[cfg(feature = "harness")]
use bitrouter_ai::harness::skills::{LocalScope, LocalSkill, LocalSkillCatalog};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[cfg(feature = "harness")]
use super::files;

#[cfg(feature = "harness")]
pub(crate) struct Library {
    roots: Result<Vec<PathBuf>, String>,
}

pub(super) fn global_roots() -> Result<Vec<PathBuf>> {
    let home = super::root::home_dir()?;
    let mut roots = vec![
        home.join(".agents"),
        home.join(".claude"),
        home.join(".codex"),
    ];
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        roots.push(PathBuf::from(home));
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

#[cfg(feature = "harness")]
impl Library {
    pub fn runtime() -> Self {
        Self {
            roots: global_roots().map_err(|error| error.to_string()),
        }
    }
}

#[cfg(feature = "harness")]
pub(super) fn scan(
    roots: &[PathBuf],
    scope: LocalScope,
    workspace: &Path,
    caller: &CallerContext,
) -> Result<LocalSkillCatalog> {
    let mut catalog = LocalSkillCatalog::default();
    let mut seen = std::collections::BTreeSet::new();
    for root in roots {
        if !root.try_exists()? {
            continue;
        }
        let root = root.canonicalize()?;
        for found in format::discover_bounded(&root)? {
            if let Some(problem) = found.problem() {
                catalog
                    .problems
                    .push(format!("{}: {problem}", found.skill_md.display()));
                continue;
            }
            let directory = found.dir.canonicalize()?;
            if !seen.insert(directory.clone()) {
                continue;
            }
            let metadata = match files::inspect(&directory).and_then(|metadata| {
                metadata.validate()?;
                Ok(metadata)
            }) {
                Ok(metadata) => metadata,
                Err(error) => {
                    catalog
                        .problems
                        .push(format!("{}: {error}", directory.display()));
                    continue;
                }
            };
            let instruction = metadata
                .files
                .iter()
                .find(|file| file.path == "SKILL.md")
                .context("missing SKILL.md")?;
            let directory_id = directory_id(&directory);
            catalog.skills.push(LocalSkill {
                scope: scope.clone(),
                directory,
                workspace: workspace.into(),
                api_key_id: caller.api_key_id().into(),
                user_id: caller.user_id().into(),
                local: caller.is_local(),
                directory_id,
                name: found.name(),
                description: found.description().into(),
                version: metadata.digest.clone(),
                receipt: serde_json::json!({"bundle":metadata}),
                instruction_sha256: instruction.digest.clone(),
                instruction_bytes: instruction.bytes,
            });
        }
    }
    catalog.skills.sort_by(|a, b| a.directory.cmp(&b.directory));
    ensure!(
        catalog.skills.len() <= 256,
        "local skill catalog exceeds 256 entries"
    );
    Ok(catalog)
}

#[cfg(feature = "harness")]
#[async_trait]
impl SkillDirectory for Library {
    async fn catalog(
        &self,
        workspace: &Path,
        caller: &CallerContext,
    ) -> Result<LocalSkillCatalog, String> {
        let operation = || -> Result<LocalSkillCatalog> {
            ensure!(
                !caller.is_anonymous(),
                "skill directories require an admitted caller"
            );
            let workspace = workspace.canonicalize()?;
            let mut catalog = scan(
                std::slice::from_ref(&workspace),
                LocalScope::Project,
                &workspace,
                caller,
            )?;
            match &self.roots {
                Ok(roots) => {
                    let global = scan(roots, LocalScope::Global, &workspace, caller)?;
                    catalog.skills.extend(global.skills);
                    catalog.problems.extend(global.problems);
                }
                Err(error) => catalog.problems.push(error.clone()),
            }
            Ok(catalog)
        };
        operation().map_err(|error| error.to_string())
    }
    async fn read(
        &self,
        workspace: &Path,
        caller: &CallerContext,
        binding: &LocalSkill,
        relative_path: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>, String> {
        let operation = || -> Result<Vec<u8>> {
            let workspace = workspace.canonicalize()?;
            ensure!(
                !caller.is_anonymous()
                    && binding.workspace == workspace
                    && binding.api_key_id == caller.api_key_id()
                    && binding.user_id == caller.user_id()
                    && binding.local == caller.is_local(),
                "bound skill caller/workspace changed"
            );
            let roots = match binding.scope {
                LocalScope::Project => vec![workspace],
                LocalScope::Global => self.roots.clone().map_err(anyhow::Error::msg)?,
            };
            ensure!(
                roots.iter().any(|root| root
                    .canonicalize()
                    .is_ok_and(|root| binding.directory.starts_with(root))),
                "skill directory is outside the admitted roots"
            );
            ensure!(
                binding.directory.canonicalize()? == binding.directory,
                "bound skill directory changed"
            );
            let metadata: files::Metadata = serde_json::from_value(
                binding
                    .receipt
                    .get("bundle")
                    .context("missing directory manifest")?
                    .clone(),
            )?;
            metadata.validate()?;
            ensure!(
                metadata.digest == binding.version,
                "directory manifest fingerprint changed"
            );
            let file = metadata
                .files
                .iter()
                .find(|file| file.path == relative_path)
                .context("file is outside the bound directory manifest")?;
            ensure!(file.bytes <= max_bytes, "skill file exceeds context budget");
            let path = binding.directory.join(files::relative(relative_path)?);
            let bytes = files::read_regular(&binding.directory, &path, file.bytes)?;
            ensure!(
                bytes.len() as u64 == file.bytes
                    && hex::encode(Sha256::digest(&bytes)) == file.digest
                    && files::executable(&std::fs::metadata(path)?) == file.executable,
                "skill directory file changed; start a new Turn to use its new content"
            );
            Ok(bytes)
        };
        operation().map_err(|error| error.to_string())
    }
}

pub(super) fn directory_id(path: &Path) -> String {
    format!(
        "skill_{}",
        hex::encode(Sha256::digest(path.to_string_lossy().as_bytes()))
    )
}

#[cfg(all(test, feature = "harness"))]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unusable_directory_does_not_hide_an_independent_valid_skill() -> Result<()> {
        let project = tempfile::tempdir()?;
        let root = project.path().canonicalize()?;
        for name in ["valid", "invalid"] {
            let directory = root.join("skills").join(name);
            std::fs::create_dir_all(&directory)?;
            std::fs::write(
                directory.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: local directory\n---\nROOT\n"),
            )?;
        }
        std::os::unix::fs::symlink(&root, root.join("skills/invalid/outside"))?;
        let catalog = Library { roots: Ok(vec![]) }
            .catalog(&root, &CallerContext::local())
            .await
            .map_err(anyhow::Error::msg)?;
        ensure!(catalog.skills.len() == 1 && catalog.skills[0].name == "valid");
        ensure!(catalog.problems.len() == 1);
        Ok(())
    }

    #[tokio::test]
    async fn existing_directory_reads_verify_the_original_manifest_and_caller_without_creating_a_store()
    -> Result<()> {
        let project = tempfile::tempdir()?;
        let root = project.path().canonicalize()?;
        let source = root.join(".agents/skills/review");
        std::fs::create_dir_all(source.join("references"))?;
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: review\ndescription: local directory\n---\nROOT\n",
        )?;
        std::fs::write(source.join("references/check.md"), "original")?;
        let library = Library { roots: Ok(vec![]) };
        let caller = CallerContext::local();
        let catalog = library
            .catalog(&root, &caller)
            .await
            .map_err(anyhow::Error::msg)?;
        let binding = catalog.skills.first().context("directory skill")?;
        ensure!(binding.directory == source);
        ensure!(
            library
                .read(&root, &caller, binding, "references/check.md", 1024)
                .await
                .map_err(anyhow::Error::msg)?
                == b"original"
        );
        ensure!(
            library
                .read(
                    &root,
                    &CallerContext::new("another", "another"),
                    binding,
                    "SKILL.md",
                    1024
                )
                .await
                .is_err()
        );
        ensure!(
            library
                .read(&root, &caller, binding, "../../outside", 1024)
                .await
                .is_err()
        );
        std::fs::write(source.join("references/check.md"), "modified")?;
        ensure!(
            library
                .read(&root, &caller, binding, "references/check.md", 1024)
                .await
                .is_err()
        );
        ensure!(!root.join(".bitrouter/skills").exists());
        Ok(())
    }
}
