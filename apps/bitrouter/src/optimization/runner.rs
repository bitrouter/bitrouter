use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

#[cfg(not(windows))]
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(not(windows))]
use tokio::io::AsyncReadExt;

use crate::optimization::OptimizationPreference;
use crate::policy_lock::{
    CertificateSource, POLICY_LOCKFILE_VERSION, PolicyCertificate, PolicyLock, PromotionVerdict,
    RouteOwner, semantic_digest, validate_document,
};
use crate::workflow_state::ir::{RouteProjection, WorkflowStateKind};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteObservation {
    pub request_key: String,
    pub selected_tier: String,
    #[serde(default)]
    pub input_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
    #[serde(default)]
    pub selected_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
    pub normalized_cost_micro_usd: Option<u64>,
}

pub struct WorkflowRunRequest<'a> {
    pub workflow: &'a super::WorkflowCommand,
    pub cwd: &'a Path,
    pub env: &'a BTreeMap<String, String>,
    pub maximum_output_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowExecution {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub elapsed: Duration,
    pub stdout: String,
    pub stderr: String,
    pub launches: u32,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateDaemonPaths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub policy: PathBuf,
    pub database: PathBuf,
    pub control_socket: PathBuf,
    pub decisions: PathBuf,
    pub workflow_evidence: PathBuf,
    pub log: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantEvidence {
    pub variant: String,
    pub policy_digest: String,
    pub execution: WorkflowExecution,
    pub request_count: usize,
    pub normalized_cost_micro_usd: u64,
    pub observed_latency_ms: u64,
    pub observations: Vec<RouteObservation>,
    pub attributions: Vec<VariantAttribution>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantAttribution {
    pub request_id: String,
    pub decision: crate::eval::types::EvalDecisionRef,
    pub usage_origin: bitrouter_sdk::language_model::UsageOrigin,
    pub pricing_source: crate::metering::PricingSource,
    pub pricing_version: String,
    pub normalized_cost_micro_usd: u64,
    pub latency_ms: u64,
}

pub struct PrivateVariantRequest<'a> {
    pub variant: &'a str,
    pub paths: &'a PrivateDaemonPaths,
    pub intent: &'a super::OptimizationIntent,
    pub policy: &'a PolicyLock,
    pub policy_digest: &'a str,
    pub source_config_raw: &'a str,
    pub workflow_cwd: &'a Path,
    pub bitrouter_executable: &'a Path,
    pub maximum_output_bytes: usize,
}

struct PrivateDaemon {
    child: tokio::process::Child,
    control_socket: PathBuf,
}

struct PrivateDaemonSupervisor {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    completion: Option<tokio::sync::oneshot::Receiver<Result<()>>>,
}

impl PrivateDaemonPaths {
    pub fn new(root: PathBuf) -> Self {
        let root = super::absolute_path(root);
        #[cfg(unix)]
        let control_socket = {
            use sha2::Digest;
            let digest = hex::encode(sha2::Sha256::digest(root.to_string_lossy().as_bytes()));
            PathBuf::from("/tmp").join(format!("br-opt-{}.sock", &digest[..20]))
        };
        #[cfg(not(unix))]
        let control_socket = root.join("bitrouter.sock");
        Self {
            config: root.join("bitrouter.yaml"),
            policy: root.join("policy-lock.yaml"),
            database: root.join("bitrouter.db"),
            control_socket,
            decisions: root.join("policy-decisions.jsonl"),
            workflow_evidence: root.join("workflow-evidence.json"),
            log: root.join("daemon.log"),
            root,
        }
    }

    pub fn database_url(&self) -> String {
        format!("sqlite://{}?mode=rwc", self.database.display())
    }
}

pub fn private_daemon_config(
    paths: &PrivateDaemonPaths,
    intent: &super::OptimizationIntent,
    source_config_raw: &str,
    port: u16,
) -> Result<String> {
    intent.validate()?;
    let source: serde_json::Value = serde_saphyr::from_str(source_config_raw)
        .context("parsing source config for private daemon")?;
    let source = source
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("source BitRouter config must be a YAML object"))?;
    let mut document = serde_json::Map::new();
    for key in [
        "upstream",
        "providers",
        "models",
        "presets",
        "variants",
        "plugins",
        "mcp",
        "mcp_servers",
        "server_tools",
        "inherit_defaults",
        "registry",
        "continuation",
    ] {
        if let Some(value) = source.get(key) {
            document.insert(key.to_string(), value.clone());
        }
    }
    let providers = document
        .entry("providers")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("source config providers must be a YAML object"))?;
    for route in [&intent.strong, &intent.economy] {
        let (provider, _) = route.split_once(':').ok_or_else(|| {
            anyhow::anyhow!("optimization tier route '{route}' must be provider-qualified")
        })?;
        providers
            .entry(provider.to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    }
    document.insert(
        "server".into(),
        serde_json::json!({
            "listen": format!("127.0.0.1:{port}"),
            "control_socket": paths.control_socket,
            "log_level": "warn",
            "skip_auth": true
        }),
    );
    document.insert(
        "database".into(),
        serde_json::json!({ "url": paths.database_url() }),
    );
    document.insert(
        "policy".into(),
        serde_json::json!({ "path": paths.policy, "mode": "frozen" }),
    );
    document.insert("trajectory".into(), serde_json::json!({ "enabled": false }));
    let presets = document
        .entry("presets")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("source config presets must be a YAML object"))?;
    let preset = presets
        .entry(intent.preset.clone())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("source optimization preset must be a YAML object"))?;
    preset.insert(
        "model".into(),
        serde_json::Value::String(intent.strong.clone()),
    );
    preset.insert(
        "policy".into(),
        serde_json::Value::String(intent.policy.clone()),
    );
    let mut rendered = serde_saphyr::to_string(&serde_json::Value::Object(document))
        .context("serializing private daemon config")?;
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(rendered)
}

pub async fn run_private_variant(request: PrivateVariantRequest<'_>) -> Result<VariantEvidence> {
    request.intent.validate()?;
    validate_document(request.policy)?;
    if semantic_digest(request.policy)? != request.policy_digest {
        anyhow::bail!("private variant policy digest does not match its frozen document");
    }
    super::secure_private_directory(&request.paths.root).await?;
    let port = reserve_loopback_port()?;
    let config = private_daemon_config(
        request.paths,
        request.intent,
        request.source_config_raw,
        port,
    )?;
    tokio::fs::write(&request.paths.config, config)
        .await
        .with_context(|| format!("writing private config {}", request.paths.config.display()))?;
    super::secure_private_file(&request.paths.config).await?;
    tokio::fs::write(
        &request.paths.policy,
        crate::policy_lock::deterministic_yaml(request.policy)?,
    )
    .await
    .with_context(|| format!("writing private policy {}", request.paths.policy.display()))?;
    super::secure_private_file(&request.paths.policy).await?;

    let mut daemon = PrivateDaemonSupervisor::start(
        request.bitrouter_executable.to_path_buf(),
        request.paths.clone(),
        port,
    )
    .await?;
    let run_result = async {
        let env =
            workflow_environment(&format!("http://127.0.0.1:{port}"), &request.intent.preset)?;
        run_workflow_command(WorkflowRunRequest {
            workflow: &request.intent.workflow,
            cwd: request.workflow_cwd,
            env: &env,
            maximum_output_bytes: request.maximum_output_bytes,
        })
        .await
    }
    .await;
    let cleanup_result = daemon.stop().await;
    let mut execution = match (run_result, cleanup_result) {
        (Ok(execution), Ok(())) => execution,
        (Err(error), Err(cleanup)) => {
            return Err(error.context(format!("private daemon cleanup also failed: {cleanup:#}")));
        }
        (Err(error), Ok(())) => return Err(error),
        (Ok(_), Err(error)) => return Err(error),
    };
    execution.stdout =
        super::evaluator::redact_and_bound(&execution.stdout, request.maximum_output_bytes);
    execution.stderr =
        super::evaluator::redact_and_bound(&execution.stderr, request.maximum_output_bytes);
    tokio::fs::write(
        &request.paths.workflow_evidence,
        serde_json::to_vec_pretty(&execution).context("serializing private workflow evidence")?,
    )
    .await
    .context("writing private workflow evidence")?;
    super::secure_private_file(&request.paths.workflow_evidence).await?;
    if tokio::fs::try_exists(&request.paths.database).await? {
        super::secure_private_file(&request.paths.database).await?;
    }

    let db = crate::db::connect(&request.paths.database_url())
        .await
        .map_err(anyhow::Error::from)
        .context("opening private optimization database")?;
    let metering = crate::metering::MeteringStore::new(db.clone());
    let mut usage = metering
        .export_usage(crate::metering::TimeWindow::ThisMonth)
        .await
        .map_err(anyhow::Error::from)?;
    let request_ids = usage
        .iter()
        .map(|record| {
            record
                .request_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("private metering row has no request identity"))
        })
        .collect::<Result<Vec<_>>>()?;
    if request_ids.is_empty() {
        anyhow::bail!("{} workflow produced no metered requests", request.variant);
    }
    let price_overrides = request
        .intent
        .normalized_price_overrides
        .iter()
        .map(|value| crate::metering::UsagePriceOverride::parse(value))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(anyhow::Error::from)?;
    crate::metering::MeteringUsageRecord::apply_price_overrides(&mut usage, &price_overrides);
    let subjects = crate::eval::store::EvalStore::new(db)
        .list_subjects()
        .await?;
    let decisions =
        crate::workflow_state::decision::PolicyDecisionRecord::load_jsonl(&request.paths.decisions)
            .map_err(anyhow::Error::from)?;
    collect_variant_evidence(
        request.variant,
        request.policy_digest,
        execution,
        &decisions,
        &subjects,
        &usage,
    )
}

