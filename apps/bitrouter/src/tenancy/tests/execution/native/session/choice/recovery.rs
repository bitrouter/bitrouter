//! Fresh real file acquisition and authenticated facade after actual producer death.
use super::*;
use crate::tenancy::transaction::{Phase, receipt_observation::Key};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
mod cases;
mod companion;
mod faults;
mod marker;

const KEY: &str = "BITROUTER_TEST_STOPPED_NATIVE_KEY";
const ROOT: &str = "BITROUTER_TEST_STOPPED_NATIVE_ROOT";
const MODE: &str = "BITROUTER_TEST_STOPPED_NATIVE_MODE";
const DIAGNOSTICS: &str = "BITROUTER_TEST_STOPPED_NATIVE_DIAGNOSTICS";
const PREFIX: &str = "tenancy::tests::execution::native::session::choice::recovery::";

#[derive(Serialize, Deserialize)]
struct Spec {
    installation: bitrouter_ai::tenancy::identity::InstallationId,
    tenant: bitrouter_ai::tenancy::identity::TenantId,
    principal: bitrouter_ai::tenancy::identity::PrincipalId,
    binding: bitrouter_ai::tenancy::identity::BindingId,
    binding_result: bitrouter_ai::tenancy::operation::OperationResult,
    headers: std::collections::BTreeMap<String, String>,
    route: ResourceRef,
    key: CredentialIssued,
    revision: bitrouter_ai::tenancy::identity::RevisionId,
    generation: String,
    mcp: Option<String>,
}
impl Spec {
    fn capture(bound: &Bound, generation: &Provider) -> Result<Self> {
        Ok(Self {
            installation: bound.fixture.installation.clone(),
            tenant: bound.fixture.tenant.clone(),
            principal: bound.fixture.principal.clone(),
            binding: bound.fixture.binding.clone(),
            binding_result: bound.fixture.binding_result.clone(),
            route: bound.route.clone(),
            key: bound.key.clone(),
            revision: bound.revision.clone(),
            generation: generation.url.clone(),
            mcp: None,
            headers: bound
                .headers
                .iter()
                .map(|(name, value)| Ok((name.to_string(), value.to_str()?.to_owned())))
                .collect::<Result<_>>()?,
        })
    }
    async fn connect(&self, root: &Path) -> Result<Bound> {
        self.connect_observed(root, crate::telemetry::receipt_latency::Sink::default())
            .await
    }
    async fn connect_observed(
        &self,
        root: &Path,
        observer: crate::telemetry::receipt_latency::Sink,
    ) -> Result<Bound> {
        let url = format!("sqlite://{}", root.join("owned.db").display());
        let database = crate::db::connect_owned_tenancy(&url).await?;
        let db = database.database().clone();
        let file = root.join("owned.db").canonicalize()?;
        ensure!(
            std::fs::metadata(&file)?.len() > 0,
            "real authority file missing"
        );
        let rows = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "PRAGMA database_list",
            ))
            .await?;
        let mains = rows
            .iter()
            .filter_map(|row| match row.try_get::<String>("", "name") {
                Ok(name) if name == "main" => Some(row.try_get::<String>("", "file")),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            mains.len() == 1
                && !mains[0].is_empty()
                && Path::new(&mains[0]).canonicalize()? == file,
            "connector did not open the actual leased file"
        );
        let service = Service::open_owned_profile_observed(
            database,
            &crate::config::tenancy::TenancyConfig {
                enabled: true,
                wrapping_key_env: Some(KEY.into()),
                ..Default::default()
            },
            &crate::config::ContinuationConfig::default(),
            observer,
        )
        .await?;
        service.dispatch_test_ready.store(true, Ordering::Release);
        ensure!(service.current().map_err(failure)?.installation == self.installation);
        let previous_headers = self
            .headers
            .iter()
            .map(|(name, value)| {
                Ok((
                    http::header::HeaderName::from_bytes(name.as_bytes())?,
                    value.parse()?,
                ))
            })
            .collect::<Result<HeaderMap>>()?;
        let fixture = Fixture {
            db,
            service,
            operator: crate::tenancy::model::Caller::Installation(
                crate::tenancy::operator_principal("test-operator").map_err(failure)?,
            ),
            installation: self.installation.clone(),
            tenant: self.tenant.clone(),
            principal: self.principal.clone(),
            binding: self.binding.clone(),
            binding_result: self.binding_result.clone(),
        };
        // A fresh Service has a new instance. Reauthenticate the same credential
        // before the campaign applies any deliberate revocation or expiry.
        let stale = fixture
            .service
            .host_authority(&previous_headers)
            .err()
            .context("previous Service binding authorized a fresh host")?;
        ensure!(stale.0.code == bitrouter_ai::tenancy::operation::ErrorCode::CapabilityMismatch);
        let headers = crate::tenancy::tests::execution::headers(
            &fixture,
            self.key
                .secret
                .as_deref()
                .context("original recovery credential")?,
            &self.revision,
        )?;
        ensure!(
            headers.len() == previous_headers.len()
                && previous_headers.iter().all(|(name, value)| {
                    matches!(
                        name.as_str(),
                        "bitrouter-instance-id" | "bitrouter-capability-revision"
                    ) || headers.get(name) == Some(value)
                }),
            "rebind changed the original credential, revision or request metadata"
        );
        Ok(Bound {
            fixture,
            headers,
            route: self.route.clone(),
            key: self.key.clone(),
            revision: self.revision.clone(),
        })
    }
}

