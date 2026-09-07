//! Resolve each SDK observation independently. A verified event attachment is
//! not a current Query cache or proof that a task finished.

use super::*;
use crate::session_evidence::store::sdk_messages::MessageRecord;
use crate::session_evidence::types::MAX_RECORDS;

/// A bounded, rotating window. `next` resumes by original source sequence;
/// after the last source the next reconciliation starts another sweep.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SdkBindingPage {
    pub observations: Vec<SdkSessionBinding>,
    pub next: Option<SdkBindingCursor>,
    pub observation_ranges: Vec<SourceRange>,
    pub process_ranges: Vec<SourceRange>,
}

#[derive(Clone)]
struct SdkSourceCut {
    source_id: String,
    generation: String,
    end: u64,
}

#[derive(Clone, Default)]
pub(super) struct SdkInventory {
    sources: Vec<SdkSourceCut>,
    gaps: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdkBindingCursor {
    pub source_id: String,
    pub start: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdkSessionBinding {
    pub observation: RecordRef,
    pub acp_session_id: String,
    pub node: Option<NodeKey>,
    pub process_id: Option<String>,
    /// Exact CLI copies that prove the process/profile of an early notification.
    pub native_records: Vec<RecordRef>,
    pub gaps: BTreeSet<String>,
}

impl ControllerEvidence {
    // Called under the reconcile gate, before any spool reads in this epoch.
    // The proxy syncs captured CLI output before forwarding it to the SDK;
    // an SDK observation in these cuts therefore precedes every directory cut.
    // Later notifications and newly discovered journals wait for the next epoch.
    pub(super) async fn begin_inventory(&self) -> Result<u64> {
        let state = self.state.lock().await;
        if state.inventory_cycle_active {
            return Ok(state.inventory_epoch);
        }
        let mut inventory = SdkInventory::default();
        // Current journals are registered before observation. Their durable
        // cursors remain discoverable if an observer is cancelled after commit
        // but before it updates the SDK source cache.
        let mut source_ids = BTreeSet::new();
        for root in state.roots.values() {
            if root.collector.root().harness == Harness::ClaudeCode {
                source_ids.insert(self.store.source_id(&SourceDescriptor {
                    namespace: root.collector.root().namespace.clone(),
                    harness: Harness::ClaudeCode,
                    format: SourceFormat::Acp,
                    locator: format!("controller:{}", self.controller_id),
                    node: None,
                })?);
            }
        }
        for id in &state.sdk_sources {
            if source_ids.contains(id) || source_ids.len() < MAX_GRAPH_ITEMS {
                source_ids.insert(id.clone());
            } else {
                inventory.gaps.insert("native_sdk_source_limit".into());
            }
        }
        drop(state);
        for id in source_ids {
            match self.store.source(&id).await {
                Ok(Some(source))
                    if source.descriptor.format == SourceFormat::Acp
                        && source.descriptor.harness == Harness::ClaudeCode
                        && source.cursor.generation == "controller/1" =>
                {
                    inventory.sources.push(SdkSourceCut {
                        source_id: id,
                        generation: source.cursor.generation,
                        end: source.cursor.next_sequence,
                    });
                }
                _ => {
                    inventory.gaps.insert("native_sdk_source_invalid".into());
                }
            }
        }
        // Cancellation while reading cuts leaves the previous epoch untouched.
        // Publishing them and starting the epoch is one state transaction.
        let mut state = self.state.lock().await;
        state.inventory_epoch = state
            .inventory_epoch
            .checked_add(1)
            .context("native inventory epoch overflow")?;
        state.inventory_cycle_active = true;
        state.completed_spools.clear();
        state.spool_after.clear();
        state.sdk_inventory = inventory;
        Ok(state.inventory_epoch)
    }

    pub(super) async fn remember_sdk_source(&self, id: &str, gaps: &mut BTreeSet<String>) {
        let mut state = self.state.lock().await;
        if state.sdk_sources.contains(id) || state.sdk_sources.len() < MAX_GRAPH_ITEMS {
            state.sdk_sources.insert(id.into());
        } else {
            state.sdk_source_limit = true;
            gaps.insert("native_sdk_source_limit".into());
        }
    }

    pub(super) async fn sdk_bindings(
        &self,
        processes: &[processes::ProcessBinding],
        gaps: &mut BTreeSet<String>,
    ) -> SdkBindingPage {
        let state = self.state.lock().await;
        let inventory = state.sdk_inventory.clone();
        let sources = inventory.sources;
        gaps.extend(inventory.gaps);
        let cursor = state.sdk_cursor.clone();
        if state.sdk_source_limit {
            gaps.insert("native_sdk_source_limit".into());
        }
        if state.process_source_limit {
            gaps.insert("native_process_source_limit".into());
        }
        drop(state);
        let start = cursor
            .as_ref()
            .and_then(|cursor| {
                sources
                    .iter()
                    .position(|source| source.source_id >= cursor.source_id)
            })
            .unwrap_or_default();
        let mut remaining = 128;
        let mut messages = Vec::new();
        let mut output = SdkBindingPage::default();
        for (index, source) in sources.iter().enumerate().skip(start) {
            output.next = None;
            let offset = cursor
                .as_ref()
                .filter(|cursor| cursor.source_id == source.source_id)
                .map_or(0, |cursor| cursor.start);
            let end = offset.saturating_add(remaining).min(source.end);
            if offset < end {
                let range = SourceRange {
                    source_id: source.source_id.clone(),
                    generation: source.generation.clone(),
                    start: offset,
                    end,
                };
                remaining -= end - offset;
                match self.store.sdk_messages(&range).await {
                    Ok(page) => {
                        gaps.extend(page.gaps);
                        messages.extend(page.messages);
                        output.next = (end < source.end).then(|| SdkBindingCursor {
                            source_id: source.source_id.clone(),
                            start: end,
                        });
                    }
                    Err(error) => {
                        tracing::warn!(%error, "native SDK source could not be read");
                        gaps.insert("native_sdk_source_invalid".into());
                    }
                }
                output.observation_ranges.push(range);
            }
            if output.next.is_some() {
                break;
            }
            output.next = sources.get(index + 1).map(|source| SdkBindingCursor {
                source_id: source.source_id.clone(),
                start: 0,
            });
            if remaining == 0 {
                break;
            }
        }
        let controllers: BTreeSet<_> = messages
            .iter()
            .filter_map(|message| {
                message
                    .source
                    .descriptor
                    .locator
                    .strip_prefix("controller:")
            })
            .collect();
        // Verified origins exclude another controller's process. Unknown
        // origins remain candidates and cannot silently shrink the inventory.
        let native_sources = processes
            .iter()
            .filter(|process| {
                process.configured_by.as_ref().is_none_or(|configuration| {
                    controllers.contains(configuration.controller_id.as_str())
                })
            })
            .map(|process| process.source_id.clone())
            .collect();
        let keys = messages
            .iter()
            .map(|message| message.message_key.clone())
            .collect();
        let inventory = if messages.is_empty() {
            super::super::store::sdk_messages::NativeCopies::default()
        } else {
            self.store
                .native_message_copies(&native_sources, &keys, MAX_RECORDS)
                .await
        };
        let incomplete = !inventory.gaps.is_empty()
            || gaps.iter().any(|gap| {
                gap.starts_with("native_recovery_")
                    || gap.starts_with("native_spool_")
                    || matches!(
                        gap.as_str(),
                        "native_process_source_limit" | "native_sdk_source_limit"
                    )
            });
        gaps.extend(inventory.gaps);
        output.process_ranges = inventory.ranges;
        if !messages.is_empty()
            && (gaps.contains("native_recovery_backlog") || gaps.contains("native_spool_backlog"))
        {
            // Retrying the window at a completed registration cut avoids
            // phase-locking raw pages against recurring recovery sweeps.
            output.next = cursor;
        }
        for message in messages {
            let copies = inventory
                .messages
                .get(&message.message_key)
                .map_or(&[][..], Vec::as_slice);
            match self
                .sdk_binding(message, copies, processes, incomplete)
                .await
            {
                Ok(binding) => {
                    gaps.extend(binding.gaps.iter().cloned());
                    output.observations.push(binding);
                }
                Err(error) => {
                    tracing::warn!(%error, "native SDK attachment could not be verified");
                    gaps.insert("native_sdk_binding_invalid".into());
                }
            }
        }
        output
    }

    async fn sdk_binding(
        &self,
        message: MessageRecord,
        copies: &[MessageRecord],
        processes: &[processes::ProcessBinding],
        incomplete: bool,
    ) -> Result<SdkSessionBinding> {
        self.recovered_root(&message.source).await?;
        let controller = message
            .source
            .descriptor
            .locator
            .strip_prefix("controller:")
            .context("SDK controller missing")?;
        let acp_session_id = message
            .acp_session_id
            .context("ACP session attachment missing")?;
        let scoped = message.scoped;
        let mut binding = SdkSessionBinding {
            observation: message.reference,
            acp_session_id,
            node: scoped.then_some(message.node.clone()),
            process_id: None,
            native_records: Vec::new(),
            gaps: BTreeSet::new(),
        };
        if incomplete {
            binding
                .gaps
                .insert("native_sdk_inventory_incomplete".into());
        }
        let mut candidates = BTreeMap::<String, (NodeKey, Vec<RecordRef>)>::new();
        for copy in copies {
            let Some(process) = processes
                .iter()
                .find(|process| process.source_id == copy.source.id)
            else {
                // Inventory paging may not yet have verified this process.
                binding.gaps.insert("native_sdk_process_unverified".into());
                continue;
            };
            let Some(configuration) = &process.configured_by else {
                binding.gaps.insert("native_sdk_process_unverified".into());
                continue;
            };
            if configuration.controller_id != controller {
                continue;
            }
            ensure!(
                copy.process_id == process.process_id,
                "SDK copy process mismatch"
            );
            if !process.gaps.is_empty()
                || process
                    .session_response
                    .as_ref()
                    .and_then(|response| response.acp_session_id.as_ref())
                    .is_some_and(|id| id != &binding.acp_session_id)
                || (scoped && copy.node != message.node)
            {
                binding.gaps.insert("native_sdk_attachment_conflict".into());
                continue;
            }
            let process_id = copy
                .process_id
                .clone()
                .context("SDK native process missing")?;
            let candidate = candidates
                .entry(process_id)
                .or_insert_with(|| (copy.node.clone(), Vec::new()));
            ensure!(
                candidate.0 == copy.node,
                "native event copies cross conversations"
            );
            candidate.1.push(copy.reference.clone());
        }
        // Multiple saved-configuration processes may emit an identical event.
        // Preserve ambiguity instead of choosing by timestamp or first arrival.
        if candidates.len() > 1 {
            binding.node = None;
            binding.gaps.insert("native_sdk_process_ambiguous".into());
        } else if binding.gaps.is_empty() {
            if let Some((process_id, (node, mut records))) = candidates.pop_first() {
                records.sort_by_key(|record| record.range.start);
                binding.node = Some(node);
                binding.process_id = Some(process_id);
                binding.native_records = records;
            }
        } else if binding
            .gaps
            .iter()
            .any(|gap| gap != "native_sdk_inventory_incomplete")
        {
            binding.node = None;
        }
        if binding.node.is_none() {
            binding.gaps.insert("native_sdk_scope_unresolved".into());
        }
        Ok(binding)
    }
}

#[cfg(test)]
mod tests;
