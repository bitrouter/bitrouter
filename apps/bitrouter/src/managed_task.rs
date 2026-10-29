//! Product CLI client for daemon-owned native managed sessions.

pub mod local;
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod owner;
pub mod protocol;

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use crate::storage::checkpoint::snapshot::RunStatus;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::core::protocol::CoreError;
use bitrouter_ai::types::ReasoningEffort;
use tokio_util::sync::CancellationToken;

use local::Client;
use protocol::{Operation, Report, Submit};

pub struct Options {
    pub prompt: String,
    pub model: Option<String>,
    pub effort: Option<ReasoningEffort>,
    pub check: Option<String>,
    pub read_only: bool,
    pub workspace: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub session: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub request_id: Option<String>,
    pub server_instance: Option<String>,
    pub skills: Option<bitrouter_ai::core::skills::SkillConstraints>,
}

#[derive(Debug, Clone, clap::Args)]
pub struct StatusOptions {
    #[command(flatten)]
    pub endpoint: crate::control_output::Endpoint,
    /// Original durable managed session name; does not load or execute it.
    #[arg(long)]
    pub session: String,
    /// Inspect this exact accepted input, rather than the latest session state.
    #[arg(long)]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, clap::Args)]
pub struct CancelOptions {
    #[command(flatten)]
    pub endpoint: crate::control_output::Endpoint,
    #[arg(long)]
    pub session: String,
    /// Exact original input key; a different active request is never cancelled.
    #[arg(long)]
    pub request_id: String,
}

#[derive(Debug, Clone, clap::Args)]
pub struct AnswerOptions {
    #[command(flatten)]
    pub endpoint: crate::control_output::Endpoint,
    #[arg(long)]
    pub session: String,
    /// Original accepted managed input key.
    #[arg(long)]
    pub request_id: String,
    /// Exact consent request from managed-status or managed_skill_consent.
    #[arg(long)]
    pub consent_id: String,
    #[arg(long, value_enum)]
    pub decision: crate::thread_cli::Decision,
}

pub async fn answer(options: AnswerOptions, output: &crate::output::Output) -> Result<()> {
    ensure!(
        options.endpoint.server_instance.is_some(),
        "managed skill consent requires --server-instance from the original session receipt"
    );
    crate::control_output::validate_id(&options.request_id)?;
    crate::control_output::validate_id(&options.consent_id)?;
    let client = control_client(&options.endpoint).await?;
    ensure!(
        client.skill_consent,
        "selected managed daemon does not support explicit skill consent"
    );
    let report = client
        .request(Operation::Status {
            name: options.session.clone(),
            operation_id: Some(options.request_id.clone()),
        })
        .await?;
    let request = report
        .skill_consents
        .iter()
        .find(|request| request.consent.request_id == options.consent_id)
        .context("consent does not belong to this original managed input")?;
    let approved = matches!(options.decision, crate::thread_cli::Decision::Allow);
    let report = client
        .request(Operation::AnswerSkill {
            name: options.session,
            operation_id: options.request_id.clone(),
            answer_operation_id: format!("skill_answer_{}", options.consent_id),
            answer: request.answer(approved),
        })
        .await?;
    crate::control_output::emit(
        output,
        "managed_skill_answer",
        &client.daemon_instance_id,
        Some(&options.request_id),
        &report,
    )
}

pub async fn status(options: StatusOptions, output: &crate::output::Output) -> Result<()> {
    let client = control_client(&options.endpoint).await?;
    let report = client
        .request(Operation::Status {
            name: options.session,
            operation_id: options.request_id.clone(),
        })
        .await?;
    crate::control_output::emit(
        output,
        "managed_status",
        &client.daemon_instance_id,
        options.request_id.as_deref(),
        &report,
    )
}

pub async fn cancel(options: CancelOptions, output: &crate::output::Output) -> Result<()> {
    ensure!(
        options.endpoint.server_instance.is_some(),
        "managed cancellation requires --server-instance from the original session receipt"
    );
    let client = control_client(&options.endpoint).await?;
    let report = client
        .request(Operation::Cancel {
            name: options.session,
            operation_id: options.request_id.clone(),
        })
        .await?;
    crate::control_output::emit(
        output,
        "managed_cancel",
        &client.daemon_instance_id,
        Some(&options.request_id),
        &report,
    )
}

async fn control_client(endpoint: &crate::control_output::Endpoint) -> Result<Client> {
    let control = endpoint.socket().await?;
    let client = Client::connect(&protocol::socket_path(&control)).await?;
    endpoint.check_instance(&client.daemon_instance_id)?;
    Ok(client)
}

pub async fn run(options: Options) -> Result<()> {
    if let Some(skills) = &options.skills {
        skills.validate()?;
    }
    ensure!(
        options.max_output_tokens != Some(0),
        "output reservation must be positive"
    );
    // Capture the caller's directory before source resolution or daemon startup.
    let workspace = options
        .workspace
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()?;
    let located = crate::daemon_locator::locate(options.config.as_deref()).await?;
    let (source, control) = match located {
        Some(located) => (located.source().clone(), located.socket().to_path_buf()),
        None => {
            let source = crate::paths::resolve_config(options.config.as_deref())?;
            let config = crate::paths::load_config(&source).await?;
            let control = crate::daemon::socket_path_for(&source, &config);
            (source, control)
        }
    };
    // Reuse the selected startup/readiness path, never build App or a database.
    crate::client::agent_local::connect_or_start(&source, &control).await?;
    let client = Client::connect(&protocol::socket_path(&control)).await?;
    ensure!(
        options.skills.is_none() || client.skill_constraints,
        "selected managed daemon does not support skill constraints; no task submitted"
    );
    ensure!(
        options
            .server_instance
            .as_ref()
            .is_none_or(|expected| expected == &client.daemon_instance_id),
        "managed daemon instance changed; no submit attempted"
    );
    let name = options
        .session
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    crate::managed_task::protocol::validate_session_name(&name)?;
    let operation_id = options
        .request_id
        .unwrap_or_else(|| format!("input_{}", uuid::Uuid::new_v4()));
    crate::control_output::validate_id(&operation_id)?;
    writeln!(
        std::io::stderr().lock(),
        "managed request: session={name} request_id={operation_id} server_instance={}",
        client.daemon_instance_id
    )?;
    let request = Submit {
        name,
        operation_id,
        workspace,
        prompt: options.prompt,
        model: options.model,
        effort: options.effort,
        max_output_tokens: options.max_output_tokens,
        check: options.check,
        read_only: options.read_only,
        skills: options.skills,
    };
    let cancelled = CancellationToken::new();
    let signal_cancel = cancelled.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });
    let result = wait(&client, request.clone(), cancelled).await;
    signal.abort();
    result.with_context(|| format!("managed session {} request {} on daemon {}; inspect this original identity before any retry", request.name, request.operation_id, client.daemon_instance_id))
}

