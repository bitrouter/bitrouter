use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::optimization::OptimizationPreference;
use crate::policy_lock::{
    LEGACY_POLICY_LOCKFILE_VERSION, PolicyLock, semantic_digest, validate_document,
};
use crate::workflow_state::ir::{RouteProjection, WorkflowStateKind};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteObservation {
    pub request_key: String,
    pub selected_tier: String,
    pub settled_cost_micro_usd: Option<u64>,
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
    pub settled_cost_micro_usd: u64,
    pub observed_latency_ms: u64,
    pub observations: Vec<RouteObservation>,
    pub attributions: Vec<VariantAttribution>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantAttribution {
    pub request_id: String,
    pub decision: crate::eval::types::EvalDecisionRef,
    pub settled_cost_micro_usd: u64,
    pub latency_ms: u64,
}

pub struct PrivateVariantRequest<'a> {
    pub variant: &'a str,
    pub paths: &'a PrivateDaemonPaths,
    pub intent: &'a super::OptimizationIntent,
    pub policy: &'a PolicyLock,
    pub policy_digest: &'a str,
    pub workflow_cwd: &'a Path,
    pub bitrouter_executable: &'a Path,
    pub settlement_bearer: &'a str,
    pub maximum_output_bytes: usize,
}

struct PrivateDaemon {
    child: tokio::process::Child,
    control_socket: PathBuf,
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
    port: u16,
) -> Result<String> {
    intent.validate()?;
    if !intent.strong.starts_with("bitrouter:") || !intent.economy.starts_with("bitrouter:") {
        anyhow::bail!("this optimization run requires Cloud-backed strong and economy models");
    }
    let document = serde_json::json!({
        "server": {
            "listen": format!("127.0.0.1:{port}"),
            "control_socket": paths.control_socket,
            "log_level": "warn",
            "skip_auth": true
        },
        "database": {
            "url": paths.database_url()
        },
        "providers": {
            "bitrouter": {
                "auto_discover": true
            }
        },
        "inherit_defaults": true,
        "policy": {
            "path": paths.policy,
            "mode": "frozen"
        },
        "trajectory": {
            "enabled": false
        },
        "presets": {
            intent.preset.clone(): {
                "model": intent.strong,
                "policy": intent.policy
            }
        }
    });
    let mut rendered =
        serde_saphyr::to_string(&document).context("serializing private daemon config")?;
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(rendered)
}

