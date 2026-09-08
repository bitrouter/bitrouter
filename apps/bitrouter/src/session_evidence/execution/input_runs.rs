//! Select execution observations by an input's native identity and connection.
//! A terminal here ends one native input, never its children or an application task.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::{FactKind, NativeFact, extract};
use crate::session_evidence::types::{
    Harness, MAX_GRAPH_ITEMS, MAX_OBJECT_BYTES, MAX_RECORDS, NodeKey, RecordRef, SourceDescriptor,
    SourceFormat, StoredRecord, identifier,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputOutcome {
    Completed,
    Failed,
    Interrupted,
    Cancelled,
    Deferred,
    Stopped,
    Discarded,
    Refused,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputTermination {
    pub state: String,
    pub record: RecordRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputResult {
    pub status: String,
    pub is_error: bool,
    pub terminal_reason: Option<String>,
    pub record: RecordRef,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InputRun {
    /// Only explicitly addressed events, not the whole interval between bookends.
    pub records: Vec<RecordRef>,
    pub starts: Vec<RecordRef>,
    pub terminations: Vec<InputTermination>,
    pub results: Vec<InputResult>,
    /// Call sites belong to this turn; their receivers still need execution ranges.
    pub agent_calls: Vec<NativeFact>,
    pub outcome: Option<InputOutcome>,
    /// Integrity/association gaps for these observations, not all-task coverage.
    pub gaps: BTreeSet<String>,
}

#[derive(Default)]
pub(crate) struct InputRunScanner {
    runs: BTreeMap<(NodeKey, String), InputRun>,
    gaps: BTreeSet<String>,
    records: usize,
    retained_bytes: usize,
}

impl InputRunScanner {
    pub fn push(
        &mut self,
        source: &SourceDescriptor,
        record: &StoredRecord,
        targets: &BTreeSet<String>,
    ) -> Result<()> {
        match source.format {
            SourceFormat::CodexAppServer => self.codex(source, record, targets),
            SourceFormat::ClaudeCli => self.claude(source, record, targets),
            _ => anyhow::bail!("unsupported input execution source"),
        }
    }

    fn selected(
        &mut self,
        source: &SourceDescriptor,
        record: &StoredRecord,
        node: &str,
        id: &str,
    ) -> Result<&mut InputRun> {
        identifier(node)?;
        identifier(id)?;
        let key = (
            NodeKey {
                namespace: source.namespace.clone(),
                harness: source.harness,
                native_id: node.into(),
                agent_id: None,
            },
            id.into(),
        );
        ensure!(
            self.runs.contains_key(&key) || self.runs.len() < MAX_GRAPH_ITEMS,
            "native input execution limit"
        );
        let reference = RecordRef::from_record(record)?;
        // Bookends and call sites also retain references. Account for their
        // serialized size separately below; source records themselves stay in DB.
        self.retained_bytes += serde_json::to_vec(&reference)?.len();
        self.records += 1;
        ensure!(
            self.records <= MAX_RECORDS && self.retained_bytes <= MAX_OBJECT_BYTES,
            "native execution record limit"
        );
        let run = self.runs.entry(key).or_default();
        run.records.push(reference);
        Ok(run)
    }

    fn codex(
        &mut self,
        source: &SourceDescriptor,
        record: &StoredRecord,
        targets: &BTreeSet<String>,
    ) -> Result<()> {
        // App Server multiplexes turns. An item completion does not end its
        // turn, and a collab call does not identify the receiver's resumed turn.
        // https://learn.chatgpt.com/docs/app-server
        let raw = &record.input.raw;
        if raw["direction"] != "server" || raw["phase"] == "response" {
            return Ok(());
        }
        let method = raw["method"].as_str().unwrap_or_default();
        let payload = &raw["payload"];
        let bookend = matches!(method, "turn/started" | "turn/completed");
        let id = if bookend {
            payload.pointer("/turn/id")
        } else {
            payload.get("turnId")
        }
        .and_then(serde_json::Value::as_str);
        let Some(id) = id else {
            if bookend || method.starts_with("item/") || method.starts_with("turn/") {
                self.gaps.insert("native_execution_identity_missing".into());
            }
            return Ok(());
        };
        if !targets.contains(id) {
            return Ok(());
        }
        let Some(node) = payload.get("threadId").and_then(serde_json::Value::as_str) else {
            self.gaps.insert("native_execution_identity_missing".into());
            return Ok(());
        };
        let facts = extract(source, record)?;
        self.retain_facts(&facts)?;
        let run = self.selected(source, record, node, id)?;
        if bookend && raw["phase"] != "notification" {
            run.gaps.insert("native_execution_direction_invalid".into());
        }
        for fact in facts {
            match &fact.event {
                FactKind::Gap { .. } => {
                    run.gaps.insert("native_execution_event_invalid".into());
                }
                FactKind::RunStarted { .. } => {
                    run.starts.push(RecordRef::from_record(record)?);
                }
                FactKind::RunFinished { status, .. } => {
                    run.terminations.push(InputTermination {
                        state: status.clone(),
                        record: RecordRef::from_record(record)?,
                    });
                }
                FactKind::AgentCall { turn_id, .. } => {
                    if fact
                        .node
                        .as_ref()
                        .is_none_or(|parent| parent.native_id != node)
                        || turn_id.as_deref() != Some(id)
                    {
                        run.gaps
                            .insert("native_execution_call_identity_conflict".into());
                    } else {
                        run.agent_calls.push(fact);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn claude(
        &mut self,
        source: &SourceDescriptor,
        record: &StoredRecord,
        targets: &BTreeSet<String>,
    ) -> Result<()> {
        // The adapter's command UUID survives native conversation resets. A
        // result needs its explicit user_message_uuid; adjacency is not proof.
        // https://github.com/agentclientprotocol/claude-agent-acp/blob/3e23c5b960b66a6d2c892e7524c952e731c076a7/src/acp-agent.ts
        // https://code.claude.com/docs/en/agent-sdk/typescript
        let raw = &record.input.raw;
        if raw["method"] != "runtime/message" {
            return Ok(());
        }
        let payload = &raw["payload"];
        let key = match payload["type"].as_str() {
            Some("command_lifecycle") => "command_uuid",
            Some("result") => "user_message_uuid",
            _ => return Ok(()),
        };
        let Some(id) = payload[key].as_str() else {
            self.gaps.insert("native_execution_identity_missing".into());
            return Ok(());
        };
        if !targets.contains(id) {
            return Ok(());
        }
        let node = payload["session_id"]
            .as_str()
            .context("native result session missing")?;
        let terminal_reason = payload
            .get("terminal_reason")
            .filter(|value| !value.is_null())
            .map(|value| {
                let reason = value.as_str().context("invalid native terminal reason")?;
                identifier(reason)?;
                Ok::<_, anyhow::Error>(reason.to_owned())
            })
            .transpose()?;
        // NativeFact predates this optional result field, so its byte count
        // alone does not account for the reason retained by InputResult.
        self.retained_bytes += terminal_reason
            .as_ref()
            .map_or(0, |reason| reason.len() + 32);
        let facts = extract(source, record)?;
        self.retain_facts(&facts)?;
        let run = self.selected(source, record, node, id)?;
        if raw["direction"] != "server" || raw["phase"] != "notification" {
            run.gaps.insert("native_execution_direction_invalid".into());
        }
        for fact in facts {
            match fact.event {
                FactKind::Gap { .. } => {
                    run.gaps.insert("native_execution_event_invalid".into());
                }
                FactKind::NativeCommand { state, .. } if state == "started" => {
                    run.starts.push(RecordRef::from_record(record)?);
                }
                FactKind::NativeCommand { state, .. } if state != "queued" => {
                    run.terminations.push(InputTermination {
                        state,
                        record: RecordRef::from_record(record)?,
                    });
                }
                FactKind::NativeResult {
                    status, is_error, ..
                } => {
                    run.results.push(InputResult {
                        status,
                        is_error,
                        terminal_reason: terminal_reason.clone(),
                        record: RecordRef::from_record(record)?,
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn retain_facts(&mut self, facts: &[NativeFact]) -> Result<()> {
        self.retained_bytes += serde_json::to_vec(facts)?.len();
        ensure!(
            self.retained_bytes <= MAX_OBJECT_BYTES,
            "native execution detail limit"
        );
        Ok(())
    }

    pub fn bind(&mut self, node: &NodeKey, id: &str, input: &RecordRef) -> InputRun {
        let mut run = self
            .runs
            .remove(&(node.clone(), id.into()))
            .unwrap_or_default();
        run.gaps.extend(self.gaps.iter().cloned());
        if run.records.iter().any(|record| {
            record.range.source_id != input.range.source_id
                || record.range.generation != input.range.generation
                || record.range.start <= input.range.start
        }) {
            run.gaps.insert("native_execution_precedes_input".into());
        }
        if run.starts.len() > 1 || run.terminations.len() > 1 || run.results.len() > 1 {
            run.gaps
                .insert("native_execution_bookends_ambiguous".into());
        }
        let outcome = run.derive_outcome(node.harness);
        if run.gaps.is_empty() {
            run.outcome = outcome;
        }
        run
    }
}

impl InputRun {
    fn derive_outcome(&mut self, harness: Harness) -> Option<InputOutcome> {
        let terminal = self.terminations.first()?;
        if terminal.state == "cancelled" && self.starts.is_empty() && self.results.is_empty() {
            // A queued command can be cancelled before dispatch, with no result.
            // SDKControlInterruptResponse: https://www.npmjs.com/package/@anthropic-ai/claude-agent-sdk/v/0.3.257
            return Some(InputOutcome::Cancelled);
        }
        if matches!(terminal.state.as_str(), "discarded" | "refused") {
            if !self.starts.is_empty() || !self.results.is_empty() {
                self.gaps
                    .insert("native_execution_rejection_conflict".into());
            }
            return Some(if terminal.state == "refused" {
                InputOutcome::Refused
            } else {
                InputOutcome::Discarded
            });
        }
        let Some(start) = self.starts.first() else {
            self.gaps.insert("native_execution_start_unobserved".into());
            return None;
        };
        if start.range.start >= terminal.record.range.start {
            self.gaps
                .insert("native_execution_bookends_reversed".into());
        }
        if harness == Harness::Codex {
            return match terminal.state.as_str() {
                "completed" => Some(InputOutcome::Completed),
                "failed" => Some(InputOutcome::Failed),
                "interrupted" => Some(InputOutcome::Interrupted),
                _ => None,
            };
        }
        let Some(result) = self.results.first() else {
            self.gaps
                .insert("native_execution_result_unobserved".into());
            return None;
        };
        if start.range.start >= result.record.range.start {
            self.gaps
                .insert("native_execution_result_precedes_start".into());
        }
        // Completed is emitted after all results caused by the command. A
        // cancelled command can instead have a late result from abort handling.
        // https://github.com/agentclientprotocol/claude-agent-acp/blob/3e23c5b960b66a6d2c892e7524c952e731c076a7/src/acp-agent.ts
        if terminal.state == "completed" && result.record.range.start >= terminal.record.range.start
        {
            self.gaps
                .insert("native_execution_result_follows_completion".into());
        }
        let Some(outcome) = result.outcome() else {
            self.gaps
                .insert("native_execution_terminal_reason_unknown".into());
            return None;
        };
        match terminal.state.as_str() {
            "completed" => Some(outcome),
            "cancelled" => Some(InputOutcome::Interrupted),
            _ => {
                self.gaps
                    .insert("native_execution_terminal_result_conflict".into());
                None
            }
        }
    }
}

impl InputResult {
    fn outcome(&self) -> Option<InputOutcome> {
        // SDKResultSuccess.is_error is boolean (API failures use that variant).
        // TerminalReason also distinguishes deferred and interrupted query loops.
        // https://www.npmjs.com/package/@anthropic-ai/claude-agent-sdk/v/0.3.257
        match self.terminal_reason.as_deref() {
            None | Some("completed") => Some(if self.is_error || self.status != "success" {
                InputOutcome::Failed
            } else {
                InputOutcome::Completed
            }),
            Some("aborted_streaming" | "aborted_tools") => Some(InputOutcome::Interrupted),
            Some("background_requested" | "tool_deferred") => Some(InputOutcome::Deferred),
            Some("stop_hook_prevented" | "hook_stopped") => Some(InputOutcome::Stopped),
            Some(
                "blocking_limit"
                | "rapid_refill_breaker"
                | "prompt_too_long"
                | "image_error"
                | "model_error"
                | "api_error"
                | "malformed_tool_use_exhausted"
                | "max_turns"
                | "budget_exhausted"
                | "structured_output_retry_exhausted"
                | "tool_deferred_unavailable"
                | "turn_setup_failed",
            ) => Some(InputOutcome::Failed),
            Some(_) => None,
        }
    }
}
