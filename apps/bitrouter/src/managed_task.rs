//! Headless managed mode for BRO's native workspace harness.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;

use bitrouter_orchestrator::agent::ToolMode;
use bitrouter_orchestrator::core::protocol::{CoreError, TaskInput, ToolExecute, Verification};
use bitrouter_orchestrator::core::session::RunStatus;
use bitrouter_orchestrator::harness::managed::session::{NativeApproval, NativeSession};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::types::ReasoningEffort;
use tokio_util::sync::CancellationToken;

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
}

struct HeadlessApproval;

#[async_trait::async_trait]
impl NativeApproval for HeadlessApproval {
    async fn approve(&self, _: &ToolExecute) -> Result<bool, CoreError> {
        // Like `bro task run`, an explicit headless coding task authorizes its
        // own tools within the chosen workspace. Read-only mode omits effects.
        Ok(true)
    }
}

pub async fn run(options: Options) -> anyhow::Result<()> {
    anyhow::ensure!(
        options.max_output_tokens != Some(0),
        "output reservation must be positive"
    );
    let workspace = options
        .workspace
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()?;
    let source = crate::paths::resolve_config(options.config.as_deref())?;
    // Match the native server's relative database/config resolution and registry
    // loading. No listener is opened and no remote auth setting is changed.
    crate::paths::ensure_home_directory(source.home())?;
    std::env::set_current_dir(source.home())?;
    let baseline = crate::reload::load_configuration_baseline(&source).await?;
    let mut config = baseline.config().clone();
    crate::claude_code::enable_if_logged_in(&mut config);
    crate::merge_registry_into(&mut config).await;
    let resources = crate::host::native_harness_config(&config, source.home());
    let assembled = crate::assemble::build_app(&config).await?;
    let name = options
        .session
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let store = Arc::new(crate::managed_store::DatabaseNativeStore::new(
        assembled.db,
        name.clone(),
    )?);
    let mode = if options.read_only {
        ToolMode::ReadOnly
    } else {
        ToolMode::Coding
    };
    let mut session = NativeSession::open(
        Arc::new(assembled.app),
        CallerContext::local(),
        store,
        &workspace,
        mode,
        &resources,
    )
    .await?;
    println!(
        "{}",
        serde_json::json!({"type":"managed_session","name":name,"session_id":session.core().snapshot().await.session_id,"workspace":workspace})
    );
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });
    let model = options
        .model
        .or_else(|| crate::policy_lock::native_default_model(&config).map(str::to_owned))
        .context("no native model configured; pass --model or enable defaults")?;
    let model_mode = crate::policy_lock::native_model_mode(&config, &model)?;
    let input = TaskInput {
        text: options.prompt,
        model,
        effort: options.effort.map(|effort| effort.to_string()),
        max_output_tokens: options.max_output_tokens,
        context_limit_bytes: None,
        routing: bitrouter_orchestrator::core::protocol::RoutingSettings {
            model: model_mode,
            ..Default::default()
        },
        max_concurrent_subagents: None,
        discardable_history: None,
        acceptance_criteria: vec![],
        required_materials: vec![],
        limits: None,
        verification: options.check.map(|command| Verification {
            tool: "shell".into(),
            arguments: serde_json::json!({"command":command}),
        }),
    };
    let result = session.run(input, Arc::new(HeadlessApproval), cancel).await;
    signal.abort();
    let snapshot = match result {
        Ok(snapshot) => snapshot,
        Err(err) => {
            // Release only if the core confirms all admitted work is settled.
            // Unknown commit/effect outcomes still refuse release and stay fenced.
            if let Err(release) = session.close().await {
                return Err(anyhow::anyhow!("{err}; native release: {release}"));
            }
            return Err(err.into());
        }
    };
    let completed = snapshot
        .run
        .as_ref()
        .is_some_and(|run| run.status == RunStatus::Completed);
    session.close().await?;
    println!(
        "{}",
        serde_json::json!({"type":"terminal","name":name,"session_id":snapshot.session_id,"agents":snapshot.agents.len(),"run":snapshot.run})
    );
    anyhow::ensure!(
        completed,
        "managed task did not complete; inspect its retained native session"
    );
    Ok(())
}
