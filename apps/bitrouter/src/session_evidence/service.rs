//! Application-owned collection behind maintained Codex and Claude controllers.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bitrouter_sdk::acp::controller::{ControllerIdentity, SessionObservation, SessionObserver};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, watch};

use super::collector::{NativeCollector, NativeRoot};
use super::history::{HistoryResolver, ResolvedHistory};
use super::journal::{Journal, import_spool};
use super::store::EvidenceStore;
use super::types::{
    Harness, MAX_GRAPH_ITEMS, NodeKey, RECORD_PAGE_SIZE, RecordRef, SourceDescriptor, SourceFormat,
    SourceRange,
};
use crate::eval::types::canonical_digest;

mod bridge;
mod checkpoints;
pub mod processes;
mod recovery;
mod roots;
pub mod sdk_bindings;
pub(crate) mod tasks;
mod workspaces;

#[derive(Clone)]
struct RootContext {
    collector: NativeCollector,
    spool: PathBuf,
    journal: Arc<Journal>,
}

#[derive(Clone)]
struct PendingScope {
    request: Option<RecordRef>,
    namespace: String,
    session_id: Option<String>,
    query_fingerprint: Option<String>,
    method: String,
    query_reused: bool,
    workspace: Option<WorkspaceScope>,
}

#[derive(Clone)]
struct WorkspaceScope {
    cwd: PathBuf,
    additional_directories: bool,
    exclusions: BTreeSet<PathBuf>,
}

fn workspace_scope(params: &Value, exclusions: BTreeSet<PathBuf>) -> Option<WorkspaceScope> {
    // The maintained adapters prefer the ACP field, then the legacy extension.
    // Claude also merges the SDK option. Only the coverage flag is retained;
    // unrelated SDK/provider configuration never enters artifact metadata.
    // https://github.com/agentclientprotocol/claude-agent-acp/blob/main/src/acp-agent.ts
    // https://github.com/agentclientprotocol/codex-acp
    let acp_directories = params
        .get("additionalDirectories")
        .filter(|value| !value.is_null())
        .or_else(|| params.pointer("/_meta/additionalRoots"));
    Some(WorkspaceScope {
        cwd: PathBuf::from(params.get("cwd")?.as_str()?),
        additional_directories: [
            acp_directories,
            params.pointer("/_meta/claudeCode/options/additionalDirectories"),
        ]
        .into_iter()
        .flatten()
        .any(|dirs| !dirs.is_null() && !dirs.as_array().is_some_and(Vec::is_empty)),
        exclusions,
    })
}

#[derive(Clone)]
struct LoadedQuery {
    namespace: String,
    fingerprint: String,
}

pub struct EvidenceLaunch<'a> {
    pub home: &'a Path,
    pub database_url: &'a str,
    pub identity: &'a ControllerIdentity,
    pub env: &'a mut HashMap<String, String>,
    pub strip_inherited_env: &'a [String],
}

/// A status object is replaceable derived state. Only a frozen manifest may
/// serve as evaluation evidence; this live view makes no readiness claim.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CollectionSnapshot {
    pub histories: Vec<ResolvedHistory>,
    pub graph: super::execution::ExecutionGraph,
    pub attempts: Vec<super::types::Attempt>,
    pub workspace_checkpoints: BTreeMap<String, super::types::WorkspaceEvidence>,
    #[serde(default)]
    pub native_checkpoints: BTreeMap<String, super::checkpoint::NativeCheckpointEvidence>,
    #[serde(default)]
    pub prompt_bindings: BTreeMap<String, super::adapter_bridge::PromptEvidence>,
    #[serde(default)]
    pub processes: Vec<processes::ProcessBinding>,
    #[serde(default)]
    pub sdk_bindings: sdk_bindings::SdkBindingPage,
    pub gaps: BTreeSet<String>,
    pub reconciled_at: Option<String>,
}

#[derive(Default)]
struct LiveState {
    bridge_capable: bool,
    inventory_epoch: u64,
    inventory_cycle_active: bool,
    completed_spools: BTreeSet<PathBuf>,
    process_sources: BTreeSet<String>,
    process_source_limit: bool,
    sdk_sources: BTreeSet<String>,
    sdk_source_limit: bool,
    sdk_cursor: Option<sdk_bindings::SdkBindingCursor>,
    sdk_inventory: sdk_bindings::SdkInventory,
    nodes: BTreeSet<NodeKey>,
    replayed: BTreeMap<String, u64>,
    replay_gaps: BTreeMap<String, BTreeSet<String>>,
    spool_nodes: BTreeMap<PathBuf, BTreeSet<NodeKey>>,
    spool_after: BTreeMap<PathBuf, PathBuf>,
    spool_gaps: BTreeMap<PathBuf, BTreeSet<String>>,
    snapshot: CollectionSnapshot,
    roots: BTreeMap<String, RootContext>,
    sessions: BTreeMap<String, String>,
    loaded: BTreeMap<String, LoadedQuery>,
    ambiguous_sessions: BTreeSet<String>,
    uncertain_queries: BTreeSet<String>,
    pending: BTreeMap<String, PendingScope>,
    workspaces: BTreeMap<String, WorkspaceScope>,
}