fn reserve_loopback_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .context("reserving private daemon port")?;
    listener
        .local_addr()
        .map(|address| address.port())
        .context("reading private daemon port")
}

impl PrivateDaemon {
    async fn start(executable: &Path, paths: &PrivateDaemonPaths, port: u16) -> Result<Self> {
        let log = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&paths.log)
            .with_context(|| format!("creating private daemon log {}", paths.log.display()))?;
        let stderr = log.try_clone().context("cloning private daemon log")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&paths.log, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("securing private daemon log {}", paths.log.display()))?;
        }
        let mut command = tokio::process::Command::new(executable);
        let mut child = command
            .arg("serve")
            .arg("--config")
            .arg(&paths.config)
            .env(
                crate::workflow_state::decision::POLICY_DECISION_JSONL_ENV,
                &paths.decisions,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "starting private BitRouter daemon with {}",
                    executable.display()
                )
            })?;
        let readiness = async {
            loop {
                if let Some(status) = child.try_wait().context("polling private daemon")? {
                    anyhow::bail!("private BitRouter daemon exited before readiness: {status}");
                }
                if tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                    .await
                    .is_ok()
                    && crate::daemon::send_command(
                        &paths.control_socket,
                        &crate::daemon::DaemonCommand::Status,
                    )
                    .await
                    .is_ok()
                {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        match tokio::time::timeout(Duration::from_secs(20), readiness).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                child
                    .start_kill()
                    .context("terminating failed private daemon")?;
                child
                    .wait()
                    .await
                    .context("reaping failed private daemon")?;
                remove_control_socket(&paths.control_socket).await?;
                return Err(error);
            }
            Err(error) => {
                child
                    .start_kill()
                    .context("terminating unready private daemon")?;
                child
                    .wait()
                    .await
                    .context("reaping unready private daemon")?;
                remove_control_socket(&paths.control_socket).await?;
                return Err(anyhow::anyhow!(
                    "private daemon readiness timed out: {error}"
                ));
            }
        }
        Ok(Self {
            child,
            control_socket: paths.control_socket.clone(),
        })
    }

    async fn stop(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        let graceful = match crate::daemon::send_command(
            &self.control_socket,
            &crate::daemon::DaemonCommand::Stop,
        )
        .await
        {
            Ok(crate::daemon::DaemonResponse::Ok) => true,
            Ok(_) => {
                failures.push("private daemon rejected its stop command".to_string());
                false
            }
            Err(error) => {
                failures.push(format!("stopping private daemon: {error:#}"));
                false
            }
        };
        if graceful {
            match tokio::time::timeout(Duration::from_secs(15), self.child.wait()).await {
                Ok(Ok(status)) if status.success() => {}
                Ok(Ok(status)) => {
                    failures.push(format!("private daemon exited unsuccessfully: {status}"));
                }
                Ok(Err(error)) => failures.push(format!("waiting for private daemon: {error}")),
                Err(_) => {
                    failures.push(
                        "private daemon did not stop within its cleanup deadline".to_string(),
                    );
                    if let Err(error) = self.force_reap().await {
                        failures.push(format!("forcing private daemon cleanup: {error:#}"));
                    }
                }
            }
        } else if let Err(error) = self.force_reap().await {
            failures.push(format!("forcing private daemon cleanup: {error:#}"));
        }
        if let Err(error) = remove_control_socket(&self.control_socket).await {
            failures.push(format!("removing private daemon socket: {error:#}"));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(failures.join("; "))
        }
    }

    async fn force_reap(&mut self) -> Result<()> {
        if self
            .child
            .try_wait()
            .context("polling private daemon")?
            .is_some()
        {
            return Ok(());
        }
        self.child
            .start_kill()
            .context("terminating private daemon")?;
        tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .context("timed out reaping private daemon")?
            .context("reaping private daemon")?;
        Ok(())
    }
}

impl PrivateDaemonSupervisor {
    async fn start(executable: PathBuf, paths: PrivateDaemonPaths, port: u16) -> Result<Self> {
        let (readiness_tx, readiness_rx) = tokio::sync::oneshot::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut daemon = match PrivateDaemon::start(&executable, &paths, port).await {
                Ok(daemon) => daemon,
                Err(error) => {
                    let _ = readiness_tx.send(Err(format!("{error:#}")));
                    return;
                }
            };
            if readiness_tx.send(Ok(())).is_err() {
                let _ = daemon.stop().await;
                return;
            }
            let _ = stop_rx.await;
            let _ = completion_tx.send(daemon.stop().await);
        });
        readiness_rx
            .await
            .context("private daemon supervisor stopped before readiness")?
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            stop: Some(stop_tx),
            completion: Some(completion_rx),
        })
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let completion = self
            .completion
            .take()
            .ok_or_else(|| anyhow::anyhow!("private daemon supervisor was already stopped"))?;
        tokio::time::timeout(Duration::from_secs(25), completion)
            .await
            .context("private daemon supervisor cleanup timed out")?
            .context("private daemon supervisor exited without a cleanup receipt")?
    }
}

impl Drop for PrivateDaemonSupervisor {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_file(&self.control_socket);
    }
}

async fn remove_control_socket(path: &Path) -> Result<()> {
    if tokio::fs::try_exists(path).await? {
        tokio::fs::remove_file(path)
            .await
            .context("removing private daemon control socket")?;
    }
    Ok(())
}

#[cfg(windows)]
pub async fn run_workflow_command(_request: WorkflowRunRequest<'_>) -> Result<WorkflowExecution> {
    anyhow::bail!(
        "controlled workflow optimization is not supported on Windows until Job Object process-tree isolation is available"
    )
}

#[cfg(not(windows))]
pub async fn run_workflow_command(request: WorkflowRunRequest<'_>) -> Result<WorkflowExecution> {
    run_workflow_command_isolated(request).await
}

