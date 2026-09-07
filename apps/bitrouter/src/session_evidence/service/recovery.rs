//! Recover registered evidence without resurrecting a native process or Query.
//! Controller metadata in the owned database authorizes each root and spool;
//! directory contents and transcript-supplied paths cannot register new roots.

use super::*;
use crate::session_evidence::types::{RegisteredSource, identifier};

const SOURCE_PAGE: u64 = 16;
const REPLAY_BUDGET: u64 = 128;

#[derive(Default)]
enum Phase {
    #[default]
    Roots,
    Records,
    Complete,
}

#[derive(Clone)]
pub(super) struct RecoveredRoot {
    pub(super) collector: NativeCollector,
    pub(super) spool: PathBuf,
}

#[derive(Clone)]
struct Replay {
    source: RegisteredSource,
    next: u64,
    /// Only a correlated load/resume may supply an omitted response id.
    pending: BTreeMap<String, (String, String)>,
}

#[derive(Default)]
pub(super) struct RecoveryState {
    epoch: u64,
    phase: Phase,
    after: Option<String>,
    roots: BTreeMap<String, RecoveredRoot>,
    replay: Option<Replay>,
    nodes: BTreeSet<NodeKey>,
    gaps: BTreeSet<String>,
}

pub(super) struct Recovered {
    pub(super) collectors: BTreeMap<String, NativeCollector>,
    pub(super) nodes: BTreeSet<NodeKey>,
}

impl ControllerEvidence {
    pub(super) async fn recover(&self, gaps: &mut BTreeSet<String>, epoch: u64) -> Recovered {
        let mut recovery = self.recovery.lock().await;
        if recovery.epoch != epoch {
            recovery.epoch = epoch;
            if matches!(recovery.phase, Phase::Complete) {
                // A different live controller can commit and retire a hook after
                // our previous sweep. Continuously rotate the registry as well as
                // the filesystem, including source ids before the previous cursor.
                recovery.phase = Phase::Roots;
                recovery.after = None;
            }
        }
        let result = match recovery.phase {
            Phase::Roots => self.recover_roots(&mut recovery).await,
            Phase::Records => self.recover_records(&mut recovery).await,
            Phase::Complete => Ok(()),
        };
        let advanced = result.is_ok();
        if let Err(error) = result {
            tracing::warn!(%error, "historical evidence recovery could not advance");
            gaps.insert("native_recovery_failed".into());
        }
        if !matches!(recovery.phase, Phase::Complete) {
            gaps.insert("native_recovery_backlog".into());
            if advanced {
                self.wake.notify_one();
            }
        }
        gaps.extend(recovery.gaps.iter().cloned());
        // A former process may still be flushing its proxy/hook files. Import
        // complete records through the same cursor CAS, and keep its files:
        // another controller may still own their retirement. A conflict is
        // visible and retried; no old journal is opened for writing.
        let roots: Vec<_> = recovery.roots.values().cloned().collect();
        for root in roots {
            if let Err(error) = self
                .reconcile_spool(
                    root.collector.root(),
                    &root.spool,
                    false,
                    &mut recovery.nodes,
                    gaps,
                )
                .await
            {
                tracing::warn!(%error, "registered historical spool could not be recovered");
                gaps.insert("native_recovery_spool_failed".into());
            }
        }
        Recovered {
            collectors: recovery
                .roots
                .values()
                .map(|root| {
                    (
                        root.collector.root().namespace.clone(),
                        root.collector.clone(),
                    )
                })
                .collect(),
            nodes: recovery.nodes.clone(),
        }
    }

    async fn recover_roots(&self, recovery: &mut RecoveryState) -> Result<()> {
        let sources = self
            .store
            .source_inventory(recovery.after.as_deref(), SOURCE_PAGE)
            .await?;
        let count = sources.len();
        for (id, source) in sources {
            let source = match source {
                Ok(source) => source,
                Err(error) => {
                    tracing::warn!(%error, "historical source registration is corrupt");
                    recovery
                        .gaps
                        .insert("native_recovery_registry_invalid".into());
                    recovery.after = Some(id);
                    continue;
                }
            };
            if source.descriptor.format == SourceFormat::Acp
                && source.descriptor.harness == self.collector.root().harness
                && source.descriptor.locator != format!("controller:{}", self.controller_id)
                && source.cursor.next_sequence > 0
            {
                match self.recovered_root(&source).await {
                    Ok(root) => {
                        if recovery.roots.contains_key(&source.id)
                            || recovery.roots.len() < MAX_GRAPH_ITEMS
                        {
                            recovery.roots.insert(source.id.clone(), root);
                        } else {
                            recovery.gaps.insert("native_recovery_root_limit".into());
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "registered historical root is invalid");
                        recovery.gaps.insert("native_recovery_root_invalid".into());
                    }
                }
            }
            recovery.after = Some(source.id.clone());
        }
        if count < SOURCE_PAGE as usize {
            recovery.phase = if recovery.roots.is_empty() {
                Phase::Complete
            } else {
                Phase::Records
            };
            recovery.after = None;
        }
        Ok(())
    }