pub struct ControllerEvidence {
    store: EvidenceStore,
    collector: NativeCollector,
    root_env: HashMap<String, String>,
    claude_model_config: Option<String>,
    controller_id: String,
    producer_version: String,
    process_capture: bool,
    spool: PathBuf,
    executable: PathBuf,
    workspace_exclusions: BTreeSet<PathBuf>,
    state: Mutex<LiveState>,
    recovery: Mutex<recovery::RecoveryState>,
    reconcile_gate: Mutex<()>,
    root_gate: Mutex<()>,
    observation_gate: Mutex<()>,
    wake: Notify,
}

/// Owns the background worker with the same lifetime as the harness process.
pub struct EvidenceHandle {
    pub service: Arc<ControllerEvidence>,
    stop: watch::Sender<bool>,
    worker: tokio::task::JoinHandle<()>,
}

impl EvidenceHandle {
    pub async fn open(launch: EvidenceLaunch<'_>) -> Result<Option<Self>> {
        let harness = match (
            launch.identity.harness_id.as_str(),
            launch.identity.adapter_package.as_str(),
        ) {
            ("codex-acp", "@agentclientprotocol/codex-acp") => Harness::Codex,
            ("claude-acp", "@agentclientprotocol/claude-agent-acp") => Harness::ClaudeCode,
            _ => return Ok(None),
        };
        let root = native_root(harness, launch.env, launch.strip_inherited_env)?;
        let root_env = ["CODEX_HOME", "CLAUDE_CONFIG_DIR", "HOME", "USERPROFILE"]
            .into_iter()
            .filter_map(|key| {
                inherited_value(key, launch.env, launch.strip_inherited_env)
                    .map(|value| (key.into(), value))
            })
            .collect();
        tokio::fs::create_dir_all(launch.home).await?;
        let db =
            crate::db::connect(&crate::db::anchor_url(launch.database_url, launch.home)).await?;
        crate::db::run_migrations(&db).await?;
        let workspace_exclusions = workspaces::runtime_exclusions(&db, launch.home).await?;
        let store = EvidenceStore::new(db, "local")?;
        let controller = uuid::Uuid::new_v4().to_string();
        let spool = launch
            .home
            .join("native-evidence")
            .join("controllers")
            .join(&controller);
        private_directory(&spool).await?;
        let spool = tokio::fs::canonicalize(spool).await?;
        let executable = std::env::current_exe()?;
        if harness == Harness::Codex {
            if !launch.env.contains_key("CODEX_PATH")
                && let Some(original) =
                    inherited_value("CODEX_PATH", launch.env, launch.strip_inherited_env)
            {
                launch.env.insert("CODEX_PATH".into(), original);
            }
            super::codex_proxy::prepare_env(launch.env, &spool, &executable)?;
        }
        let claude_process_capture = if harness == Harness::ClaudeCode {
            if !launch.env.contains_key("CLAUDE_CODE_EXECUTABLE")
                && let Some(original) = inherited_value(
                    "CLAUDE_CODE_EXECUTABLE",
                    launch.env,
                    launch.strip_inherited_env,
                )
            {
                launch.env.insert("CLAUDE_CODE_EXECUTABLE".into(), original);
            }
            super::claude_proxy::prepare_env(launch.env, &spool, &executable, &root)?
        } else {
            true
        };
        let producer_version = format!(
            "{}@{}",
            launch.identity.adapter_package, launch.identity.adapter_version
        );
        let journal = Arc::new(
            Journal::new(
                store.clone(),
                SourceDescriptor {
                    namespace: root.namespace.clone(),
                    harness,
                    format: SourceFormat::Acp,
                    locator: format!("controller:{controller}"),
                    node: None,
                },
                producer_version.clone(),
            )
            .await?,
        );
        journal
            .append(
                json!({"method":"controller/started","phase":"metadata","payload":{
            "native_root":root.directory,"namespace":root.namespace,"spool":spool,
            "harness":harness,"adapter_version":launch.identity.adapter_version,
            "native_process_capture":claude_process_capture,
        },"observed_at":chrono::Utc::now().to_rfc3339()}),
            )
            .await?;
        let collector = NativeCollector::new(store.clone(), root)?;
        let roots = BTreeMap::from([(
            collector.root().namespace.clone(),
            RootContext {
                collector: collector.clone(),
                spool: spool.clone(),
                journal,
            },
        )]);
        let service = Arc::new(ControllerEvidence {
            store,
            collector,
            root_env,
            claude_model_config: inherited_value(
                "CLAUDE_MODEL_CONFIG",
                launch.env,
                launch.strip_inherited_env,
            ),
            controller_id: controller,
            producer_version,
            process_capture: claude_process_capture,
            spool,
            executable,
            workspace_exclusions,
            state: Mutex::new(LiveState {
                roots,
                ..LiveState::default()
            }),
            recovery: Mutex::new(recovery::RecoveryState::default()),
            reconcile_gate: Mutex::new(()),
            root_gate: Mutex::new(()),
            observation_gate: Mutex::new(()),
            wake: Notify::new(),
        });
        let (stop, mut stopped) = watch::channel(false);
        let running = Arc::clone(&service);
        let worker = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    _ = running.wake.notified() => {},
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {},
                }
                if let Err(error) = running.reconcile().await {
                    running
                        .state
                        .lock()
                        .await
                        .snapshot
                        .gaps
                        .insert("native_collection_failed".into());
                    tracing::warn!(%error, "native session evidence reconciliation failed");
                }
            }
        });
        Ok(Some(Self {
            service,
            stop,
            worker,
        }))
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        let _ = self.stop.send(true);
        // Finish any in-flight read before the final reconciliation. Killing
        // a collector at an arbitrary point must not become a ready checkpoint.
        tokio::time::timeout(Duration::from_secs(30), &mut self.worker)
            .await
            .context("native collector did not stop")??;
        tokio::time::timeout(Duration::from_secs(30), self.service.reconcile())
            .await
            .context("native evidence final reconciliation timed out")??;
        Ok(())
    }
}

