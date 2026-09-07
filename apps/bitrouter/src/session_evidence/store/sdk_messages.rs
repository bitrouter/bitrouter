//! Read SDK event identities from original records. Candidate completeness is
//! checked over raw process ranges, never inferred from an indexed UUID lookup.
//! <https://github.com/agentclientprotocol/claude-agent-acp/blob/main/src/acp-agent.ts>

use super::*;
use crate::session_evidence::{
    claude_sdk,
    execution::{FactKind, extract, validate_claude_message},
    types::{Harness, MAX_RECORDS, NodeKey, RecordRef, SourceFormat},
};
use serde_json::Value;

pub(crate) struct MessageRecord {
    pub source: RegisteredSource,
    pub reference: RecordRef,
    pub scoped: bool,
    pub message_key: String,
    pub acp_session_id: Option<String>,
    pub process_id: Option<String>,
    pub node: NodeKey,
}

pub(crate) struct MessagePage {
    pub messages: Vec<MessageRecord>,
    pub gaps: BTreeSet<String>,
}

#[derive(Default)]
pub(crate) struct NativeCopies {
    pub messages: BTreeMap<String, Vec<MessageRecord>>,
    pub ranges: Vec<SourceRange>,
    pub gaps: BTreeSet<String>,
}

pub(crate) fn has_message_identity(source: &SourceDescriptor, record: &StoredRecord) -> bool {
    identity(source, record).is_ok_and(|identity| identity.is_some())
}

impl EvidenceStore {
    /// Advance by raw sequence even when a record cannot be decoded or matched.
    /// Only one raw body is materialized at a time; results contain references.
    pub(crate) async fn sdk_messages(&self, range: &SourceRange) -> Result<MessagePage> {
        range.validate()?;
        ensure!(range.end - range.start <= 128, "SDK raw page limit");
        let source = self
            .source(&range.source_id)
            .await?
            .context("SDK source missing")?;
        ensure!(
            source.descriptor.format == SourceFormat::Acp,
            "SDK source is not ACP"
        );
        ensure!(
            source.cursor.generation == range.generation
                && source.cursor.next_sequence >= range.end,
            "SDK observation cut changed"
        );
        let mut page = MessagePage {
            messages: Vec::new(),
            gaps: BTreeSet::new(),
        };
        for sequence in range.start..range.end {
            match self.sdk_record(&source, sequence).await {
                Ok(Some(message)) => page.messages.push(message),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "native SDK observation could not be resolved");
                    page.gaps.insert("native_sdk_scope_unresolved".into());
                }
            }
        }
        Ok(page)
    }

    /// Scan every registered process at explicit committed cuts. Missing or
    /// invalid records prevent a negative claim about another matching copy.
    pub(crate) async fn native_message_copies(
        &self,
        sources: &BTreeSet<String>,
        keys: &BTreeSet<String>,
        record_limit: usize,
    ) -> NativeCopies {
        let mut inventory = NativeCopies::default();
        let mut scanned = 0;
        for id in sources {
            let source = match self.source(id).await {
                Ok(Some(source)) if source.descriptor.format == SourceFormat::ClaudeCli => source,
                _ => {
                    inventory.gaps.insert("native_sdk_inventory_invalid".into());
                    continue;
                }
            };
            match self.spool_extent(&source).await {
                Ok(Some(end)) if end <= source.cursor.offset => {}
                Ok(Some(_)) => {
                    inventory
                        .gaps
                        .insert("native_spool_tail_uncollected".into());
                }
                _ => {
                    inventory
                        .gaps
                        .insert("native_spool_extent_unavailable".into());
                }
            }
            for sequence in 0..source.cursor.next_sequence {
                if scanned >= record_limit.min(MAX_RECORDS) {
                    inventory.gaps.insert("native_sdk_inventory_limit".into());
                    return inventory;
                }
                scanned += 1;
                match self.sdk_record(&source, sequence).await {
                    Ok(Some(message)) if keys.contains(&message.message_key) => {
                        inventory
                            .messages
                            .entry(message.message_key.clone())
                            .or_default()
                            .push(message);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(%error, "native SDK process inventory is incomplete");
                        inventory.gaps.insert("native_sdk_inventory_invalid".into());
                    }
                }
            }
            if source.cursor.next_sequence > 0 {
                inventory.ranges.push(SourceRange {
                    source_id: source.id,
                    generation: source.cursor.generation,
                    start: 0,
                    end: source.cursor.next_sequence,
                });
            }
        }
        inventory
    }

    async fn sdk_record(
        &self,
        source: &RegisteredSource,
        sequence: u64,
    ) -> Result<Option<MessageRecord>> {
        let records = self
            .records(&SourceRange {
                source_id: source.id.clone(),
                generation: source.cursor.generation.clone(),
                start: sequence,
                end: sequence + 1,
            })
            .await?;
        let record = records
            .into_iter()
            .next()
            .context("native SDK raw record missing")?;
        if source.descriptor.format == SourceFormat::ClaudeCli {
            ensure!(
                !extract(&source.descriptor, &record)?
                    .iter()
                    .any(|fact| matches!(fact.event, FactKind::Gap { .. })),
                "native process capture gap"
            );
        }
        let Some((key, acp_session_id, process_id, node)) = identity(&source.descriptor, &record)?
        else {
            return Ok(None);
        };
        Ok(Some(MessageRecord {
            source: source.clone(),
            reference: RecordRef::from_record(&record)?,
            scoped: matches!(
                record.input.raw.get("native_scope").and_then(Value::as_str),
                Some("session" | "operation")
            ),
            message_key: key,
            acp_session_id,
            process_id,
            node,
        }))
    }
}

