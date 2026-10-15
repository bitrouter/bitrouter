//! Bounded verifier source from the immutable dispatch baseline, not live files.
use super::*;
use bitrouter_orchestrator::goal::verification::{CheckRecipe, CheckSpec};

pub(super) async fn collect(
    root: &Path,
    workspace: &Path,
    base: &str,
    checks: &[CheckSpec],
) -> Result<serde_json::Value> {
    anyhow::ensure!(
        (base.len() == 40 || base.len() == 64) && base.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid verifier baseline"
    );
    let tree = git(
        workspace,
        &[
            "ls-tree".as_ref(),
            "-r".as_ref(),
            "-l".as_ref(),
            "-z".as_ref(),
            base.as_ref(),
        ],
    )
    .await?
    .stdout;
    anyhow::ensure!(
        tree.len() <= 4 * 1024 * 1024,
        "verifier tree needs bounded context preparation"
    );
    let node = checks.iter().any(|check| {
        matches!(
            check.recipe,
            CheckRecipe::NpmTest | CheckRecipe::PnpmTest | CheckRecipe::BunTest
        )
    });
    let cargo = checks.iter().any(|check| {
        matches!(
            check.recipe,
            CheckRecipe::CargoTest | CheckRecipe::CargoClippy | CheckRecipe::CargoFmt
        )
    });
    let mut files = Vec::new();
    let mut omitted = 0usize;
    let mut total = 0usize;
    for entry in tree
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let entry = std::str::from_utf8(entry)?;
        let (metadata, path) = entry.split_once('\t').context("invalid baseline tree")?;
        let parts = metadata.split_whitespace().collect::<Vec<_>>();
        let selected = (node
            && (path.ends_with("package.json")
                || path.starts_with("tests/")
                || path.starts_with("test/")
                || path.contains(".test.")
                || path.contains(".spec.")))
            || (cargo && (path.ends_with("Cargo.toml") || path.starts_with("tests/")))
            || checks.iter().any(|check| match &check.recipe {
                CheckRecipe::FileExists { path: target }
                | CheckRecipe::FileContains { path: target, .. } => path == target,
                _ => false,
            });
        if !selected {
            continue;
        }
        let size = parts.get(3).and_then(|part| part.parse::<usize>().ok());
        if files.len() >= 64
            || parts.first().copied() != Some("100644") && parts.first().copied() != Some("100755")
            || size.is_none_or(|size| size > 48 * 1024 || total.saturating_add(size) > 128 * 1024)
        {
            omitted = omitted.saturating_add(1);
            continue;
        }
        let content = git(
            workspace,
            &["show".as_ref(), format!("{base}:{path}").as_ref()],
        )
        .await?
        .stdout;
        anyhow::ensure!(
            Some(content.len()) == size,
            "baseline verifier size changed"
        );
        let Ok(text) = std::str::from_utf8(&content) else {
            omitted = omitted.saturating_add(1);
            continue;
        };
        total = total.saturating_add(content.len());
        let reference = crate::goal_artifacts::put(root, &content).await?;
        files.push(serde_json::json!({"path":path,"artifact":reference,"content":text}));
    }
    Ok(
        serde_json::json!({"base_commit":base,"files":files,"omitted_selected_files":omitted,
        "scope":"Original manifests and selected verifier inputs from the frozen worker baseline. These bytes do not prove a check ran. Other source/configuration is unreported; omitted or absent inputs cannot establish correctness."}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn baseline_verifier_bytes_survive_worker_test_changes_and_mark_omissions() -> Result<()>
    {
        let temporary = tempfile::tempdir()?;
        let workspace = temporary.path().join("work");
        tokio::fs::create_dir_all(workspace.join("tests")).await?;
        git(&workspace, &["init".as_ref(), "-q".as_ref()]).await?;
        tokio::fs::write(
            workspace.join("package.json"),
            "{\"scripts\":{\"test\":\"node --test\"}}\n",
        )
        .await?;
        tokio::fs::write(workspace.join("tests/original.mjs"), "original assertion\n").await?;
        tokio::fs::write(workspace.join("tests/large.mjs"), vec![b'x'; 49 * 1024]).await?;
        git(&workspace, &["add".as_ref(), "-A".as_ref()]).await?;
        git(
            &workspace,
            &[
                "-c".as_ref(),
                "user.name=Fixture".as_ref(),
                "-c".as_ref(),
                "user.email=fixture@localhost".as_ref(),
                "-c".as_ref(),
                "commit.gpgsign=false".as_ref(),
                "-c".as_ref(),
                "core.hooksPath=/dev/null".as_ref(),
                "commit".as_ref(),
                "-qm".as_ref(),
                "fixture".as_ref(),
            ],
        )
        .await?;
        let base = String::from_utf8(
            git(&workspace, &["rev-parse".as_ref(), "HEAD".as_ref()])
                .await?
                .stdout,
        )?;
        tokio::fs::write(workspace.join("tests/original.mjs"), "weakened assertion\n").await?;
        let checks = vec![serde_json::from_value(
            serde_json::json!({"id":"npm","description":"behavior","recipe":{"kind":"npm_test"}}),
        )?];
        let root = temporary.path().join("artifacts");
        let value = collect(&root, &workspace, base.trim(), &checks).await?;
        assert_eq!(value["omitted_selected_files"], 1);
        let original = value["files"]
            .as_array()
            .context("inputs missing")?
            .iter()
            .find(|file| file["path"] == "tests/original.mjs")
            .context("original test missing")?;
        assert_eq!(original["content"], "original assertion\n");
        let reference: ArtifactRef = serde_json::from_value(original["artifact"].clone())?;
        assert_eq!(
            crate::goal_artifacts::read(&root, &reference).await?,
            b"original assertion\n"
        );
        Ok(())
    }
}