fn private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.flush()?;
    Ok(())
}
fn read_spec(root: &Path) -> Result<Spec> {
    let bytes = std::fs::read(root.join("spec.json"))?;
    ensure!(bytes.len() <= 64 * 1024, "test spec bound");
    Ok(serde_json::from_slice(&bytes)?)
}
fn signal(value: &str) -> Result<()> {
    let mut out = std::io::stdout().lock();
    write!(out, "\n{value}\n")?;
    out.flush()?;
    Ok(())
}

// Optional private evidence for nested fixture failures. Never print child
// stderr, which can contain fixture credentials, into the parent/CI logs.
fn retain_child_diagnostics(selector: &str, mode: &str, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let Some(directory) = std::env::var_os(DIAGNOSTICS) else {
        return Ok(());
    };
    if bytes.is_empty() {
        return Ok(());
    }
    // Preserve the existing 8193-byte reader, including its over-limit sentinel;
    // the original 8192-byte assertion still rejects overflow after retention.
    ensure!(bytes.len() <= 8193, "diagnostic reader bound changed");
    let directory = PathBuf::from(directory);
    let metadata = std::fs::symlink_metadata(&directory)?;
    ensure!(
        directory.is_absolute()
            && metadata.is_dir()
            && metadata.permissions().mode() & 0o077 == 0
            && directory.canonicalize()? == directory,
        "diagnostics require an explicit private canonical directory"
    );
    let path = directory.join(format!("{}.stderr", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(bytes)?;
    file.flush()?;
    eprintln!(
        "native recovery {selector}/{mode}: private stderr retained at {} ({} bytes)",
        path.display(),
        bytes.len()
    );
    Ok(())
}

struct Child {
    selector: String,
    mode: String,
    child: tokio::process::Child,
    output: BufReader<tokio::io::Take<tokio::process::ChildStdout>>,
    diagnostics: AbortOnDropHandle<std::io::Result<Vec<u8>>>,
}
impl Child {
    fn start(selector: &str, root: &Path, mode: &str) -> Result<Self> {
        use base64::Engine;
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        if mode.starts_with("observation-") {
            for (key, _) in std::env::vars_os() {
                let name = key.to_string_lossy();
                if name.starts_with("OTEL_") || name.starts_with("BITROUTER_TELEMETRY_") {
                    command.env_remove(key);
                }
            }
            for key in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                command.env_remove(key);
            }
            command.env(
                "OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE",
                "cumulative",
            );
        }
        let mut child = command
            .args([
                "--ignored",
                "--exact",
                &format!("{PREFIX}{selector}"),
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROOT, root)
            .env(MODE, mode)
            .env(
                KEY,
                base64::engine::general_purpose::STANDARD.encode([29u8; 32]),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let output = BufReader::new(child.stdout.take().context("test stdout")?.take(8193));
        let error = child.stderr.take().context("test stderr")?;
        let diagnostics = AbortOnDropHandle::new(tokio::spawn(async move {
            let mut bytes = Vec::new();
            error.take(8193).read_to_end(&mut bytes).await?;
            Ok(bytes)
        }));
        Ok(Self {
            selector: selector.to_owned(),
            mode: mode.to_owned(),
            child,
            output,
            diagnostics,
        })
    }
    async fn line(&mut self, expected: &str) -> Result<()> {
        self.line_with_deadline(expected, Duration::from_secs(40))
            .await
    }
    async fn line_with_deadline(&mut self, expected: &str, deadline: Duration) -> Result<()> {
        tokio::time::timeout(deadline, async {
            loop {
                let mut line = String::new();
                let read = self.output.read_line(&mut line).await?;
                ensure!(
                    read > 0 && line.len() <= 8192,
                    "child exited or output bound reached before witness"
                );
                if line.trim_end() == expected {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        })
        .await
        .context("stopped owner witness deadline")?
    }
    async fn reap(mut self, killed: bool) -> Result<()> {
        if killed {
            self.child.start_kill()?;
        }
        let status = tokio::time::timeout(TIMEOUT, self.child.wait()).await??;
        let bytes = tokio::time::timeout(TIMEOUT, self.diagnostics).await???;
        retain_child_diagnostics(&self.selector, &self.mode, &bytes)?;
        ensure!(bytes.len() <= 8192, "child stderr bound");
        ensure!(
            status.success() != killed,
            "child status {status}, stderr sha256 {}",
            crate::storage::checkpoint::sha256(&bytes)
        );
        // Keep the bounded pipe reader through actual wait/reap, including the
        // libtest terminal line. A witness never substitutes for child exit.
        drop(self.output);
        Ok(())
    }
}

#[tokio::test]
#[ignore = "isolated real native stopped-owner file and process campaign"]
async fn native_stopped_owner_actual_receipt_repair_and_no_resend() -> Result<()> {
    for mode in [
        "completed-child",
        "missing-child",
        "precommit-rollback",
        "commit-error",
        "revoked-after-prepare",
        "repair-commit-crash",
        "legacy-unreleased",
        "missing-seed",
    ] {
        let root = tempfile::tempdir()?;
        let mut child = Child::start("native_stopped_owner_controller", root.path(), mode)?;
        let observed = child.line("native-recovery-controller-checked").await;
        let reaped = child.reap(observed.is_err()).await;
        observed?;
        reaped?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "internal stopped-owner campaign controller; select the parent only"]
async fn native_stopped_owner_controller() -> Result<()> {
    let Some(root) = std::env::var_os(ROOT) else {
        return Ok(());
    };
    cases::campaign(Path::new(&root), &std::env::var(MODE)?).await?;
    signal("native-recovery-controller-checked")
}

#[tokio::test]
#[ignore = "internal stopped-owner producer; select the parent only"]
async fn native_stopped_owner_producer() -> Result<()> {
    let Some(root) = std::env::var_os(ROOT) else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let spec = read_spec(&root)?;
    let bound = spec.connect(&root).await?;
    let app = Arc::new(
        host_app(
            &bound.fixture,
            &spec.generation,
            Arc::new(HostSpies::default()),
        )?
        .with_tenancy(bound.fixture.service.clone()),
    );
    let mode = std::env::var(MODE)?;
    if mode.starts_with("marker-") {
        if mode == "marker-released" {
            let released = NativeSession::open_authenticated(
                app.clone(),
                bound.headers.clone(),
                "choice-session",
                &root.join("workspace"),
                ToolMode::Coding,
                &HarnessConfig::default(),
            )
            .await?;
            released.close().await?;
            tokio::time::timeout(TIMEOUT, async {
                while bound.fixture.service.host_slots_for_test() != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            private_json(
                &root.join("released-before.json"),
                &fixture::journal(&bound).await?,
            )?;
        }
        signal("native-marker-opening")?;
        let config = marker::config(spec.mcp.as_deref().context("actual MCP URL")?);
        let opened = NativeSession::open_authenticated(
            app,
            bound.headers.clone(),
            "choice-session",
            &root.join("workspace"),
            ToolMode::Coding,
            &config,
        )
        .await;
        let session = opened.context("held MCP discovery failed before producer termination")?;
        session.close().await?;
        anyhow::bail!("held MCP discovery returned before producer termination");
    }
    let mut session = NativeSession::open_authenticated(
        app,
        bound.headers.clone(),
        "choice-session",
        &root.join("workspace"),
        ToolMode::ReadOnly,
        &HarnessConfig::default(),
    )
    .await?;
    if mode == "profile-oauth" {
        let row = fixture::journal(&bound).await?;
        ensure!(row["released"] == false);
        signal("native-unsupported-open")?;
        std::future::pending::<()>().await;
        drop(session);
        return Ok(());
    }
    if matches!(mode.as_str(), "profile-generation" | "profile-count") {
        let running = AbortOnDropHandle::new(tokio::spawn(async move {
            session
                .run(input(), Arc::new(Approval), CancellationToken::new())
                .await
        }));
        signal("native-uncovered-running")?;
        // The parent observes the real provider POST before killing/reaping us.
        // Keep the native driver and all owned work alive until that boundary.
        std::future::pending::<()>().await;
        drop(running);
        return Ok(());
    }
    let core = session.core().clone();
    let running = AbortOnDropHandle::new(tokio::spawn(async move {
        session
            .run(
                fixture::task(),
                Arc::new(Approval),
                CancellationToken::new(),
            )
            .await
    }));
    let saved = tokio::time::timeout(TIMEOUT, async {
        loop {
            let value = fixture::journal(&bound).await?;
            if value["owned_choices"]
                .as_object()
                .is_some_and(|links| links.len() >= if mode == "shared-parent" { 2 } else { 1 })
            {
                break Ok::<_, anyhow::Error>(value);
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    let (decision, link) = saved["owned_choices"]
        .as_object()
        .context("real intent")?
        .iter()
        .next()
        .context("real intent link")?;
    ensure!(link["version"] == 2 && link["seeds"].as_array().is_some_and(|v| v.len() == 2));
    let child = link["child_execution"].as_str().context("actual child")?;
    private_json(&root.join("intent.json"), &saved)?;
    if std::env::var(MODE)? == "completed-child" {
        bound
            .fixture
            .service
            .cleanup
            .observe_receipt_for_test(Key {
                installation: bound.fixture.installation.clone(),
                tenant: bound.fixture.tenant.clone(),
                execution: child.try_into()?,
            })?;
        let receipt = bound.fixture.service.cleanup.hold(Phase::Committing);
        signal("native-recovery-intent")?;
        tokio::time::timeout(TIMEOUT, receipt.entered.notified()).await?;
        bound.fixture.service.cleanup.release_gate();
        let outcome = bound.fixture.service.cleanup.hold(Phase::Committing);
        tokio::time::timeout(TIMEOUT, outcome.entered.notified()).await?;
        let trace = bound.fixture.service.cleanup.receipt_trace_for_test()?;
        let fact = trace.facts[0].as_ref().context("actual receipt COMMIT")?;
        ensure!(
            !trace.overflow && fact.inserted == Some(true) && fact.commit.is_some_and(|v| v.ok)
        );
        ensure!(
            core.snapshot().await.context_store.decisions[decision]
                .outcome
                .is_none()
        );
        // The same actor is held before native COMMIT. Read through its public
        // immutable receipt read would self-wait; exact SQL is read-only here.
        let reader = crate::db::connect(&format!(
            "sqlite://{}?mode=ro",
            root.join("owned.db").display()
        ))
        .await?;
        let row=reader.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT receipt_json,disposition_json FROM tenancy_execution_receipts WHERE installation_id=? AND tenant_id=? AND execution_id=?",
            [bound.fixture.installation.as_str().into(),bound.fixture.tenant.as_str().into(),child.into()])).await?.context("committed child receipt")?;
        private_json(
            &root.join("receipt-before.json"),
            &(
                row.try_get::<String>("", "receipt_json")?,
                row.try_get::<String>("", "disposition_json")?,
            ),
        )?;
        reader.close().await?;
        signal("native-recovery-terminal")?;
    } else {
        signal("native-recovery-intent")?;
    }
    ensure!(
        !running.is_finished(),
        "producer finished before death boundary"
    );
    std::future::pending::<()>().await;
    Ok(())
}

#[tokio::test]
#[ignore = "internal native repair producer; select the campaign parent only"]
async fn native_stopped_owner_repairer() -> Result<()> {
    let Some(root) = std::env::var_os(ROOT) else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let spec = read_spec(&root)?;
    let bound = spec.connect(&root).await?;
    let app = Arc::new(
        host_app(
            &bound.fixture,
            &spec.generation,
            Arc::new(HostSpies::default()),
        )?
        .with_tenancy(bound.fixture.service.clone()),
    );
    let gate = bound.fixture.service.cleanup.hold(Phase::Committing);
    let headers = bound.headers.clone();
    let workspace = root.join("workspace");
    let opening = AbortOnDropHandle::new(tokio::spawn(async move {
        NativeSession::open_authenticated(
            app,
            headers,
            "choice-session",
            &workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await
    }));
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    bound.fixture.service.cleanup.release_gate();
    let retirement = bound.fixture.service.cleanup.hold(Phase::Committing);
    tokio::time::timeout(TIMEOUT, retirement.entered.notified()).await?;
    let reader = crate::db::connect(&format!(
        "sqlite://{}?mode=ro",
        root.join("owned.db").display()
    ))
    .await?;
    let row = reader
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT payload FROM bro_managed_sessions",
        ))
        .await?
        .context("repair committed native row")?;
    let payload: Value = serde_json::from_str(&row.try_get::<String>("", "payload")?)?;
    ensure!(
        payload["recovery"]["phase"] == "retiring"
            && payload["recovery"]["workspace_grant"].is_object()
    );
    let rows=reader.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT execution_id,receipt_json,disposition_json FROM tenancy_execution_receipts ORDER BY execution_id")).await?;
    ensure!(rows.len() == 2);
    let receipts = rows
        .iter()
        .map(|row| {
            Ok((
                row.try_get::<String>("", "execution_id")?,
                row.try_get::<String>("", "receipt_json")?,
                row.try_get::<String>("", "disposition_json")?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    private_json(&root.join("repair-terminals.json"), &receipts)?;
    reader.close().await?;
    signal("native-repair-committed-before-retirement")?;
    std::future::pending::<()>().await;
    drop(opening);
    Ok(())
}