impl Drop for EvidenceHandle {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

impl ControllerEvidence {
    pub async fn snapshot(&self) -> CollectionSnapshot {
        self.state.lock().await.snapshot.clone()
    }

    pub async fn reconcile(&self) -> Result<CollectionSnapshot> {
        let _guard = self.reconcile_gate.lock().await;
        let epoch = self.begin_inventory().await?;
        let mut gaps = BTreeSet::new();
        if !self.process_capture {
            gaps.insert("native_process_capture_unavailable".into());
        }
        let recovered = self.recover(&mut gaps, epoch).await;
        let roots = self.state.lock().await.roots.clone();
        let mut collectors = recovered.collectors;
        for context in roots.values() {
            collectors.insert(
                context.collector.root().namespace.clone(),
                context.collector.clone(),
            );
            let mut found = BTreeSet::new();
            if let Err(error) = self
                .reconcile_spool(
                    context.collector.root(),
                    &context.spool,
                    true,
                    &mut found,
                    &mut gaps,
                )
                .await
            {
                tracing::warn!(%error, "native root spool could not be reconciled");
                gaps.insert("native_spool_failed".into());
            }
            let mut state = self.state.lock().await;
            for node in found {
                if state.nodes.contains(&node) || state.nodes.len() < MAX_GRAPH_ITEMS {
                    state.nodes.insert(node);
                } else {
                    gaps.insert("native_execution_node_limit".into());
                }
            }
        }
        let mut nodes = self.state.lock().await.nodes.clone();
        for node in recovered.nodes {
            if nodes.contains(&node) || nodes.len() < MAX_GRAPH_ITEMS {
                nodes.insert(node);
            } else {
                gaps.insert("native_recovery_node_limit".into());
            }
        }
        let process_sources = self.state.lock().await.process_sources.clone();
        let mut processes = Vec::with_capacity(process_sources.len());
        for source_id in process_sources {
            let binding = self.process_binding(source_id).await;
            gaps.extend(binding.gaps.iter().cloned());
            processes.push(binding);
        }
        let sdk_bindings = self.sdk_bindings(&processes, &mut gaps).await;
        for binding in &sdk_bindings.observations {
            if let Some(node) = &binding.node {
                if nodes.contains(node) || nodes.len() < MAX_GRAPH_ITEMS {
                    nodes.insert(node.clone());
                } else {
                    gaps.insert("native_execution_node_limit".into());
                }
            }
        }
        for node in nodes.clone() {
            let Some(collector) = collectors.get(&node.namespace) else {
                gaps.insert("native_root_unavailable".into());
                continue;
            };
            if node.harness == Harness::ClaudeCode && node.agent_id.is_none() {
                match collector.discover(&node.native_id).await {
                    Ok(children) => {
                        for (child, _) in children {
                            ensure!(
                                nodes.contains(&child) || nodes.len() < MAX_GRAPH_ITEMS,
                                "native execution node limit"
                            );
                            nodes.insert(child);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "native children could not be discovered");
                        gaps.insert("native_child_discovery_failed".into());
                    }
                }
            }
        }
        let graph = self.store.execution_graph(&nodes).await?;
        nodes.extend(graph.nodes.iter().cloned());
        let mut histories = Vec::with_capacity(nodes.len());
        for node in &nodes {
            let history = if let Some(collector) = collectors.get(&node.namespace) {
                let resolver = HistoryResolver::new(self.store.clone(), collector.clone());
                match resolver.resolve(node.clone()).await {
                    Ok(history) => history,
                    Err(error) => {
                        tracing::warn!(%error, "native execution source could not be reconciled");
                        ResolvedHistory::missing(node.clone(), "native_source_failed")
                    }
                }
            } else {
                ResolvedHistory::missing(node.clone(), "native_root_unavailable")
            };
            if node.harness == Harness::ClaudeCode
                && node.agent_id.is_some()
                && let Some(collector) = collectors.get(&node.namespace)
                && let Some(transcript) = &history.source
            {
                match collector.reconcile_agent_metadata(transcript).await {
                    // A disappeared sidecar does not erase already stored
                    // parent evidence. The graph checks unresolved children.
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(%error, "native agent metadata could not be collected");
                        gaps.insert("native_agent_metadata_failed".into());
                    }
                }
            }
            gaps.extend(history.gaps.iter().cloned());
            histories.push(history);
        }
        let graph = self.store.execution_graph(&nodes).await?;
        gaps.extend(graph.gaps.iter().cloned());
        if !graph.nodes.is_subset(&nodes) {
            gaps.insert("native_graph_backlog".into());
            self.wake.notify_one();
        }
        let mut attempts = Vec::new();
        let mut workspace_checkpoints = BTreeMap::new();
        let mut native_checkpoints = BTreeMap::new();
        let mut prompt_bindings = BTreeMap::new();
        let namespaces = collectors.keys().cloned().collect();
        let (task_sessions, task_gaps) = self
            .store
            .task_sessions(self.collector.root().harness, &namespaces)
            .await?;
        gaps.extend(task_gaps);
        for session in &task_sessions {
            let status = async {
                let attempt = self.store.active_attempt(session).await?;
                let unobserved = attempt.is_some()
                    && self
                        .store
                        .has_unobserved_prompts(session, &self.controller_id)
                        .await?;
                let workspace = match self.store.workspace_evidence(session).await {
                    Ok(workspace) => workspace,
                    Err(error) => {
                        tracing::warn!(%error, "workspace checkpoint could not be read");
                        super::types::WorkspaceEvidence {
                            gaps: BTreeSet::from(["workspace_checkpoint_invalid".into()]),
                            ..super::types::WorkspaceEvidence::default()
                        }
                    }
                };
                anyhow::Ok((attempt, unobserved, workspace))
            }
            .await;
            match status {
                Ok((Some(attempt), unobserved, workspace)) => {
                    let bindings = match self
                        .store
                        .prompt_bridge_evidence(session, &attempt.id)
                        .await
                    {
                        Ok(bindings) => bindings,
                        Err(error) => {
                            tracing::warn!(%error, "adapter prompt bindings could not be verified");
                            super::adapter_bridge::PromptEvidence {
                                gaps: BTreeSet::from(["native_bridge_invalid".into()]),
                                ..Default::default()
                            }
                        }
                    };
                    prompt_bindings.insert(attempt.id.clone(), bindings);
                    if attempt.members.is_empty() {
                        gaps.insert("native_attempt_membership_unavailable".into());
                    }
                    if unobserved {
                        gaps.insert("native_prompt_response_unobserved".into());
                    }
                    gaps.extend(workspace.gaps.iter().cloned());
                    workspace_checkpoints.insert(attempt.id.clone(), workspace);
                    match self.store.native_checkpoint_evidence(session).await {
                        Ok(checkpoint) => {
                            // Historical gaps describe the frozen boundary;
                            // they must not keep today's inventory epoch open
                            // or enter the next checkpoint as live backlog.
                            native_checkpoints.insert(attempt.id.clone(), checkpoint);
                        }
                        Err(error) => {
                            tracing::warn!(%error, "native checkpoint could not be read");
                            gaps.insert("native_checkpoint_invalid".into());
                            native_checkpoints.insert(
                                attempt.id.clone(),
                                super::checkpoint::NativeCheckpointEvidence {
                                    gaps: BTreeSet::from(["native_checkpoint_invalid".into()]),
                                    ..super::checkpoint::NativeCheckpointEvidence::default()
                                },
                            );
                        }
                    }
                    attempts.push(attempt);
                }
                Ok((None, _, _)) => {}
                Err(error) => {
                    tracing::warn!(%error, "native task state could not be read");
                    gaps.insert("native_task_state_invalid".into());
                }
            }
        }
        let mut state = self.state.lock().await;
        if !state.ambiguous_sessions.is_empty() {
            gaps.insert("ambiguous_acp_session_scope".into());
        }
        if !state.uncertain_queries.is_empty() {
            gaps.insert("native_query_scope_unknown".into());
        }
        state.sdk_cursor = sdk_bindings.next.clone();
        state.inventory_cycle_active =
            gaps.contains("native_recovery_backlog") || gaps.contains("native_spool_backlog");
        let snapshot = CollectionSnapshot {
            histories,
            graph,
            attempts,
            workspace_checkpoints,
            native_checkpoints,
            prompt_bindings,
            processes,
            sdk_bindings,
            gaps,
            reconciled_at: Some(chrono::Utc::now().to_rfc3339()),
        };
        state.snapshot = snapshot.clone();
        Ok(snapshot)
    }

