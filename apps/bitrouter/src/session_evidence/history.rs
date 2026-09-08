//! Resolve native context dependencies at their exact, immutable source cuts.

use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::collector::{CollectedSource, FileBound, NativeCollector, codex_parent};
use super::projection::{Projection, Projector};
use super::store::EvidenceStore;
use super::types::{
    EdgeKind, ExecutionEdge, ForkBinding, Harness, MAX_GRAPH_ITEMS, MAX_RECORDS, NodeKey,
    RECORD_PAGE_SIZE, RegisteredSource, RolloutPair, SourceRange, StoredRecord,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedHistory {
    pub node: NodeKey,
    pub source: Option<CollectedSource>,
    pub projection: Option<Projection>,
    /// Dependencies are context ancestry, not additional task executions.
    pub parent: Option<Box<ResolvedHistory>>,
    pub edge: Option<ExecutionEdge>,
    pub gaps: BTreeSet<String>,
    /// Several observed physical histories may belong to one stable thread.
    /// Their filenames and mtimes do not select an active history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<ResolvedHistory>,
}

impl ResolvedHistory {
    pub(crate) fn missing(node: NodeKey, gap: &str) -> Self {
        Self {
            node,
            source: None,
            projection: None,
            parent: None,
            edge: None,
            gaps: BTreeSet::from([gap.into()]),
            variants: vec![],
        }
    }
}

#[derive(Clone)]
pub struct HistoryResolver {
    store: EvidenceStore,
    collector: NativeCollector,
}

struct Budget {
    nodes: usize,
    records: usize,
    remaining_import: usize,
}

impl HistoryResolver {
    pub fn new(store: EvidenceStore, collector: NativeCollector) -> Self {
        Self { store, collector }
    }

    pub async fn resolve(&self, node: NodeKey) -> Result<ResolvedHistory> {
        self.visit(
            node,
            None,
            &mut BTreeSet::new(),
            &mut Budget {
                nodes: 0,
                records: 0,
                remaining_import: MAX_RECORDS,
            },
        )
        .await
    }

    async fn visit(
        &self,
        node: NodeKey,
        stored: Option<CollectedSource>,
        ancestors: &mut BTreeSet<(String, String)>,
        budget: &mut Budget,
    ) -> Result<ResolvedHistory> {
        node.validate()?;
        if ancestors.len() >= 64 || budget.nodes >= MAX_GRAPH_ITEMS {
            return Ok(ResolvedHistory::missing(node, "history_dependency_limit"));
        }
        budget.nodes += 1;
        let source = if let Some(source) = stored {
            source
        } else {
            let (sources, collection_gaps) = self.observed_sources(&node, budget).await?;
            if sources.is_empty() {
                let mut history = ResolvedHistory::missing(node, "native_history_unavailable");
                history.gaps.extend(collection_gaps);
                return Ok(history);
            }
            if sources.len() > 1 || !collection_gaps.is_empty() {
                let gap = if node.harness == Harness::Codex {
                    "native_active_rollout_unselected"
                } else {
                    "native_history_ambiguous"
                };
                let mut history = ResolvedHistory::missing(node.clone(), gap);
                history.gaps.extend(collection_gaps);
                for source in sources.into_values() {
                    let result = Box::pin(self.visit(
                        node.clone(),
                        Some(source.clone()),
                        &mut ancestors.clone(),
                        budget,
                    ))
                    .await;
                    let variant = match result {
                        Ok(variant) => variant,
                        Err(error) => {
                            tracing::warn!(%error, "native history variant invalid");
                            let mut invalid = ResolvedHistory::missing(
                                node.clone(),
                                "native_history_variant_invalid",
                            );
                            invalid.source = Some(source);
                            invalid
                        }
                    };
                    history.gaps.extend(variant.gaps.iter().cloned());
                    history.variants.push(variant);
                }
                return Ok(history);
            }
            sources
                .into_values()
                .next()
                .context("native source candidate")?
        };
        let node = source
            .source
            .descriptor
            .node
            .clone()
            .context("history source node missing")?;
        let ancestry_key = (
            node.namespace.clone(),
            source
                .rollout_id
                .clone()
                .unwrap_or_else(|| source.source.id.clone()),
        );
        if ancestors.contains(&ancestry_key) {
            return Ok(ResolvedHistory::missing(node, "history_dependency_cycle"));
        }
        let mut history = ResolvedHistory {
            node: node.clone(),
            source: Some(source.clone()),
            projection: None,
            parent: None,
            edge: None,
            gaps: source.gaps.clone(),
            variants: vec![],
        };
        let Some(range) = &source.range else {
            return Ok(history);
        };
        if budget.records + (range.end - range.start) as usize > MAX_RECORDS {
            history.gaps.insert("history_record_limit".into());
            return Ok(history);
        }
        budget.records += (range.end - range.start) as usize;
        let mut projector = Projector::new(node.clone(), source.source.descriptor.format)?;
        if node.harness == Harness::Codex
            && let Some(metadata) = &source.metadata
        {
            match codex_parent(metadata) {
                Ok(Some((parent_rollout, cut))) => {
                    let placeholder = NodeKey {
                        native_id: parent_rollout.clone(),
                        ..node.clone()
                    };
                    let ordinal = cut.end_ordinal_exclusive.context("fork ordinal missing")?;
                    let bytes = cut.end_byte_offset.context("fork byte cut missing")?;
                    let Some(child_rollout) = source.rollout_id.clone() else {
                        history
                            .gaps
                            .insert("native_rollout_identity_unavailable".into());
                        return Ok(history);
                    };
                    let rollouts = RolloutPair {
                        child: child_rollout,
                        parent: parent_rollout.clone(),
                    };
                    let binding_id = ForkBinding::history_key(
                        &node,
                        &placeholder,
                        Some(&rollouts),
                        ordinal,
                        bytes,
                    )?;
                    let mut binding = self.store.fork_binding(&binding_id).await?;
                    // Old ordinary-fork bindings remain immutable. Their key
                    // predates distinct rollout IDs, so only that exact shape
                    // is eligible for this compatibility read.
                    if binding.is_none() && rollouts.child == node.native_id && placeholder != node
                    {
                        binding = self
                            .store
                            .fork_binding(&ForkBinding::key(&node, &placeholder, ordinal, bytes)?)
                            .await?;
                    }
                    let parent_source = match &binding {
                        Some(binding) => Some(self.binding_source(binding).await?),
                        None => {
                            self.parent_source(&node.namespace, &parent_rollout, &cut, budget)
                                .await?
                        }
                    };
                    ancestors.insert(ancestry_key.clone());
                    let parent = match parent_source {
                        Some(parent_source) => {
                            Box::pin(self.visit(
                                placeholder,
                                Some(parent_source),
                                ancestors,
                                budget,
                            ))
                            .await?
                        }
                        None => ResolvedHistory::missing(placeholder, "native_history_unavailable"),
                    };
                    ancestors.remove(&ancestry_key);
                    let parent_node = parent.node.clone();
                    history.gaps.extend(parent.gaps.iter().cloned());
                    if let Some(projection) = &parent.projection {
                        projector.inherit(projection)?;
                    }
                    if let Some(checkpoint) = parent
                        .source
                        .as_ref()
                        .and_then(|source| source.range.clone())
                    {
                        let first = self
                            .store
                            .records(&SourceRange {
                                end: range.start + 1,
                                ..range.clone()
                            })
                            .await?;
                        if binding.is_none()
                            && parent
                                .source
                                .as_ref()
                                .is_some_and(|source| source.gaps.is_empty())
                        {
                            let child_record =
                                first.first().context("fork child metadata missing")?;
                            let parent_source = parent
                                .source
                                .as_ref()
                                .context("fork parent source missing")?;
                            let parent_first = self
                                .store
                                .records(&SourceRange {
                                    end: 1,
                                    ..checkpoint.clone()
                                })
                                .await?;
                            let prefix = self
                                .store
                                .fork_prefix(
                                    &parent_source.source,
                                    parent_first.first().context("parent metadata")?,
                                    ordinal,
                                    bytes,
                                )
                                .await?
                                .context("parent prefix incomplete")?;
                            self.store
                                .bind_fork(&ForkBinding {
                                    id: binding_id,
                                    revision: 0,
                                    child: node.clone(),
                                    parent: parent_node.clone(),
                                    rollouts: Some(rollouts),
                                    end_ordinal_exclusive: ordinal,
                                    end_byte_offset: bytes,
                                    child_record_id: child_record.id.clone(),
                                    child_record_digest: child_record.digest.clone(),
                                    checkpoint: checkpoint.clone(),
                                    record_digests: prefix.record_digests,
                                    observed_path: parent_source.path.clone(),
                                })
                                .await?;
                        }
                        // A physical history base is not necessarily the
                        // logical fork parent. Revert can replace only the base.
                        // https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/protocol/src/protocol.rs
                        let kind = if parent_node == node {
                            Some(EdgeKind::Rewind)
                        } else if metadata
                            .get("forked_from_id")
                            .and_then(serde_json::Value::as_str)
                            == Some(parent_node.native_id.as_str())
                            && metadata
                                .get("forked_from_ordinal_exclusive")
                                .and_then(serde_json::Value::as_u64)
                                == Some(ordinal)
                        {
                            Some(EdgeKind::Fork)
                        } else {
                            None
                        };
                        history.edge = kind.map(|kind| ExecutionEdge {
                            kind,
                            from: parent_node,
                            to: node.clone(),
                            checkpoint: Some(checkpoint),
                            evidence_record_ids: first
                                .into_iter()
                                .map(|record| record.id)
                                .collect(),
                        });
                    }
                    history.parent = Some(Box::new(parent));
                }
                Ok(None) => {
                    if metadata
                        .get("forked_from_id")
                        .is_some_and(|value| !value.is_null())
                    {
                        // Copied history may be sufficient for the current context,
                        // but cannot establish an exact parent execution boundary.
                        history.gaps.insert("fork_checkpoint_unavailable".into());
                    }
                }
                Err(_) => {
                    history.gaps.insert("history_base_unsupported".into());
                }
            }
        }
        let mut start = range.start;
        while start < range.end {
            let end = (start + RECORD_PAGE_SIZE).min(range.end);
            let records = self
                .store
                .records(&SourceRange {
                    start,
                    end,
                    ..range.clone()
                })
                .await?;
            ensure!(
                records.len() as u64 == end - start,
                "native history record disappeared"
            );
            self.store
                .index_execution_range(&SourceRange {
                    start,
                    end,
                    ..range.clone()
                })
                .await?;
            for record in &records {
                projector.push(record)?;
            }
            start = end;
        }
        let projection = projector.finish();
        history.gaps.extend(projection.gaps.iter().cloned());
        history.projection = Some(projection);
        Ok(history)
    }

    async fn observed_sources(
        &self,
        node: &NodeKey,
        budget: &mut Budget,
    ) -> Result<(
        std::collections::BTreeMap<String, CollectedSource>,
        BTreeSet<String>,
    )> {
        let mut gaps = BTreeSet::new();
        let candidates = match self.collector.discover(&node.native_id).await {
            Ok(candidates) => candidates,
            Err(error) => {
                tracing::warn!(%error, "native history discovery failed");
                gaps.insert("native_history_discovery_failed".into());
                vec![]
            }
        };
        let mut sources = std::collections::BTreeMap::new();
        for (candidate, path) in candidates {
            if &candidate != node {
                continue;
            }
            match self
                .collector
                .reconcile_limited(node.clone(), &path, None, &mut budget.remaining_import)
                .await
            {
                Ok(source) => {
                    sources.insert(source.source.id.clone(), source);
                }
                Err(error) => {
                    tracing::warn!(%error, "native history candidate invalid");
                    gaps.insert("native_history_candidate_invalid".into());
                }
            }
        }
        if node.harness == Harness::Codex {
            let (inventory, inventory_gaps) = self
                .store
                .rollout_inventory(&node.namespace, Some(node), None)
                .await?;
            gaps.extend(inventory_gaps);
            for source in inventory {
                if sources.contains_key(&source.id) {
                    continue;
                }
                let restored = match self.stored_current(source.clone()).await {
                    Ok(restored) => restored,
                    Err(error) => {
                        tracing::warn!(%error, "stored native history invalid");
                        CollectedSource {
                            source: source.clone(),
                            range: None,
                            path: None,
                            metadata: None,
                            rollout_id: None,
                            gaps: BTreeSet::from(["native_stored_history_invalid".into()]),
                        }
                    }
                };
                sources.insert(source.id, restored);
            }
        }
        Ok((sources, gaps))
    }

    async fn stored_current(&self, source: RegisteredSource) -> Result<CollectedSource> {
        let mut gaps = BTreeSet::from(["native_source_file_unavailable".into()]);
        let rollout_id = self
            .store
            .rollout_identity(&source.id)
            .await?
            .map(|identity| identity.rollout_id);
        if rollout_id.is_none() {
            gaps.insert("native_rollout_identity_unavailable".into());
        }
        let range = (source.cursor.next_sequence > 0).then(|| SourceRange {
            source_id: source.id.clone(),
            generation: source.cursor.generation.clone(),
            start: 0,
            end: source.cursor.next_sequence,
        });
        let metadata = if let Some(range) = &range {
            let records = self
                .store
                .records(&SourceRange {
                    end: 1,
                    ..range.clone()
                })
                .await?;
            records
                .first()
                .context("stored history metadata missing")?
                .input
                .raw
                .get("payload")
                .cloned()
        } else {
            None
        };
        if source.cursor.generation.contains("/replacement/") {
            gaps.insert("source_replaced_or_truncated".into());
        }
        Ok(CollectedSource {
            source,
            range,
            path: None,
            metadata,
            rollout_id,
            gaps,
        })
    }

    async fn binding_source(&self, binding: &ForkBinding) -> Result<CollectedSource> {
        self.store.verify_fork_binding(binding).await?;
        let source = self
            .store
            .source(&binding.checkpoint.source_id)
            .await?
            .context("bound parent source missing")?;
        ensure!(
            source.descriptor.node.as_ref() == Some(&binding.parent),
            "bound parent identity changed"
        );
        let first = self
            .store
            .records(&SourceRange {
                end: 1,
                ..binding.checkpoint.clone()
            })
            .await?;
        let first = first.first().context("bound parent metadata missing")?;
        let prefix = self
            .store
            .fork_prefix(
                &source,
                first,
                binding.end_ordinal_exclusive,
                binding.end_byte_offset,
            )
            .await?
            .context("bound parent prefix is incomplete")?;
        ensure!(
            prefix.range == binding.checkpoint && prefix.record_digests == binding.record_digests,
            "bound parent records changed"
        );
        Ok(Self::stored_source(
            source,
            first,
            prefix.range,
            binding.observed_path.clone(),
            Some(
                binding
                    .rollouts
                    .as_ref()
                    .map_or(binding.parent.native_id.as_str(), |ids| &ids.parent)
                    .into(),
            ),
        ))
    }

    /// Reconcile available files, then compare every stored candidate for the
    /// requested cut. A tail-only rewrite has the same prefix fingerprint;
    /// differing prefixes cannot be resolved from the reference alone.
    async fn parent_source(
        &self,
        namespace: &str,
        rollout: &str,
        cut: &FileBound,
        budget: &mut Budget,
    ) -> Result<Option<CollectedSource>> {
        let ordinal = cut
            .end_ordinal_exclusive
            .context("parent ordinal missing")?;
        let bytes = cut.end_byte_offset.context("parent byte cut missing")?;
        let mut current = None;
        let (candidates, mut gaps) = match self.collector.discover_rollout(rollout).await {
            Ok(discovery) => discovery,
            Err(error) => {
                tracing::warn!(%error, "parent rollout discovery failed");
                (
                    vec![],
                    BTreeSet::from(["native_rollout_discovery_failed".into()]),
                )
            }
        };
        for (candidate, path) in candidates {
            if candidate.namespace == namespace {
                match self
                    .collector
                    .reconcile_limited(
                        candidate,
                        &path,
                        Some(cut.clone()),
                        &mut budget.remaining_import,
                    )
                    .await
                {
                    Ok(source) => {
                        // Rewriting a later tail does not invalidate an older
                        // verified prefix. Prefix comparison below still marks
                        // conflicting or replacement-only generations.
                        gaps.extend(
                            source
                                .gaps
                                .iter()
                                .filter(|gap| gap.as_str() != "source_replaced_or_truncated")
                                .cloned(),
                        );
                        current = Some(source);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "parent rollout candidate invalid");
                        gaps.insert("native_rollout_candidate_invalid".into());
                    }
                }
            }
        }
        let (inventory, inventory_gaps) = self
            .store
            .rollout_inventory(namespace, None, Some(rollout))
            .await?;
        gaps.extend(inventory_gaps);
        let mut selected: Option<(String, CollectedSource)> = None;
        let mut generations = 0;
        let mut work = 0u64;
        for source in inventory {
            let inspected = async {
                let mut after = None;
                loop {
                    let starts = self
                        .store
                        .generation_starts(&source.id, after.as_deref())
                        .await?;
                    let done = starts.len() < RECORD_PAGE_SIZE as usize;
                    after = starts.last().map(|record| record.id.clone());
                    for first in starts {
                        generations += 1;
                        ensure!(generations <= MAX_GRAPH_ITEMS, "fork generation scan limit");
                        let first_ordinal = first
                            .input
                            .raw
                            .get("ordinal")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(ordinal);
                        work = work.saturating_add(ordinal.saturating_sub(first_ordinal));
                        ensure!(work <= MAX_RECORDS as u64, "fork prefix comparison limit");
                        let Some(prefix) = self
                            .store
                            .fork_prefix(&source, &first, ordinal, bytes)
                            .await?
                        else {
                            gaps.insert("native_rollout_prefix_incomplete".into());
                            continue;
                        };
                        if let Some((fingerprint, selected)) = &mut selected {
                            if fingerprint != &prefix.fingerprint {
                                gaps.insert("fork_history_ambiguous".into());
                                return anyhow::Ok(());
                            }
                            if !selected.gaps.contains("source_replaced_or_truncated") {
                                continue;
                            }
                        }
                        let path = current
                            .as_ref()
                            .filter(|current| current.source.id == source.id)
                            .and_then(|current| current.path.clone());
                        let mut resolved = Self::stored_source(
                            source.clone(),
                            &first,
                            prefix.range,
                            path,
                            Some(rollout.into()),
                        );
                        if first.input.generation.contains("/replacement/") {
                            resolved.gaps.insert("source_replaced_or_truncated".into());
                        }
                        selected = Some((prefix.fingerprint, resolved));
                    }
                    if done {
                        break;
                    }
                }
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = inspected {
                tracing::warn!(%error, "parent rollout prefix invalid");
                gaps.insert("native_rollout_prefix_invalid".into());
            }
        }
        let mut selected = selected.map(|(_, source)| source).or(current);
        if let Some(source) = &mut selected {
            source.gaps.extend(gaps);
        }
        Ok(selected)
    }

    fn stored_source(
        source: RegisteredSource,
        first: &StoredRecord,
        range: SourceRange,
        path: Option<std::path::PathBuf>,
        rollout_id: Option<String>,
    ) -> CollectedSource {
        CollectedSource {
            source,
            range: Some(range),
            path,
            gaps: BTreeSet::new(),
            metadata: first.input.raw.get("payload").cloned(),
            rollout_id,
        }
    }
}

#[cfg(test)]
mod tests;