#[cfg(not(windows))]
async fn run_workflow_command_isolated(
    request: WorkflowRunRequest<'_>,
) -> Result<WorkflowExecution> {
    request.workflow.validate()?;
    if request.maximum_output_bytes == 0 {
        anyhow::bail!("maximum workflow output bytes must be positive");
    }
    let (program, arguments) = request
        .workflow
        .command
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("workflow command is empty"))?;
    let harness_overlay = workflow_harness_overlay(program, arguments, request.env)?;
    let started = Instant::now();
    let mut command = tokio::process::Command::new(program);
    for name in super::model_credential_environment_names() {
        command.env_remove(name);
    }
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .args(&harness_overlay.args)
        .args(arguments)
        .current_dir(request.cwd)
        .envs(request.env)
        .envs(harness_overlay.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("launching workflow program '{program}'"))?;
    let child_pid = child.id();
    let mut group_guard = WorkflowProcessGroupGuard::new(child_pid);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("workflow stdout pipe is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("workflow stderr pipe is unavailable"))?;
    let stdout_task = tokio::spawn(drain_bounded(stdout, request.maximum_output_bytes));
    let stderr_task = tokio::spawn(drain_bounded(stderr, request.maximum_output_bytes));
    let deadline = Duration::from_secs(request.workflow.timeout_secs);
    let (exit_code, timed_out) = match tokio::time::timeout(deadline, child.wait()).await {
        Ok(status) => {
            let exit_code = status.context("waiting for workflow program")?.code();
            if !wait_for_workflow_process_group_exit(child_pid, Duration::from_secs(2)).await? {
                terminate_workflow_tree(&mut child, child_pid)
                    .await
                    .context("terminating workflow background descendants")?;
                group_guard.disarm();
                anyhow::bail!("workflow exited while background descendants were still running");
            }
            group_guard.disarm();
            (exit_code, false)
        }
        Err(_) => {
            terminate_workflow_tree(&mut child, child_pid)
                .await
                .context("terminating timed-out workflow process tree")?;
            group_guard.disarm();
            (None, true)
        }
    };
    let mut stdout_task = stdout_task;
    let mut stderr_task = stderr_task;
    let streams = tokio::time::timeout(Duration::from_secs(5), async {
        let stdout = (&mut stdout_task)
            .await
            .context("joining workflow stdout reader")??;
        let stderr = (&mut stderr_task)
            .await
            .context("joining workflow stderr reader")??;
        Ok::<_, anyhow::Error>((stdout, stderr))
    })
    .await;
    let (stdout, stderr) = match streams {
        Ok(result) => result?,
        Err(_) => {
            let cleanup = terminate_workflow_tree(&mut child, child_pid).await;
            stdout_task.abort();
            stderr_task.abort();
            cleanup.context("terminating workflow descendants that retained output pipes")?;
            group_guard.disarm();
            anyhow::bail!("workflow process tree retained output pipes after its deadline");
        }
    };
    Ok(WorkflowExecution {
        exit_code,
        timed_out,
        elapsed: started.elapsed(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        launches: 1,
        cwd: request.cwd.to_string_lossy().into_owned(),
    })
}

#[cfg(not(windows))]
fn workflow_harness_overlay(
    program: &str,
    arguments: &[String],
    environment: &BTreeMap<String, String>,
) -> Result<crate::harness::RoutingOverlay> {
    let executable_name = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str());
    let harness = executable_name
        .and_then(crate::harness::by_interactive_binary)
        .or_else(|| crate::harness::match_invocation(program, arguments));
    let Some(harness) = harness else {
        return Ok(crate::harness::RoutingOverlay::default());
    };
    if !harness.env_args_routable() {
        anyhow::bail!(
            "workflow harness '{}' cannot be proven to route through the private daemon using an env/argv adapter",
            harness.id
        );
    }
    let base_url = environment.get("BITROUTER_BASE_URL").ok_or_else(|| {
        anyhow::anyhow!("workflow harness routing is missing the private daemon base URL")
    })?;
    let model = environment
        .get("BITROUTER_MODEL")
        .ok_or_else(|| anyhow::anyhow!("workflow harness routing is missing the private preset"))?;
    Ok(harness.routing_overlay(base_url, crate::harness::PLACEHOLDER_API_KEY, Some(model)))
}