    async fn reconcile_spool(
        &self,
        root: &NativeRoot,
        spool: &Path,
        retire_hooks: bool,
        found: &mut BTreeSet<NodeKey>,
        gaps: &mut BTreeSet<String>,
    ) -> Result<()> {
        // Node publication and replay watermarks share one state transaction.
        // Restore them even if a cancelled pass already retired its hook.
        if let Some(nodes) = self.state.lock().await.spool_nodes.get(spool) {
            for node in nodes {
                ensure!(
                    found.contains(node) || found.len() < MAX_GRAPH_ITEMS,
                    "native spool node limit"
                );
                found.insert(node.clone());
            }
        }
        {
            let state = self.state.lock().await;
            if state.inventory_cycle_active && state.completed_spools.contains(spool) {
                // Preserve this directory's completed cut while other roots or
                // registration pages catch up in the same inventory epoch.
                for (path, file_gaps) in &state.spool_gaps {
                    if path.parent() == Some(spool) {
                        gaps.extend(file_gaps.iter().cloned());
                    }
                }
                return Ok(());
            }
        }
        // Scan the directory with bounded memory, then advance in lexical
        // pages. Retained proxy and historical hook files must not permanently
        // occupy the first page and starve later invocations.
        ensure!(
            tokio::fs::canonicalize(spool).await? == spool,
            "native spool directory identity changed"
        );
        let after = self.state.lock().await.spool_after.get(spool).cloned();
        let mut entries = tokio::fs::read_dir(spool).await?;
        let mut files = BTreeSet::new();
        let mut more = false;
        let mut scanned = 0;
        while let Some(entry) = entries.next_entry().await? {
            scanned += 1;
            ensure!(scanned <= 100_000, "native spool directory entry limit");
            if entry.file_type().await?.is_file()
                && entry.path().extension().is_some_and(|ext| ext == "jsonl")
                && after.as_ref().is_none_or(|after| entry.path() > *after)
            {
                files.insert(entry.path());
                if files.len() > 128 {
                    files.pop_last();
                    more = true;
                }
            }
        }
        let unfinished: Vec<_> = self
            .state
            .lock()
            .await
            .spool_gaps
            .iter()
            .filter(|(path, file_gaps)| {
                path.parent() == Some(spool) && file_gaps.contains("native_spool_backlog")
            })
            .map(|(path, _)| path.clone())
            .collect();
        for path in unfinished {
            if !tokio::fs::symlink_metadata(&path)
                .await
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
                && let Some(file_gaps) = self.state.lock().await.spool_gaps.get_mut(&path)
            {
                // A vanished pending file cannot make further import progress.
                // Its durable extent keeps the missing tail visible on reopen.
                file_gaps.remove("native_spool_backlog");
                file_gaps.insert("native_spool_tail_unavailable".into());
            }
        }
        let last = files.last().cloned();
        for path in files {
            let mut file_gaps = BTreeSet::new();
            if let Err(error) = self
                .reconcile_spool_file(root, &path, retire_hooks, found, &mut file_gaps)
                .await
            {
                tracing::warn!(%error, "native spool file could not be reconciled");
                file_gaps.insert("native_spool_failed".into());
            }
            let mut state = self.state.lock().await;
            if file_gaps.is_empty() {
                state.spool_gaps.remove(&path);
            } else {
                state.spool_gaps.insert(path, file_gaps);
            }
        }
        let mut state = self.state.lock().await;
        if more {
            if let Some(last) = last {
                state.spool_after.insert(spool.into(), last);
            }
            gaps.insert("native_spool_backlog".into());
        } else {
            state.spool_after.remove(spool);
        }
        for (path, file_gaps) in &state.spool_gaps {
            if path.parent() == Some(spool) {
                gaps.extend(file_gaps.iter().cloned());
            }
        }
        if state.inventory_cycle_active
            && !more
            && !state.spool_gaps.iter().any(|(path, file_gaps)| {
                path.parent() == Some(spool) && file_gaps.contains("native_spool_backlog")
            })
        {
            state.completed_spools.insert(spool.into());
        }
        Ok(())
    }