async fn wait(client: &Client, request: Submit, cancelled: CancellationToken) -> Result<()> {
    let query = || Operation::Status {
        name: request.name.clone(),
        operation_id: Some(request.operation_id.clone()),
    };
    let mut report = match client.request(Operation::Submit(request.clone())).await {
        Ok(report) => report,
        Err(error) if error.downcast_ref::<CoreError>().is_some() => return Err(error),
        Err(error) => client.request(query()).await.with_context(|| format!("submit ACK unknown ({error}); only the original status was queried, not resubmitted"))?,
    };
    let mut announced = false;
    let mut cancellation_sent = false;
    let mut announced_consents = std::collections::BTreeSet::new();
    loop {
        if !announced && report.receipt.is_some() {
            line(
                &serde_json::json!({"type":"managed_session","name":request.name,
                "session_id":report.grant.as_ref().map(|grant| &grant.session_id),"workspace":report.workspace,
                "request_id":request.operation_id,"server_instance_id":client.daemon_instance_id,"grant":report.grant}),
            )?;
            announced = true;
        }
        for consent in &report.skill_consents {
            if !consent.consent.revoked
                && consent.consent.approved.is_none()
                && announced_consents.insert(consent.consent.request_id.clone())
            {
                line(
                    &serde_json::json!({"type":"managed_skill_consent","name":request.name,
                    "request_id":request.operation_id,"server_instance_id":client.daemon_instance_id,
                    "notice":"Approve this exact MCP skill, including local commands its instructions may induce. Existing command permissions and approvals still apply. Nested skills require separate consent.",
                    "consent":consent}),
                )?;
            }
        }
        if !report.resident {
            return terminal(client, &request, report);
        }
        if cancelled.is_cancelled() && !cancellation_sent {
            report = client
                .request(Operation::Cancel {
                    name: request.name.clone(),
                    operation_id: request.operation_id.clone(),
                })
                .await?;
            cancellation_sent = true;
        } else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            report = client.request(query()).await?;
        }
    }
}

fn terminal(client: &Client, request: &Submit, report: Report) -> Result<()> {
    line(&serde_json::json!({"type":"terminal","name":request.name,
        "session_id":report.grant.as_ref().map(|grant| &grant.session_id),"agents":report.agents,"run":report.run,
        "request_id":request.operation_id,"server_instance_id":client.daemon_instance_id,
        "released":report.released,"state":report.state,"error":report.error}))?;
    if let Some(error) = report.error {
        return Err(error.into());
    }
    ensure!(
        report.released && report.receipt.is_some(),
        "managed operation is not confirmed released ({}); query is passive and cannot recover lost work",
        report.state
    );
    ensure!(
        report
            .run
            .as_ref()
            .is_some_and(|run| run.status == RunStatus::Completed),
        "managed task did not complete, or its original terminal result is historical; no task was replayed"
    );
    Ok(())
}

fn line(value: &serde_json::Value) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    writeln!(stdout)?;
    stdout.flush()?;
    Ok(())
}