    pub(super) async fn recovered_root(&self, source: &RegisteredSource) -> Result<RecoveredRoot> {
        ensure!(
            source.descriptor.node.is_none()
                && source.cursor.generation == "controller/1"
                && source.descriptor.format == SourceFormat::Acp
                && source.descriptor.harness == self.collector.root().harness,
            "invalid historical controller source"
        );
        let controller = source
            .descriptor
            .locator
            .strip_prefix("controller:")
            .context("historical controller id missing")?;
        ensure!(
            uuid::Uuid::parse_str(controller)?.to_string() == controller,
            "noncanonical historical controller id"
        );
        let rows = self
            .store
            .records(&SourceRange {
                source_id: source.id.clone(),
                generation: source.cursor.generation.clone(),
                start: 0,
                end: 1,
            })
            .await?;
        let raw = &rows
            .first()
            .context("historical root registration missing")?
            .input
            .raw;
        ensure!(
            raw.get("phase").and_then(Value::as_str) == Some("metadata"),
            "historical source has no root registration"
        );
        let payload = raw
            .get("payload")
            .context("historical registration payload missing")?;
        let directory = path_field(payload, "native_root")?;
        let namespace = string(payload, "namespace")?;
        let harness: Harness = serde_json::from_value(
            payload
                .get("harness")
                .context("historical harness missing")?
                .clone(),
        )?;
        ensure!(
            directory.is_absolute()
                && canonical_future_path(&directory)? == directory
                && harness == source.descriptor.harness
                && namespace == source.descriptor.namespace
                && namespace == canonical_digest(&(harness, &directory))?,
            "historical root identity mismatch"
        );
        let controller_directory = self
            .spool
            .parent()
            .context("controller directory missing")?
            .join(controller);
        let expected = match raw.get("method").and_then(Value::as_str) {
            Some("controller/started") => controller_directory,
            Some("controller/root_registered") => controller_directory.join(format!(
                "root-{}",
                namespace
                    .strip_prefix("sha256:")
                    .context("root digest missing")?
            )),
            _ => anyhow::bail!("unknown historical root registration"),
        };
        let spool = path_field(payload, "spool")?;
        ensure!(
            spool == expected && canonical_future_path(&spool)? == spool,
            "historical spool is outside its registered controller"
        );
        Ok(RecoveredRoot {
            collector: NativeCollector::new(
                self.store.clone(),
                NativeRoot {
                    directory,
                    namespace,
                    harness,
                },
            )?,
            spool,
        })
    }

    async fn recover_records(&self, recovery: &mut RecoveryState) -> Result<()> {
        // Leave the previous page checkpoint intact across cancellation. The
        // next pass replays both the pending-operation map and raw records.
        if let Some(mut replay) = recovery.replay.clone() {
            if let Err(error) = self.replay_historical_source(recovery, &mut replay).await {
                tracing::warn!(%error, "historical native source could not be replayed");
                recovery.gaps.insert("native_recovery_source_failed".into());
                recovery.after = Some(replay.source.id);
                recovery.replay = None;
                return Ok(());
            }
            if replay.next < replay.source.cursor.next_sequence {
                recovery.replay = Some(replay);
                return Ok(());
            }
            recovery.after = Some(replay.source.id);
            recovery.replay = None;
        }
        let sources = self
            .store
            .source_inventory(recovery.after.as_deref(), SOURCE_PAGE)
            .await?;
        let count = sources.len();
        for (id, source) in sources {
            let source = match source {
                Ok(source) => source,
                Err(error) => {
                    tracing::warn!(%error, "historical source registration is corrupt");
                    recovery
                        .gaps
                        .insert("native_recovery_registry_invalid".into());
                    recovery.after = Some(id);
                    continue;
                }
            };
            if self.historical_source_root(recovery, &source).is_some()
                && (source.cursor.next_sequence > 0
                    || source.descriptor.format == SourceFormat::ClaudeCli)
            {
                recovery.replay = Some(Replay {
                    source,
                    next: 0,
                    pending: BTreeMap::new(),
                });
                return Ok(());
            }
            recovery.after = Some(source.id);
        }
        if count < SOURCE_PAGE as usize {
            recovery.phase = Phase::Complete;
        }
        Ok(())
    }

