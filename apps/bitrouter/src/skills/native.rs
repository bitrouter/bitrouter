//! The native host reads the installer-owned store; it never republishes aliases.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bitrouter_orchestrator::harness::skills::library::{
    LibraryRevision, LibraryScope, ManagedSkill, ManagedSkillCatalog, SkillLibrary,
};
use bitrouter_sdk::caller::CallerContext;
use sha2::{Digest, Sha256};

use super::bundle;
use super::managed::{Kind, Scope, Store, Version};

pub(crate) struct Library {
    home: Result<PathBuf, String>,
}

impl Library {
    pub fn runtime() -> Self {
        Self {
            home: crate::paths::runtime_home().map_err(|error| error.to_string()),
        }
    }

    fn global_scope(&self) -> Result<Scope> {
        Scope::native_global(
            self.home
                .as_ref()
                .map_err(|error| anyhow::anyhow!(error.clone()))?,
        )
    }
}

fn check_caller(caller: &CallerContext) -> Result<()> {
    ensure!(
        !caller.is_anonymous(),
        "native library requires an admitted caller/workspace"
    );
    Ok(())
}

/// Discovery prefers an explicitly installed workspace index, even when empty
/// or invalid. A frozen read instead keeps its original authorized root; a new
/// workspace index cannot redirect an existing Git-root binding.
async fn project_scope(workspace: &Path, bound_library: Option<&Path>) -> Result<Scope> {
    let direct = Scope::resolve(false, Some(workspace)).await?;
    if bound_library == Some(direct.library.as_path()) {
        return Ok(direct);
    }
    if bound_library.is_none() {
        direct.check_paths()?;
        if direct.index().try_exists()? {
            return Ok(direct);
        }
    }
    let root = super::source::project_root(workspace).await?;
    let project = Scope::resolve(false, Some(&root)).await?;
    if let Some(bound_library) = bound_library {
        ensure!(
            project.library == bound_library,
            "bound project library no longer belongs to this workspace's Git root"
        );
    }
    Ok(project)
}

fn catalog(
    scopes: Vec<Scope>,
    workspace: PathBuf,
    caller: CallerContext,
) -> Result<ManagedSkillCatalog> {
    let mut catalog = ManagedSkillCatalog::default();
    for scope in scopes {
        let store = match Store::load(scope.clone()) {
            Ok(store) => store,
            Err(error) => {
                catalog.problems.push(format!(
                    "{:?} library {}: {error}",
                    scope.kind,
                    scope.library.display()
                ));
                continue;
            }
        };
        catalog.revisions.push(LibraryRevision {
            scope: if scope.kind == Kind::Project {
                LibraryScope::Project
            } else {
                LibraryScope::Global
            },
            library: scope.library.clone(),
            revision: store.revision(),
        });
        for installation in store.active_installations() {
            let root = installation
                .current
                .bundle
                .files
                .iter()
                .find(|file| file.path == "SKILL.md")
                .context("installed receipt has no SKILL.md")?;
            catalog.skills.push(ManagedSkill {
                scope: if scope.kind == Kind::Project {
                    LibraryScope::Project
                } else {
                    LibraryScope::Global
                },
                library: scope.library.clone(),
                workspace: workspace.clone(),
                api_key_id: caller.api_key_id().into(),
                user_id: caller.user_id().into(),
                local: caller.is_local(),
                installation_id: installation.id.clone(),
                catalog_revision: store.revision(),
                name: installation.name.clone(),
                description: installation.description.clone(),
                version: installation.current.bundle.digest.clone(),
                receipt: serde_json::to_value(&installation.current)?,
                instruction_sha256: root.digest.clone(),
                instruction_bytes: root.bytes,
            });
        }
    }
    Ok(catalog)
}

fn read(
    store: Store,
    binding: ManagedSkill,
    relative_path: String,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    ensure!(
        store.scope.library == binding.library,
        "bound library root changed"
    );
    let receipt: Version = serde_json::from_value(binding.receipt.clone())
        .context("invalid bound installation receipt")?;
    let version = store.retained_version(&binding.installation_id, &receipt)?;
    ensure!(
        version.bundle.digest == binding.version,
        "bound bundle version differs from receipt"
    );
    let file = version
        .bundle
        .files
        .iter()
        .find(|file| file.path == relative_path)
        .context("skill file is outside its immutable bundle manifest")?;
    ensure!(
        file.bytes <= max_bytes,
        "skill file exceeds the host/context byte budget"
    );
    let relative = bundle::relative(&relative_path)?;
    let root = store
        .scope
        .library
        .join("versions")
        .join(&version.bundle.digest);
    check_directories(&store.scope.library, &root, &relative)?;
    let bytes = bundle::read_regular(&store.scope.library, &root.join(&relative), file.bytes)?;
    check_directories(&store.scope.library, &root, &relative)?;
    ensure!(
        bundle::executable(&std::fs::metadata(root.join(&relative))?) == file.executable,
        "bound skill executable mode differs from its immutable receipt"
    );
    ensure!(
        bytes.len() as u64 == file.bytes && hex::encode(Sha256::digest(&bytes)) == file.digest,
        "bound skill raw byte size/digest mismatch"
    );
    if relative_path == "SKILL.md" {
        ensure!(
            file.digest == binding.instruction_sha256 && file.bytes == binding.instruction_bytes,
            "bound instruction metadata differs from receipt"
        );
        let text = std::str::from_utf8(&bytes).context("SKILL.md requires UTF-8")?;
        let frontmatter = bitrouter_orchestrator::harness::skills::format::parse_frontmatter(text)?;
        frontmatter.validate()?;
        ensure!(
            frontmatter.name == binding.name && frontmatter.description == binding.description,
            "bound skill identity differs from original version"
        );
    }
    Ok(bytes)
}

