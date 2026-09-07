//! Resolve native context dependencies at their exact, immutable source cuts.

use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::collector::{CollectedSource, FileBound, NativeCollector, codex_parent};
use super::projection::{Projection, Projector};
use super::store::EvidenceStore;
use super::types::{
    EdgeKind, ExecutionEdge, ForkBinding, Harness, MAX_GRAPH_ITEMS, MAX_RECORDS, NodeKey,
    RECORD_PAGE_SIZE, RegisteredSource, SourceRange, StoredRecord,
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
}

impl HistoryResolver {
    pub fn new(store: EvidenceStore, collector: NativeCollector) -> Self {
        Self { store, collector }
    }

    pub async fn resolve(&self, node: NodeKey) -> Result<ResolvedHistory> {
        self.visit(
            node,
            None,
            None,
            &mut BTreeSet::new(),
            &mut Budget {
                nodes: 0,
                records: 0,
            },
        )
        .await
    }

    async fn visit(
        &self,
        node: NodeKey,
        bound: Option<FileBound>,
        stored: Option<CollectedSource>,
        ancestors: &mut BTreeSet<NodeKey>,
        budget: &mut Budget,
    ) -> Result<ResolvedHistory> {
        node.validate()?;
        if ancestors.contains(&node) {
            return Ok(ResolvedHistory::missing(node, "history_dependency_cycle"));
        }
        if ancestors.len() >= 64 || budget.nodes >= MAX_GRAPH_ITEMS {
            return Ok(ResolvedHistory::missing(node, "history_dependency_limit"));
        }
        budget.nodes += 1;
        let source = if let Some(source) = stored {
            source
        } else if let Some(bound) = &bound {
            match self.parent_source(&node, bound).await? {
                Some(source) => source,
                None => return Ok(ResolvedHistory::missing(node, "native_history_unavailable")),
            }
        } else {
            let candidates: Vec<_> = self
                .collector
                .discover(&node.native_id)
                .await?
                .into_iter()
                .filter(|(candidate, _)| candidate == &node)
                .collect();
            if candidates.len() != 1 {
                return Ok(ResolvedHistory::missing(
                    node,
                    if candidates.is_empty() {
                        "native_history_unavailable"
                    } else {
                        "native_history_ambiguous"
                    },
                ));
            }
            let (_, path) = candidates.first().context("native source candidate")?;
            self.collector.reconcile(node.clone(), path, None).await?
        };
        let mut history = ResolvedHistory {
            node: node.clone(),
            source: Some(source.clone()),
            projection: None,
            parent: None,
            edge: None,
            gaps: source.gaps.clone(),
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
                Ok(Some((native_id, cut))) => {
                    let parent_node = NodeKey {
                        native_id,
                        ..node.clone()
                    };
                    let ordinal = cut.end_ordinal_exclusive.context("fork ordinal missing")?;
                    let bytes = cut.end_byte_offset.context("fork byte cut missing")?;
                    let binding_id = ForkBinding::key(&node, &parent_node, ordinal, bytes)?;
                    let binding = self.store.fork_binding(&binding_id).await?;
                    let stored = match &binding {
                        Some(binding) => Some(self.binding_source(binding).await?),
                        None => None,
                    };
                    ancestors.insert(node.clone());
                    let parent = Box::pin(self.visit(
                        parent_node.clone(),
                        Some(cut),
                        stored,
                        ancestors,
                        budget,
                    ))
                    .await?;
                    ancestors.remove(&node);
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
                        if binding.is_none() && parent.gaps.is_empty() {
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
                        history.edge = Some(ExecutionEdge {
                            kind: EdgeKind::Fork,
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

    async fn binding_source(&self, binding: &ForkBinding) -> Result<CollectedSource> {
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
        ))
    }

    /// Reconcile available files, then compare every stored candidate for the
    /// requested cut. A tail-only rewrite has the same prefix fingerprint;
    /// differing prefixes cannot be resolved from the reference alone.
    async fn parent_source(
        &self,
        node: &NodeKey,
        cut: &FileBound,
    ) -> Result<Option<CollectedSource>> {
        let ordinal = cut
            .end_ordinal_exclusive
            .context("parent ordinal missing")?;
        let bytes = cut.end_byte_offset.context("parent byte cut missing")?;
        let mut current = None;
        for (candidate, path) in self.collector.discover(&node.native_id).await? {
            if &candidate == node {
                current = Some(
                    self.collector
                        .reconcile(node.clone(), &path, Some(cut.clone()))
                        .await?,
                );
            }
        }
        let mut selected: Option<(String, CollectedSource)> = None;
        let mut generations = 0;
        let mut work = 0u64;
        for source in self.store.node_sources(node).await? {
            if source.descriptor.format != super::types::SourceFormat::CodexRollout {
                continue;
            }
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
                        continue;
                    };
                    if let Some((fingerprint, selected)) = &mut selected {
                        if fingerprint != &prefix.fingerprint {
                            selected.gaps.insert("fork_history_ambiguous".into());
                            return Ok(Some(selected.clone()));
                        }
                        if !selected.gaps.contains("source_replaced_or_truncated") {
                            continue;
                        }
                    }
                    let path = current
                        .as_ref()
                        .filter(|current| current.source.id == source.id)
                        .and_then(|current| current.path.clone());
                    let mut resolved =
                        Self::stored_source(source.clone(), &first, prefix.range, path);
                    if first.input.generation.contains("/replacement/") {
                        resolved.gaps.insert("source_replaced_or_truncated".into());
                    }
                    selected = Some((prefix.fingerprint, resolved));
                }
                if done {
                    break;
                }
            }
        }
        Ok(selected.map(|(_, source)| source).or(current))
    }

    fn stored_source(
        source: RegisteredSource,
        first: &StoredRecord,
        range: SourceRange,
        path: Option<std::path::PathBuf>,
    ) -> CollectedSource {
        CollectedSource {
            source,
            range: Some(range),
            path,
            gaps: BTreeSet::new(),
            metadata: first.input.raw.get("payload").cloned(),
        }
    }
}

#[cfg(test)]
mod tests;
