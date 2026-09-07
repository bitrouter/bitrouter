//! Native lifecycle facts retain their record provenance. A stop hook is an
//! attempted stop, not proof of completion, and context ancestry is not spawn.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::types::{
    EdgeKind, MAX_GRAPH_ITEMS, NodeKey, PARSER_VERSION, RecordRef, SourceDescriptor, SourceFormat,
    StoredRecord, identifier,
};
use crate::eval::types::canonical_digest;

mod claude;
pub mod input_runs;
pub mod runs;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FactKind {
    Node,
    HistorySource {
        inherited: bool,
    },
    Relation {
        relation: EdgeKind,
    },
    Group {
        group_id: String,
    },
    RunStarted {
        run_id: String,
    },
    RunFinished {
        run_id: String,
        status: String,
    },
    RunAborted {
        run_id: Option<String>,
        reason: String,
    },
    Runtime {
        version: String,
        capabilities: BTreeSet<String>,
    },
    ProcessLifecycle {
        state: String,
        clean: Option<bool>,
        exit_code: Option<i32>,
    },
    NativeCommand {
        command_id: String,
        state: String,
    },
    SessionState {
        state: String,
    },
    BackgroundTasks {
        task_ids: BTreeSet<String>,
    },
    NativeTask {
        task_id: String,
        tool_use_id: Option<String>,
        task_type: Option<String>,
        status: Option<String>,
        background: Option<bool>,
    },
    NativeResult {
        result_id: String,
        command_id: Option<String>,
        status: String,
        is_error: bool,
    },
    ConversationReset {
        new_conversation_id: String,
    },
    SpawnedBy {
        tool_call_id: String,
    },
    AgentCall {
        call_id: String,
        tool: String,
        status: String,
        turn_id: Option<String>,
    },
    Activity {
        activity: String,
        native_id: Option<String>,
    },
    StopAttempt {
        prompt_id: Option<String>,
        /// Hook arrays describe the owning session, including SubagentStop.
        session_background_work: Option<bool>,
    },
    Gap {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeFact {
    pub id: String,
    pub parser_version: String,
    pub record_id: String,
    pub record_digest: String,
    pub source_id: String,
    /// Present on current parser output; older frozen facts retain their bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<RecordRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_format: Option<SourceFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_id: Option<String>,
    /// ACP attachment of this specific SDK observation, separate from the
    /// native conversation node (which can change after a reset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_session_id: Option<String>,
    /// The execution being described. Relations point from related_node to node.
    pub node: Option<NodeKey>,
    pub related_node: Option<NodeKey>,
    pub event: FactKind,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionGraph {
    pub nodes: BTreeSet<NodeKey>,
    pub facts: Vec<NativeFact>,
    #[serde(default)]
    pub codex_runs: Vec<runs::CodexRun>,
    pub gaps: BTreeSet<String>,
}

struct Extractor<'a> {
    source: &'a SourceDescriptor,
    record: &'a StoredRecord,
    reference: RecordRef,
    facts: BTreeMap<String, NativeFact>,
    process_id: Option<String>,
    acp_session_id: Option<String>,
}

/// Extraction failure is durable uncertainty, never a reason to discard a raw
/// native record or prevent its source cursor from advancing.
pub fn extract(source: &SourceDescriptor, record: &StoredRecord) -> Result<Vec<NativeFact>> {
    let mut extractor = Extractor {
        source,
        record,
        reference: RecordRef::from_record(record)?,
        facts: BTreeMap::new(),
        process_id: None,
        acp_session_id: None,
    };
    if extractor.read().is_err() {
        extractor.facts.clear();
        extractor.push(
            source.node.clone(),
            None,
            FactKind::Gap {
                reason: "native_lifecycle_invalid".into(),
            },
        )?;
    }
    Ok(extractor.facts.into_values().collect())
}

pub(crate) fn validate_claude_message(
    source: &SourceDescriptor,
    record: &StoredRecord,
    message: &Value,
) -> Result<()> {
    let mut extractor = Extractor {
        source,
        record,
        reference: RecordRef::from_record(record)?,
        facts: BTreeMap::new(),
        process_id: None,
        acp_session_id: None,
    };
    let node = extractor.node(&text(message, "session_id")?, None)?;
    extractor.claude_message(node, message)?;
    ensure!(!extractor.facts.is_empty(), "unsupported native message");
    Ok(())
}

impl Extractor<'_> {
    fn push(
        &mut self,
        node: Option<NodeKey>,
        related_node: Option<NodeKey>,
        event: FactKind,
    ) -> Result<()> {
        for node in [&node, &related_node].into_iter().flatten() {
            node.validate()?;
            ensure!(
                node.namespace == self.source.namespace && node.harness == self.source.harness,
                "lifecycle source identity mismatch"
            );
        }
        ensure!(
            self.facts.len() < MAX_GRAPH_ITEMS,
            "native lifecycle fact limit"
        );
        let base_id = canonical_digest(&(
            PARSER_VERSION,
            &self.record.id,
            &node,
            &related_node,
            &event,
        ))?;
        // Keep legacy fact identities and serialization stable. Process-aware
        // facts use a separate digest, even when their native session is equal.
        let id = match &self.process_id {
            Some(process) => canonical_digest(&(base_id, process))?,
            None => base_id,
        };
        let id = match &self.acp_session_id {
            Some(session) => canonical_digest(&(id, session))?,
            None => id,
        };
        self.facts.insert(
            id.clone(),
            NativeFact {
                id,
                parser_version: PARSER_VERSION.into(),
                record_id: self.record.id.clone(),
                record_digest: self.record.digest.clone(),
                source_id: self.record.source_id.clone(),
                record: Some(self.reference.clone()),
                source_format: Some(self.source.format),
                process_id: self.process_id.clone(),
                acp_session_id: self.acp_session_id.clone(),
                node,
                related_node,
                event,
            },
        );
        Ok(())
    }

    fn node(&self, id: &str, agent_id: Option<&str>) -> Result<NodeKey> {
        let node = NodeKey {
            namespace: self.source.namespace.clone(),
            harness: self.source.harness,
            native_id: id.into(),
            agent_id: agent_id.map(str::to_owned),
        };
        node.validate()?;
        Ok(node)
    }

    fn relation(&mut self, parent: NodeKey, child: NodeKey, relation: EdgeKind) -> Result<()> {
        ensure!(parent != child, "native node cannot parent itself");
        self.push(Some(child), Some(parent), FactKind::Relation { relation })
    }

    fn read(&mut self) -> Result<()> {
        match self.source.format {
            SourceFormat::CodexRollout => self.codex_rollout(),
            SourceFormat::CodexAppServer => self.codex_event(),
            SourceFormat::ClaudeHook => self.claude_hook(),
            SourceFormat::ClaudeCli => self.claude_cli(),
            SourceFormat::ClaudeAgentMetadata => self.claude_metadata(),
            SourceFormat::Acp => self.claude_sdk(),
            SourceFormat::ClaudeTranscript => Ok(()),
        }
    }

    fn codex_rollout(&mut self) -> Result<()> {
        // SessionMeta.source records actual spawn ancestry. history_base and
        // forked_from_id describe copied context and are deliberately separate.
        // https://github.com/openai/codex/blob/main/codex-rs/protocol/src/protocol.rs
        let raw = &self.record.input.raw;
        let Some(node) = self.source.node.clone() else {
            return Ok(());
        };
        let payload = raw.get("payload").unwrap_or(&Value::Null);
        if raw.get("type").and_then(Value::as_str) == Some("session_meta") {
            ensure!(
                payload.get("id").and_then(Value::as_str) == Some(node.native_id.as_str()),
                "native metadata id mismatch"
            );
            self.push(Some(node.clone()), None, FactKind::Node)?;
            if self.record.input.sequence == 0 {
                // Copied forks can contain parent turn bookends. Referenced
                // interrupted snapshots can also contain synthetic aborts.
                // Neither record is proof that the child executed that turn.
                // https://github.com/openai/codex/blob/50379197779be0e5afcbddb014a34cd1fc08af53/codex-rs/core/src/session/mod.rs
                self.push(
                    Some(node.clone()),
                    None,
                    FactKind::HistorySource {
                        inherited: ["forked_from_id", "history_base"]
                            .iter()
                            .any(|key| payload.get(*key).is_some_and(|value| !value.is_null())),
                    },
                )?;
            }
            if let Some(parent) = payload.get("parent_thread_id").and_then(Value::as_str) {
                self.relation(self.node(parent, None)?, node.clone(), EdgeKind::Spawn)?;
            }
            if let Some(parent) = payload
                .pointer("/source/subagent/thread_spawn/parent_thread_id")
                .and_then(Value::as_str)
            {
                self.relation(self.node(parent, None)?, node.clone(), EdgeKind::Spawn)?;
            }
            if let Some(group) = payload.get("session_id").and_then(Value::as_str) {
                identifier(group)?;
                self.push(
                    Some(node),
                    None,
                    FactKind::Group {
                        group_id: group.into(),
                    },
                )?;
            }
        } else if raw.get("type").and_then(Value::as_str) == Some("event_msg") {
            match payload.get("type").and_then(Value::as_str) {
                Some("task_started" | "turn_started") => self.push(
                    Some(node),
                    None,
                    FactKind::RunStarted {
                        run_id: text(payload, "turn_id")?,
                    },
                )?,
                Some("task_complete" | "turn_complete") => self.push(
                    Some(node),
                    None,
                    FactKind::RunFinished {
                        run_id: text(payload, "turn_id")?,
                        status: if payload.get("error").is_some_and(|error| !error.is_null()) {
                            "failed"
                        } else {
                            "completed"
                        }
                        .into(),
                    },
                )?,
                Some("turn_aborted") => {
                    // Older rollouts omit turn_id. Preserve that observation;
                    // neither record adjacency nor a timestamp selects its turn.
                    // https://github.com/openai/codex/blob/50379197779be0e5afcbddb014a34cd1fc08af53/codex-rs/protocol/src/protocol.rs
                    let run_id = optional_text(payload, "turn_id")?;
                    let reason = text(payload, "reason")?;
                    ensure!(
                        matches!(
                            reason.as_str(),
                            "interrupted" | "replaced" | "review_ended" | "budget_limited"
                        ),
                        "unsupported native abort reason"
                    );
                    self.push(Some(node), None, FactKind::RunAborted { run_id, reason })?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn codex_event(&mut self) -> Result<()> {
        // App Server thread.parentThreadId is spawn ancestry; sessionId is a
        // mutable grouping identifier. Tool completion is not agent completion.
        // https://learn.chatgpt.com/docs/app-server
        let raw = &self.record.input.raw;
        if raw.get("direction").and_then(Value::as_str) != Some("server") {
            return Ok(());
        }
        let payload = raw.get("payload").unwrap_or(&Value::Null);
        if payload.get("error_code").is_some() {
            return Ok(());
        }
        let method = raw.get("method").and_then(Value::as_str).unwrap_or("");
        let phase = raw.get("phase").and_then(Value::as_str).unwrap_or("");
        if let Some(thread) = payload.get("thread")
            && (phase == "response" || method == "thread/started")
        {
            let node = self.node(&text(thread, "id")?, None)?;
            self.push(Some(node.clone()), None, FactKind::Node)?;
            if let Some(parent) = thread.get("parentThreadId").and_then(Value::as_str) {
                self.relation(self.node(parent, None)?, node.clone(), EdgeKind::Spawn)?;
            }
            if let Some(group) = thread.get("sessionId").and_then(Value::as_str) {
                identifier(group)?;
                self.push(
                    Some(node.clone()),
                    None,
                    FactKind::Group {
                        group_id: group.into(),
                    },
                )?;
            }
            if thread.get("ephemeral").and_then(Value::as_bool) == Some(true) {
                self.push(
                    Some(node),
                    None,
                    FactKind::Gap {
                        reason: "native_history_ephemeral".into(),
                    },
                )?;
            }
        }
        if matches!(method, "turn/started" | "turn/completed") && phase == "notification" {
            let node = self.node(&text(payload, "threadId")?, None)?;
            let turn = payload.get("turn").context("native turn missing")?;
            let run_id = text(turn, "id")?;
            let event = if method == "turn/started" {
                ensure!(
                    turn.get("status").and_then(Value::as_str) == Some("inProgress"),
                    "invalid started turn status"
                );
                FactKind::RunStarted { run_id }
            } else {
                let status = text(turn, "status")?;
                ensure!(
                    matches!(status.as_str(), "completed" | "interrupted" | "failed"),
                    "unknown terminal status"
                );
                FactKind::RunFinished { run_id, status }
            };
            self.push(Some(node), None, event)?;
        }
        if matches!(method, "item/started" | "item/completed") && phase == "notification" {
            let item = payload.get("item").context("native item missing")?;
            if item.get("type").and_then(Value::as_str) == Some("collabAgentToolCall") {
                let parent = self.node(&text(item, "senderThreadId")?, None)?;
                let children = item
                    .get("receiverThreadIds")
                    .and_then(Value::as_array)
                    .context("native receivers missing")?;
                ensure!(
                    children.len() <= MAX_GRAPH_ITEMS / 2,
                    "native receiver limit"
                );
                let tool = text(item, "tool")?;
                let status = text(item, "status")?;
                let call_id = text(item, "id")?;
                let turn_id = payload
                    .get("turnId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(turn_id) = &turn_id {
                    identifier(turn_id)?;
                }
                if children.is_empty() {
                    self.push(
                        Some(parent.clone()),
                        None,
                        FactKind::AgentCall {
                            call_id: call_id.clone(),
                            tool: tool.clone(),
                            status: status.clone(),
                            turn_id: turn_id.clone(),
                        },
                    )?;
                }
                for child in children {
                    let child = self.node(child.as_str().context("native receiver id")?, None)?;
                    self.push(
                        Some(parent.clone()),
                        Some(child.clone()),
                        FactKind::AgentCall {
                            call_id: call_id.clone(),
                            tool: tool.clone(),
                            status: status.clone(),
                            turn_id: turn_id.clone(),
                        },
                    )?;
                    if tool == "spawnAgent" && status == "completed" {
                        self.relation(parent.clone(), child, EdgeKind::Spawn)?;
                    }
                }
            } else if item.get("type").and_then(Value::as_str) == Some("subAgentActivity") {
                let node = self.node(&text(item, "agentThreadId")?, None)?;
                self.push(
                    Some(node),
                    None,
                    FactKind::Activity {
                        activity: text(item, "kind")?,
                        native_id: Some(text(item, "id")?),
                    },
                )?;
            }
        }
        Ok(())
    }

    fn claude_hook(&mut self) -> Result<()> {
        // Stop/SubagentStop can be blocked by another hook or leave background
        // work running. Preserve that observation without inventing a run end.
        // https://code.claude.com/docs/en/hooks
        let raw = &self.record.input.raw;
        let payload = raw.get("payload").unwrap_or(&Value::Null);
        let Some(id) = payload.get("session_id").and_then(Value::as_str) else {
            return Ok(());
        };
        let node = self.node(id, payload.get("agent_id").and_then(Value::as_str))?;
        let prompt_id = payload
            .get("prompt_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(id) = &prompt_id {
            identifier(id)?;
        }
        match raw.get("method").and_then(Value::as_str) {
            Some("Stop" | "SubagentStop") => {
                let session_background_work = match (
                    payload.get("background_tasks"),
                    payload.get("session_crons"),
                ) {
                    (Some(Value::Array(tasks)), Some(Value::Array(crons))) => {
                        Some(!tasks.is_empty() || !crons.is_empty())
                    }
                    _ => None,
                };
                self.push(
                    Some(node),
                    None,
                    FactKind::StopAttempt {
                        prompt_id,
                        session_background_work,
                    },
                )?;
            }
            Some("UserPromptSubmit") => {
                if let Some(run_id) = prompt_id {
                    self.push(Some(node), None, FactKind::RunStarted { run_id })?;
                }
            }
            Some(
                "SessionStart" | "SubagentStart" | "SessionEnd" | "StopFailure" | "PreCompact"
                | "PostCompact",
            ) => {
                self.push(
                    Some(node),
                    None,
                    FactKind::Activity {
                        activity: text(raw, "method")?,
                        native_id: prompt_id,
                    },
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    fn claude_metadata(&mut self) -> Result<()> {
        // The native SDK reads parentAgentId and toolUseId from agent-*.meta.json.
        // Missing parent metadata is not evidence that a child is depth one.
        // https://code.claude.com/docs/en/agent-sdk/session-storage
        let child = self
            .source
            .node
            .clone()
            .context("agent metadata node missing")?;
        ensure!(
            child.agent_id.is_some(),
            "agent metadata belongs to a child"
        );
        self.push(Some(child.clone()), None, FactKind::Node)?;
        let raw = &self.record.input.raw;
        let parent_agent = match raw.get("parentAgentId") {
            Some(Value::Null) => None,
            Some(Value::String(parent)) => Some(parent.as_str()),
            _ => {
                self.push(
                    Some(child),
                    None,
                    FactKind::Gap {
                        reason: "native_parent_agent_unknown".into(),
                    },
                )?;
                return Ok(());
            }
        };
        let parent = self.node(&child.native_id, parent_agent)?;
        self.relation(parent.clone(), child.clone(), EdgeKind::Spawn)?;
        if let Some(tool) = raw.get("toolUseId").and_then(Value::as_str) {
            identifier(tool)?;
            self.push(
                Some(child),
                Some(parent),
                FactKind::SpawnedBy {
                    tool_call_id: tool.into(),
                },
            )?;
        }
        Ok(())
    }
}

fn text(value: &Value, key: &str) -> Result<String> {
    let text = value
        .get(key)
        .and_then(Value::as_str)
        .context("native lifecycle field missing")?;
    identifier(text)?;
    Ok(text.into())
}

fn optional_text(value: &Value, key: &str) -> Result<Option<String>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => text(value, key).map(Some),
    }
}

#[cfg(test)]
mod tests;