fn check_directories(library: &Path, root: &Path, relative: &Path) -> Result<()> {
    let parent = root
        .join(relative)
        .parent()
        .context("bound file parent")?
        .to_path_buf();
    let mut path = library.to_path_buf();
    for component in parent.strip_prefix(library)?.components() {
        path.push(component);
        let metadata = std::fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_dir() && !metadata.is_symlink(),
            "bound skill directories must remain real directories"
        );
    }
    Ok(())
}

#[async_trait]
impl SkillLibrary for Library {
    async fn catalog(
        &self,
        workspace: &Path,
        caller: &CallerContext,
    ) -> Result<ManagedSkillCatalog, String> {
        check_caller(caller).map_err(|error| error.to_string())?;
        let workspace = workspace
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let mut scopes = Vec::new();
        let mut problems = Vec::new();
        for (kind, result) in [
            ("project", project_scope(&workspace, None).await),
            ("global", self.global_scope()),
        ] {
            match result {
                Ok(scope) => scopes.push(scope),
                Err(error) => problems.push(format!("{kind} library: {error}")),
            }
        }
        let caller = caller.clone();
        let mut catalog = tokio::task::spawn_blocking(move || catalog(scopes, workspace, caller))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        catalog.problems.extend(problems);
        Ok(catalog)
    }

