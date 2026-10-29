//! A custom asynchronous directory must not resume file I/O after revocation.
use super::*;
use crate::skills::library::SkillDirectory;
use bitrouter_ai::harness::skills::{LocalScope, LocalSkill, LocalSkillCatalog};
use std::path::PathBuf;

struct DelayedDirectory {
    root: PathBuf,
    version: String,
    receipt: Value,
    entered: AtomicUsize,
    reads: AtomicUsize,
    release: CancellationToken,
}

impl DelayedDirectory {
    fn new(workspace: &Path) -> Result<Self> {
        let root = workspace.join("library/runtime-skill");
        std::fs::create_dir_all(&root)?;
        let mut files = Vec::new();
        for (path, text) in [("SKILL.md", INSTRUCTIONS), ("guide.txt", GUIDE)] {
            std::fs::write(root.join(path), text)?;
            files.push(json!({"path":path,"bytes":text.len(),"digest":sha256(text.as_bytes())}));
        }
        // This custom directory owns its manifest fingerprint; reads below
        // verify the exact caller, receipt and actual files it published.
        let version = sha256(&serde_json::to_vec(&files)?);
        Ok(Self {
            root: root.canonicalize()?,
            receipt: json!({"bundle":{"digest":version,"files":files}}),
            version,
            entered: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
            release: CancellationToken::new(),
        })
    }
}

#[async_trait]
impl SkillDirectory for DelayedDirectory {
    async fn catalog(
        &self,
        workspace: &Path,
        caller: &CallerContext,
    ) -> std::result::Result<LocalSkillCatalog, String> {
        if caller.is_anonymous() || !self.root.starts_with(workspace) {
            return Err("directory caller or workspace is unavailable".into());
        }
        Ok(LocalSkillCatalog {
            skills: vec![LocalSkill {
                scope: LocalScope::Project,
                directory: self.root.clone(),
                workspace: workspace.into(),
                api_key_id: caller.api_key_id().into(),
                user_id: caller.user_id().into(),
                local: caller.is_local(),
                directory_id: "skill_async_runtime".into(),
                name: "runtime-skill".into(),
                description: "Runtime authority fixture".into(),
                version: self.version.clone(),
                receipt: self.receipt.clone(),
                instruction_sha256: sha256(INSTRUCTIONS.as_bytes()),
                instruction_bytes: INSTRUCTIONS.len() as u64,
            }],
            problems: Vec::new(),
        })
    }

    async fn read(
        &self,
        workspace: &Path,
        caller: &CallerContext,
        binding: &LocalSkill,
        relative_path: &str,
        max_bytes: u64,
    ) -> std::result::Result<Vec<u8>, String> {
        if binding.workspace != workspace
            || binding.directory != self.root
            || binding.api_key_id != caller.api_key_id()
            || binding.user_id != caller.user_id()
            || binding.local != caller.is_local()
            || binding.version != self.version
            || binding.receipt != self.receipt
        {
            return Err("bound directory identity changed".into());
        }
        let expected = match relative_path {
            "SKILL.md" => INSTRUCTIONS,
            "guide.txt" => GUIDE,
            _ => return Err("file is outside the original directory manifest".into()),
        };
        if expected.len() as u64 > max_bytes {
            return Err("bound file exceeds the read limit".into());
        }
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.release.cancelled().await;
        self.reads.fetch_add(1, Ordering::SeqCst);
        let bytes = tokio::fs::read(self.root.join(relative_path))
            .await
            .map_err(|error| error.to_string())?;
        if bytes.len() != expected.len() || sha256(&bytes) != sha256(expected.as_bytes()) {
            return Err("original directory file changed".into());
        }
        Ok(bytes)
    }
}

#[tokio::test]
async fn runtime_local_skills_recheck_before_resuming_custom_directory_io() -> Result<()> {
    for change in ["allowed", "revoke", "scope", "legacy"] {
        let provider = Provider::start(false).await?;
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let library = Arc::new(DelayedDirectory::new(&workspace)?);
        let (fixture, route) = file_fixture(&provider, directory.path()).await?;
        let checked = tokio::time::timeout(Duration::from_secs(20), async {
            let (headers, key) = ready(&fixture, &route).await?;
            let legacy = change == "legacy";
            let config = HarnessConfig {
                library: Some(library.clone()),
                ..HarnessConfig::default()
            };
            let session =
                open_session(&fixture, &provider, headers, &workspace, &config, legacy).await?;
            let port = session.harness_port_for_test();
            let catalog = port
                .skill_catalog()
                .await?
                .context("native skill catalog")?;
            ensure!(catalog.local.problems.is_empty() && catalog.local.skills.len() == 1);
            let source = SkillSource::Local(
                catalog
                    .local
                    .skills
                    .into_iter()
                    .next()
                    .context("bound local skill")?,
            );
            let cancel = CancellationToken::new();
            let mut instructions = Box::pin(port.skill_instructions(&source, 4096, None, &cancel));
            let mut file = Box::pin(port.skill_file(&source, "guide.txt", 4096, None, &cancel));
            ensure!(futures::poll!(instructions.as_mut()).is_pending());
            ensure!(futures::poll!(file.as_mut()).is_pending());
            ensure!(library.entered.load(Ordering::SeqCst) == 2);
            ensure!(library.reads.load(Ordering::SeqCst) == 0);
            if change != "allowed" {
                change_authority(
                    &fixture,
                    &key,
                    &route,
                    if legacy { "revoke" } else { change },
                )
                .await?;
            }
            library.release.cancel();
            let (instructions, file) = tokio::join!(instructions, file);
            if matches!(change, "revoke" | "scope") {
                unauthorized(instructions)?;
                unauthorized(file)?;
                ensure!(
                    library.reads.load(Ordering::SeqCst) == 0,
                    "{change}: revoked local reader resumed I/O"
                );
            } else {
                ensure!(instructions? == INSTRUCTIONS.as_bytes());
                ensure!(file? == GUIDE.as_bytes());
                ensure!(library.reads.load(Ordering::SeqCst) == 2);
                std::fs::remove_file(library.root.join("guide.txt"))?;
                let error = port
                    .skill_file(&source, "guide.txt", 4096, None, &cancel)
                    .await
                    .err()
                    .context("missing local file was accepted")?;
                ensure!(error.code == bitrouter_ai::core::protocol::ErrorCode::ArtifactUnavailable);
                ensure!(library.reads.load(Ordering::SeqCst) == 3);
            }
            drop(port);
            close_session(&fixture, session).await?;
            ensure!(provider.calls() == 0);
            Ok(())
        })
        .await
        .context("runtime local skill authority deadline")
        .and_then(|outcome| outcome);
        library.release.cancel();
        finish(&fixture, provider, checked)
            .await
            .with_context(|| format!("runtime local case: {change}"))?;
    }
    Ok(())
}