    async fn reconcile_spool_file(
        &self,
        root: &NativeRoot,
        path: &Path,
        retire_hooks: bool,
        found: &mut BTreeSet<NodeKey>,
        gaps: &mut BTreeSet<String>,
    ) -> Result<()> {
        let format = match root.harness {
            Harness::Codex => SourceFormat::CodexAppServer,
            Harness::ClaudeCode
                if path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("cli-")) =>
            {
                SourceFormat::ClaudeCli
            }
            Harness::ClaudeCode => SourceFormat::ClaudeHook,
        };
        let descriptor = SourceDescriptor {
            namespace: root.namespace.clone(),
            harness: root.harness,
            format,
            locator: format!("spool:{}", path.to_string_lossy()),
            node: None,
        };
        if format == SourceFormat::ClaudeCli {
            // A cancelled import may commit its extent before its first raw
            // record. Keep that unknown process in the candidate inventory.
            self.remember_process_source(&self.store.source_id(&descriptor)?, gaps)
                .await;
        }
        let imported = import_spool(
            &self.store,
            path.parent().context("spool directory missing")?,
            path,
            descriptor,
        )
        .await?;
        gaps.extend(imported.gaps);
        let source = imported.source;
        let mut start = self
            .state
            .lock()
            .await
            .replayed
            .get(&source.id)
            .copied()
            .unwrap_or(0);
        if let Some(known) = self.state.lock().await.replay_gaps.get(&source.id) {
            gaps.extend(known.iter().cloned());
        }
        let replay_end = (start + 128).min(source.cursor.next_sequence);
        if replay_end < source.cursor.next_sequence {
            gaps.insert("native_spool_backlog".into());
        }
        while start < replay_end {
            let end = (start + RECORD_PAGE_SIZE).min(replay_end);
            let records = self
                .store
                .records(&SourceRange {
                    source_id: source.id.clone(),
                    generation: source.cursor.generation.clone(),
                    start,
                    end,
                })
                .await?;
            ensure!(
                records.len() as u64 == end - start,
                "native spool records missing"
            );
            let mut page_nodes = BTreeSet::new();
            let mut page_gaps = BTreeSet::new();
            for record in records {
                for node in source_nodes(&source.descriptor, &record, root, &mut page_gaps)? {
                    page_nodes.insert(node);
                }
            }
            let directory = path.parent().context("spool directory missing")?;
            let mut state = self.state.lock().await;
            let cached = state.spool_nodes.entry(directory.into()).or_default();
            ensure!(
                cached.union(&page_nodes).count() <= MAX_GRAPH_ITEMS
                    && found.union(&page_nodes).count() <= MAX_GRAPH_ITEMS,
                "native spool node limit"
            );
            cached.extend(page_nodes.iter().cloned());
            found.extend(page_nodes);
            if !page_gaps.is_empty() {
                state
                    .replay_gaps
                    .entry(source.id.clone())
                    .or_default()
                    .extend(page_gaps.iter().cloned());
                gaps.extend(page_gaps);
            }
            start = end;
            state.replayed.insert(source.id.clone(), start);
        }
        // Only the originating controller retires a hook after durable replay.
        if retire_hooks
            && format == SourceFormat::ClaudeHook
            && source.cursor.next_sequence == 1
            && gaps.is_empty()
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("hook-"))
            && tokio::fs::metadata(path).await?.len() == source.cursor.offset
        {
            tokio::fs::remove_file(path).await?;
            self.state.lock().await.replayed.remove(&source.id);
        }
        Ok(())
    }
}

