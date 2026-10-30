//! Actual process death cannot turn a persisted open row into a live provider link.
use super::*;
use bitrouter_ai::tenancy::{
    identity::{InstallationId, RevisionId, TenantId},
    operation::OperationResult,
};
use std::{io::Write, process::Stdio};
use tokio::io::{AsyncBufReadExt, BufReader};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

const ROOT: &str = "BITROUTER_BLOCK_RESTART_ROOT";
const MODE: &str = "BITROUTER_BLOCK_RESTART_MODE";
const UPSTREAM: &str = "BITROUTER_BLOCK_RESTART_UPSTREAM";
const CHILD: &str =
    "tenancy::tests::execution::native::capture::block::restart::actual_block_restart_child";
const READY: &str = "owned-block-open-capture-durable";
#[derive(serde::Serialize, serde::Deserialize)]
struct Transfer {
    database: PathBuf,
    marker: PathBuf,
    installation: InstallationId,
    tenant: TenantId,
    principal: PrincipalId,
    binding: BindingId,
    binding_result: OperationResult,
    key: CredentialIssued,
    revision: RevisionId,
    old_link: String,
    old_controller: String,
}

#[tokio::test]
#[ignore = "exact selector used only by the bounded physical crash/restart parent"]
async fn actual_block_restart_child() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("private parent root")?);
    let url = std::env::var(UPSTREAM)?;
    let mode = std::env::var(MODE)?;
    ensure!(root.is_absolute() && root.is_dir() && matches!(mode.as_str(), "produce" | "reopen"));
    let directory = tempfile::tempdir_in(&root)?;
    if mode == "produce" {
        let mut host = Host::new_in(directory, Some(&url)).await?;
        host.marker = root.join("producer-controller.json");
        let checked = std::panic::AssertUnwindSafe(async {
            let native = host.open().await?;
            host.current()?
                .prompt(&native, "actual durable turn before process crash")
                .await?;
            tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
            host.current()?.checkpoint(&native).await?;
            let rows = host.records("owned_block_intent_v1").await?;
            ensure!(rows.len() == 1 && host.fixture.service.host_slots_for_test() == 0);
            let endpoint = host.endpoint_headers().await?;
            let transfer = Transfer {
                database: host.database.clone(),
                marker: host.marker.clone(),
                installation: host.fixture.installation.clone(),
                tenant: host.fixture.tenant.clone(),
                principal: host.fixture.principal.clone(),
                binding: host.fixture.binding.clone(),
                binding_result: host.fixture.binding_result.clone(),
                key: host.key.clone(),
                revision: rows[0]["descriptor"]["revision"]
                    .as_str()
                    .context("actual config")?
                    .try_into()?,
                old_link: endpoint
                    .get(crate::tenancy::execution::BLOCK_LINK_HEADER)
                    .context("actual link")?
                    .to_str()?
                    .into(),
                old_controller: rows[0]["descriptor"]["controller"]
                    .as_str()
                    .context("actual controller")?
                    .into(),
            };
            let encoded = serde_json::to_vec(&transfer)?;
            ensure!(encoded.len() <= 16 * 1024);
            tokio::fs::write(root.join("actual-transfer.json"), encoded).await?;
            println!("\n{READY}");
            std::io::stdout().flush()?;
            // The parent kills the real runtime; returning would clean the row
            // and would not exercise an abandoned-but-still-open capture.
            let mut line = String::new();
            tokio::time::timeout(
                Duration::from_secs(45),
                BufReader::new(tokio::io::stdin().take(32)).read_line(&mut line),
            )
            .await??;
            anyhow::bail!("crash producer was not killed")
        })
        .catch_unwind()
        .await;
        let checked = match checked {
            Ok(value) => value,
            Err(_) => Err(anyhow::anyhow!("crash observation unwound")),
        };
        return host.finish(checked).await;
    }
    ensure!(
        tokio::fs::metadata(root.join("actual-transfer.json"))
            .await?
            .len()
            <= 16 * 1024
    );
    let bytes = tokio::fs::read(root.join("actual-transfer.json")).await?;
    ensure!(bytes.len() <= 16 * 1024);
    let transfer: Transfer = serde_json::from_slice(&bytes)?;
    ensure!(
        transfer
            .database
            .canonicalize()?
            .starts_with(root.canonicalize()?)
    );
    let database = crate::db::connect_owned_tenancy(&format!(
        "sqlite://{}?mode=rw",
        transfer.database.display()
    ))
    .await?;
    let (db, owner, claim) = database.claim()?;
    let snapshot = store::load(&db, chrono::Utc::now()).await?;
    ensure!(snapshot.installation == transfer.installation);
    let service = Service::start_resources_with_continuations(
        db.clone(),
        Some(owner),
        OperationLimits::default(),
        snapshot,
        Arc::new(Resources::with_key(
            transfer.installation.clone(),
            [29; 32],
        )?),
        (
            None,
            &crate::config::ContinuationConfig::default(),
            Some(claim),
            crate::telemetry::receipt_latency::Sink::default(),
        ),
        None,
    )
    .await?;
    let fixture = Fixture {
        service,
        db,
        operator: crate::tenancy::Caller::Installation(
            crate::tenancy::operator_principal("test-operator").map_err(failure)?,
        ),
        installation: transfer.installation,
        tenant: transfer.tenant,
        principal: transfer.principal,
        binding: transfer.binding,
        binding_result: transfer.binding_result,
    };
    let mut listener = None;
    let mut session = None;
    let marker = root.join("reopened-controller.json");
    let stop = CancellationToken::new();
    let checked=std::panic::AssertUnwindSafe(async {
        fixture.service.dispatch_test_ready.store(true,Ordering::Release);
        let ingress=headers(&fixture,transfer.key.secret.as_deref().context("original issued key")?,&transfer.revision)?;
        let old=fixture.db.query_all(Statement::from_string(DatabaseBackend::Sqlite,"SELECT metadata_json FROM acp_capture_connections")).await?;
        ensure!(old.len()==1);
        let before:String=old[0].try_get("","metadata_json")?;
        let value:Value=serde_json::from_str(&before)?;
        ensure!(value["closed"]==false,"fixture lost the actual crash-open row");
        let app=Arc::new(host_app(&fixture,&format!("{url}/v1"),Arc::new(HostSpies::default()))?.with_tenancy(fixture.service.clone()));
        listener=Some(Listener::start(app.clone()).await?);
        let mut old_headers=ingress.clone();
        old_headers.insert(crate::tenancy::execution::BLOCK_LINK_HEADER,transfer.old_link.parse()?);
        let response=reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?.post(format!("{}/v1/chat/completions",listener.as_ref().context("listener")?.origin))
            .headers(old_headers).json(&json!({"model":"literal:subject","messages":[{"role":"user","content":"old process link"}]})).send().await?;
        ensure!(response.status()==StatusCode::FORBIDDEN);
        let body=response.bytes().await?;ensure!(body.len()<=8192);
        let rejected:bitrouter_ai::tenancy::operation::Error=serde_json::from_slice(&body)?;ensure!(rejected.code==ErrorCode::Forbidden);
        let unchanged=fixture.db.query_one(Statement::from_string(DatabaseBackend::Sqlite,"SELECT metadata_json FROM acp_capture_connections")).await?.context("old capture")?;
        ensure!(unchanged.try_get::<String>("","metadata_json")?==before);
        // Same key/tenant/R1, but a new real opener is required to create a live link.
        session=Some(ControlledSession::open_authenticated(app,ingress,launch(&executable()?,&marker,&listener.as_ref().context("listener")?.origin,None),stop.clone()).await?);
        let current=session.as_ref().context("new actual controller")?;
        let native=current.new_session(directory.path().to_owned()).await?;
        ensure!(native=="native-1");
        current.prompt(&native,"genuine second owner after process death").await?;
        tokio::time::timeout(TIMEOUT,fixture.service.admission.wait_drained()).await?;
        ensure!(fixture.service.current().is_ok());
        let rows=fixture.db.query_all(Statement::from_string(DatabaseBackend::Sqlite,"SELECT body FROM checkpoint_evolution WHERE kind='owned_block_intent_v1'")).await?;
        ensure!(rows.len()==2);
        let owners=fixture.db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT metadata_json FROM acp_capture_connections")).await?;
        ensure!(owners.len()==2);
        ensure!(owners.iter().map(|row| row.try_get::<String>("", "metadata_json"))
            .collect::<std::result::Result<Vec<_>, _>>()?.iter()
            .any(|raw| raw.contains(&transfer.old_controller)), "original controller evidence disappeared");
        Ok::<_,anyhow::Error>(())
    }).catch_unwind().await;
    let checked = match checked {
        Ok(value) => value,
        Err(_) => Err(anyhow::anyhow!("restart observation unwound")),
    };
    stop.cancel();
    drop(session);
    let drained = tokio::time::timeout(Duration::from_secs(40), fixture.service.shutdown())
        .await
        .map_err(anyhow::Error::from)
        .and_then(|v| v);
    let reaped = if marker.exists() {
        reaped(&marker).await
    } else {
        Ok(())
    };
    let joined = match listener {
        Some(value) => value.finish().await,
        None => Ok(()),
    };
    let closed = fixture
        .db
        .clone()
        .close()
        .await
        .map_err(anyhow::Error::from);
    checked.and(drained).and(reaped).and(joined).and(closed)
}

