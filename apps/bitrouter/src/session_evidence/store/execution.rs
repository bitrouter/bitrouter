//! Derived indexes are checked against the original record on every read.

use sea_orm::Condition;

use super::super::execution::{ExecutionGraph, FactKind, NativeFact, extract};
use super::super::types::{MAX_RECORDS, NodeKey, PARSER_VERSION, RecordRef, SourceFormat};
use super::*;

struct CopiedBoundary {
    base: u64,
    cut: u64,
    metadata: RecordRef,
}

type CopiedSourceCache = BTreeMap<(String, String), Option<CopiedBoundary>>;

mod fact_entity {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "native_execution_facts")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub owner: String,
        pub namespace: String,
        pub parser_version: String,
        pub record_id: String,
        pub digest: String,
        pub node_id: Option<String>,
        pub related_node_id: Option<String>,
        pub fact_json: String,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

impl EvidenceStore {
    pub(super) async fn write_facts(
        &self,
        db: &impl ConnectionTrait,
        source: &SourceDescriptor,
        record: &StoredRecord,
    ) -> Result<()> {
        for fact in extract(source, record)? {
            let digest = canonical_digest(&fact)?;
            let row = fact_entity::ActiveModel {
                id: Set(canonical_digest(&(&self.owner_key, &fact.id))?),
                owner: Set(self.owner_key.clone()),
                namespace: Set(canonical_digest(&(source.harness, &source.namespace))?),
                parser_version: Set(canonical_digest(&PARSER_VERSION)?),
                record_id: Set(fact.record_id.clone()),
                digest: Set(digest.clone()),
                node_id: Set(fact.node.as_ref().map(NodeKey::id).transpose()?),
                related_node_id: Set(fact.related_node.as_ref().map(NodeKey::id).transpose()?),
                fact_json: Set(serde_json::to_string(&fact)?),
            };
            fact_entity::Entity::insert(row)
                .on_conflict(
                    OnConflict::column(fact_entity::Column::Id)
                        .do_nothing()
                        .to_owned(),
                )
                .do_nothing()
                .exec(db)
                .await?;
            let stored =
                fact_entity::Entity::find_by_id(canonical_digest(&(&self.owner_key, &fact.id))?)
                    .one(db)
                    .await?
                    .context("native fact disappeared")?;
            ensure!(
                stored.owner == self.owner_key
                    && stored.digest == digest
                    && stored.namespace == canonical_digest(&(source.harness, &source.namespace))?
                    && stored.parser_version == canonical_digest(&PARSER_VERSION)?
                    && stored.record_id == fact.record_id
                    && stored.node_id == fact.node.as_ref().map(NodeKey::id).transpose()?
                    && stored.related_node_id
                        == fact.related_node.as_ref().map(NodeKey::id).transpose()?
                    && stored.fact_json == serde_json::to_string(&fact)?,
                "immutable native fact conflict"
            );
        }
        Ok(())
    }

