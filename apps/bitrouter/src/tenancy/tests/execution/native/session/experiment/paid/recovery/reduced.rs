//! Actual closed native-v4 source consumed by separately built feature profiles.
use super::*;
use crate::tenancy::tests::learning_startup::Transfer;
use std::os::unix::fs::DirBuilderExt;

fn directory(path: &Path) -> Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(())
}

async fn clean(root: &Path, subject: &str, judge: &str) -> Result<()> {
    directory(root)?;
    let (fixture, headers) = ready(root, subject, judge).await?;
    let mut session = None;
    let checked = async {
        let revision = headers
            .get("Bitrouter-Execution-Revision")
            .context("actual clean revision")?
            .to_str()?
            .try_into()?;
        let transfer = Transfer {
            installation: fixture.installation.clone(),
            tenant: fixture.tenant.clone(),
            revision,
        };
        session = Some(open(&fixture, &headers, subject, root, "reduced-native-clean").await?);
        session
            .as_mut()
            .context("clean native owner")?
            .run(
                human::observed_task(),
                Arc::new(Approval),
                CancellationToken::new(),
            )
            .await?;
        ensure!(records(&fixture, "owned_judge_budget").await?.is_empty());
        std::fs::write(
            root.join("native-paid.json"),
            serde_json::to_vec(&transfer)?,
        )?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let closed = match session {
        Some(session) => tokio::time::timeout(TIMEOUT, session.close())
            .await
            .map_err(anyhow::Error::from)
            .and_then(|value| value.map_err(Into::into)),
        None => Ok(()),
    };
    let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
    let database = fixture.db.clone().close().await;
    drop(fixture);
    checked
        .and(closed)
        .and(stopped.map_err(anyhow::Error::from).and_then(|value| value))
        .and(database.map_err(Into::into))?;
    drop(crate::tenancy::ownership::acquire_path(
        &root.join("native-paid.db"),
        false,
    )?);
    Ok(())
}

async fn reduced(binary: &Path, root: &Path, profile: &str) -> Result<()> {
    let mut child = tokio::process::Command::new(binary)
        .args([
            "--exact",
            "tenancy::tests::learning_startup::reduced_native_consumer",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(crate::tenancy::tests::learning_startup::ROOT, root)
        .env("BITROUTER_PAID_STARTUP_PROFILE", profile)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child.stdout.take().context("reduced stdout")?.take(8193);
    let mut stderr = child.stderr.take().context("reduced stderr")?.take(8193);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let completed = tokio::time::timeout(Duration::from_secs(45), async {
        let (_, _, status) = tokio::try_join!(
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
            child.wait(),
        )?;
        ensure!(
            out.len() <= 8192 && err.len() <= 8192,
            "reduced diagnostics bound"
        );
        ensure!(
            status.success(),
            "reduced native consumer failed: {}",
            String::from_utf8_lossy(&err)
        );
        Ok::<_, anyhow::Error>(())
    })
    .await;
    if !matches!(completed, Ok(Ok(()))) {
        let _ = child.start_kill();
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .context("reduced consumer kill/reap failed")??;
    }
    completed.context("reduced native consumer watchdog")??;
    ensure!(
        std::fs::read(root.join("native-consumer-ack"))?
            == format!("native-v4-refused:{profile}").as_bytes(),
        "zero matches is not consumer success"
    );
    Ok(())
}

async fn campaign(profile: &str, variable: &str) -> Result<()> {
    let binary =
        PathBuf::from(std::env::var_os(variable).with_context(|| format!("missing {variable}"))?);
    ensure!(
        binary.is_absolute() && binary.is_file(),
        "explicit reduced executable required"
    );
    tokio::time::timeout(
        Duration::from_secs(150),
        Box::pin(async {
            let subject = upstream().await?;
            let mut judge = Judge::start().await?;
            let root = tempfile::tempdir()?;
            let url = format!("{}/v1", subject.uri());
            let checked = async {
                clean(&root.path().join("clean"), &url, &judge.url).await?;
                let pending = root.path().join("pending");
                directory(&pending)?;
                let mut producer =
                    Child::start("producer", &pending, "observed", Some((&url, &judge.url)))?;
                let witnessed = async {
                    producer.line("native-paid-source-ready").await?;
                    let answer = std::fs::read(pending.join("answer.json"))?;
                    ensure!(answer.len() <= 64 * 1024);
                    judge.retained_answer(serde_json::from_slice(&answer)?)?;
                    producer.advance().await?;
                    producer.line("native-paid-accepted").await?;
                    ensure!(judge.received().await?["model"] == "judge-model");
                    judge.release.cancel();
                    producer.line("native-paid-stopped").await
                }
                .await;
                judge.release.cancel();
                producer.finish(true, witnessed).await?;
                // The original source is no longer alive before any reduced owner
                // opens its physical SQLite path. No copied DB or positive row seed.
                let lease = crate::tenancy::ownership::acquire_path(
                    &pending.join("native-paid.db"),
                    false,
                )?;
                let bytes = std::fs::read(pending.join("accepted.json"))?;
                ensure!(bytes.len() <= 1024 * 1024);
                let actual: Value = serde_json::from_slice(&bytes)?;
                let seed = &actual["recovery"]["seed"];
                let transfer = Transfer {
                    installation: seed["installation"]
                        .as_str()
                        .context("actual installation")?
                        .try_into()?,
                    tenant: seed["tenant"]
                        .as_str()
                        .context("actual tenant")?
                        .try_into()?,
                    revision: seed["configuration"]
                        .as_str()
                        .context("actual revision")?
                        .try_into()?,
                };
                std::fs::write(
                    pending.join("native-paid.json"),
                    serde_json::to_vec(&transfer)?,
                )?;
                drop(lease);
                reduced(&binary, root.path(), profile).await?;
                ensure!(
                    !judge.extra_request()
                        && subject
                            .received_requests()
                            .await
                            .context("actual subject requests")?
                            .len()
                            == 2,
                    "reduced bootstrap resent native subject or paid judge work"
                );
                Ok::<_, anyhow::Error>(())
            }
            .await;
            checked.and(judge.finish().await)
        }),
    )
    .await?
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_PAID_GATEWAY_LIBTEST"]
async fn gateway_refuses_stopped_native_paid_v4_with_clean_control() -> Result<()> {
    campaign("gateway", "BITROUTER_PAID_GATEWAY_LIBTEST").await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_PAID_CORE_LIBTEST"]
async fn core_only_refuses_stopped_native_paid_v4_with_clean_control() -> Result<()> {
    campaign("core-only", "BITROUTER_PAID_CORE_LIBTEST").await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_PAID_HARNESS_LIBTEST"]
async fn harness_only_refuses_stopped_native_paid_v4_with_clean_control() -> Result<()> {
    campaign("harness-only", "BITROUTER_PAID_HARNESS_LIBTEST").await
}