    fn historical_source_root<'a>(
        &self,
        recovery: &'a RecoveryState,
        source: &RegisteredSource,
    ) -> Option<&'a RecoveredRoot> {
        if source.descriptor.harness != self.collector.root().harness {
            return None;
        }
        match source.descriptor.format {
            SourceFormat::Acp => recovery.roots.get(&source.id),
            SourceFormat::ClaudeHook | SourceFormat::ClaudeCli | SourceFormat::CodexAppServer => {
                let path = Path::new(source.descriptor.locator.strip_prefix("spool:")?);
                recovery.roots.values().find(|root| {
                    source.descriptor.namespace == root.collector.root().namespace
                        && path.parent() == Some(root.spool.as_path())
                        && path
                            .extension()
                            .is_some_and(|extension| extension == "jsonl")
                })
            }
            SourceFormat::CodexRollout
            | SourceFormat::ClaudeTranscript
            | SourceFormat::ClaudeAgentMetadata => recovery.roots.values().find(|root| {
                source.descriptor.node.is_some()
                    && source.descriptor.namespace == root.collector.root().namespace
            }),
        }
    }

    async fn replay_historical_source(
        &self,
        recovery: &mut RecoveryState,
        replay: &mut Replay,
    ) -> Result<()> {
        let root = self
            .historical_source_root(recovery, &replay.source)
            .context("historical source registration disappeared")?
            .collector
            .root()
            .clone();
        if replay.source.descriptor.format == SourceFormat::ClaudeCli {
            self.remember_process_source(&replay.source.id, &mut recovery.gaps)
                .await;
        }
        if replay.source.descriptor.format == SourceFormat::Acp
            && replay.source.descriptor.harness == Harness::ClaudeCode
        {
            self.remember_sdk_source(&replay.source.id, &mut recovery.gaps)
                .await;
        }
        if let Some(node) = &replay.source.descriptor.node {
            insert_node(&mut recovery.nodes, node.clone())?;
            // Native projections already validate these source records and
            // derive their execution indexes when resolving the restored node.
            replay.next = replay.source.cursor.next_sequence;
            return Ok(());
        }
        let end = (replay.next + REPLAY_BUDGET).min(replay.source.cursor.next_sequence);
        while replay.next < end {
            let range = SourceRange {
                source_id: replay.source.id.clone(),
                generation: replay.source.cursor.generation.clone(),
                start: replay.next,
                end: (replay.next + RECORD_PAGE_SIZE).min(end),
            };
            let rows = self.store.records(&range).await?;
            ensure!(
                rows.len() as u64 == range.end - range.start,
                "historical source records missing"
            );
            self.store.index_execution_range(&range).await?;
            recovery
                .gaps
                .extend(self.store.index_lifecycle_range(&range).await?);
            for row in rows {
                let raw = &row.input.raw;
                if replay.source.descriptor.format == SourceFormat::Acp
                    && raw.get("method").and_then(Value::as_str)
                        == Some(super::super::claude_sdk::METHOD)
                    && !matches!(
                        raw.get("native_scope").and_then(Value::as_str),
                        Some("operation" | "session")
                    )
                    && !super::super::store::sdk_messages::has_message_identity(
                        &replay.source.descriptor,
                        &row,
                    )
                {
                    recovery.gaps.insert("native_sdk_scope_unresolved".into());
                }
                let nodes = if replay.source.descriptor.format == SourceFormat::Acp {
                    replay_acp(raw, &root, &mut replay.pending, &mut recovery.gaps)
                } else {
                    source_nodes(&replay.source.descriptor, &row, &root, &mut recovery.gaps)
                };
                match nodes {
                    Ok(nodes) => {
                        for node in nodes {
                            insert_node(&mut recovery.nodes, node)?;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "historical native identity is invalid");
                        recovery
                            .gaps
                            .insert("native_recovery_record_invalid".into());
                    }
                }
            }
            replay.next = range.end;
        }
        Ok(())
    }
}