fn source_nodes(
    source: &SourceDescriptor,
    record: &super::types::StoredRecord,
    root: &NativeRoot,
    gaps: &mut BTreeSet<String>,
) -> Result<BTreeSet<NodeKey>> {
    if source.format != SourceFormat::ClaudeCli {
        return native_nodes(&record.input.raw, root);
    }
    // The same provenance checks gate both facts and transcript discovery.
    // An invalid process envelope must not authorize collecting its session id.
    let mut nodes = BTreeSet::new();
    for fact in super::execution::extract(source, record)? {
        if let super::execution::FactKind::Gap { reason } = &fact.event {
            gaps.insert(reason.clone());
            continue;
        }
        if matches!(&fact.event, super::execution::FactKind::Activity { activity, .. } if activity == "native_user_input")
        {
            continue;
        }
        if let super::execution::FactKind::ConversationReset {
            new_conversation_id,
        } = &fact.event
        {
            let node = NodeKey {
                namespace: root.namespace.clone(),
                harness: root.harness,
                native_id: new_conversation_id.clone(),
                agent_id: None,
            };
            node.validate()?;
            nodes.insert(node);
        }
        nodes.extend(fact.node);
        nodes.extend(fact.related_node);
    }
    Ok(nodes)
}

fn native_nodes(event: &Value, root: &NativeRoot) -> Result<BTreeSet<NodeKey>> {
    let payload = event.get("payload").unwrap_or(&Value::Null);
    let mut candidates = Vec::new();
    if root.harness == Harness::ClaudeCode {
        if let Some(id) = payload.get("session_id").and_then(Value::as_str) {
            candidates.push((id, payload.get("agent_id").and_then(Value::as_str)));
        }
        if payload.get("type").and_then(Value::as_str) == Some("conversation_reset")
            && let Some(id) = payload.get("new_conversation_id").and_then(Value::as_str)
        {
            candidates.push((id, None));
        }
    } else {
        for pointer in ["/threadId", "/thread/id", "/item/agentThreadId"] {
            if let Some(id) = payload.pointer(pointer).and_then(Value::as_str) {
                candidates.push((id, None));
            }
        }
        if let Some(children) = payload
            .pointer("/item/receiverThreadIds")
            .and_then(Value::as_array)
        {
            ensure!(children.len() <= MAX_GRAPH_ITEMS, "native child limit");
            candidates.extend(
                children
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|id| (id, None)),
            );
        }
    }
    let mut nodes = BTreeSet::new();
    for (native_id, agent_id) in candidates {
        let node = NodeKey {
            namespace: root.namespace.clone(),
            harness: root.harness,
            native_id: native_id.into(),
            agent_id: agent_id.map(str::to_owned),
        };
        node.validate()?;
        nodes.insert(node);
    }
    Ok(nodes)
}