struct Process {
    child: tokio::process::Child,
    output: BufReader<tokio::io::Take<tokio::process::ChildStdout>>,
    errors: AbortOnDropHandle<std::io::Result<Vec<u8>>>,
    consumed: usize,
}
impl Process {
    fn start(root: &Path, url: &str, mode: &str) -> Result<Self> {
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                CHILD,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROOT, root)
            .env(UPSTREAM, url)
            .env(MODE, mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let output = BufReader::new(child.stdout.take().context("stdout")?.take(8193));
        let stderr = child.stderr.take().context("stderr")?;
        let errors = AbortOnDropHandle::new(tokio::spawn(async move {
            let mut bytes = Vec::new();
            stderr.take(8193).read_to_end(&mut bytes).await?;
            Ok(bytes)
        }));
        Ok(Self {
            child,
            output,
            errors,
            consumed: 0,
        })
    }
    async fn ready(&mut self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                let mut line = String::new();
                let n = self.output.read_line(&mut line).await?;
                self.consumed = self.consumed.checked_add(n).context("stdout count")?;
                ensure!(
                    n > 0 && self.consumed <= 8192,
                    "no bounded producer witness"
                );
                if line.trim() == READY {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        })
        .await?
    }
    async fn finish(mut self, kill: bool, checked: Result<()>) -> Result<()> {
        let killing = if kill {
            self.child.start_kill().map_err(anyhow::Error::from)
        } else {
            Ok(())
        };
        drop(self.child.stdin.take());
        let status = match tokio::time::timeout(Duration::from_secs(60), self.child.wait()).await {
            Ok(result) => result.map_err(anyhow::Error::from),
            Err(error) => {
                let retry = self.child.start_kill();
                let reaped = tokio::time::timeout(Duration::from_secs(6), self.child.wait()).await;
                Err(anyhow::anyhow!(
                    "child deadline {error}; kill={retry:?}; reap={reaped:?}"
                ))
            }
        };
        let mut tail = Vec::new();
        let output =
            tokio::time::timeout(Duration::from_secs(5), self.output.read_to_end(&mut tail)).await;
        let errors = tokio::time::timeout(Duration::from_secs(5), self.errors).await;
        let errors = errors???;
        ensure!(errors.len() <= 8192, "child stderr exceeded bound");
        checked.with_context(|| {
            format!("actual child stderr: {}", String::from_utf8_lossy(&errors))
        })?;
        killing?;
        output??;
        ensure!(self.consumed + tail.len() <= 8192);
        let status = status?;
        ensure!(
            status.success() != kill,
            "unexpected child status; stderr: {}",
            String::from_utf8_lossy(&errors)
        );
        Ok(())
    }
}
async fn contain_controller(root: &Path, names: &[&str]) -> Result<()> {
    let mut failures = Vec::new();
    for name in names {
        let result = async {
            let marker = root.join(name);
            if !marker.try_exists()? {
                return Ok::<_, anyhow::Error>(());
            }
            ensure!(marker.canonicalize()?.starts_with(root.canonicalize()?));
            ensure!(tokio::fs::metadata(&marker).await?.len() <= 1024);
            let value: Value = serde_json::from_slice(&tokio::fs::read(&marker).await?)?;
            let raw = i32::try_from(value["pid"].as_u64().context("actual controller pid")?)?;
            ensure!(raw > 1 && u32::try_from(raw)? != std::process::id());
            let pid = rustix::process::Pid::from_raw(raw).context("pid")?;
            match rustix::process::kill_process_group(pid, rustix::process::Signal::KILL) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                // A denied signal proves neither liveness nor absence. The
                // peer may still exit after its owning process dies. Keep the
                // same bounded observation below and accept only actual ESRCH.
                Err(rustix::io::Errno::PERM) => {}
                Err(error) => return Err(error.into()),
            }
            tokio::time::timeout(Duration::from_secs(6), async {
                while rustix::process::test_kill_process_group(pid) != Err(rustix::io::Errno::SRCH)
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            failures.push(format!("{name}: {error:#}"));
        }
    }
    ensure!(
        failures.is_empty(),
        "controller containment failed: {}",
        failures.join("; ")
    );
    Ok(())
}

#[tokio::test]
#[ignore = "actual child/runtime process death, physical SQLite and real Rust ACP peer; no nested Cargo"]
async fn process_crash_reopen_refuses_stale_open_capture_link_before_any_new_provider_send()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let result = crash_and_reopen(root.path()).await;
    if result.is_err() {
        eprintln!("failed Block restart fixture: {}", root.keep().display());
    }
    result
}