/// ACP history restores identities only. Pending commands, idle/background
/// state, model settings and Query caches belong to their original connection.
/// <https://agentclientprotocol.com/protocol/v1/session-setup>
fn replay_acp(
    raw: &Value,
    root: &NativeRoot,
    pending: &mut BTreeMap<String, (String, String)>,
    gaps: &mut BTreeSet<String>,
) -> Result<BTreeSet<NodeKey>> {
    if raw.get("method").and_then(Value::as_str) == Some("controller/started")
        && raw.pointer("/payload/native_process_capture") == Some(&Value::Bool(false))
    {
        gaps.insert("native_process_capture_unavailable".into());
    }
    let method = raw.get("method").and_then(Value::as_str).unwrap_or("");
    let phase = raw.get("phase").and_then(Value::as_str).unwrap_or("");
    let scope = raw.get("native_scope").and_then(Value::as_str);
    if method == super::super::claude_sdk::METHOD
        && phase == "notification"
        && matches!(scope, Some("operation" | "session"))
    {
        let message = raw
            .pointer("/payload/message")
            .context("SDK message missing")?;
        ensure!(
            message.get("bitrouter_capture_invalid") != Some(&Value::Bool(true)),
            "invalid SDK message fields"
        );
        return native_nodes(&json!({"payload":message}), root);
    }
    if !matches!(
        method,
        "session/new" | "session/load" | "session/resume" | "session/fork"
    ) {
        return Ok(BTreeSet::new());
    }
    // Claude lifecycle responses identify the adapter conversation, not its
    // native transcript. Native SDK/CLI/hook records supply the latter.
    // https://github.com/agentclientprotocol/claude-agent-acp/blob/main/src/acp-agent.ts
    if root.harness == Harness::ClaudeCode {
        return Ok(BTreeSet::new());
    }
    let operation = string(raw, "operation_id")?;
    let payload = raw
        .get("payload")
        .context("historical operation payload missing")?;
    if matches!(method, "session/load" | "session/resume")
        && ((phase == "prepared" && scope != Some("unresolved"))
            || (phase == "request"
                && root.harness == Harness::Codex
                && matches!(scope, Some("controller" | "session" | "operation"))))
    {
        if phase == "prepared" {
            ensure!(
                payload.get("namespace").and_then(Value::as_str) == Some(&root.namespace)
                    && payload.get("native_root") == Some(&json!(root.directory)),
                "historical prepared root mismatch"
            );
        }
        ensure!(
            pending.contains_key(&operation) || pending.len() < MAX_GRAPH_ITEMS,
            "historical operation limit"
        );
        pending.insert(operation, (method.into(), string(payload, "sessionId")?));
    } else if phase == "response" {
        let requested = pending
            .remove(&operation)
            .filter(|(name, _)| name == method);
        if payload.get("error_code").is_none()
            && matches!(scope, Some("controller" | "session" | "operation"))
        {
            let id = payload
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| requested.map(|(_, id)| id));
            if let Some(native_id) = id {
                let node = NodeKey {
                    namespace: root.namespace.clone(),
                    harness: root.harness,
                    native_id,
                    agent_id: None,
                };
                node.validate()?;
                return Ok(BTreeSet::from([node]));
            }
            gaps.insert("native_recovery_session_id_missing".into());
        }
    }
    Ok(BTreeSet::new())
}

fn insert_node(nodes: &mut BTreeSet<NodeKey>, node: NodeKey) -> Result<()> {
    node.validate()?;
    ensure!(
        nodes.contains(&node) || nodes.len() < MAX_GRAPH_ITEMS,
        "recovered node limit"
    );
    nodes.insert(node);
    Ok(())
}

fn string(value: &Value, field: &str) -> Result<String> {
    let value = value
        .get(field)
        .and_then(Value::as_str)
        .context("recovery field missing")?;
    identifier(value)?;
    Ok(value.into())
}

fn path_field(value: &Value, field: &str) -> Result<PathBuf> {
    let value = value
        .get(field)
        .and_then(Value::as_str)
        .context("recovery path missing")?;
    ensure!(!value.is_empty(), "recovery path is empty");
    Ok(value.into())
}

#[cfg(test)]
mod tests;
