//! Real archive CLI process and socket. The DB fixture uses actual current migrations.
#![cfg(unix)]
use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
};
const WAIT: Duration = Duration::from_secs(20);
const TOKEN: &str = "audit-cli-secret-at-least-thirty-two-characters";
const INSTALLATION: &str = "archive-cli-installation";
fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
    command
        .current_dir(home)
        .env("BITROUTER_HOME", home)
        .env("AUDIT_TEST_TOKEN", TOKEN)
        .env("RUST_LOG", "error")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .kill_on_drop(true);
    command
}
fn files(path: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut result = BTreeMap::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        result.insert(
            entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("filename"))?,
            std::fs::read(entry.path())?,
        );
    }
    Ok(result)
}
async fn fixture(home: &Path) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let source = home.join("retired.sqlite");
    std::fs::File::create(&source)?;
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600))?;
    let db = bitrouter::db::connect(&format!("sqlite://{}", source.display())).await?;
    bitrouter::db::run_migrations(&db).await?;
    db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO tenancy_installation(singleton,installation_id,schema_version,authority_revision,operation_ids,safety_operation_ids,created_at,saturated) VALUES(1,?,1,0,0,0,'2026-10-26T00:00:00Z',1)",
        [INSTALLATION.into()])).await?;
    db.close().await?;
    let output = tokio::time::timeout(
        WAIT,
        command(home)
            .args([
                "tenancy",
                "offline",
                "snapshot",
                "--database",
                "retired.sqlite",
                "--bundle",
                "bundle",
            ])
            .output(),
    )
    .await??;
    ensure!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout)?;
    std::fs::write(home.join("audit-control.json"), br#"{"credentials":[{"id":"audit","token_env":"AUDIT_TEST_TOKEN","scopes":["control:tenancy"]}]}"#)?;
    std::fs::set_permissions(
        home.join("audit-control.json"),
        std::fs::Permissions::from_mode(0o600),
    )?;
    // These ambient files must not become full host/bootstrap inputs.
    std::fs::write(
        home.join("bitrouter.yaml"),
        "invalid: [do-not-load-ambient-host",
    )?;
    Ok(report["evidence_digest"].as_str().context("anchor")?.into())
}