async fn crash_and_reopen(root: &Path) -> Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"actual-block","object":"chat.completion","created":0,"model":"model-a",
        "choices":[{"index":0,"message":{"role":"assistant","content":"native reply"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},"completion_tokens_details":{"reasoning_tokens":0}}}))).mount(&server).await;
    let mut producer = Process::start(root, &server.uri(), "produce")?;
    let ready = producer.ready().await;
    let crash = ready.is_ok();
    let killed = producer.finish(crash, ready).await;
    // Attempt both independent process cleanups even if the runtime witness failed.
    let contained = contain_controller(root, &["producer-controller.json"]).await;
    killed.and(contained)?;
    ensure!(server.received_requests().await.context("provider")?.len() == 1);
    ensure!(
        tokio::fs::metadata(root.join("actual-transfer.json"))
            .await?
            .len()
            <= 16 * 1024
    );
    let transfer: Transfer =
        serde_json::from_slice(&tokio::fs::read(root.join("actual-transfer.json")).await?)?;
    let owner = crate::tenancy::ownership::acquire_path(&transfer.database, false)?;
    drop(owner);
    let reopened = Process::start(root, &server.uri(), "reopen")?;
    let result = reopened.finish(false, Ok(())).await;
    let contained = contain_controller(root, &["reopened-controller.json"]).await;
    result.and(contained)?;
    ensure!(
        server.received_requests().await.context("provider")?.len() == 2,
        "stale-link probe reached provider or real reopen did not send"
    );
    let owner = crate::tenancy::ownership::acquire_path(&transfer.database, false)?;
    drop(owner);
    Ok(())
}