    async fn read(
        &self,
        workspace: &Path,
        caller: &CallerContext,
        binding: &ManagedSkill,
        relative_path: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>, String> {
        check_caller(caller).map_err(|error| error.to_string())?;
        let workspace = workspace
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if binding.workspace != workspace
            || binding.api_key_id != caller.api_key_id()
            || binding.user_id != caller.user_id()
            || binding.local != caller.is_local()
        {
            return Err("bound skill caller/workspace changed".into());
        }
        let scope = match binding.scope {
            LibraryScope::Project => project_scope(&workspace, Some(&binding.library)).await,
            LibraryScope::Global => self.global_scope(),
        }
        .map_err(|error| error.to_string())?;
        let binding = binding.clone();
        let relative_path = relative_path.to_string();
        tokio::task::spawn_blocking(move || {
            read(Store::load(scope)?, binding, relative_path, max_bytes)
        })
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error: anyhow::Error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bitrouter_orchestrator::agent::ToolMode;
    use bitrouter_orchestrator::harness::HarnessConfig;
    use bitrouter_orchestrator::harness::managed::NativeResources;
    use tokio_util::sync::CancellationToken;

    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>;

    struct CapturePrompts(Arc<tokio::sync::Mutex<Vec<bitrouter_ai::types::Prompt>>>);

    #[async_trait]
    impl bitrouter_sdk::language_model::hooks::ObserveHook for CapturePrompts {
        async fn after_phase(
            &self,
            _: bitrouter_sdk::language_model::hooks::Phase,
            _: &bitrouter_sdk::language_model::context::PipelineContext,
        ) {
        }
        async fn on_stream_part(
            &self,
            _: &bitrouter_sdk::language_model::context::StreamContext,
            _: &bitrouter_ai::types::StreamPart,
        ) {
        }
        async fn on_request_end(
            &self,
            _: &bitrouter_sdk::language_model::context::PipelineContext,
            _: &bitrouter_sdk::language_model::hooks::RequestOutcome,
        ) {
        }
        async fn on_hop_start(
            &self,
            ctx: &bitrouter_sdk::language_model::context::PipelineContext,
            _: &bitrouter_sdk::language_model::RoutingTarget,
        ) {
            if let Some(prompt) = ctx.input().generation_prompt() {
                self.0.lock().await.push(prompt.clone());
            }
        }
    }

    struct WorkflowDecision;

    #[async_trait]
    impl bitrouter_sdk::decision_model::DecisionExecutor for WorkflowDecision {
        async fn execute(
            &self,
            request: &bitrouter_sdk::decision_model::types::DecisionRequest,
            _: &CancellationToken,
        ) -> Result<
            bitrouter_sdk::decision_model::types::DecisionResponse,
            bitrouter_sdk::decision_model::types::DecisionError,
        > {
            use bitrouter_sdk::decision_model::types::{
                Answer, DecisionError, DecisionResponse, DecisionUsage, Question,
            };
            let mut answers = std::collections::BTreeMap::new();
            for (id, question) in &request.questions {
                let answer = match question {
                    Question::Noul { .. } => Answer::Noul { noul: 0.99 },
                    Question::Choice { criteria, .. } => {
                        let choice = criteria
                            .keys()
                            .find(|key| key.as_str() == "full")
                            .or_else(|| criteria.keys().next())
                            .ok_or_else(|| {
                                DecisionError::invalid_request("fixture choice has no alternatives")
                            })?
                            .clone();
                        Answer::Choice {
                            choice: choice.clone(),
                            confidence: 1.0,
                            probabilities: criteria
                                .keys()
                                .map(|key| (key.clone(), if *key == choice { 1.0 } else { 0.0 }))
                                .collect(),
                        }
                    }
                    Question::Score { criteria, .. } => Answer::Score {
                        score: 0.0,
                        confidence: 1.0,
                        legend: criteria
                            .iter()
                            .enumerate()
                            .map(|(index, value)| (index.to_string(), value.clone()))
                            .collect(),
                        probabilities: criteria
                            .iter()
                            .enumerate()
                            .map(|(index, _)| {
                                (index.to_string(), if index == 0 { 1.0 } else { 0.0 })
                            })
                            .collect(),
                    },
                };
                answers.insert(id.clone(), answer);
            }
            Ok(DecisionResponse {
                model: request.model.clone(),
                answers,
                usage: DecisionUsage {
                    input_tokens: 17,
                    output_tokens: 3,
                },
            })
        }
    }

    fn fixture_routing() -> bitrouter_sdk::language_model::StaticRoutingTable {
        use bitrouter_ai::types::{ApiProtocol, AuthScheme};
        use bitrouter_sdk::language_model::{RoutingTarget, StaticRoutingTable};
        let routing = StaticRoutingTable::new();
        routing.insert(
            "fixture-model",
            vec![RoutingTarget {
                provider_name: "fixture".into(),
                service_id: "fixture-model".into(),
                api_base: "https://example.invalid".into(),
                api_key: "fixture-key".into(),
                api_protocol: ApiProtocol::ChatCompletions,
                chat_token_limit_field: None,
                chat_supports_store: None,
                chat_supports_stream_options: None,
                chat_google_extensions: false,
                reasoning_effort: None,
                model_constraints: Default::default(),
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                auth_scheme: AuthScheme::Bearer,
                headers: vec![],
            }],
        );
        routing
    }

    fn workflow_runtime() -> bitrouter_sdk::decision_model::DecisionRuntime {
        bitrouter_sdk::decision_model::DecisionRuntime {
            model: "workflow-fixture".into(),
            executor: Arc::new(WorkflowDecision),
            policy: Default::default(),
            pricing: None,
        }
    }

    struct RejectNativeEffects;

    #[async_trait]
    impl bitrouter_orchestrator::harness::managed::session::NativeApproval for RejectNativeEffects {
        async fn approve(
            &self,
            _: &bitrouter_orchestrator::core::protocol::ToolExecute,
        ) -> Result<bool, bitrouter_orchestrator::core::protocol::CoreError> {
            Ok(false)
        }
    }

    fn write_source(root: &Path, marker: &str) -> Result<PathBuf> {
        let source = root.join("review");
        std::fs::create_dir_all(source.join("references"))?;
        std::fs::write(
            source.join("SKILL.md"),
            format!(
                "---\nname: review\ndescription: review workflow\nmetadata:\n  version: fixture\n---\n{marker}\n"
            ),
        )?;
        std::fs::write(
            source.join("references/check.md"),
            format!("support {marker}\n"),
        )?;
        Ok(source)
    }

    fn config(home: &Path) -> HarnessConfig {
        HarnessConfig {
            library: Some(Arc::new(Library {
                home: Ok(home.into()),
            })),
            ..Default::default()
        }
    }

    async fn resources(workspace: &Path, home: &Path) -> Result<NativeResources> {
        Ok(NativeResources::discover_for_caller(
            workspace,
            ToolMode::ReadOnly,
            &config(home),
            &CallerContext::local(),
            &CancellationToken::new(),
        )
        .await?)
    }

    async fn init_git(root: &Path) -> Result<()> {
        let status = tokio::process::Command::new("git")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", root.join("missing-global-git-config"))
            .args(["init", "--quiet", "--template="])
            .arg(root)
            .status()
            .await?;
        ensure!(status.success(), "isolated fixture Git init failed");
        Ok(())
    }

    #[tokio::test]
    async fn nested_git_workspace_uses_project_library_but_keeps_explicit_scope_and_frozen_reads()
    -> TestResult {
        let repository = tempfile::tempdir()?;
        let home = tempfile::tempdir()?;
        let sources = tempfile::tempdir()?;
        let workspace = repository.path().join("apps/nested");
        std::fs::create_dir_all(&workspace)?;
        init_git(repository.path()).await?;
        let root_scope = Scope::resolve(false, Some(repository.path())).await?;
        let source = write_source(sources.path(), "GIT_ROOT_VERSION")?;
        Store::load(root_scope.clone())?
            .install(source.to_str().context("source")?, None, None)
            .await?;
        let library = Library {
            home: Ok(home.path().into()),
        };
        let caller = CallerContext::local();
        let catalog = library.catalog(&workspace, &caller).await?;
        assert_eq!(catalog.skills.len(), 1);
        let original = catalog.skills[0].clone();
        assert_eq!(original.library, root_scope.library);
        assert_eq!(original.workspace, workspace.canonicalize()?);
        assert!(
            String::from_utf8(
                library
                    .read(&workspace, &caller, &original, "SKILL.md", 65536)
                    .await?
            )?
            .contains("GIT_ROOT_VERSION")
        );

        let child_scope = Scope::resolve(false, Some(&workspace)).await?;
        write_source(sources.path(), "EXPLICIT_CHILD_VERSION")?;
        Store::load(child_scope.clone())?
            .install(source.to_str().context("source")?, None, None)
            .await?;
        let child = library.catalog(&workspace, &caller).await?;
        assert_eq!(child.skills.len(), 1);
        assert_eq!(child.skills[0].library, child_scope.library);
        assert_ne!(child.skills[0].installation_id, original.installation_id);
        assert!(
            String::from_utf8(
                library
                    .read(&workspace, &caller, &child.skills[0], "SKILL.md", 65536)
                    .await?
            )?
            .contains("EXPLICIT_CHILD_VERSION")
        );
        assert!(
            String::from_utf8(
                library
                    .read(&workspace, &caller, &original, "SKILL.md", 65536)
                    .await?
            )?
            .contains("GIT_ROOT_VERSION")
        );
        Store::load(child_scope)?.uninstall("review")?;
        assert!(
            library
                .catalog(&workspace, &caller)
                .await?
                .skills
                .is_empty(),
            "an explicit empty scope must not fall back to a broader project library"
        );
        assert!(
            library
                .read(&workspace, &caller, &original, "SKILL.md", 65536)
                .await
                .is_ok()
        );
        std::fs::rename(
            repository.path().join(".git"),
            repository.path().join("removed-git"),
        )?;
        assert!(
            library
                .read(&workspace, &caller, &original, "SKILL.md", 65536)
                .await
                .is_err(),
            "a frozen binding cannot manufacture an obsolete Git-root authorization"
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_explicit_workspace_library_never_falls_back_to_the_git_root() -> TestResult {
        let repository = tempfile::tempdir()?;
        let workspace = repository.path().join("nested");
        std::fs::create_dir(&workspace)?;
        init_git(repository.path()).await?;
        let home = tempfile::tempdir()?;
        let sources = tempfile::tempdir()?;
        let source = write_source(sources.path(), "VALID_PROJECT")?;
        let root = Scope::resolve(false, Some(repository.path())).await?;
        Store::load(root)?
            .install(source.to_str().context("source")?, None, None)
            .await?;
        let library = Library {
            home: Ok(home.path().into()),
        };
        let caller = CallerContext::local();
        let original = library
            .catalog(&workspace, &caller)
            .await?
            .skills
            .into_iter()
            .next()
            .context("Git binding")?;
        let child = Scope::resolve(false, Some(&workspace)).await?;
        Store::load(child.clone())?
            .install(source.to_str().context("source")?, None, None)
            .await?;
        std::fs::write(child.index(), "invalid installed index")?;
        let invalid = library.catalog(&workspace, &caller).await?;
        assert!(invalid.skills.is_empty());
        assert!(
            invalid
                .problems
                .iter()
                .any(|problem| problem.contains("installed.json"))
        );
        assert!(
            library
                .read(&workspace, &caller, &original, "SKILL.md", 65536)
                .await
                .is_ok()
        );
        #[cfg(unix)]
        {
            std::fs::rename(workspace.join(".bitrouter"), workspace.join("old-library"))?;
            std::os::unix::fs::symlink("missing-target", workspace.join(".bitrouter"))?;
            let linked = library.catalog(&workspace, &caller).await?;
            assert!(linked.skills.is_empty());
            assert!(
                linked
                    .problems
                    .iter()
                    .any(|problem| problem.contains("real directories"))
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn native_library_preserves_original_versions_after_update_uninstall_and_reload()
    -> TestResult {
        let workspace = tempfile::tempdir()?;
        let home = tempfile::tempdir()?;
        let source_root = tempfile::tempdir()?;
        let source = write_source(source_root.path(), "PROJECT_V1")?;
        let project = Scope::resolve(false, Some(workspace.path())).await?;
        let global = Scope::native_global(home.path())?;
        Store::load(project.clone())?
            .install(source.to_str().context("source path")?, None, None)
            .await?;
        write_source(source_root.path(), "GLOBAL_V1")?;
        Store::load(global)?
            .install(source.to_str().context("source path")?, None, None)
            .await?;
        let owner = resources(workspace.path(), home.path()).await?;
        assert_eq!(owner.managed_skill_catalog().skills.len(), 2);
        let bound = owner
            .managed_skill_catalog()
            .skills
            .iter()
            .find(|skill| skill.scope == LibraryScope::Project)
            .context("project binding")?
            .clone();
        let global = owner
            .managed_skill_catalog()
            .skills
            .iter()
            .find(|skill| skill.scope == LibraryScope::Global)
            .context("global binding")?;
        assert_ne!(bound.installation_id, global.installation_id);
        assert_ne!(
            bound.material()?.material_id,
            global.material()?.material_id
        );
        let material = bound.material()?;
        let loaded = owner
            .material(&material.material_id, &material.version)
            .await?;
        assert!(
            loaded
                .content
                .as_deref()
                .is_some_and(|body| body.contains("PROJECT_V1"))
        );
        let original = serde_json::to_vec(&bound)?;
        write_source(source_root.path(), "PROJECT_V1")?;
        std::fs::write(source.join("references/check.md"), "SUPPORT_ONLY_UPDATE\n")?;
        Store::load(project.clone())?.update("review", None).await?;
        let next = resources(workspace.path(), home.path()).await?;
        let updated = next
            .managed_skill_catalog()
            .skills
            .iter()
            .find(|skill| skill.scope == LibraryScope::Project)
            .context("updated binding")?;
        assert_eq!(updated.installation_id, bound.installation_id);
        assert_ne!(updated.version, bound.version);
        assert_eq!(
            updated.instruction_sha256, bound.instruction_sha256,
            "whole-bundle identity changes even when SKILL.md does not"
        );
        assert_ne!(updated.material()?.material_id, material.material_id);
        assert!(
            String::from_utf8(next.managed_skill_file(updated, "SKILL.md", 65536).await?)?
                .contains("PROJECT_V1")
        );
        assert_eq!(
            next.managed_skill_file(updated, "references/check.md", 65536)
                .await?,
            b"SUPPORT_ONLY_UPDATE\n"
        );
        Store::load(project)?.uninstall("review")?;
        next.shutdown().await?;
        owner.shutdown().await?;
        drop(next);
        drop(owner);
        let restored: ManagedSkill = serde_json::from_slice(&original)?;
        let restarted = resources(workspace.path(), home.path()).await?;
        assert_eq!(
            restarted.managed_skill_catalog().skills.len(),
            1,
            "uninstalled project alias is not a new candidate"
        );
        assert!(
            String::from_utf8(
                restarted
                    .managed_skill_file(&restored, "SKILL.md", 65536)
                    .await?
            )?
            .contains("PROJECT_V1")
        );
        assert_eq!(
            restarted
                .managed_skill_file(&restored, "references/check.md", 65536)
                .await?,
            b"support PROJECT_V1\n"
        );
        restarted.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn native_library_rechecks_scope_manifest_and_actual_bytes_without_prefetch() -> TestResult
    {
        let workspace = tempfile::tempdir()?;
        let other_workspace = tempfile::tempdir()?;
        let home = tempfile::tempdir()?;
        let source_root = tempfile::tempdir()?;
        let source = write_source(source_root.path(), "ORIGINAL")?;
        let scope = Scope::resolve(false, Some(workspace.path())).await?;
        Store::load(scope)?
            .install(source.to_str().context("source path")?, None, None)
            .await?;
        let library = Library {
            home: Ok(home.path().into()),
        };
        let caller = CallerContext::local();
        let bound = library
            .catalog(workspace.path(), &caller)
            .await?
            .skills
            .into_iter()
            .next()
            .context("binding")?;
        assert!(
            library
                .read(other_workspace.path(), &caller, &bound, "SKILL.md", 65536)
                .await
                .is_err()
        );
        assert!(
            library
                .read(
                    workspace.path(),
                    &CallerContext::new("other", "other"),
                    &bound,
                    "SKILL.md",
                    65536
                )
                .await
                .is_err()
        );
        assert!(
            library
                .read(workspace.path(), &caller, &bound, "../SKILL.md", 65536)
                .await
                .is_err()
        );
        assert!(
            library
                .read(
                    workspace.path(),
                    &caller,
                    &bound,
                    "references/missing",
                    65536
                )
                .await
                .is_err()
        );
        assert!(
            library
                .read(workspace.path(), &caller, &bound, "SKILL.md", 1)
                .await
                .is_err()
        );
        let payload = bound
            .library
            .join("versions")
            .join(&bound.version)
            .join("SKILL.md");
        bundle::mode(&payload, false, false)?;
        std::fs::write(&payload, "x".repeat(bound.instruction_bytes as usize))?;
        let fresh = resources(workspace.path(), home.path()).await?;
        assert_eq!(
            fresh.managed_skill_catalog().skills.len(),
            1,
            "catalog uses receipt metadata, not a body prefetch"
        );
        assert!(
            fresh
                .managed_skill_file(&bound, "SKILL.md", 65536)
                .await
                .is_err()
        );
        fresh.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn unavailable_global_library_does_not_hide_an_authorized_project_version() -> TestResult
    {
        let workspace = tempfile::tempdir()?;
        let source_root = tempfile::tempdir()?;
        let source = write_source(source_root.path(), "PROJECT_ONLY")?;
        let scope = Scope::resolve(false, Some(workspace.path())).await?;
        Store::load(scope)?
            .install(source.to_str().context("source path")?, None, None)
            .await?;
        let library = Library {
            home: Err("global home unavailable".into()),
        };
        let catalog = library
            .catalog(workspace.path(), &CallerContext::local())
            .await?;
        assert_eq!(catalog.skills.len(), 1);
        assert!(
            catalog
                .problems
                .iter()
                .any(|problem| problem.contains("global home unavailable"))
        );
        assert!(
            String::from_utf8(
                library
                    .read(
                        workspace.path(),
                        &CallerContext::local(),
                        &catalog.skills[0],
                        "SKILL.md",
                        65536
                    )
                    .await?
            )?
            .contains("PROJECT_ONLY")
        );
        Ok(())
    }

    #[tokio::test]
    async fn native_thread_skill_selection_refreshes_versions_without_changing_tool_authority()
    -> TestResult {
        use bitrouter_ai::types::{FinishReason, StreamPart};
        use bitrouter_orchestrator::agent::AgentConfig;
        use bitrouter_orchestrator::service::ThreadService;
        use bitrouter_orchestrator::store::{
            ExecutionRecord, ExecutionStore, MemoryExecutionStore,
        };
        use bitrouter_orchestrator::thread::{PermissionProfile, ThreadRequest, ThreadTarget};
        use bitrouter_orchestrator::turn::{TurnRequest, TurnStatus};
        use bitrouter_sdk::language_model::{MockExecutor, MockResponse};

        let workspace = tempfile::tempdir()?;
        let home = tempfile::tempdir()?;
        let source_root = tempfile::tempdir()?;
        let source = write_source(source_root.path(), "PROJECT_V1")?;
        let scope = Scope::resolve(false, Some(workspace.path())).await?;
        Store::load(scope.clone())?
            .install(source.to_str().context("source path")?, None, None)
            .await?;
        let routing = fixture_routing();
        let prompts = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let executor = MockExecutor::new(
            (0..3)
                .map(|_| {
                    MockResponse::Stream(vec![
                        StreamPart::TextDelta {
                            text: "done".into(),
                        },
                        StreamPart::Finish {
                            reason: FinishReason::Stop,
                        },
                    ])
                })
                .collect(),
        );
        let app = Arc::new(
            bitrouter_sdk::App::builder()
                .language_model(|builder| {
                    builder
                        .routing_table(Arc::new(routing))
                        .executor(Arc::new(executor))
                        .observe_hook(CapturePrompts(prompts.clone()));
                })
                .decision_model(workflow_runtime())
                .build()?,
        );
        let store = Arc::new(MemoryExecutionStore::default());
        let service = ThreadService::with_store(app, &[workspace.path().into()], store.clone())?
            .with_resources(config(home.path()))?;
        let caller = CallerContext::local();
        let created = service
            .create_thread(
                &service.capabilities().server_instance_id,
                ThreadRequest {
                    caller: caller.clone(),
                    workspace: workspace.path().into(),
                    config: AgentConfig::fixed("fixture-model", None).read_only(),
                    permission_profile: PermissionProfile::ReadOnly,
                    verification_command: None,
                    idempotency_key: "catalog-thread".into(),
                },
            )
            .await?;
        let target = ThreadTarget {
            thread_id: created.thread_id.clone(),
            server_instance_id: created.server_instance_id.clone(),
        };
        for phase in 0..3 {
            if phase == 1 {
                write_source(source_root.path(), "PROJECT_V2")?;
                Store::load(scope.clone())?.update("review", None).await?;
            } else if phase == 2 {
                Store::load(scope.clone())?.uninstall("review")?;
            }
            let receipt = service
                .start_turn(
                    &target,
                    &caller,
                    TurnRequest {
                        prompt: format!("inspect phase {phase}"),
                        idempotency_key: format!("catalog-phase-{phase}"),
                    },
                )
                .await?;
            let settled = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    let snapshot = service.read_turn(&target, &caller, &receipt.turn_id)?;
                    if snapshot.status.terminal() {
                        return Ok::<_, bitrouter_orchestrator::service::ServiceError>(snapshot);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await??;
            assert_eq!(
                settled.status,
                TurnStatus::Completed,
                "{:?}",
                settled.detail
            );
        }
        let journal = store
            .load(&target.thread_id)
            .await?
            .context("thread journal")?;
        let inventories = journal
            .records
            .iter()
            .filter_map(|record| match record {
                ExecutionRecord::TurnRecord { fact, .. } => match fact.as_ref() {
                    ExecutionRecord::HarnessInventory { inventory, .. } => Some(inventory),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(inventories.len(), 3);
        assert_eq!(inventories[0].binding_digest, inventories[1].binding_digest);
        assert_eq!(inventories[0].tools, inventories[2].tools);
        let first = inventories[0]
            .managed_skills
            .skills
            .first()
            .context("first catalog")?;
        let second = inventories[1]
            .managed_skills
            .skills
            .first()
            .context("second catalog")?;
        assert_eq!(first.installation_id, second.installation_id);
        assert_ne!(first.version, second.version);
        let requests = prompts.lock().await;
        assert_eq!(requests.len(), 3);
        let first_request = requests[0].system.as_deref().context("first system")?;
        assert!(first_request.contains("PROJECT_V1"));
        assert!(first_request.contains(&first.version));
        assert!(first_request.contains(&first.installation_id));
        let second_request = requests[1].system.as_deref().context("second system")?;
        assert!(second_request.contains("PROJECT_V2"));
        assert!(!second_request.contains("PROJECT_V1"));
        assert!(second_request.contains(&second.version));
        assert!(
            !requests[2]
                .system
                .as_deref()
                .context("third system")?
                .contains("Activated skill instructions")
        );
        drop(requests);
        assert!(inventories[2].managed_skills.skills.is_empty());
        assert!(
            inventories[2]
                .managed_skills
                .revisions
                .iter()
                .any(|revision| revision.scope == LibraryScope::Project
                    && revision.revision > second.catalog_revision)
        );
        service.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn managed_native_model_and_reopened_sqlite_keep_the_original_skill_receipt() -> TestResult
    {
        use bitrouter_ai::types::{Content, FinishReason, GenerateResult};
        use bitrouter_orchestrator::core::protocol::TaskInput;
        use bitrouter_orchestrator::core::session::RunStatus;
        use bitrouter_orchestrator::core::skills::SkillSource;
        use bitrouter_orchestrator::harness::managed::session::NativeSession;
        use bitrouter_sdk::language_model::{MockExecutor, MockResponse};

        let workspace = tempfile::tempdir()?;
        let home = tempfile::tempdir()?;
        let source_root = tempfile::tempdir()?;
        let source = write_source(source_root.path(), "MANAGED_ORIGINAL")?;
        let scope = Scope::resolve(false, Some(workspace.path())).await?;
        Store::load(scope.clone())?
            .install(source.to_str().context("source")?, None, None)
            .await?;
        let database_url = format!("sqlite://{}/native.db", home.path().display());
        let db = crate::db::connect(&database_url).await?;
        crate::db::run_migrations(&db).await?;
        let store = Arc::new(crate::managed_store::DatabaseNativeStore::new(
            db,
            "managed-skills".into(),
        )?);
        let prompts = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let binding = Library {
            home: Ok(home.path().into()),
        }
        .catalog(workspace.path(), &CallerContext::local())
        .await?
        .skills
        .into_iter()
        .next()
        .context("original binding")?;
        let first=GenerateResult {
            content:vec![Content::ToolCall{id:"read-bound-support".into(),name:"skill_read".into(),arguments:serde_json::json!({"skill_id":SkillSource::Managed(binding).identity()?,"reference":"references/check.md"}).to_string(),provider_executed:false,dynamic:false,provider_metadata:Default::default()}],
            finish_reason:Some(FinishReason::ToolCalls),usage:None,response_id:None,stop_details:None,provider_metadata:Default::default(),
        };
        let mut last = first.clone();
        last.content = vec![Content::Text {
            text: "done".into(),
            provider_metadata: Default::default(),
        }];
        last.finish_reason = Some(FinishReason::Stop);
        let executor = MockExecutor::new(vec![
            MockResponse::Generate(first),
            MockResponse::Generate(last),
        ]);
        let app = Arc::new(
            bitrouter_sdk::App::builder()
                .language_model(|builder| {
                    builder
                        .routing_table(Arc::new(fixture_routing()))
                        .executor(Arc::new(executor))
                        .observe_hook(CapturePrompts(prompts.clone()));
                })
                .decision_model(workflow_runtime())
                .build()?,
        );
        let mut native = NativeSession::open(
            app.clone(),
            CallerContext::local(),
            store.clone(),
            workspace.path(),
            ToolMode::ReadOnly,
            &config(home.path()),
        )
        .await?;
        let input: TaskInput = serde_json::from_value(serde_json::json!({
            "text":"review this work","model":"fixture-model","effort":null,"routing":{},
            "acceptance_criteria":[],"required_materials":[],"verification":null,"limits":null,
        }))?;
        let snapshot = native
            .run(
                input,
                Arc::new(RejectNativeEffects),
                CancellationToken::new(),
            )
            .await?;
        assert_eq!(
            snapshot.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        let selection = snapshot
            .root_turn()
            .and_then(|turn| snapshot.skill_selection(&turn.agent_turn_id))
            .context("selected skills")?;
        let selected = selection
            .candidates
            .iter()
            .find(|candidate| candidate.selected)
            .context("selected managed skill")?;
        let SkillSource::Managed(binding) = &selected.source else {
            return Err("wrong source kind".into());
        };
        let requests = prompts.lock().await;
        assert_eq!(requests.len(), 2);
        let system = requests[0]
            .system
            .as_deref()
            .context("actual model system")?;
        assert!(system.contains("MANAGED_ORIGINAL"));
        assert!(system.contains(&binding.version));
        assert!(system.contains(&binding.installation_id));
        assert!(system.contains("\"receipt\""));
        assert_eq!(
            selected
                .files
                .get("references/check.md")
                .map(String::as_str),
            Some("support MANAGED_ORIGINAL\n")
        );
        assert!(requests[1].messages.iter().any(|message| {
            message.role == bitrouter_ai::types::Role::User
                && serde_json::to_string(message)
                    .is_ok_and(|text| text.contains("support MANAGED_ORIGINAL"))
        }));
        drop(requests);
        native.close().await?;

        write_source(source_root.path(), "MANAGED_NEW_ALIAS")?;
        Store::load(scope.clone())?.update("review", None).await?;
        Store::load(scope)?.uninstall("review")?;
        let reopened_db = crate::db::connect(&database_url).await?;
        let reopened_store = Arc::new(crate::managed_store::DatabaseNativeStore::new(
            reopened_db,
            "managed-skills".into(),
        )?);
        let cold = NativeSession::inspect(reopened_store.as_ref())
            .await?
            .context("cold original journal")?;
        let retained = cold
            .snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .root_turn()
                    .and_then(|turn| snapshot.skill_selection(&turn.agent_turn_id))
            })
            .context("cold selection")?;
        assert_eq!(retained.candidates[0].source, selected.source);
        assert_eq!(retained.candidates[0].files, selected.files);
        assert!(
            retained.candidates[0]
                .content
                .as_deref()
                .is_some_and(|body| body.contains("MANAGED_ORIGINAL"))
        );
        assert!(cold.released);
        let restored = NativeSession::open(
            app,
            CallerContext::local(),
            reopened_store,
            workspace.path(),
            ToolMode::ReadOnly,
            &config(home.path()),
        )
        .await?;
        let recovered = restored.core().snapshot().await;
        assert_eq!(recovered.session_id, snapshot.session_id);
        assert_eq!(
            recovered
                .root_turn()
                .and_then(|turn| recovered.skill_selection(&turn.agent_turn_id))
                .map(|selection| &selection.candidates[0].source),
            Some(&selected.source)
        );
        assert_eq!(
            recovered
                .root_turn()
                .and_then(|turn| recovered.skill_selection(&turn.agent_turn_id))
                .context("restored files")?
                .candidates[0]
                .files,
            selected.files
        );
        assert_eq!(prompts.lock().await.len(), 2);
        restored.close().await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_matching_payload_outside_the_version_tree_cannot_replace_its_directory() -> TestResult
    {
        let workspace = tempfile::tempdir()?;
        let home = tempfile::tempdir()?;
        let source_root = tempfile::tempdir()?;
        let source = write_source(source_root.path(), "ORIGINAL")?;
        let scope = Scope::resolve(false, Some(workspace.path())).await?;
        Store::load(scope)?
            .install(source.to_str().context("source path")?, None, None)
            .await?;
        let library = Library {
            home: Ok(home.path().into()),
        };
        let catalog = library
            .catalog(workspace.path(), &CallerContext::local())
            .await?;
        let bound = catalog.skills.first().context("binding")?;
        let version = bound.library.join("versions").join(&bound.version);
        let moved = home.path().join("moved-version");
        std::fs::rename(&version, &moved)?;
        std::os::unix::fs::symlink(&moved, &version)?;
        assert!(
            library
                .read(
                    workspace.path(),
                    &CallerContext::local(),
                    bound,
                    "SKILL.md",
                    65536
                )
                .await
                .is_err()
        );
        Ok(())
    }
}