pub async fn run_private_variant(request: PrivateVariantRequest<'_>) -> Result<VariantEvidence> {
    if request.settlement_bearer.trim().is_empty() {
        anyhow::bail!("BitRouter Cloud API key is required for authoritative settlement");
    }
    request.intent.validate()?;
    validate_document(request.policy)?;
    if semantic_digest(request.policy)? != request.policy_digest {
        anyhow::bail!("private variant policy digest does not match its frozen document");
    }
    tokio::fs::create_dir(&request.paths.root)
        .await
        .with_context(|| {
            format!(
                "creating private run directory {}",
                request.paths.root.display()
            )
        })?;
    let port = reserve_loopback_port()?;
    let config = private_daemon_config(request.paths, request.intent, port)?;
    tokio::fs::write(&request.paths.config, config)
        .await
        .with_context(|| format!("writing private config {}", request.paths.config.display()))?;
    tokio::fs::write(
        &request.paths.policy,
        crate::policy_lock::deterministic_yaml(request.policy)?,
    )
    .await
    .with_context(|| format!("writing private policy {}", request.paths.policy.display()))?;

    let mut daemon = PrivateDaemon::start(
        request.bitrouter_executable,
        request.paths,
        port,
        request.settlement_bearer,
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
    let execution = match (run_result, cleanup_result) {
        (Ok(execution), Ok(())) => execution,
        (Err(error), _) => return Err(error),
        (Ok(_), Err(error)) => return Err(error),
    };
    tokio::fs::write(
        &request.paths.workflow_evidence,
        serde_json::to_vec_pretty(&execution).context("serializing private workflow evidence")?,
    )
    .await
    .context("writing private workflow evidence")?;

    let db = crate::db::connect(&request.paths.database_url())
        .await
        .map_err(anyhow::Error::from)
        .context("opening private optimization database")?;
    let metering = crate::metering::MeteringStore::new(db.clone());
    let initial_usage = metering
        .export_usage(crate::metering::TimeWindow::ThisMonth)
        .await
        .map_err(anyhow::Error::from)?;
    let request_ids = initial_usage
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
    if initial_usage.iter().any(|record| {
        record.reconciliation_status != crate::metering::ReconciliationStatus::Pending
    }) {
        anyhow::bail!("private workflow emitted a request outside Cloud settlement authority");
    }
    let settlement = bitrouter_cloud_sdk::settlement::SettlementClient::new(
        format!(
            "{}/v1",
            bitrouter_cloud_sdk::auth::settings::DEFAULT_AS.trim_end_matches('/')
        ),
        request.settlement_bearer,
    )
    .context("building Cloud settlement client")?;
    let summary = crate::metering::reconcile_authoritative_requests(
        &metering,
        &settlement,
        &request_ids,
        8,
        Duration::from_millis(500),
    )
    .await
    .map_err(anyhow::Error::from)
    .context("reconciling exact private workflow requests")?;
    if !summary.accepted() {
        anyhow::bail!("authoritative settlement was not conclusive for every workflow request");
    }
    let usage = metering
        .export_usage(crate::metering::TimeWindow::ThisMonth)
        .await
        .map_err(anyhow::Error::from)?;
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
    async fn start(
        executable: &Path,
        paths: &PrivateDaemonPaths,
        port: u16,
        cloud_api_key: &str,
    ) -> Result<Self> {
        let log = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&paths.log)
            .with_context(|| format!("creating private daemon log {}", paths.log.display()))?;
        let stderr = log.try_clone().context("cloning private daemon log")?;
        let mut child = tokio::process::Command::new(executable)
            .arg("serve")
            .arg("--config")
            .arg(&paths.config)
            .env(
                crate::workflow_state::decision::POLICY_DECISION_JSONL_ENV,
                &paths.decisions,
            )
            .env(crate::harness::BITROUTER_API_KEY_ENV, cloud_api_key)
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
        let response =
            crate::daemon::send_command(&self.control_socket, &crate::daemon::DaemonCommand::Stop)
                .await
                .context("stopping private daemon")?;
        if !matches!(response, crate::daemon::DaemonResponse::Ok) {
            anyhow::bail!("private daemon rejected its stop command");
        }
        match tokio::time::timeout(Duration::from_secs(15), self.child.wait()).await {
            Ok(status) => {
                let status = status.context("waiting for private daemon")?;
                if !status.success() {
                    anyhow::bail!("private daemon exited unsuccessfully: {status}");
                }
            }
            Err(_) => {
                self.child
                    .start_kill()
                    .context("terminating stuck private daemon")?;
                self.child
                    .wait()
                    .await
                    .context("reaping stuck private daemon")?;
                anyhow::bail!("private daemon did not stop within its cleanup deadline");
            }
        }
        remove_control_socket(&self.control_socket).await?;
        Ok(())
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

pub async fn run_workflow_command(request: WorkflowRunRequest<'_>) -> Result<WorkflowExecution> {
    request.workflow.validate()?;
    if request.maximum_output_bytes == 0 {
        anyhow::bail!("maximum workflow output bytes must be positive");
    }
    let (program, arguments) = request
        .workflow
        .command
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("workflow command is empty"))?;
    let started = Instant::now();
    let mut child = tokio::process::Command::new(program)
        .args(arguments)
        .current_dir(request.cwd)
        .envs(request.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("launching workflow program '{program}'"))?;
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
        Ok(status) => (
            status.context("waiting for workflow program")?.code(),
            false,
        ),
        Err(_) => {
            child
                .start_kill()
                .context("terminating timed-out workflow program")?;
            child
                .wait()
                .await
                .context("reaping timed-out workflow program")?;
            (None, true)
        }
    };
    let stdout = stdout_task
        .await
        .context("joining workflow stdout reader")??;
    let stderr = stderr_task
        .await
        .context("joining workflow stderr reader")??;
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
    let model = format!("@{preset}");
    Ok(BTreeMap::from([
        ("BITROUTER_BASE_URL".into(), base.into()),
        ("BITROUTER_API_BASE".into(), v1.clone()),
        ("BITROUTER_MODEL".into(), model.clone()),
        ("OPENAI_BASE_URL".into(), v1.clone()),
        ("OPENAI_API_BASE".into(), v1),
        ("OPENAI_API_KEY".into(), "bitrouter-local".into()),
        ("OPENAI_MODEL".into(), model.clone()),
        ("ANTHROPIC_BASE_URL".into(), base.into()),
        ("ANTHROPIC_AUTH_TOKEN".into(), "bitrouter-local".into()),
        ("ANTHROPIC_MODEL".into(), model),
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
        {
            anyhow::bail!("{variant} request {request_id} has an ambiguous decision join");
        }
        if !matches!(
            record.reconciliation_status,
            crate::metering::ReconciliationStatus::Computed
                | crate::metering::ReconciliationStatus::NotCharged
        ) || record.authoritative_receipt.is_none()
        {
            anyhow::bail!("{variant} request {request_id} lacks an authoritative settlement");
        }
        let settled_cost =
            if record.reconciliation_status == crate::metering::ReconciliationStatus::Computed {
                record.final_charge_micro_usd.ok_or_else(|| {
                    anyhow::anyhow!("{variant} request {request_id} has no final settled cost")
                })?
            } else {
                0
            };
        total_cost = total_cost
            .checked_add(settled_cost)
            .ok_or_else(|| anyhow::anyhow!("{variant} settled cost overflow"))?;
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
            settled_cost_micro_usd: Some(settled_cost),
        });
        attributions.push(VariantAttribution {
            request_id: request_id.to_string(),
            decision: subject.decisions[0].clone(),
            settled_cost_micro_usd: settled_cost,
            latency_ms,
        });
    }
    if observations.is_empty() {
        anyhow::bail!("{variant} produced no settled named-policy decisions");
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
        settled_cost_micro_usd: total_cost,
        observed_latency_ms,
        observations,
        attributions,
    })
}

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
    settled_cost_micro_usd: u64,
    priced_count: u64,
    already_economy: bool,
}