#[cfg(not(windows))]
async fn terminate_workflow_tree(
    child: &mut tokio::process::Child,
    child_pid: Option<u32>,
) -> Result<()> {
    if let Some(pid) = child_pid {
        let group = format!("-{pid}");
        let status = tokio::process::Command::new("kill")
            .args(["-KILL", &group])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .context("launching workflow process-group kill")?;
        if !status.success() && workflow_process_group_has_live_members(child_pid).await? {
            anyhow::bail!("could not terminate workflow process group {pid}");
        }
    }
    if child
        .try_wait()
        .context("polling workflow process")?
        .is_none()
    {
        let _ = child.start_kill();
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .context("timed out reaping workflow process")?
            .context("reaping workflow process")?;
    }
    let gone = async {
        loop {
            if !workflow_process_group_has_live_members(child_pid).await? {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(2), gone)
        .await
        .context("workflow process group remained alive after termination")??;
    Ok(())
}

#[cfg(unix)]
struct WorkflowProcessGroupGuard {
    pid: Option<u32>,
    armed: bool,
}

#[cfg(unix)]
impl WorkflowProcessGroupGuard {
    fn new(pid: Option<u32>) -> Self {
        Self { pid, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for WorkflowProcessGroupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(pid) = self.pid {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &format!("-{pid}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[cfg(not(windows))]
async fn wait_for_workflow_process_group_exit(
    child_pid: Option<u32>,
    grace: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + grace;
    loop {
        if !workflow_process_group_has_live_members(child_pid).await? {
            return Ok(true);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        tokio::time::sleep((deadline - now).min(Duration::from_millis(25))).await;
    }
}

#[cfg(not(windows))]
async fn workflow_process_group_has_live_members(child_pid: Option<u32>) -> Result<bool> {
    let Some(pid) = child_pid else {
        return Ok(false);
    };
    let output = tokio::process::Command::new("ps")
        .args(["-A", "-o", "pgid=,stat="])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("listing workflow process group")?;
    if !output.status.success() {
        anyhow::bail!(
            "could not inspect workflow process group: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(process_listing_has_live_group(&output.stdout, pid))
}

#[cfg(not(windows))]
fn process_listing_has_live_group(output: &[u8], pid: u32) -> bool {
    // `kill -0` also succeeds for an unreaped zombie on Linux. A zombie cannot
    // issue requests or retain pipes, so residue means a non-zombie member of
    // the workflow's process group; the direct child is reaped separately.
    let target = pid.to_string();
    String::from_utf8_lossy(output).lines().any(|line| {
        let mut fields = line.split_whitespace();
        let Some(group) = fields.next() else {
            return false;
        };
        let Some(state) = fields.next() else {
            return false;
        };
        group == target && !state.starts_with('Z')
    })
}

pub fn workflow_environment(base_url: &str, preset: &str) -> Result<BTreeMap<String, String>> {
    let parsed = reqwest::Url::parse(base_url).context("parsing private daemon base URL")?;
    if parsed.scheme() != "http" || parsed.host_str() != Some("127.0.0.1") {
        anyhow::bail!("private daemon base URL must use loopback HTTP");
    }
    if preset.trim().is_empty() || preset.chars().any(char::is_control) {
        anyhow::bail!("workflow preset must be a non-empty bounded identifier");
    }
    let base = base_url.trim_end_matches('/');
    let v1 = format!("{base}/v1");
    // The `@preset` form, not the public `bitrouter/auto` slug: this wires the
    // optimizer's harness to the optimizer's own private daemon and is generic
    // over `preset`. The reserved namespace only names `auto`, so composing it
    // here would 400 for every other optimization lineage.
    let model = format!("@{preset}");
    Ok(BTreeMap::from([
        ("BITROUTER_BASE_URL".into(), base.into()),
        ("BITROUTER_API_BASE".into(), v1.clone()),
        ("BITROUTER_API_KEY".into(), "bitrouter-local".into()),
        ("BITROUTER_MODEL".into(), model.clone()),
        ("OPENAI_BASE_URL".into(), v1.clone()),
        ("OPENAI_API_BASE".into(), v1),
        ("OPENAI_API_KEY".into(), "bitrouter-local".into()),
        ("OPENAI_MODEL".into(), model.clone()),
        ("ANTHROPIC_BASE_URL".into(), base.into()),
        ("ANTHROPIC_AUTH_TOKEN".into(), "bitrouter-local".into()),
        ("ANTHROPIC_MODEL".into(), model.clone()),
        ("GOOGLE_GEMINI_BASE_URL".into(), base.into()),
        ("GEMINI_API_KEY".into(), "bitrouter-local".into()),
        ("GEMINI_MODEL".into(), model),
    ]))
}

pub fn collect_variant_evidence(
    variant: &str,
    expected_policy_digest: &str,
    execution: WorkflowExecution,
    decisions: &[crate::workflow_state::decision::PolicyDecisionRecord],
    subjects: &[crate::eval::types::EvalSubject],
    usage: &[crate::metering::MeteringUsageRecord],
) -> Result<VariantEvidence> {
    if !matches!(variant, "baseline" | "candidate") {
        anyhow::bail!("variant must be baseline or candidate");
    }
    if !usage.is_empty() && usage.iter().all(|record| record.error_code.is_some()) {
        anyhow::bail!(
            "{variant} produced no successful model request ({} failed); verify provider login and route health before optimizing",
            usage.len()
        );
    }
    let mut by_request = BTreeMap::new();
    for record in usage {
        let request_id = record
            .request_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("{variant} metering row has no request identity"))?;
        by_request.insert(request_id, record);
    }
    if by_request.len() != usage.len() {
        anyhow::bail!("{variant} metering contains duplicate request identities");
    }
    let by_subject = subjects
        .iter()
        .map(|subject| (subject.subject_id.as_str(), subject))
        .collect::<BTreeMap<_, _>>();
    if by_subject.len() != subjects.len() {
        anyhow::bail!("{variant} eval subjects contain duplicate request identities");
    }
    let mut observations = Vec::new();
    let mut attributions = Vec::new();
    let mut total_cost = 0_u64;
    let mut total_latency = 0_u64;
    for decision in decisions {
        let request_id = decision
            .request_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("{variant} decision has no request identity"))?;
        if decision.policy_digest.as_deref() != Some(expected_policy_digest) {
            anyhow::bail!("{variant} decision policy digest does not match the frozen policy");
        }
        let selected_tier = decision
            .selected_tier
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("{variant} decision has no selected tier"))?;
        let record = by_request.get(request_id).ok_or_else(|| {
            anyhow::anyhow!("{variant} decision {request_id} has no exact metering row")
        })?;
        let subject = by_subject.get(request_id).ok_or_else(|| {
            anyhow::anyhow!("{variant} decision {request_id} has no exact eval subject")
        })?;
        if subject.policy_digest != expected_policy_digest
            || subject.decisions.len() != 1
            || subject.decisions[0].decision_id
                != format!("{request_id}:{}", subject.decisions[0].policy)
            || decision.policy.as_deref() != Some(subject.decisions[0].policy.as_str())
            || subject.decisions[0].request_key != decision.request_key
            || decision.selected_tier.as_deref()
                != Some(subject.decisions[0].selected_tier.as_str())
            || decision.selected_effort != subject.decisions[0].selected_effort
            || decision.baseline_tier != subject.decisions[0].baseline_tier
            || decision.baseline_effort != subject.decisions[0].baseline_effort
            || subject.decisions[0].policy_digest != expected_policy_digest
        {
            anyhow::bail!("{variant} request {request_id} has an ambiguous decision join");
        }
        if record.usage_origin == bitrouter_sdk::language_model::UsageOrigin::Unknown
            || record.charge_status != crate::metering::ChargeStatus::Computed
        {
            anyhow::bail!(
                "{variant} request {request_id} lacks complete normalized showback evidence; configure provider pricing or add a frozen normalized_price_overrides entry"
            );
        }
        let charge_evidence = record.charge_evidence.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "{variant} request {request_id} has no normalized showback calculation evidence"
            )
        })?;
        let normalized_cost = record.final_charge_micro_usd.ok_or_else(|| {
            anyhow::anyhow!("{variant} request {request_id} has no normalized showback cost")
        })?;
        total_cost = total_cost
            .checked_add(normalized_cost)
            .ok_or_else(|| anyhow::anyhow!("{variant} normalized cost overflow"))?;
        let latency_ms = subject
            .evidence
            .iter()
            .flat_map(|item| item.attributes.get("request_duration_ms"))
            .next()
            .ok_or_else(|| {
                anyhow::anyhow!("{variant} request {request_id} has no latency evidence")
            })?
            .parse::<u64>()
            .context("parsing request latency evidence")?;
        total_latency = total_latency
            .checked_add(latency_ms)
            .ok_or_else(|| anyhow::anyhow!("{variant} latency overflow"))?;
        observations.push(RouteObservation {
            request_key: decision.request_key.clone(),
            selected_tier: selected_tier.to_string(),
            input_effort: decision.input_effort,
            selected_effort: decision.selected_effort,
            normalized_cost_micro_usd: Some(normalized_cost),
        });
        attributions.push(VariantAttribution {
            request_id: request_id.to_string(),
            decision: subject.decisions[0].clone(),
            usage_origin: record.usage_origin,
            pricing_source: charge_evidence.pricing_source,
            pricing_version: charge_evidence.pricing_version.clone(),
            normalized_cost_micro_usd: normalized_cost,
            latency_ms,
        });
    }
    if observations.is_empty() {
        anyhow::bail!("{variant} produced no metered named-policy decisions");
    }
    if observations.len() != usage.len() || observations.len() != subjects.len() {
        anyhow::bail!(
            "{variant} metering or eval subjects contain requests outside the exact policy decision set"
        );
    }
    let observed_latency_ms = total_latency
        .checked_div(u64::try_from(observations.len()).context("counting observations")?)
        .unwrap_or_default();
    Ok(VariantEvidence {
        variant: variant.into(),
        policy_digest: expected_policy_digest.into(),
        execution,
        request_count: observations.len(),
        normalized_cost_micro_usd: total_cost,
        observed_latency_ms,
        observations,
        attributions,
    })
}

#[cfg(not(windows))]
async fn drain_bounded(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    maximum_bytes: usize,
) -> Result<Vec<u8>> {
    let mut retained = Vec::with_capacity(maximum_bytes.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .context("reading workflow output")?;
        if read == 0 {
            break;
        }
        let remaining = maximum_bytes.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(retained)
}

#[derive(Default)]
struct ObservationSummary {
    count: u64,
    normalized_cost_micro_usd: u64,
    priced_count: u64,
    selected_tiers: std::collections::BTreeSet<String>,
    invalid_effort_treatment: bool,
}

pub fn select_target_request_key(
    active: &PolicyLock,
    policy_name: &str,
    strong_tier: &str,
    strong_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
    economy_tier: &str,
    preference: OptimizationPreference,
    observations: &[RouteObservation],
) -> Result<String> {
    validate_document(active)?;
    let policy = active
        .policies
        .get(policy_name)
        .ok_or_else(|| anyhow::anyhow!("optimization policy '{policy_name}' does not exist"))?;
    if !policy.tiers.contains_key(strong_tier) {
        anyhow::bail!(
            "optimization strong tier '{strong_tier}' is absent from policy '{policy_name}'"
        );
    }
    if !policy.tiers.contains_key(economy_tier) {
        anyhow::bail!(
            "optimization economy tier '{economy_tier}' is absent from policy '{policy_name}'"
        );
    }

    let mut summaries = BTreeMap::<String, ObservationSummary>::new();
    for observation in observations {
        let Some(projection) = RouteProjection::parse_key(&observation.request_key) else {
            continue;
        };
        if projection.state_kind == WorkflowStateKind::Opening && !policy.adequacy.explore_opening {
            continue;
        }
        let summary = summaries
            .entry(observation.request_key.clone())
            .or_default();
        summary.count = summary.count.saturating_add(1);
        summary
            .selected_tiers
            .insert(observation.selected_tier.clone());
        summary.invalid_effort_treatment |= match strong_effort {
            Some(effort) => observation.selected_effort != Some(effort),
            None => observation.selected_effort != observation.input_effort,
        };
        if let Some(cost) = observation.normalized_cost_micro_usd {
            summary.normalized_cost_micro_usd =
                summary.normalized_cost_micro_usd.saturating_add(cost);
            summary.priced_count = summary.priced_count.saturating_add(1);
        }
    }
    let mut eligible = summaries
        .into_iter()
        .filter(|(_, summary)| {
            summary.count > 0
                && summary.selected_tiers.len() == 1
                && summary.selected_tiers.contains(strong_tier)
                && !summary.invalid_effort_treatment
        })
        .filter(|(request_key, _)| {
            if active.is_compiled() {
                active
                    .certificate(policy_name, request_key)
                    .is_none_or(|certificate| certificate.owner == RouteOwner::Compiler)
            } else {
                // Without the legacy compiler ledger, every explicit v1
                // route is conservatively operator-owned. An implicit route
                // may still be added by the evidence compiler.
                !policy.routes.contains_key(request_key)
            }
        })
        .collect::<Vec<_>>();
    if eligible.is_empty() {
        anyhow::bail!(
            "baseline produced no compiler-owned strong route key to optimize; operator-owned routes remain unchanged"
        );
    }

    match preference {
        OptimizationPreference::QualityFirst => eligible.sort_by(|left, right| {
            left.1
                .count
                .cmp(&right.1.count)
                .then_with(|| {
                    left.1
                        .normalized_cost_micro_usd
                        .cmp(&right.1.normalized_cost_micro_usd)
                })
                .then_with(|| left.0.cmp(&right.0))
        }),
        OptimizationPreference::Balanced => eligible.sort_by(|left, right| {
            right
                .1
                .count
                .cmp(&left.1.count)
                .then_with(|| left.0.cmp(&right.0))
        }),
        OptimizationPreference::SavingsFirst => {
            eligible.retain(|(_, summary)| summary.priced_count > 0);
            eligible.sort_by(|left, right| {
                right
                    .1
                    .normalized_cost_micro_usd
                    .cmp(&left.1.normalized_cost_micro_usd)
                    .then_with(|| right.1.count.cmp(&left.1.count))
                    .then_with(|| left.0.cmp(&right.0))
            });
            if eligible.is_empty() {
                anyhow::bail!(
                    "savings-first optimization requires at least one normalized priced request"
                );
            }
        }
    }
    Ok(eligible.remove(0).0)
}

pub fn build_experiment_lock(
    active: &PolicyLock,
    expected_active_digest: &str,
    policy_name: &str,
    target_request_key: &str,
    economy_tier: &str,
) -> Result<PolicyLock> {
    validate_document(active)?;
    let actual_digest = semantic_digest(active)?;
    if actual_digest != expected_active_digest {
        anyhow::bail!("active policy changed before the controlled experiment was frozen");
    }
    if RouteProjection::parse_key(target_request_key).is_none() {
        anyhow::bail!("controlled experiment target is not a canonical route key");
    }
    if active
        .policies
        .values()
        .any(|policy| policy.progress_guard.is_some())
    {
        anyhow::bail!(
            "workflow optimization does not yet support active progress guards; removing one would violate the single-variable experiment contract"
        );
    }
    let mut experiment = active.clone();
    experiment.lockfile_version = POLICY_LOCKFILE_VERSION;
    let artifact = experiment
        .artifact
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("controlled experiment requires compiled policy lineage"))?;
    artifact.compiler.id = crate::policy_lock::OPTIMIZATION_EXPERIMENT_COMPILER_ID.to_owned();
    artifact.parent_digest = Some(expected_active_digest.to_owned());
    let compiler_config_digest = artifact.compiler.config_digest.clone();
    let baseline_tier = {
        let policy = experiment
            .policies
            .get_mut(policy_name)
            .ok_or_else(|| anyhow::anyhow!("optimization policy '{policy_name}' does not exist"))?;
        if !policy.tiers.contains_key(economy_tier) {
            anyhow::bail!("controlled experiment economy tier '{economy_tier}' does not exist");
        }
        if policy.routes.get(target_request_key).map(String::as_str) == Some(economy_tier) {
            anyhow::bail!("controlled experiment target already routes to the economy tier");
        }
        let baseline_tier = policy
            .routes
            .get(target_request_key)
            .cloned()
            .or_else(|| policy.default_tier.clone());
        policy
            .routes
            .insert(target_request_key.to_string(), economy_tier.to_string());
        baseline_tier
    };
    use sha2::Digest;
    let evidence_digest = format!(
        "sha256:{}",
        hex::encode(sha2::Sha256::digest(format!(
            "bitrouter.optimization.experiment.v1\0{expected_active_digest}\0{policy_name}\0{target_request_key}\0{economy_tier}"
        )))
    );
    experiment
        .certificates
        .entry(policy_name.to_owned())
        .or_default()
        .insert(
            target_request_key.to_owned(),
            PolicyCertificate {
                owner: RouteOwner::Operator,
                selected_tier: economy_tier.to_owned(),
                baseline_tier,
                source: CertificateSource::Operator,
                eligible_episodes: 0,
                independent_tasks: 0,
                quality: None,
                economics: None,
                latency: None,
                critical_violations: 0,
                verdict: PromotionVerdict::Experiment,
                evaluator_config_digest: None,
                compiler_config_digest,
                evidence_digest,
                legacy: None,
            },
        );
    validate_document(&experiment)?;
    Ok(experiment)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use bitrouter_sdk::config::AdequacyConfig;
    use bitrouter_sdk::language_model::types::ReasoningEffort;

    #[cfg(not(windows))]
    use super::workflow_harness_overlay;
    use super::{
        PrivateDaemonPaths, RouteObservation, WorkflowExecution, build_experiment_lock,
        collect_variant_evidence, private_daemon_config, select_target_request_key,
        workflow_environment,
    };
    #[cfg(not(windows))]
    use super::{WorkflowRunRequest, run_workflow_command};
    use crate::eval::types::{
        EVAL_SCHEMA_VERSION, EvalDecisionRef, EvalScope, EvalSubject, EvidenceItem, evidence_digest,
    };
    use crate::metering::{
        ChargeStatus, MeteringUsageRecord, ModelPricing, PricingSource, ReconciliationStatus,
        calculate_charge_evidence,
    };
    use crate::optimization::{
        EvaluatorRoute, OptimizationIntent, OptimizationPreference, ResolvedEvaluator,
        WorkflowCommand,
    };
    use crate::policy_lock::{
        PolicyDefinition, PolicyLock, PromotionVerdict, RouteOwner, deterministic_yaml,
        publish_candidate, semantic_digest, validate_document,
    };

    fn active_lock() -> PolicyLock {
        let mut lock = PolicyLock::default();
        lock.policies.insert(
            "auto".into(),
            PolicyDefinition {
                tiers: BTreeMap::from([
                    ("strong".into(), "bitrouter:openai/gpt-5.6".into()),
                    (
                        "economy".into(),
                        "bitrouter:deepseek/deepseek-v4-flash-0731".into(),
                    ),
                ]),
                routes: BTreeMap::new(),
                default_tier: Some("strong".into()),
                tool_use_tier: Some("strong".into()),
                tool_safe_tiers: vec!["strong".into(), "economy".into()],
                adequacy: AdequacyConfig {
                    explore_tier: Some("economy".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        lock
    }

    fn observations() -> Vec<RouteObservation> {
        vec![
            RouteObservation {
                request_key: "agent_trace/v2|edit|normal".into(),
                selected_tier: "strong".into(),
                input_effort: None,
                selected_effort: None,
                normalized_cost_micro_usd: Some(900),
            },
            RouteObservation {
                request_key: "agent_trace/v2|edit|normal".into(),
                selected_tier: "strong".into(),
                input_effort: None,
                selected_effort: None,
                normalized_cost_micro_usd: Some(800),
            },
            RouteObservation {
                request_key: "agent_trace/v2|test|normal".into(),
                selected_tier: "strong".into(),
                input_effort: None,
                selected_effort: None,
                normalized_cost_micro_usd: Some(4_000),
            },
        ]
    }

    fn digest(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    fn decision(policy_digest: &str) -> crate::workflow_state::decision::PolicyDecisionRecord {
        crate::workflow_state::decision::PolicyDecisionRecord {
            captured_at: None,
            request_id: Some("req-1".into()),
            ingress_request_id_sha256: None,
            input_model: "@auto".into(),
            input_effort: None,
            key_strategy: "agent_trace".into(),
            request_key: "agent_trace/v2|edit|normal".into(),
            ledger_key: None,
            policy: Some("auto".into()),
            policy_digest: Some(policy_digest.into()),
            preset_variant: Some("auto".into()),
            baseline_tier: Some("strong".into()),
            baseline_effort: None,
            legacy_fingerprint: "legacy".into(),
            workflow_state: "edit".into(),
            workflow_identity: Default::default(),
            static_tier: Some("strong".into()),
            static_model: Some("bitrouter:openai/gpt-5.6".into()),
            static_effort: None,
            selected_tier: Some("strong".into()),
            selected_model: Some("bitrouter:openai/gpt-5.6".into()),
            selected_effort: None,
            continuation_proposed_tier: None,
            continuation_proposed_model: None,
            continuation_proposed_effort: None,
            continuation_adjustment: None,
            predicted_role: None,
            predicted_action: None,
            prediction_confidence_ppm: None,
            predictor_contract_digest: None,
            prediction_confidence_kind: None,
            prediction_reason_codes: Vec::new(),
            observed_route_projection: None,
            trajectory_episode_id: None,
            trajectory_sequence: None,
            trajectory_completeness: None,
            trajectory_health_digest: None,
            candidate_tier: None,
            progress_clause_ids: Vec::new(),
            reason: "static_table".into(),
            pinned: false,
            request_qualified: true,
            semantic_successes: 0,
            semantic_success_threshold: 0,
            locked: false,
            trialed: false,
        }
    }

    fn subject(policy_digest: &str) -> anyhow::Result<EvalSubject> {
        let evidence = vec![EvidenceItem {
            evidence_id: "request-outcome".into(),
            kind: "request.outcome".into(),
            digest: digest('b'),
            redacted: true,
            attributes: BTreeMap::from([("request_duration_ms".into(), "250".into())]),
        }];
        Ok(EvalSubject {
            schema_version: EVAL_SCHEMA_VERSION,
            eval_id: "request:req-1".into(),
            scope: EvalScope::Request,
            subject_id: "req-1".into(),
            policy_digest: policy_digest.into(),
            preset: Some("auto".into()),
            cohort: None,
            holdout: false,
            decisions: vec![EvalDecisionRef {
                decision_id: "req-1:auto".into(),
                policy: "auto".into(),
                request_key: "agent_trace/v2|edit|normal".into(),
                selected_tier: "strong".into(),
                selected_effort: None,
                baseline_tier: Some("strong".into()),
                baseline_effort: None,
                policy_digest: policy_digest.into(),
                experiment: None,
            }],
            requested_dimensions: Default::default(),
            evidence_digest: evidence_digest(&evidence)?,
            evidence,
            observed_at: "2026-08-08T00:00:00Z".into(),
        })
    }

    fn normalized_usage() -> MeteringUsageRecord {
        let usage = bitrouter_sdk::language_model::Usage {
            prompt_tokens: 20,
            completion_tokens: 10,
            origin: bitrouter_sdk::language_model::UsageOrigin::ProviderReported,
            ..Default::default()
        };
        let charge_evidence = calculate_charge_evidence(
            &usage,
            &ModelPricing::new(5.0, 30.0),
            PricingSource::Override,
        );
        MeteringUsageRecord {
            request_id: Some("req-1".into()),
            provider_id: "openai-codex".into(),
            model_id: "gpt-5.6-sol".into(),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            usage_origin: usage.origin,
            final_charge_micro_usd: Some(400),
            charge_status: ChargeStatus::Computed,
            charge_evidence: Some(charge_evidence),
            reconciliation_status: ReconciliationStatus::NotApplicable,
            ..Default::default()
        }
    }

    #[test]
    fn qualitative_profiles_choose_deterministic_observed_keys() -> anyhow::Result<()> {
        let active = active_lock();
        let observations = observations();
        assert_eq!(
            select_target_request_key(
                &active,
                "auto",
                "strong",
                None,
                "economy",
                OptimizationPreference::QualityFirst,
                &observations,
            )?,
            "agent_trace/v2|test|normal"
        );
        assert_eq!(
            select_target_request_key(
                &active,
                "auto",
                "strong",
                None,
                "economy",
                OptimizationPreference::Balanced,
                &observations,
            )?,
            "agent_trace/v2|edit|normal"
        );
        assert_eq!(
            select_target_request_key(
                &active,
                "auto",
                "strong",
                None,
                "economy",
                OptimizationPreference::SavingsFirst,
                &observations,
            )?,
            "agent_trace/v2|test|normal"
        );
        Ok(())
    }

    #[test]
    fn scalar_baseline_accepts_and_preserves_caller_owned_effort() -> anyhow::Result<()> {
        let active = active_lock();
        let mut observations = observations();
        for observation in &mut observations {
            observation.input_effort = Some(ReasoningEffort::High);
            observation.selected_effort = Some(ReasoningEffort::High);
        }

        assert_eq!(
            select_target_request_key(
                &active,
                "auto",
                "strong",
                None,
                "economy",
                OptimizationPreference::Balanced,
                &observations,
            )?,
            "agent_trace/v2|edit|normal"
        );

        observations[0].selected_effort = Some(ReasoningEffort::Low);
        assert_eq!(
            select_target_request_key(
                &active,
                "auto",
                "strong",
                None,
                "economy",
                OptimizationPreference::Balanced,
                &observations,
            )?,
            "agent_trace/v2|test|normal"
        );
        for observation in &mut observations {
            observation.selected_effort = Some(ReasoningEffort::Low);
        }
        assert!(
            select_target_request_key(
                &active,
                "auto",
                "strong",
                None,
                "economy",
                OptimizationPreference::Balanced,
                &observations,
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn experiment_lock_preserves_compiled_lineage_and_changes_exactly_one_route()
    -> anyhow::Result<()> {
        let active = active_lock();
        let active_digest = semantic_digest(&active)?;
        let experiment = build_experiment_lock(
            &active,
            &active_digest,
            "auto",
            "agent_trace/v2|edit|normal",
            "economy",
        )?;

        validate_document(&experiment)?;
        assert_eq!(
            experiment.lockfile_version,
            crate::policy_lock::POLICY_LOCKFILE_VERSION
        );
        assert_eq!(
            experiment
                .artifact
                .as_ref()
                .and_then(|artifact| artifact.parent_digest.as_deref()),
            Some(active_digest.as_str())
        );
        let certificate = &experiment.certificates["auto"]["agent_trace/v2|edit|normal"];
        assert_eq!(certificate.owner, RouteOwner::Operator);
        assert_eq!(certificate.verdict, PromotionVerdict::Experiment);
        assert_eq!(
            experiment.policies["auto"].routes["agent_trace/v2|edit|normal"],
            "economy"
        );
        assert!(
            !experiment.policies["auto"]
                .routes
                .contains_key("agent_trace/v2|test|normal")
        );
        assert_ne!(semantic_digest(&experiment)?, active_digest);
        let dir = tempfile::tempdir()?;
        let active_path = dir.path().join("policy-lock.yaml");
        std::fs::write(&active_path, deterministic_yaml(&active)?)?;
        let publish_error = publish_candidate(
            &active_path,
            &active_digest,
            &experiment,
            &dir.path().join("history"),
        )
        .err()
        .ok_or_else(|| anyhow::anyhow!("private experiment lock was publishable"))?;
        assert!(
            publish_error
                .to_string()
                .contains("private optimization experiment")
        );
        Ok(())
    }

    #[test]
    fn selection_rejects_unobserved_or_already_economy_routes() {
        let active = active_lock();
        let observations = vec![RouteObservation {
            request_key: "agent_trace/v2|edit|normal".into(),
            selected_tier: "economy".into(),
            input_effort: None,
            selected_effort: None,
            normalized_cost_micro_usd: Some(10),
        }];
        assert!(
            select_target_request_key(
                &active,
                "auto",
                "strong",
                None,
                "economy",
                OptimizationPreference::Balanced,
                &observations,
            )
            .is_err()
        );
    }

    #[test]
    fn selection_skips_operator_owned_v1_routes() -> anyhow::Result<()> {
        let mut active = active_lock();
        active.lockfile_version = crate::policy_lock::LEGACY_POLICY_LOCKFILE_VERSION;
        active.artifact = None;
        active.certificates.clear();
        active
            .policies
            .get_mut("auto")
            .ok_or_else(|| anyhow::anyhow!("auto policy is unavailable"))?
            .routes
            .insert("agent_trace/v2|edit|normal".into(), "strong".into());
        assert_eq!(
            select_target_request_key(
                &active,
                "auto",
                "strong",
                None,
                "economy",
                OptimizationPreference::Balanced,
                &observations(),
            )?,
            "agent_trace/v2|test|normal"
        );
        Ok(())
    }

    /// Timeouts here are sized so the test measures the runner, not the
    /// machine.
    ///
    /// A **success**-path deadline exists only to bound a hang, so it is set
    /// far above any plausible scheduling delay: if a `printf` cannot finish
    /// inside [`GENEROUS_TIMEOUT_SECS`], the host is broken and a red test is
    /// the correct outcome. Sizing these to "about how long it should take"
    /// is what makes a suite flaky under parallel load — the assertion then
    /// reports CPU contention rather than a defect.
    #[cfg(not(windows))]
    const GENEROUS_TIMEOUT_SECS: u64 = 60;

    /// The **timeout** path is the one place a short deadline is the subject
    /// of the test, so it stays short — and the child instead sleeps far
    /// longer than the runner should ever let it, so "we cut it short" is
    /// unmistakable rather than a narrow tolerance.
    #[cfg(not(windows))]
    const TIMEOUT_UNDER_TEST_SECS: u64 = 1;

    /// How long the timed-out child would sleep if nothing stopped it.
    #[cfg(not(windows))]
    const UNINTERRUPTED_CHILD_SECS: u64 = 120;

    /// Upper bound on the timed-out run. Legitimate teardown after the
    /// deadline is bounded by `terminate_workflow_tree` plus the 5s stream
    /// drain in `run_workflow_command`, so ~7s is the honest worst case; this
    /// sits well above that and *far* below [`UNINTERRUPTED_CHILD_SECS`], which
    /// is what makes it a real assertion instead of a stopwatch.
    #[cfg(not(windows))]
    const TEARDOWN_CEILING_SECS: u64 = 30;

    #[cfg(not(windows))]
    #[tokio::test]
    async fn workflow_runner_uses_exact_argv_env_and_timeout_without_retry() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        #[cfg(unix)]
        let success_command = vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '%s' \"$BITROUTER_MODEL\"".into(),
        ];
        #[cfg(windows)]
        let success_command = vec![
            "powershell.exe".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            "[Console]::Out.Write($env:BITROUTER_MODEL)".into(),
        ];
        let success = run_workflow_command(WorkflowRunRequest {
            workflow: &WorkflowCommand {
                command: success_command,
                inputs: Vec::new(),
                timeout_secs: GENEROUS_TIMEOUT_SECS,
            },
            cwd: dir.path(),
            env: &BTreeMap::from([("BITROUTER_MODEL".into(), "@auto".into())]),
            maximum_output_bytes: 1024,
        })
        .await?;
        assert_eq!(success.exit_code, Some(0));
        assert!(!success.timed_out);
        assert_eq!(success.stdout, "@auto");

        #[cfg(unix)]
        let timeout_command = vec!["/bin/sleep".into(), UNINTERRUPTED_CHILD_SECS.to_string()];
        #[cfg(windows)]
        let timeout_command = vec![
            "powershell.exe".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            format!("Start-Sleep -Seconds {UNINTERRUPTED_CHILD_SECS}"),
        ];
        let timeout = run_workflow_command(WorkflowRunRequest {
            workflow: &WorkflowCommand {
                command: timeout_command,
                inputs: Vec::new(),
                timeout_secs: TIMEOUT_UNDER_TEST_SECS,
            },
            cwd: dir.path(),
            env: &BTreeMap::new(),
            maximum_output_bytes: 1024,
        })
        .await?;
        assert!(timeout.timed_out);
        assert_eq!(timeout.exit_code, None);
        // The deadline cannot fire early, so this only rules out reporting a
        // timeout without having waited for one.
        assert!(timeout.elapsed >= Duration::from_millis(900));
        // The property under test: the child was cut short rather than waited
        // out. It would have slept for `UNINTERRUPTED_CHILD_SECS`.
        assert!(
            timeout.elapsed < Duration::from_secs(TEARDOWN_CEILING_SECS),
            "timed-out run took {:?}, which is teardown gone wrong rather than a deadline",
            timeout.elapsed
        );
        // The point of the test: a timeout must not be retried behind the
        // caller's back.
        assert_eq!(timeout.launches, 1);
        assert_eq!(PathBuf::from(timeout.cwd), dir.path());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let fake_codex = dir.path().join("codex");
            tokio::fs::write(&fake_codex, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").await?;
            std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o700))?;
            let routed_environment = workflow_environment("http://127.0.0.1:43123", "auto")?;
            let routed = run_workflow_command(WorkflowRunRequest {
                workflow: &WorkflowCommand {
                    command: vec![
                        fake_codex.display().to_string(),
                        "exec".into(),
                        "run the eval".into(),
                    ],
                    inputs: Vec::new(),
                    timeout_secs: GENEROUS_TIMEOUT_SECS,
                },
                cwd: dir.path(),
                env: &routed_environment,
                maximum_output_bytes: 8 * 1024,
            })
            .await?;
            assert_eq!(routed.exit_code, Some(0));
            assert!(!routed.timed_out);
            assert!(
                routed.stdout.contains("model_provider=\"bitrouter\""),
                "routed argv:\n{}",
                routed.stdout
            );
            assert!(
                routed.stdout.contains("model=\"@auto\""),
                "routed argv:\n{}",
                routed.stdout
            );
            assert!(
                routed.stdout.contains("exec\nrun the eval"),
                "routed argv:\n{}",
                routed.stdout
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workflow_runner_allows_short_lived_descendants_to_drain() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let execution = run_workflow_command(WorkflowRunRequest {
            workflow: &WorkflowCommand {
                command: vec!["/bin/sh".into(), "-c".into(), "(sleep 0.05) &".into()],
                inputs: Vec::new(),
                timeout_secs: GENEROUS_TIMEOUT_SECS,
            },
            cwd: dir.path(),
            env: &BTreeMap::new(),
            maximum_output_bytes: 1024,
        })
        .await?;

        assert_eq!(execution.exit_code, Some(0));
        assert!(!execution.timed_out);
        assert!(execution.elapsed >= Duration::from_millis(40));
        Ok(())
    }

    #[cfg(not(windows))]
    #[test]
    fn process_group_listing_ignores_zombies_but_not_live_descendants() {
        assert!(!super::process_listing_has_live_group(
            b" 4312 Z\n 9000 Ss\n",
            4312,
        ));
        assert!(super::process_listing_has_live_group(
            b" 4312 Z\n 4312 S\n",
            4312,
        ));
    }

    #[test]
    fn workflow_environment_routes_common_clients_through_the_private_preset() -> anyhow::Result<()>
    {
        let env = workflow_environment("http://127.0.0.1:43123", "auto")?;
        assert_eq!(env["OPENAI_BASE_URL"], "http://127.0.0.1:43123/v1");
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:43123");
        assert_eq!(env["BITROUTER_MODEL"], "@auto");
        assert!(workflow_environment("https://api.example.com", "auto").is_err());
        Ok(())
    }

    #[cfg(not(windows))]
    #[test]
    fn codex_workflow_gets_the_explicit_private_daemon_provider_adapter() -> anyhow::Result<()> {
        let environment = workflow_environment("http://127.0.0.1:43123", "auto")?;
        let overlay = workflow_harness_overlay(
            "/usr/local/bin/codex",
            &["exec".into(), "run the eval".into()],
            &environment,
        )?;

        assert!(
            overlay
                .args
                .contains(&"model_provider=\"bitrouter\"".to_string())
        );
        assert!(overlay.args.contains(
            &"model_providers.bitrouter.base_url=\"http://127.0.0.1:43123/v1\"".to_string()
        ));
        assert!(overlay.args.contains(&"model=\"@auto\"".to_string()));
        assert!(
            overlay
                .args
                .iter()
                .all(|argument| !argument.contains("api.bitrouter.ai"))
        );
        Ok(())
    }

    #[test]
    fn evidence_requires_exact_decision_subject_and_normalized_showback() -> anyhow::Result<()> {
        let policy_digest = digest('a');
        let execution = WorkflowExecution {
            exit_code: Some(0),
            timed_out: false,
            elapsed: Duration::from_millis(500),
            stdout: "passed".into(),
            stderr: String::new(),
            launches: 1,
            cwd: "/tmp/project".into(),
        };
        let evidence = collect_variant_evidence(
            "baseline",
            &policy_digest,
            execution.clone(),
            &[decision(&policy_digest)],
            &[subject(&policy_digest)?],
            &[normalized_usage()],
        )?;
        assert_eq!(evidence.normalized_cost_micro_usd, 400);
        assert_eq!(evidence.observed_latency_ms, 250);
        assert_eq!(evidence.request_count, 1);

        let mut failed_execution = execution.clone();
        failed_execution.exit_code = Some(2);
        let failed = collect_variant_evidence(
            "baseline",
            &policy_digest,
            failed_execution,
            &[decision(&policy_digest)],
            &[subject(&policy_digest)?],
            &[normalized_usage()],
        )?;
        assert_eq!(failed.execution.exit_code, Some(2));

        let mut cloud_pending = normalized_usage();
        cloud_pending.provider_id = "bitrouter".into();
        cloud_pending.model_id = "deepseek/deepseek-v4-flash-0731".into();
        cloud_pending.reconciliation_status = ReconciliationStatus::Pending;
        let cloud_evidence = collect_variant_evidence(
            "baseline",
            &policy_digest,
            execution.clone(),
            &[decision(&policy_digest)],
            &[subject(&policy_digest)?],
            &[cloud_pending],
        )?;
        assert_eq!(cloud_evidence.normalized_cost_micro_usd, 400);

        let mut missing_price = normalized_usage();
        missing_price.charge_status = ChargeStatus::Unknown;
        missing_price.charge_evidence = None;
        missing_price.final_charge_micro_usd = None;
        assert!(
            collect_variant_evidence(
                "baseline",
                &policy_digest,
                execution,
                &[decision(&policy_digest)],
                &[subject(&policy_digest)?],
                &[missing_price.clone()],
            )
            .is_err()
        );

        let mut failed_request = missing_price;
        failed_request.error_code = Some("upstream_bad_gateway".into());
        let error = collect_variant_evidence(
            "baseline",
            &policy_digest,
            WorkflowExecution {
                exit_code: Some(1),
                timed_out: false,
                elapsed: Duration::from_millis(500),
                stdout: String::new(),
                stderr: String::new(),
                launches: 1,
                cwd: "/tmp/project".into(),
            },
            &[decision(&policy_digest)],
            &[subject(&policy_digest)?],
            &[failed_request],
        )
        .err()
        .ok_or_else(|| anyhow::anyhow!("all-failed evidence unexpectedly succeeded"))?;
        assert!(error.to_string().contains("no successful model request"));
        assert!(!error.to_string().contains("normalized showback"));
        Ok(())
    }

    #[tokio::test]
    async fn private_daemon_config_preserves_subscription_and_cloud_routes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let paths = PrivateDaemonPaths::new(dir.path().join("baseline"));
        #[cfg(unix)]
        {
            assert!(paths.control_socket.starts_with("/tmp"));
            assert!(paths.control_socket.as_os_str().len() < 100);
        }
        let intent = OptimizationIntent {
            version: 1,
            workflow: WorkflowCommand {
                command: vec!["workflow".into()],
                inputs: Vec::new(),
                timeout_secs: 60,
            },
            contract: PathBuf::from("bitrouter.eval.md"),
            source_config: PathBuf::from("bitrouter.yaml"),
            policy: "auto".into(),
            preset: "auto".into(),
            strong: "openai-codex:gpt-5.6-sol".into(),
            strong_effort: None,
            economy: "bitrouter:deepseek/deepseek-v4-flash-0731".into(),
            economy_effort: None,
            normalized_price_overrides: vec!["openai-codex:gpt-5.6-sol=5,0.5,6.25,30".into()],
            preference: OptimizationPreference::Balanced,
            evaluator: ResolvedEvaluator {
                agent: "codex-acp".into(),
                model: "bitrouter:openai/gpt-5.6".into(),
                route: EvaluatorRoute::Cloud,
            },
        };
        let source = r#"
providers:
  openai-codex: {}
  bitrouter: {}
inherit_defaults: true
registry:
  enabled: true
presets:
  auto:
    system_prompt: preserve-source-preset
"#;
        let yaml = private_daemon_config(&paths, &intent, source, 43123)?;
        assert!(!yaml.contains("brk_"));
        assert!(yaml.contains("127.0.0.1:43123"));
        assert!(yaml.contains("openai-codex"));
        assert!(yaml.contains("bitrouter"));
        assert!(yaml.contains("preserve-source-preset"));

        tokio::fs::create_dir_all(&paths.root).await?;
        tokio::fs::write(&paths.config, yaml).await?;
        tokio::fs::write(&paths.policy, serde_saphyr::to_string(&active_lock())?).await?;
        let parsed = bitrouter_sdk::config::load(&paths.config).await?;
        assert_eq!(parsed.server.listen, "127.0.0.1:43123");
        assert_eq!(parsed.database.url, paths.database_url());
        assert!(parsed.server.skip_auth);
        assert_eq!(parsed.policy.path.as_deref(), Some(paths.policy.as_path()));
        assert_eq!(parsed.presets["auto"].policy.as_deref(), Some("auto"));
        assert_eq!(
            parsed.presets["auto"].model.as_deref(),
            Some("openai-codex:gpt-5.6-sol")
        );
        assert!(parsed.providers.contains_key("bitrouter"));
        assert!(parsed.providers.contains_key("openai-codex"));
        Ok(())
    }
}
