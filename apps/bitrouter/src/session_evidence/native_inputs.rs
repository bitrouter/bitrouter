//! Native input receipts retain exact request/acknowledgement provenance.
//! They describe inspected prefixes, not complete attempt membership or settlement.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::adapter_bridge::{Event, PromptOrigin};
use super::execution::input_runs::{InputRun, InputRunScanner};
use super::execution::{FactKind, extract};
use super::service::processes::{ProcessConfiguration, ProcessSessionResponse};
use super::types::{
    Harness, MAX_GRAPH_ITEMS, NodeKey, RecordRef, SourceDescriptor, SourceFormat, SourceRange,
    StoredRecord, identifier,
};

pub mod rollouts;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeAcknowledgement {
    pub state: String,
    pub record: RecordRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeInputBinding {
    pub origin: PromptOrigin,
    pub producer: RecordRef,
    pub node: NodeKey,
    /// Codex turn id or Claude command UUID; neither is a controller RPC id.
    pub native_id: String,
    /// Identifies the native connection/process, including after native reset.
    pub process_id: String,
    pub input: RecordRef,
    pub acknowledgements: Vec<NativeAcknowledgement>,
    pub process_header: RecordRef,
    pub controller_registration: RecordRef,
    pub configuration: Option<ProcessConfiguration>,
    pub session_response: Option<ProcessSessionResponse>,
    #[serde(default)]
    pub execution: InputRun,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_history: Option<rollouts::CodexHistoryEvidence>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NativeInputEvidence {
    pub bindings: Vec<NativeInputBinding>,
    pub inspected: Vec<SourceRange>,
    /// An empty set does not establish complete native, auxiliary or cost coverage.
    pub gaps: BTreeSet<String>,
}

pub(super) struct Receipt {
    pub node: NodeKey,
    pub native_id: String,
    pub input: RecordRef,
    pub acknowledgements: Vec<NativeAcknowledgement>,
    pub execution: InputRun,
    pub codex_history: Option<rollouts::CodexHistoryEvidence>,
}

pub(super) struct ScannedInputs {
    pub receipts: Vec<Receipt>,
    pub gaps: BTreeSet<String>,
    /// Negative evidence must survive local receipt filtering so a sibling
    /// connection cannot make the same native execution appear unique.
    pub ambiguous: BTreeSet<(NodeKey, String)>,
}

struct Request {
    method: String,
    thread: Option<String>,
    reference: RecordRef,
    rollout: rollouts::Request,
}

struct ClaudeInput {
    reference: RecordRef,
    ambiguous: bool,
    nodes: BTreeMap<NodeKey, Vec<NativeAcknowledgement>>,
}

pub(super) struct Scanner<'a> {
    source: &'a SourceDescriptor,
    targets: &'a BTreeSet<String>,
    next: u64,
    pending: BTreeMap<String, Request>,
    poisoned: BTreeSet<String>,
    claude: BTreeMap<String, ClaudeInput>,
    receipts: Vec<Receipt>,
    acknowledgements: usize,
    gaps: BTreeSet<String>,
    executions: InputRunScanner,
    rollouts: rollouts::Scanner,
}

impl<'a> Scanner<'a> {
    pub fn new(source: &'a SourceDescriptor, targets: &'a BTreeSet<String>) -> Self {
        Self {
            source,
            targets,
            next: 0,
            pending: BTreeMap::new(),
            poisoned: BTreeSet::new(),
            claude: BTreeMap::new(),
            receipts: Vec::new(),
            acknowledgements: 0,
            gaps: BTreeSet::new(),
            executions: InputRunScanner::default(),
            rollouts: rollouts::Scanner::default(),
        }
    }

    pub fn push(&mut self, record: &StoredRecord) -> Result<()> {
        let reference = RecordRef::from_record(record)?;
        let raw = &record.input.raw;
        ensure!(
            record.input.generation == "spool/1"
                && record.input.sequence == self.next
                && raw.get("sequence").and_then(Value::as_u64) == Some(self.next),
            "native input scan is not a contiguous spool prefix"
        );
        self.next += 1;
        if raw.get("method").and_then(Value::as_str) == Some("runtime/gap") {
            self.gaps.insert("native_input_capture_gap".into());
        }
        self.executions.push(self.source, record, self.targets)?;
        match self.source.format {
            SourceFormat::CodexAppServer => self.codex(raw, reference),
            SourceFormat::ClaudeCli => self.claude(record, reference),
            _ => anyhow::bail!("unsupported native input source"),
        }
    }