#[tokio::test]
async fn cli_audit_real_listener_old_installation_and_graceful_close_preserve_bundle() -> Result<()>
{
    let home = tempfile::tempdir()?;
    let digest = fixture(home.path()).await?;
    let bundle = home.path().join("bundle");
    let before = files(&bundle)?;
    let source = std::fs::read(home.path().join("retired.sqlite"))?;
    let mut child = command(home.path())
        .args([
            "tenancy",
            "offline",
            "audit",
            "--archive",
            "bundle",
            "--projection",
            "projection",
            "--evidence-digest",
            &digest,
            "--control-config",
            "audit-control.json",
            "--listen",
            "127.0.0.1:0",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = BufReader::new(child.stdout.take().context("stdout")?.take(16_385));
    let mut stderr = child.stderr.take().context("stderr")?.take(32_769);
    let diagnostics = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await?;
        Ok::<_, std::io::Error>(bytes)
    });
    let checked = async {
        let ready: Value = tokio::time::timeout(WAIT, async {
            let mut bytes = Vec::new();
            loop {
                let count = stdout.read_until(b'\n', &mut bytes).await?;
                ensure!(
                    count != 0 && bytes.len() <= 16_384,
                    "audit exited before bounded readiness"
                );
                match serde_json::from_slice(&bytes) {
                    Ok(value) => break Ok::<Value, anyhow::Error>(value),
                    Err(error) if error.is_eof() => {}
                    Err(error) => return Err(error.into()),
                }
            }
        })
        .await??;
        ensure!(ready["action"] == "audit" && ready["installation_id"] == INSTALLATION);
        ensure!(ready["complete"] == false && ready["cutover_ready"] == false);
        let endpoint = format!("http://{}", ready["listen"].as_str().context("listen")?);
        let sdk = bitrouter_sdk::tenancy::TenancyClient::new(&endpoint, TOKEN)?;
        let page = sdk
            .executions(
                &INSTALLATION.try_into()?,
                &"tenant".try_into()?,
                &"correlation".try_into()?,
                None,
                10,
            )
            .await?;
        ensure!(page.execution_ids.is_empty() && page.installation_id.as_str() == INSTALLATION);
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(WAIT)
            .build()?;
        let uri =
            format!("{endpoint}/control/v1/tenants/tenant/executions?correlation_id=correlation");
        let response = client
            .get(&uri)
            .bearer_auth(TOKEN)
            .header("bitrouter-installation-id", INSTALLATION)
            .send()
            .await?;
        ensure!(
            response.status() == reqwest::StatusCode::OK
                && response
                    .headers()
                    .get("cache-control")
                    .is_some_and(|v| v == "no-store")
        );
        ensure!(
            sdk.executions(
                &"fresh-installation".try_into()?,
                &"tenant".try_into()?,
                &"correlation".try_into()?,
                None,
                10
            )
            .await
            .is_err()
        );
        let response = client
            .post(format!("{endpoint}/control/v1/tenants/tenant"))
            .bearer_auth(TOKEN)
            .header("bitrouter-installation-id", INSTALLATION)
            .send()
            .await?;
        ensure!(response.status() == reqwest::StatusCode::NOT_FOUND);
        ensure!(files(&bundle)? == before);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let pid = child.id().context("owned audit process ID")?;
    let signal = tokio::time::timeout(
        WAIT,
        Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status(),
    )
    .await;
    let ended = tokio::time::timeout(WAIT, child.wait()).await;
    let status = match ended {
        Ok(result) => result?,
        Err(error) => {
            child.kill().await?;
            tokio::time::timeout(WAIT, child.wait()).await??;
            return Err(error.into());
        }
    };
    let diagnostic = tokio::time::timeout(WAIT, diagnostics).await???;
    ensure!(diagnostic.len() <= 32_768 && !String::from_utf8_lossy(&diagnostic).contains(TOKEN));
    checked?;
    ensure!(signal??.success() && status.success());
    drop(stdout); // Keep the read end through real process exit.
    ensure!(
        files(&bundle)? == before && std::fs::read(home.path().join("retired.sqlite"))? == source
    );
    Ok(())
}
#[tokio::test]
async fn cli_audit_refuses_ambient_context_nonloopback_and_missing_control_scope() -> Result<()> {
    let home = tempfile::tempdir()?;
    let digest = fixture(home.path()).await?;
    let before = files(&home.path().join("bundle"))?;
    for (prefix, listen) in [
        (vec!["--context", "remote"], "127.0.0.1:0"),
        (Vec::new(), "0.0.0.0:0"),
    ] {
        let mut command = command(home.path());
        command.args(prefix).args([
            "tenancy",
            "offline",
            "audit",
            "--archive",
            "bundle",
            "--projection",
            "rejected",
            "--evidence-digest",
            &digest,
            "--control-config",
            "audit-control.json",
            "--listen",
            listen,
        ]);
        let output = tokio::time::timeout(WAIT, command.output()).await??;
        ensure!(!output.status.success() && !home.path().join("rejected").exists());
    }
    std::fs::write(home.path().join("audit-control.json"), br#"{"credentials":[{"id":"reader","token_env":"AUDIT_TEST_TOKEN","scopes":["control:read"]}]}"#)?;
    let output = tokio::time::timeout(
        WAIT,
        command(home.path())
            .args([
                "tenancy",
                "offline",
                "audit",
                "--archive",
                "bundle",
                "--projection",
                "rejected",
                "--evidence-digest",
                &digest,
                "--control-config",
                "audit-control.json",
                "--listen",
                "127.0.0.1:0",
            ])
            .output(),
    )
    .await??;
    ensure!(!output.status.success() && !home.path().join("rejected").exists());
    ensure!(
        !String::from_utf8_lossy(&output.stdout).contains(TOKEN)
            && !String::from_utf8_lossy(&output.stderr).contains(TOKEN)
    );
    ensure!(files(&home.path().join("bundle"))? == before);
    Ok(())
}