type Identity = (String, Option<String>, Option<String>, NodeKey);

fn identity(source: &SourceDescriptor, record: &StoredRecord) -> Result<Option<Identity>> {
    if source.harness != Harness::ClaudeCode || source.node.is_some() {
        return Ok(None);
    }
    let raw = &record.input.raw;
    if raw.get("phase").and_then(Value::as_str) != Some("notification") {
        return Ok(None);
    }
    let (message, acp_session_id, process_id) = match source.format {
        SourceFormat::Acp
            if raw.get("method").and_then(Value::as_str) == Some(claude_sdk::METHOD) =>
        {
            ensure!(
                record.input.generation == "controller/1",
                "SDK journal generation"
            );
            identifier(
                source
                    .locator
                    .strip_prefix("controller:")
                    .context("SDK controller missing")?,
            )?;
            (
                &raw["payload"]["message"],
                Some(text(&raw["payload"], "sessionId")?),
                None,
            )
        }
        SourceFormat::ClaudeCli
            if raw.get("method").and_then(Value::as_str) == Some("runtime/message") =>
        {
            ensure!(record.input.generation == "spool/1", "SDK spool generation");
            (&raw["payload"], None, Some(text(raw, "process_id")?))
        }
        _ => return Ok(None),
    };
    ensure!(
        message.get("bitrouter_capture_invalid") != Some(&Value::Bool(true)),
        "invalid original SDK fields"
    );
    validate_claude_message(source, record, message)?;
    let uuid = text(message, "uuid")?;
    let selected = claude_sdk::notification_fields(&serde_json::json!({"message":message}))
        .context("unsupported SDK message")?;
    ensure!(
        selected["message"].get("bitrouter_capture_invalid") != Some(&Value::Bool(true)),
        "invalid SDK field shape"
    );
    let node = NodeKey {
        namespace: source.namespace.clone(),
        harness: source.harness,
        native_id: text(message, "session_id")?,
        agent_id: None,
    };
    node.validate()?;
    Ok(Some((
        canonical_digest(&("claude-native-message/1", uuid, &selected["message"]))?,
        acp_session_id,
        process_id,
        node,
    )))
}

fn text(value: &Value, key: &str) -> Result<String> {
    let value = value
        .get(key)
        .and_then(Value::as_str)
        .context("SDK identity missing")?;
    identifier(value)?;
    Ok(value.into())
}
