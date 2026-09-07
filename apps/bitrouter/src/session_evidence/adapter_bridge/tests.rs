use super::*;

#[tokio::test]
async fn launch_uses_the_exact_package_and_preserves_adapter_arguments() -> Result<()> {
    let home = tempfile::tempdir()?;
    for (harness, id) in [
        (Harness::Codex, "codex-acp"),
        (Harness::ClaudeCode, "claude-acp"),
    ] {
        let pin = pin(harness)?;
        let catalog = crate::harness::by_id(id).context("maintained harness missing")?;
        assert_eq!(
            catalog.maintained_adapter_identity(),
            Some((pin.package.as_str(), pin.version.as_str()))
        );
        let original = vec![
            "-y".into(),
            format!("{}@{}", pin.package, pin.version),
            "--version".into(),
        ];
        let prepared = prepare_launch(home.path(), harness, "npx", &original)
            .await?
            .context("missing bridge launch")?;
        assert_eq!(
            prepared[1],
            format!("--package={}@{}", pin.package, pin.version)
        );
        assert_eq!(prepared[2], "node");
        assert!(Path::new(&prepared[3]).is_file());
        assert_eq!(&prepared[4..], [harness_name(harness), "--", "--version"]);
        assert!(
            prepare_launch(home.path(), harness, "bash", &original)
                .await?
                .is_none()
        );
        let different = vec!["-y".into(), format!("{}@unknown", pin.package)];
        assert!(
            prepare_launch(home.path(), harness, "npx", &different)
                .await?
                .is_none()
        );
    }
    Ok(())
}

#[tokio::test]
async fn runtime_isolated_prompts_and_cancellation_keep_their_original_identity() -> Result<()> {
    let test = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/session_evidence/adapter_bridge/runtime.test.mjs");
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new("node")
            .arg("--test")
            .arg(test)
            .output(),
    )
    .await??;
    ensure!(
        output.status.success(),
        "bridge runtime checks failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