#[async_trait]
impl SessionObserver for ControllerEvidence {
    fn initialization_metadata(
        &self,
        metadata: Option<&agent_client_protocol::schema::v1::Meta>,
    ) -> Option<Value> {
        let capability = super::adapter_bridge::capability(
            metadata?.get(super::adapter_bridge::META_KEY)?,
            self.collector.root().harness,
        )?;
        let mut selected = json!({});
        selected[super::adapter_bridge::META_KEY] = capability;
        Some(selected)
    }

    fn task_control_enabled(&self) -> bool {
        true
    }

    async fn task_status(
        &self,
        request: bitrouter_sdk::acp::controller::tasks::TaskStatusRequest,
    ) -> Result<
        bitrouter_sdk::acp::controller::tasks::TaskStatusResponse,
        agent_client_protocol::Error,
    > {
        self.control_task_status(request)
            .await
            .map_err(tasks::control_error)
    }

    async fn task_select(
        &self,
        request: bitrouter_sdk::acp::controller::tasks::TaskSelectRequest,
    ) -> Result<
        bitrouter_sdk::acp::controller::tasks::TaskStatusResponse,
        agent_client_protocol::Error,
    > {
        self.control_task_select(request)
            .await
            .map_err(tasks::selection_error)
    }

    fn notification_fields(&self, method: &str, params: &Value) -> Option<Value> {
        if method == super::adapter_bridge::METHOD {
            Some(super::adapter_bridge::notification_fields(params))
        } else if method == "session/update" {
            Some(params.clone())
        } else if self.collector.root().harness == Harness::ClaudeCode
            && method == super::claude_sdk::METHOD
        {
            super::claude_sdk::notification_fields(params)
        } else {
            None
        }
    }

    async fn observe(
        &self,
        observation: SessionObservation,
    ) -> Result<(), agent_client_protocol::Error> {
        let persist = async {
            let _guard = self.observation_gate.lock().await;
            let (context, scope) = self.observation_context(&observation).await?;
            let mut event = serde_json::to_value(&observation)?;
            if observation.method == "session/prompt"
                && matches!(observation.phase.as_str(), "request" | "response")
            {
                let state = self.state.lock().await;
                let workspace = if observation.phase == "request" && scope == "session" {
                    observation
                        .payload
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .and_then(|id| state.workspaces.get(id))
                        .cloned()
                } else if observation.phase == "response" {
                    state
                        .pending
                        .get(&observation.operation_id)
                        .and_then(|pending| pending.workspace.clone())
                } else {
                    None
                };
                drop(state);
                let mut gaps = BTreeSet::new();
                if workspace
                    .as_ref()
                    .is_some_and(|scope| scope.additional_directories)
                {
                    gaps.insert("workspace_additional_directories_unavailable".into());
                }
                // PendingScope owns the original capture authority even when a
                // later native error makes the response's Query profile unknown.
                let exclusions = workspace.as_ref().map_or_else(
                    || self.workspace_exclusions.clone(),
                    |scope| scope.exclusions.clone(),
                );
                let artifact =
                    super::workspace::capture(workspace.map(|scope| scope.cwd), exclusions, gaps)
                        .await?;
                event["workspace_artifact"] = json!(self.store.save_workspace(artifact).await?);
                let session = if observation.phase == "request" && scope == "session" {
                    Some(super::types::AcpSessionKey {
                        namespace: context.collector.root().namespace.clone(),
                        harness: context.collector.root().harness,
                        session_id: observation
                            .payload
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .context("confirmed ACP session missing")?
                            .into(),
                    })
                } else if observation.phase == "response" {
                    self.store
                        .prompt_session(&self.controller_id, &observation.operation_id)
                        .await?
                } else {
                    None
                };
                if let Some(session) = session {
                    event["native_checkpoint"] = json!(
                        self.capture_native_checkpoint(&observation, session)
                            .await?
                    );
                }
            }
            event["observed_at"] = json!(chrono::Utc::now().to_rfc3339());
            event["native_scope"] = json!(scope);
            let record = context.journal.append_record(event).await?;
            if observation.method == "initialize" && observation.phase == "response" {
                self.state.lock().await.bridge_capable = observation
                    .payload
                    .get("_meta")
                    .and_then(|metadata| metadata.get(super::adapter_bridge::META_KEY))
                    .and_then(|capability| {
                        super::adapter_bridge::capability(capability, self.collector.root().harness)
                    })
                    .is_some();
            }
            if observation.method == super::claude_sdk::METHOD {
                let mut gaps = BTreeSet::new();
                self.remember_sdk_source(&record.source_id, &mut gaps).await;
            }
            self.finish_observation(&observation, context.collector.root(), &record)
                .await?;
            if observation.phase == "response"
                || observation.phase == "disconnect"
                || observation.method == "session/prompt"
                || observation.method == super::claude_sdk::METHOD
                || observation.method == super::adapter_bridge::METHOD
            {
                self.wake.notify_one();
            }
            anyhow::Ok(())
        }
        .await;
        persist.map_err(|error| {
            tracing::error!(%error, "native session observation failed");
            agent_client_protocol::util::internal_error(
                "native session evidence could not be persisted",
            )
        })
    }