    fn codex(&mut self, raw: &Value, reference: RecordRef) -> Result<()> {
        // App Server request ids are local to one JSON-RPC connection.
        // https://developers.openai.com/codex/app-server
        let method = text(raw, "method")?;
        let direction = raw.get("direction").and_then(Value::as_str);
        let phase = raw.get("phase").and_then(Value::as_str);
        if direction == Some("client") && phase == Some("request") {
            let id = text(raw, "operation_id")?;
            if self.poisoned.contains(id) {
                return Ok(());
            }
            if self.pending.remove(id).is_some() {
                ensure!(
                    self.poisoned.len() < MAX_GRAPH_ITEMS,
                    "native RPC conflict limit"
                );
                self.poisoned.insert(id.into());
                self.rollouts.poison();
                self.gaps.insert("native_input_rpc_ambiguous".into());
                // Do not reuse this id after the first response: another response
                // may still belong to either of the conflicting old requests.
                return Ok(());
            }
            ensure!(
                self.pending.len() < MAX_GRAPH_ITEMS,
                "native pending RPC limit"
            );
            self.pending.insert(
                id.into(),
                Request {
                    method: method.into(),
                    thread: raw
                        .pointer("/payload/threadId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    rollout: self
                        .rollouts
                        .request(method, &raw["payload"], reference.clone())?,
                    reference,
                },
            );
        } else if direction == Some("server") && phase == Some("response") {
            let id = text(raw, "operation_id")?;
            if self.poisoned.contains(id) {
                return Ok(());
            }
            let pending = self.pending.remove(id);
            if let Some(request) = &pending {
                ensure!(request.method == method, "native RPC method mismatch");
            }
            let (pending, codex_history) = if let Some(request) = pending {
                let history = self.rollouts.response(
                    method,
                    &raw["payload"],
                    request.rollout,
                    reference.clone(),
                )?;
                (Some((request.thread, request.reference)), history)
            } else {
                (None, None)
            };
            let turn = raw.pointer("/payload/turn/id").and_then(Value::as_str);
            if method != "turn/start" || !turn.is_some_and(|id| self.targets.contains(id)) {
                return Ok(());
            }
            let (thread, input) = pending.context("native acceptance has no original request")?;
            ensure!(
                raw.pointer("/payload/error_code").is_none()
                    && raw.pointer("/payload/turn/status").and_then(Value::as_str)
                        == Some("inProgress"),
                "native response is not a direct turn acceptance"
            );
            ensure!(
                self.receipts.len() < MAX_GRAPH_ITEMS,
                "native input receipt limit"
            );
            let node = NodeKey {
                namespace: self.source.namespace.clone(),
                harness: Harness::Codex,
                native_id: thread.context("native request thread missing")?,
                agent_id: None,
            };
            node.validate()?;
            self.receipts.push(Receipt {
                node,
                native_id: turn.context("accepted turn missing")?.into(),
                input,
                acknowledgements: vec![NativeAcknowledgement {
                    state: "accepted".into(),
                    record: reference,
                }],
                execution: InputRun::default(),
                codex_history,
            });
        } else if direction == Some("server") && phase == Some("notification") {
            self.rollouts
                .notification(method, &raw["payload"], reference)?;
        }
        Ok(())
    }

    fn claude(&mut self, record: &StoredRecord, reference: RecordRef) -> Result<()> {
        // The input's session_id may be the adapter attachment after a reset.
        // Only CLI command_lifecycle frames supply the native conversation here.
        // https://github.com/agentclientprotocol/claude-agent-acp/blob/3e23c5b960b66a6d2c892e7524c952e731c076a7/src/acp-agent.ts
        let facts = extract(self.source, record)?;
        let raw = &record.input.raw;
        ensure!(
            !facts
                .iter()
                .any(|fact| matches!(fact.event, FactKind::Gap { .. })),
            "native process capture or scope gap"
        );
        if raw.get("method").and_then(Value::as_str) == Some("runtime/input") {
            ensure!(
                raw["phase"] == "request" && raw["direction"] == "client",
                "native input direction mismatch"
            );
            let payload = raw.get("payload").context("native input missing")?;
            ensure!(
                payload["type"] == "user",
                "native input is not a user message"
            );
            let Some(id) = payload
                .get("uuid")
                .and_then(Value::as_str)
                .filter(|id| self.targets.contains(*id))
            else {
                return Ok(());
            };
            if let Some(input) = self.claude.get_mut(id) {
                input.ambiguous = true;
                self.gaps.insert("native_input_command_ambiguous".into());
            } else {
                ensure!(
                    self.claude.len() < MAX_GRAPH_ITEMS,
                    "native command input limit"
                );
                self.claude.insert(
                    id.into(),
                    ClaudeInput {
                        reference,
                        ambiguous: false,
                        nodes: BTreeMap::new(),
                    },
                );
            }
        } else {
            for fact in facts {
                if let FactKind::NativeCommand { command_id, state } = fact.event {
                    if !self.targets.contains(&command_id) {
                        continue;
                    }
                    ensure!(
                        raw["phase"] == "notification" && raw["direction"] == "server",
                        "native acknowledgement direction mismatch"
                    );
                    let input = self
                        .claude
                        .get_mut(&command_id)
                        .context("native command has no original input")?;
                    ensure!(
                        self.acknowledgements < MAX_GRAPH_ITEMS,
                        "native acknowledgement limit"
                    );
                    self.acknowledgements += 1;
                    input
                        .nodes
                        .entry(fact.node.context("native command conversation missing")?)
                        .or_default()
                        .push(NativeAcknowledgement {
                            state,
                            record: reference.clone(),
                        });
                }
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> ScannedInputs {
        for (native_id, input) in self.claude {
            if !input.ambiguous {
                for (node, acknowledgements) in input.nodes {
                    self.receipts.push(Receipt {
                        node,
                        native_id: native_id.clone(),
                        input: input.reference.clone(),
                        acknowledgements,
                        execution: InputRun::default(),
                        codex_history: None,
                    });
                }
            }
        }
        // Reject same-connection ambiguity before materializing execution
        // details. Otherwise many acceptances can clone one large turn log.
        let mut occurrences = BTreeMap::<(NodeKey, String), usize>::new();
        for receipt in &self.receipts {
            if receipt.node.harness == Harness::Codex {
                *occurrences
                    .entry((receipt.node.clone(), receipt.native_id.clone()))
                    .or_default() += 1;
            }
        }
        let ambiguous: BTreeSet<_> = occurrences
            .into_iter()
            .filter_map(|(key, count)| (count > 1).then_some(key))
            .collect();
        if !ambiguous.is_empty() {
            self.gaps.insert("native_input_turn_ambiguous".into());
        }
        self.receipts.retain(|receipt| {
            !ambiguous.contains(&(receipt.node.clone(), receipt.native_id.clone()))
        });
        if self.gaps.contains("native_input_capture_gap") {
            self.receipts.clear();
        }
        for receipt in &mut self.receipts {
            receipt.execution =
                self.executions
                    .bind(&receipt.node, &receipt.native_id, &receipt.input);
        }
        ScannedInputs {
            receipts: self.receipts,
            gaps: self.gaps,
            ambiguous,
        }
    }
}

pub(super) fn target(event: &Event) -> Option<&str> {
    match event {
        Event::CodexAccepted { turn_id, .. } => Some(turn_id),
        Event::ClaudeEnqueued { command_id } => Some(command_id),
        _ => None,
    }
}

pub(super) fn claim_key(event: &Event) -> Option<(String, String)> {
    match event {
        Event::CodexAccepted {
            thread_id, turn_id, ..
        } => Some((thread_id.clone(), turn_id.clone())),
        Event::ClaudeEnqueued { command_id } => Some((String::new(), command_id.clone())),
        _ => None,
    }
}

pub(super) fn matches(event: &Event, receipt: &Receipt) -> bool {
    match event {
        Event::CodexAccepted {
            thread_id, turn_id, ..
        } => {
            receipt.node.harness == Harness::Codex
                && thread_id == &receipt.node.native_id
                && turn_id == &receipt.native_id
        }
        Event::ClaudeEnqueued { command_id } => {
            receipt.node.harness == Harness::ClaudeCode && command_id == &receipt.native_id
        }
        _ => false,
    }
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    let value = value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("native {key} missing"))?;
    identifier(value)?;
    Ok(value)
}

#[cfg(test)]
mod tests;
