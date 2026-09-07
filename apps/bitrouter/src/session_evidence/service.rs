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
    Harness, MAX_GRAPH_ITEMS, NodeKey, RECORD_PAGE_SIZE, SourceDescriptor, SourceFormat,
    SourceRange,
};
use crate::eval::types::canonical_digest;

mod roots;

#[derive(Clone)]
struct RootContext {
    collector: NativeCollector,
    spool: PathBuf,
    journal: Arc<Journal>,
}

#[derive(Clone)]
struct PendingScope {
    namespace: String,
    session_id: Option<String>,
    query_fingerprint: Option<String>,
    method: String,
    query_reused: bool,
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
    pub gaps: BTreeSet<String>,
    pub reconciled_at: Option<String>,
}

#[derive(Default)]
struct LiveState {
    nodes: BTreeSet<NodeKey>,
    replayed: BTreeMap<String, u64>,
    snapshot: CollectionSnapshot,
    roots: BTreeMap<String, RootContext>,
    sessions: BTreeMap<String, String>,
    loaded: BTreeMap<String, LoadedQuery>,
    ambiguous_sessions: BTreeSet<String>,
    uncertain_queries: BTreeSet<String>,
    pending: BTreeMap<String, PendingScope>,
}

pub struct ControllerEvidence {
    store: EvidenceStore,
    collector: NativeCollector,
    root_env: HashMap<String, String>,
    claude_model_config: Option<String>,
    controller_id: String,
    producer_version: String,
    spool: PathBuf,
    executable: PathBuf,
    state: Mutex<LiveState>,
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
            spool,
            executable,
            state: Mutex::new(LiveState {
                roots,
                ..LiveState::default()
            }),
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
        let mut gaps = BTreeSet::new();
        let roots = self.state.lock().await.roots.clone();
        for context in roots.values() {
            if let Err(error) = self.reconcile_spool(context, &mut gaps).await {
                tracing::warn!(%error, "native root spool could not be reconciled");
                gaps.insert("native_spool_failed".into());
            }
        }
        let mut nodes = self.state.lock().await.nodes.clone();
        for node in nodes.clone() {
            let Some(context) = roots.get(&node.namespace) else {
                gaps.insert("native_root_unavailable".into());
                continue;
            };
            if node.harness == Harness::ClaudeCode && node.agent_id.is_none() {
                match context.collector.discover(&node.native_id).await {
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
        let mut histories = Vec::with_capacity(nodes.len());
        for node in nodes {
            let history = if let Some(context) = roots.get(&node.namespace) {
                let resolver = HistoryResolver::new(self.store.clone(), context.collector.clone());
                match resolver.resolve(node.clone()).await {
                    Ok(history) => history,
                    Err(error) => {
                        tracing::warn!(%error, "native execution source could not be reconciled");
                        ResolvedHistory::missing(node, "native_source_failed")
                    }
                }
            } else {
                ResolvedHistory::missing(node, "native_root_unavailable")
            };
            gaps.extend(history.gaps.iter().cloned());
            histories.push(history);
        }
        let mut state = self.state.lock().await;
        if !state.ambiguous_sessions.is_empty() {
            gaps.insert("ambiguous_acp_session_scope".into());
        }
        if !state.uncertain_queries.is_empty() {
            gaps.insert("native_query_scope_unknown".into());
        }
        let snapshot = CollectionSnapshot {
            histories,
            gaps,
            reconciled_at: Some(chrono::Utc::now().to_rfc3339()),
        };
        state.snapshot = snapshot.clone();
        Ok(snapshot)
    }

    async fn reconcile_spool(
        &self,
        context: &RootContext,
        gaps: &mut BTreeSet<String>,
    ) -> Result<()> {
        let root = context.collector.root();
        let mut entries = tokio::fs::read_dir(&context.spool).await?;
        let mut files = vec![];
        while let Some(entry) = entries.next_entry().await? {
            if files.len() == 128 {
                gaps.insert("native_spool_backlog".into());
                break;
            }
            if entry.file_type().await?.is_file()
                && entry.path().extension().is_some_and(|ext| ext == "jsonl")
            {
                files.push(entry.path());
            }
        }
        files.sort();
        for path in files {
            let format = match root.harness {
                Harness::Codex => SourceFormat::CodexAppServer,
                Harness::ClaudeCode => SourceFormat::ClaudeHook,
            };
            let imported = import_spool(
                &self.store,
                &context.spool,
                &path,
                SourceDescriptor {
                    namespace: root.namespace.clone(),
                    harness: root.harness,
                    format,
                    locator: format!("spool:{}", path.to_string_lossy()),
                    node: None,
                },
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
                for record in records {
                    self.observe_nodes(&record.input.raw, root).await?;
                }
                start = end;
                self.state
                    .lock()
                    .await
                    .replayed
                    .insert(source.id.clone(), start);
            }
            // Hooks publish one newline-terminated event per private file.
            // Once that event is durable, retire its transient spool entry so
            // long-lived controllers do not accumulate a permanent file cap.
            if format == SourceFormat::ClaudeHook
                && source.cursor.next_sequence == 1
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("hook-"))
                && tokio::fs::metadata(&path).await?.len() == source.cursor.offset
            {
                tokio::fs::remove_file(&path).await?;
                self.state.lock().await.replayed.remove(&source.id);
            }
        }
        Ok(())
    }

    async fn observe_nodes(&self, event: &Value, root: &NativeRoot) -> Result<()> {
        let payload = event.get("payload").unwrap_or(&Value::Null);
        let mut candidates = Vec::new();
        if root.harness == Harness::ClaudeCode {
            if let Some(id) = payload.get("session_id").and_then(Value::as_str) {
                candidates.push((id, payload.get("agent_id").and_then(Value::as_str)));
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
        let mut state = self.state.lock().await;
        for (native_id, agent_id) in candidates {
            let node = NodeKey {
                namespace: root.namespace.clone(),
                harness: root.harness,
                native_id: native_id.into(),
                agent_id: agent_id.map(str::to_owned),
            };
            node.validate()?;
            ensure!(
                state.nodes.contains(&node) || state.nodes.len() < MAX_GRAPH_ITEMS,
                "native execution node limit"
            );
            state.nodes.insert(node);
        }
        Ok(())
    }
}

#[async_trait]
impl SessionObserver for ControllerEvidence {
    async fn observe(
        &self,
        observation: SessionObservation,
    ) -> Result<(), agent_client_protocol::Error> {
        let persist = async {
            let _guard = self.observation_gate.lock().await;
            let (context, scope) = self.observation_context(&observation).await?;
            let mut event = serde_json::to_value(&observation)?;
            event["observed_at"] = json!(chrono::Utc::now().to_rfc3339());
            event["native_scope"] = json!(scope);
            context.journal.append(event.clone()).await?;
            self.finish_observation(&observation, context.collector.root())
                .await?;
            if observation.phase == "response" || observation.phase == "disconnect" {
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
        if self.collector.root().harness != Harness::ClaudeCode {
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

fn native_root(
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