    /// Backfill older raw evidence through the current parser. The caller pins
    /// a bounded source range, so unobserved future records cannot enter it.
    pub async fn index_execution_range(&self, range: &SourceRange) -> Result<()> {
        let source = self
            .source(&range.source_id)
            .await?
            .context("native fact source not owned")?;
        let records = self.records(range).await?;
        ensure!(
            records.len() as u64 == range.end - range.start,
            "native fact range is incomplete"
        );
        let transaction = self.db.begin().await?;
        for record in records {
            self.write_facts(&transaction, &source.descriptor, &record)
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn node_facts(
        &self,
        node: &NodeKey,
        after: Option<&str>,
        limit: u64,
    ) -> Result<Vec<NativeFact>> {
        node.validate()?;
        ensure!(
            (1..=MAX_GRAPH_ITEMS as u64).contains(&limit),
            "invalid native fact page"
        );
        let mut query = fact_entity::Entity::find()
            .filter(fact_entity::Column::Owner.eq(&self.owner_key))
            .filter(
                fact_entity::Column::Namespace
                    .eq(canonical_digest(&(node.harness, &node.namespace))?),
            )
            .filter(fact_entity::Column::ParserVersion.eq(canonical_digest(&PARSER_VERSION)?))
            .filter(
                Condition::any()
                    .add(fact_entity::Column::NodeId.eq(node.id()?))
                    .add(fact_entity::Column::RelatedNodeId.eq(node.id()?))
                    .add(fact_entity::Column::NodeId.is_null()),
            )
            .order_by_asc(fact_entity::Column::Id)
            .limit(limit);
        if let Some(id) = after {
            digest_identifier(id)?;
            query =
                query.filter(fact_entity::Column::Id.gt(canonical_digest(&(&self.owner_key, id))?));
        }
        let rows = query.all(&self.db).await?;
        let mut facts = Vec::with_capacity(rows.len());
        for row in rows {
            ensure!(
                row.fact_json.len() <= MAX_OBJECT_BYTES,
                "native fact size limit"
            );
            let fact: NativeFact = serde_json::from_str(&row.fact_json)?;
            ensure!(
                row.owner == self.owner_key
                    && row.digest == canonical_digest(&fact)?
                    && row.namespace == canonical_digest(&(node.harness, &node.namespace))?
                    && row.parser_version == canonical_digest(&PARSER_VERSION)?
                    && row.id == canonical_digest(&(&self.owner_key, &fact.id))?
                    && row.record_id == fact.record_id
                    && row.node_id == fact.node.as_ref().map(NodeKey::id).transpose()?
                    && row.related_node_id
                        == fact.related_node.as_ref().map(NodeKey::id).transpose()?
                    && fact.parser_version == PARSER_VERSION,
                "native fact index is corrupt"
            );
            let record = record_entity::Entity::find_by_id(&fact.record_id)
                .filter(record_entity::Column::Owner.eq(&self.owner_key))
                .one(&self.db)
                .await?
                .context("native fact record unavailable")?;
            ensure!(record.owner == self.owner_key, "foreign native fact record");
            let record = decode_record(record)?;
            let source = self
                .source(&record.source_id)
                .await?
                .context("native fact source unavailable")?;
            ensure!(
                source.descriptor.namespace == node.namespace
                    && source.descriptor.harness == node.harness
                    && extract(&source.descriptor, &record)?.contains(&fact),
                "native fact does not match its evidence"
            );
            facts.push(fact);
        }
        Ok(facts)
    }

    /// Candidate execution relations, not task membership. Callers still need
    /// attempt boundaries and source cuts before attributing work or costs.
    pub async fn execution_graph(&self, roots: &BTreeSet<NodeKey>) -> Result<ExecutionGraph> {
        ensure!(roots.len() <= MAX_GRAPH_ITEMS, "native graph root limit");
        let mut graph = ExecutionGraph::default();
        let mut pending = roots.clone();
        let mut facts = BTreeMap::new();
        let mut scanned = 0;
        let mut copied_sources = BTreeMap::new();
        while let Some(node) = pending.pop_first() {
            if graph.nodes.contains(&node) {
                continue;
            }
            if graph.nodes.len() >= MAX_GRAPH_ITEMS {
                graph.gaps.insert("native_graph_limit".into());
                break;
            }
            graph.nodes.insert(node.clone());
            let mut after = None;
            loop {
                let page = match self.node_facts(&node, after.as_deref(), 128).await {
                    Ok(page) => page,
                    Err(error) => {
                        // This is a replaceable candidate view. Strict fact
                        // reads still reject corruption, while other histories
                        // and SDK observations must remain inspectable.
                        tracing::warn!(%error, "native candidate graph evidence is invalid");
                        graph.gaps.insert("native_graph_evidence_invalid".into());
                        break;
                    }
                };
                if page.is_empty() {
                    break;
                }
                scanned += page.len();
                if scanned > MAX_RECORDS {
                    graph.gaps.insert("native_graph_fact_limit".into());
                    return Ok(finish_graph(graph, facts));
                }
                for fact in &page {
                    match self.copied_fact_metadata(fact, &mut copied_sources).await {
                        Ok(Some(metadata)) => {
                            // Keep immutable node_facts readable; this is an
                            // execution view with verified context boundaries.
                            graph
                                .inherited_records
                                .insert(fact.record_id.clone(), metadata);
                            continue;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(%error,"native copied fact boundary unavailable");
                            graph.gaps.insert("native_graph_evidence_invalid".into());
                        }
                    }
                    match &fact.event {
                        FactKind::Relation {
                            relation: EdgeKind::Spawn,
                        } if fact.related_node.as_ref() == Some(&node) => {
                            if let Some(child) = &fact.node {
                                pending.insert(child.clone());
                            }
                        }
                        FactKind::AgentCall { .. } if fact.node.as_ref() == Some(&node) => {
                            if let Some(child) = &fact.related_node {
                                pending.insert(child.clone());
                            }
                        }
                        FactKind::ConversationReset {
                            new_conversation_id,
                        } if fact.node.as_ref() == Some(&node) => {
                            // Reset names another native transcript while the
                            // adapter retains its ACP session. Discover it as a
                            // candidate, without assigning task membership or
                            // treating the previous conversation as its parent.
                            // https://github.com/agentclientprotocol/claude-agent-acp/blob/main/src/acp-agent.ts
                            let reset = NodeKey {
                                namespace: node.namespace.clone(),
                                harness: node.harness,
                                native_id: new_conversation_id.clone(),
                                agent_id: None,
                            };
                            reset.validate()?;
                            pending.insert(reset);
                        }
                        FactKind::Gap { reason } => {
                            graph.gaps.insert(reason.clone());
                        }
                        _ => {}
                    }
                    facts.insert(fact.id.clone(), fact.clone());
                    if facts.len() >= MAX_RECORDS {
                        graph.gaps.insert("native_graph_fact_limit".into());
                        return Ok(finish_graph(graph, facts));
                    }
                }
                after = page.last().map(|fact| fact.id.clone());
                if page.len() < 128 {
                    break;
                }
            }
        }
        Ok(finish_graph(graph, facts))
    }

    async fn copied_fact_metadata(
        &self,
        fact: &NativeFact,
        cache: &mut CopiedSourceCache,
    ) -> Result<Option<RecordRef>> {
        let Some(reference) = fact.record.as_ref().filter(|reference| {
            fact.source_format == Some(SourceFormat::CodexRollout) && reference.range.start > 0
        }) else {
            return Ok(None);
        };
        let key = (
            reference.range.source_id.clone(),
            reference.range.generation.clone(),
        );
        if !cache.contains_key(&key) {
            ensure!(cache.len() < MAX_GRAPH_ITEMS, "native copied source limit");
            let first = self
                .records(&SourceRange {
                    start: 0,
                    end: 1,
                    ..reference.range.clone()
                })
                .await?;
            let first = first.first().context("native source metadata missing")?;
            let raw = &first.input.raw;
            let source = self
                .source(&reference.range.source_id)
                .await?
                .context("native source missing")?;
            let boundary = if raw["type"] == "session_meta"
                && source
                    .descriptor
                    .node
                    .as_ref()
                    .is_some_and(|node| raw["payload"]["id"].as_str() == Some(&node.native_id))
            {
                raw["ordinal"]
                    .as_u64()
                    .zip(
                        raw.pointer("/payload/subagent_history_start_ordinal")
                            .and_then(serde_json::Value::as_u64),
                    )
                    .filter(|(base, _)| {
                        *base
                            == raw
                                .pointer("/payload/history_base/end_ordinal_exclusive")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0)
                    })
                    .map(|(base, cut)| {
                        Ok::<_, anyhow::Error>(CopiedBoundary {
                            base,
                            cut,
                            metadata: RecordRef::from_record(first)?,
                        })
                    })
                    .transpose()?
            } else {
                None
            };
            cache.insert(key.clone(), boundary);
        }
        let Some(Some(boundary)) = cache.get(&key) else {
            return Ok(None);
        };
        // Full-history child files contain copied parent metadata and bookends.
        // Filter only records proven inside that prefix; parser/index bytes do
        // not change, and own-source invalid metadata keeps its original gap.
        // https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/protocol/src/protocol.rs
        let records = self.records(&reference.range).await?;
        let record = records.first().context("native copied record missing")?;
        ensure!(
            RecordRef::from_record(record)? == *reference,
            "native copied record changed"
        );
        Ok(record.input.raw["ordinal"]
            .as_u64()
            .filter(|ordinal| {
                *ordinal < boundary.cut
                    && boundary.base.checked_add(reference.range.start) == Some(*ordinal)
            })
            .map(|_| boundary.metadata.clone()))
    }
}

fn finish_graph(mut graph: ExecutionGraph, facts: BTreeMap<String, NativeFact>) -> ExecutionGraph {
    let mut children = BTreeMap::<NodeKey, BTreeSet<NodeKey>>::new();
    let mut parents = BTreeMap::<NodeKey, BTreeSet<NodeKey>>::new();
    for fact in facts.values() {
        if matches!(
            fact.event,
            FactKind::Relation {
                relation: EdgeKind::Spawn
            }
        ) && let (Some(child), Some(parent)) = (&fact.node, &fact.related_node)
        {
            children
                .entry(parent.clone())
                .or_default()
                .insert(child.clone());
            children.entry(child.clone()).or_default();
            parents
                .entry(child.clone())
                .or_default()
                .insert(parent.clone());
            parents.entry(parent.clone()).or_default();
        }
    }
    if parents.values().any(|values| values.len() > 1) {
        graph.gaps.insert("conflicting_native_parent".into());
    }
    // A sidecar may initially omit parentAgentId and later acquire it. Retain
    // both records, but derive current uncertainty from the available relation.
    graph.gaps.remove("native_parent_agent_unknown");
    if graph.nodes.iter().any(|node| {
        node.harness == super::super::types::Harness::ClaudeCode
            && node.agent_id.is_some()
            && parents.get(node).is_none_or(BTreeSet::is_empty)
    }) {
        graph.gaps.insert("native_parent_agent_unknown".into());
    }
    let mut ready: BTreeSet<_> = parents
        .iter()
        .filter(|(_, values)| values.is_empty())
        .map(|(node, _)| node.clone())
        .collect();
    let mut visited = 0;
    while let Some(node) = ready.pop_first() {
        visited += 1;
        for child in children.get(&node).into_iter().flatten() {
            if let Some(remaining) = parents.get_mut(child) {
                remaining.remove(&node);
                if remaining.is_empty() {
                    ready.insert(child.clone());
                }
            }
        }
    }
    if visited != parents.len() {
        graph.gaps.insert("native_spawn_cycle".into());
    }
    graph.facts = facts.into_values().collect();
    let (runs, gaps) = super::super::execution::runs::summarize(&graph.facts);
    graph.codex_runs = runs;
    graph.gaps.extend(gaps);
    graph
}

#[cfg(test)]
mod tests;
