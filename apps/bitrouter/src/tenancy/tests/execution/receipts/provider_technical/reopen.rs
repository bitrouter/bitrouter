//! Two actual libtest/Core-Service processes, following the existing receipt
//! latency/unknown-child fixture pattern. This is not a daemon-build claim.
use super::*;
use crate::tenancy::{Service, resources::Resources};
use std::{
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    process::Stdio,
};
const ROLE: &str = "BITROUTER_TECHNICAL_REOPEN_ROLE";
const ROOT: &str = "BITROUTER_TECHNICAL_REOPEN_ROOT";
const CASE: &str = "tenancy::tests::execution::receipts::provider_technical::reopen::actual_two_process_stop_reopen_and_original_archive_preserve_exact_key";
fn private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
async fn child(root: &std::path::Path, role: &str) -> Result<()> {
    ensure!(
        root.canonicalize()? == root
            && std::fs::metadata(root)?.permissions().mode() & 0o777 == 0o700,
        "private canonical process root"
    );
    let path = root.join("authority.sqlite");
    if role == "create" {
        let harness = Harness::with_observed_storage(
            "chat_completions",
            StatusCode::OK,
            serde_json::to_string(&json_response(
                "chat_completions",
                full_usage("chat_completions"),
            )?)?,
            false,
            None,
            false,
            (
                crate::telemetry::receipt_latency::Sink::default(),
                Some(&path),
            ),
        )
        .await?;
        let checked:Result<()>=async {
            let client=Harness::client(false)?;
            let response=harness.send(&client).send().await?;
            ensure!(response.status()==StatusCode::OK);let id=execution_id(&response)?;response.bytes().await?;
            let facts=measured(&harness,&client,&id).await?.context("actual retained process facts")?;
            let scoped=measured_v2(&harness,&client,&id).await?.context("actual retained process scope")?;
            ensure!(scoped.pipeline_scope.is_some());
            let original=store::lookup_terminal(&harness.fixture.db,&harness.fixture.installation,&harness.fixture.tenant,&id).await?.context("actual original terminal")?;
            private(&root.join("original.private.json"),&serde_json::to_vec(&json!({
                "execution":id,"token":harness.credential.secret.as_deref().context("issued credential")?,
                "public":original.json(),"private":original.private_json(),"digest":original.digest(),"facts":serde_json::to_string(&facts)?,"scope":serde_json::to_string(&scoped)?
            }))?)?;
            ensure!(harness.calls.load(Ordering::SeqCst)==1);Ok(())
        }.await;
        let stopped = harness.fixture.service.shutdown().await;
        harness._gateway.abort();
        let gateway = harness._gateway.await;
        harness._provider.abort();
        let provider = harness._provider.await;
        let closed = harness.fixture.db.close().await;
        checked?;
        stopped?;
        closed?;
        ensure!(gateway.is_ok() || gateway.is_err_and(|error| error.is_cancelled()));
        ensure!(provider.is_ok() || provider.is_err_and(|error| error.is_cancelled()));
    } else {
        ensure!(role == "reopen", "unknown process role");
        let original_path = root.join("original.private.json");
        let metadata = std::fs::metadata(&original_path)?;
        ensure!(
            metadata.is_file()
                && metadata.len() <= 256 * 1024
                && metadata.permissions().mode() & 0o777 == 0o600
        );
        let original: Value = serde_json::from_slice(&std::fs::read(&original_path)?)?;
        let id: ExecutionId = original["execution"]
            .as_str()
            .context("original execution")?
            .try_into()?;
        let owner = crate::tenancy::ownership::acquire_path(&path, false)?;
        let db =
            crate::db::connect_tenancy(&format!("sqlite://{}?mode=rw", path.display()), &owner)
                .await?;
        let initial = crate::tenancy::store::load(&db, chrono::Utc::now()).await?;
        let installation = initial.installation.clone();
        let service = Service::start_prepared(
            db.clone(),
            Some(owner),
            initial,
            Arc::new(Resources::with_key(installation.clone(), [29; 32])?),
        )
        .await?;
        let checked: Result<()> = async {
            let receipt: ExecutionReceipt =
                serde_json::from_str(original["public"].as_str().context("original receipt")?)?;
            let caller = service
                .authenticate(original["token"].as_str().context("issued credential")?)
                .await
                .map_err(failure)?;
            let read = || {
                service.read_receipt(
                    caller.clone(),
                    receipt.tenant_id.clone(),
                    read::Request::ProviderTechnical(id.clone()),
                )
            };
            let bytes = read().await.map_err(failure)?;
            let facts: ProviderTechnicalFactsV1 = serde_json::from_slice(&bytes)?;
            facts.validate_against(&receipt)?;
            ensure!(
                serde_json::to_string(&facts)?
                    == original["facts"].as_str().context("retained facts")?
            );
            let scoped_bytes = service
                .read_receipt(
                    caller.clone(),
                    receipt.tenant_id.clone(),
                    read::Request::ProviderTechnicalV2(id.clone()),
                )
                .await
                .map_err(failure)?;
            let scoped: Option<
                bitrouter_ai::tenancy::provider_technical_facts_v2::ProviderTechnicalFactsV2,
            > = serde_json::from_slice(&scoped_bytes)?;
            let scoped = scoped.context("reopened scope")?;
            scoped.validate_against(&receipt)?;
            ensure!(
                serde_json::to_string(&scoped)?
                    == original["scope"].as_str().context("retained scope")?
            );
            ensure!(scoped.operation_view() == Some(facts));
            ensure!(
                service.archive_history_for_test().await.map_err(failure)?,
                "actual original archive did not run"
            );
            ensure!(
                read().await.map_err(failure)? == bytes,
                "archive changed optional object"
            );
            ensure!(
                service
                    .read_receipt(
                        caller.clone(),
                        receipt.tenant_id.clone(),
                        read::Request::ProviderTechnicalV2(id.clone())
                    )
                    .await
                    .map_err(failure)?
                    == scoped_bytes,
                "archive changed v2 scope bytes"
            );
            let loaded = store::lookup_terminal(&db, &installation, &receipt.tenant_id, &id)
                .await?
                .context("archived original")?;
            ensure!(
                loaded.json() == original["public"].as_str().context("public")?
                    && loaded.private_json() == original["private"].as_str().context("private")?
                    && loaded.digest() == original["digest"].as_str().context("digest")?
            );
            let actual =
                crate::tenancy::receipts::provider_technical::storage::retained_bytes(&db).await?;
            let (logical, physical, fenced) = service
                .admission
                .optional_accounting_for_test()
                .map_err(|e| anyhow::anyhow!("accounting unavailable: {e:?}"))?;
            ensure!(logical == actual && physical == 0 && !fenced);
            Ok(())
        }
        .await;
        let stopped = service.shutdown().await;
        let closed = db.close().await;
        checked?;
        stopped?;
        closed?;
    }
    private(
        &root.join(format!("{role}.closed.json")),
        &serde_json::to_vec(&json!({"pid":std::process::id(),"closed":true,"role":role}))?,
    )?;
    Ok(())
}
#[tokio::test]
async fn actual_two_process_stop_reopen_and_original_archive_preserve_exact_key() -> Result<()> {
    if let Some(role) = std::env::var_os(ROLE) {
        let root = std::env::var_os(ROOT).context("private child root")?;
        return child(
            std::path::Path::new(&root),
            role.to_str().context("role encoding")?,
        )
        .await;
    }
    let home = tempfile::tempdir()?;
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700))?;
    let root = home.path().canonicalize()?;
    for role in ["create", "reopen"] {
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .args(["--exact", CASE, "--test-threads=1"])
            .env_clear()
            .env(ROLE, role)
            .env(ROOT, &root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id().context("actual child PID")?;
        let status = match tokio::time::timeout(TIMEOUT, child.wait()).await {
            Ok(status) => status?,
            Err(error) => {
                child.start_kill()?;
                let _joined = child.wait().await?;
                return Err(error.into());
            }
        };
        ensure!(
            status.success(),
            "Core Service process role {role} failed: {status}"
        );
        let marker: Value =
            serde_json::from_slice(&std::fs::read(root.join(format!("{role}.closed.json")))?)?;
        ensure!(
            marker["pid"] == pid && marker["role"] == role && marker["closed"] == true,
            "zero-match or wrong process completion"
        );
    }
    Ok(())
}