pub fn select_target_request_key(
    active: &PolicyLock,
    policy_name: &str,
    economy_tier: &str,
    preference: OptimizationPreference,
    observations: &[RouteObservation],
) -> Result<String> {
    validate_document(active)?;
    let policy = active
        .policies
        .get(policy_name)
        .ok_or_else(|| anyhow::anyhow!("optimization policy '{policy_name}' does not exist"))?;
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
        summary.already_economy |= observation.selected_tier == economy_tier;
        if let Some(cost) = observation.settled_cost_micro_usd {
            summary.settled_cost_micro_usd = summary.settled_cost_micro_usd.saturating_add(cost);
            summary.priced_count = summary.priced_count.saturating_add(1);
        }
    }
    let mut eligible = summaries
        .into_iter()
        .filter(|(_, summary)| summary.count > 0 && !summary.already_economy)
        .collect::<Vec<_>>();
    if eligible.is_empty() {
        anyhow::bail!("baseline produced no eligible non-economy route key to optimize");
    }

    match preference {
        OptimizationPreference::QualityFirst => eligible.sort_by(|left, right| {
            left.1
                .count
                .cmp(&right.1.count)
                .then_with(|| {
                    left.1
                        .settled_cost_micro_usd
                        .cmp(&right.1.settled_cost_micro_usd)
                })
                .then_with(|| left.0.cmp(&right.0))
        }),
        OptimizationPreference::Balanced | OptimizationPreference::Custom => {
            eligible.sort_by(|left, right| {
                right
                    .1
                    .count
                    .cmp(&left.1.count)
                    .then_with(|| left.0.cmp(&right.0))
            })
        }
        OptimizationPreference::SavingsFirst => {
            eligible.retain(|(_, summary)| summary.priced_count > 0);
            eligible.sort_by(|left, right| {
                right
                    .1
                    .settled_cost_micro_usd
                    .cmp(&left.1.settled_cost_micro_usd)
                    .then_with(|| right.1.count.cmp(&left.1.count))
                    .then_with(|| left.0.cmp(&right.0))
            });
            if eligible.is_empty() {
                anyhow::bail!("savings-first optimization requires at least one priced settlement");
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
    let mut experiment = active.clone();
    experiment.lockfile_version = LEGACY_POLICY_LOCKFILE_VERSION;
    experiment.artifact = None;
    experiment.certificates.clear();
    for policy in experiment.policies.values_mut() {
        policy.progress_guard = None;
    }
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
    policy
        .routes
        .insert(target_request_key.to_string(), economy_tier.to_string());
    validate_document(&experiment)?;
    Ok(experiment)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use bitrouter_sdk::config::AdequacyConfig;

    use super::{
        PrivateDaemonPaths, RouteObservation, WorkflowExecution, WorkflowRunRequest,
        build_experiment_lock, collect_variant_evidence, private_daemon_config,
        run_workflow_command, select_target_request_key, workflow_environment,
    };
    use crate::eval::types::{
        EVAL_SCHEMA_VERSION, EvalDecisionRef, EvalScope, EvalSubject, EvidenceItem, evidence_digest,
    };
    use crate::metering::{ChargeStatus, MeteringUsageRecord, ReconciliationStatus};
    use crate::optimization::{
        EvaluatorRoute, OptimizationIntent, OptimizationPreference, ResolvedEvaluator,
        WorkflowCommand,
    };
    use crate::policy_lock::{PolicyDefinition, PolicyLock, semantic_digest, validate_document};

    fn active_lock() -> PolicyLock {
        let mut lock = PolicyLock::default();
        lock.lockfile_version = 1;
        lock.artifact = None;
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
                routes: BTreeMap::from([
                    ("agent_trace/v2|edit|normal".into(), "strong".into()),
                    ("agent_trace/v2|test|normal".into(), "strong".into()),
                ]),
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
                settled_cost_micro_usd: Some(900),
            },
            RouteObservation {
                request_key: "agent_trace/v2|edit|normal".into(),
                selected_tier: "strong".into(),
                settled_cost_micro_usd: Some(800),
            },
            RouteObservation {
                request_key: "agent_trace/v2|test|normal".into(),
                selected_tier: "strong".into(),
                settled_cost_micro_usd: Some(4_000),
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
            input_model: "@auto".into(),
            key_strategy: "agent_trace".into(),
            request_key: "agent_trace/v2|edit|normal".into(),
            ledger_key: None,
            policy: Some("auto".into()),
            policy_digest: Some(policy_digest.into()),
            preset_variant: Some("auto".into()),
            baseline_tier: Some("strong".into()),
            legacy_fingerprint: "legacy".into(),
            workflow_state: "edit".into(),
            workflow_identity: Default::default(),
            static_tier: Some("strong".into()),
            static_model: Some("bitrouter:openai/gpt-5.6".into()),
            selected_tier: Some("strong".into()),
            selected_model: Some("bitrouter:openai/gpt-5.6".into()),
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
                baseline_tier: Some("strong".into()),
                policy_digest: policy_digest.into(),
            }],
            requested_dimensions: Default::default(),
            evidence_digest: evidence_digest(&evidence)?,
            evidence,
            observed_at: "2026-08-08T00:00:00Z".into(),
        })
    }

    fn settled_usage() -> MeteringUsageRecord {
        MeteringUsageRecord {
            request_id: Some("req-1".into()),
            provider_id: "bitrouter".into(),
            model_id: "openai/gpt-5.6".into(),
            final_charge_micro_usd: Some(400),
            charge_status: ChargeStatus::Computed,
            reconciliation_status: ReconciliationStatus::Computed,
            authoritative_receipt: Some(serde_json::json!({"request_id": "req-1"})),
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
                "economy",
                OptimizationPreference::SavingsFirst,
                &observations,
            )?,
            "agent_trace/v2|test|normal"
        );
        Ok(())
    }

    #[test]
    fn experiment_lock_is_nonpublishable_and_changes_exactly_one_route() -> anyhow::Result<()> {
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
        assert_eq!(experiment.lockfile_version, 1);
        assert!(experiment.artifact.is_none());
        assert!(experiment.certificates.is_empty());
        assert_eq!(
            experiment.policies["auto"].routes["agent_trace/v2|edit|normal"],
            "economy"
        );
        assert_eq!(
            experiment.policies["auto"].routes["agent_trace/v2|test|normal"],
            "strong"
        );
        assert!(
            experiment
                .policies
                .values()
                .all(|policy| policy.progress_guard.is_none())
        );
        assert_ne!(semantic_digest(&experiment)?, active_digest);
        Ok(())
    }

    #[test]
    fn selection_rejects_unobserved_or_already_economy_routes() {
        let active = active_lock();
        let observations = vec![RouteObservation {
            request_key: "agent_trace/v2|edit|normal".into(),
            selected_tier: "economy".into(),
            settled_cost_micro_usd: Some(10),
        }];
        assert!(
            select_target_request_key(
                &active,
                "auto",
                "economy",
                OptimizationPreference::Balanced,
                &observations,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn workflow_runner_uses_exact_argv_env_and_timeout_without_retry() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let success = run_workflow_command(WorkflowRunRequest {
            workflow: &WorkflowCommand {
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf '%s' \"$BITROUTER_MODEL\"".into(),
                ],
                timeout_secs: 2,
            },
            cwd: dir.path(),
            env: &BTreeMap::from([("BITROUTER_MODEL".into(), "@auto".into())]),
            maximum_output_bytes: 1024,
        })
        .await?;
        assert_eq!(success.exit_code, Some(0));
        assert!(!success.timed_out);
        assert_eq!(success.stdout, "@auto");

        let timeout = run_workflow_command(WorkflowRunRequest {
            workflow: &WorkflowCommand {
                command: vec!["/bin/sleep".into(), "5".into()],
                timeout_secs: 1,
            },
            cwd: dir.path(),
            env: &BTreeMap::new(),
            maximum_output_bytes: 1024,
        })
        .await?;
        assert!(timeout.timed_out);
        assert_eq!(timeout.exit_code, None);
        assert!(timeout.elapsed >= Duration::from_millis(900));
        assert!(timeout.elapsed < Duration::from_secs(4));
        assert_eq!(timeout.launches, 1);
        assert_eq!(PathBuf::from(timeout.cwd), dir.path());
        Ok(())
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

    #[test]
    fn evidence_requires_exact_decision_subject_and_authoritative_settlement() -> anyhow::Result<()>
    {
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
            &[settled_usage()],
        )?;
        assert_eq!(evidence.settled_cost_micro_usd, 400);
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
            &[settled_usage()],
        )?;
        assert_eq!(failed.execution.exit_code, Some(2));

        let mut pending = settled_usage();
        pending.reconciliation_status = ReconciliationStatus::Pending;
        pending.authoritative_receipt = None;
        assert!(
            collect_variant_evidence(
                "baseline",
                &policy_digest,
                execution,
                &[decision(&policy_digest)],
                &[subject(&policy_digest)?],
                &[pending],
            )
            .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn private_daemon_config_is_isolated_cloud_only_and_secret_free() -> anyhow::Result<()> {
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
                timeout_secs: 60,
            },
            contract: PathBuf::from("bitrouter.eval.md"),
            source_config: PathBuf::from("bitrouter.yaml"),
            policy: "auto".into(),
            preset: "auto".into(),
            strong: "bitrouter:openai/gpt-5.6".into(),
            economy: "bitrouter:deepseek/deepseek-v4-flash-0731".into(),
            preference: OptimizationPreference::Balanced,
            custom_quality: None,
            evaluator: ResolvedEvaluator {
                agent: "codex-acp".into(),
                model: "bitrouter:openai/gpt-5.6".into(),
                route: EvaluatorRoute::Cloud,
            },
        };
        let yaml = private_daemon_config(&paths, &intent, 43123)?;
        assert!(!yaml.contains("brk_"));
        assert!(yaml.contains("127.0.0.1:43123"));
        assert!(yaml.contains("auto_discover: true"));

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
            Some("bitrouter:openai/gpt-5.6")
        );
        assert!(parsed.providers.contains_key("bitrouter"));
        Ok(())
    }
}