    async fn prepare_session_request(
        &self,
        operation_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, agent_client_protocol::Error> {
        if method == "session/prompt" {
            // Prompt metadata does not configure a Query or replace its saved
            // working directory. Its provenance is prepared independently.
            return self
                .prepare_prompt_origin(operation_id, params)
                .await
                .map_err(|error| {
                    tracing::error!(%error, "native prompt origin could not be prepared");
                    agent_client_protocol::util::internal_error(
                        "native prompt provenance unavailable",
                    )
                });
        }
        if self.collector.root().harness != Harness::ClaudeCode {
            // The SDK's generic observer filters _meta. Use the full original
            // request here to account for the adapter's additional-root options.
            if let Some(pending) = self.state.lock().await.pending.get_mut(operation_id) {
                pending.workspace =
                    workspace_scope(&params, self.exclusions_for(self.collector.root()));
            }
            return Ok(params);
        }
        self.prepare_claude_session(operation_id, method, params)
            .await
            .map_err(|error| {
                tracing::error!(%error, "native lifecycle hook preparation failed");
                agent_client_protocol::util::internal_error(
                    "native lifecycle hooks could not be prepared",
                )
            })
    }
}

fn inherited_value(
    key: &str,
    env: &HashMap<String, String>,
    stripped: &[String],
) -> Option<String> {
    env.get(key).cloned().or_else(|| {
        (!stripped.iter().any(|name| name == key))
            .then(|| std::env::var(key).ok())
            .flatten()
    })
}

pub(super) fn native_root(
    harness: Harness,
    env: &HashMap<String, String>,
    stripped: &[String],
) -> Result<NativeRoot> {
    native_root_at(harness, env, stripped, &std::env::current_dir()?)
}

fn native_root_at(
    harness: Harness,
    env: &HashMap<String, String>,
    stripped: &[String],
    cwd: &Path,
) -> Result<NativeRoot> {
    // Native roots follow the harness's own documented environment contract:
    // https://developers.openai.com/codex/config-reference/
    // https://code.claude.com/docs/en/settings
    let key = match harness {
        Harness::Codex => "CODEX_HOME",
        Harness::ClaudeCode => "CLAUDE_CONFIG_DIR",
    };
    let home = match inherited_value(key, env, stripped) {
        Some(value) => {
            ensure!(!value.is_empty(), "native data root is empty");
            PathBuf::from(value)
        }
        None => PathBuf::from(
            inherited_value("HOME", env, stripped)
                .or_else(|| inherited_value("USERPROFILE", env, stripped))
                .context("native user home unavailable")?,
        )
        .join(match harness {
            Harness::Codex => ".codex",
            Harness::ClaudeCode => ".claude",
        }),
    };
    let home = if home.is_absolute() {
        home
    } else {
        cwd.join(home)
    };
    let directory = home.join(match harness {
        Harness::Codex => "sessions",
        Harness::ClaudeCode => "projects",
    });
    let directory = canonical_future_path(&directory)?;
    Ok(NativeRoot {
        namespace: canonical_digest(&(harness, &directory))?,
        harness,
        directory,
    })
}

fn canonical_future_path(path: &Path) -> Result<PathBuf> {
    let mut missing = vec![];
    let mut ancestor = path;
    while !ancestor.exists() {
        missing.push(
            ancestor
                .file_name()
                .context("native root has no existing ancestor")?
                .to_owned(),
        );
        ancestor = ancestor
            .parent()
            .context("native root has no existing ancestor")?;
    }
    let mut result = std::fs::canonicalize(ancestor)?;
    for component in missing.into_iter().rev() {
        result.push(component);
    }
    Ok(result)
}

async fn private_directory(path: &Path) -> Result<()> {
    tokio::fs::create_dir_all(path).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
