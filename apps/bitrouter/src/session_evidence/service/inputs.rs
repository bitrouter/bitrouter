//! Corroborate prompt producer claims with owned native connection prefixes.

use super::*;
use crate::session_evidence::adapter_bridge::{Event, PromptEvidence, ProvenObservation};
use crate::session_evidence::native_inputs::{
    self, NativeInputBinding, NativeInputEvidence, Scanner,
};
use crate::session_evidence::types::{MAX_OBJECT_BYTES, MAX_RECORDS, RegisteredSource};

mod rollouts;

struct Group {
    source: RegisteredSource,
    spool: PathBuf,
    native_root: PathBuf,
    registration: RecordRef,
    prompts: Vec<(String, ProvenObservation)>,
    inspected: Vec<SourceRange>,
    gaps: BTreeSet<String>,
    bindings: Vec<NativeInputBinding>,
}

impl ControllerEvidence {
    pub(super) async fn native_inputs(
        &self,
        prompts: &BTreeMap<String, PromptEvidence>,
        collection_gaps: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, NativeInputEvidence>> {
        let mut output: BTreeMap<_, _> = prompts
            .iter()
            .map(|(id, evidence)| {
                (
                    id.clone(),
                    NativeInputEvidence {
                        gaps: evidence.gaps.clone(),
                        ..Default::default()
                    },
                )
            })
            .collect();
        let mut groups = BTreeMap::<String, Group>::new();
        for (attempt, evidence) in prompts {
            for proven in &evidence.observations {
                if native_inputs::target(&proven.observation.event).is_none() {
                    continue;
                }
                let origin = &proven.observation.origin;
                let id = &origin.request.range.source_id;
                if !groups.contains_key(id) {
                    ensure!(
                        groups.len() < MAX_GRAPH_ITEMS,
                        "native input controller limit"
                    );
                    let source = self
                        .store
                        .source(id)
                        .await?
                        .context("original prompt source missing")?;
                    let root = self.recovered_root(&source).await?;
                    let registration = self
                        .store
                        .records(&SourceRange {
                            source_id: id.clone(),
                            generation: "controller/1".into(),
                            start: 0,
                            end: 1,
                        })
                        .await?
                        .into_iter()
                        .next()
                        .context("original controller registration missing")?;
                    groups.insert(
                        id.clone(),
                        Group {
                            source,
                            spool: root.spool,
                            native_root: root.collector.root().directory.clone(),
                            registration: RecordRef::from_record(&registration)?,
                            prompts: Vec::new(),
                            inspected: Vec::new(),
                            gaps: BTreeSet::new(),
                            bindings: Vec::new(),
                        },
                    );
                }
                let group = groups
                    .get_mut(id)
                    .context("input controller group missing")?;
                ensure!(
                    group.prompts.len() < MAX_GRAPH_ITEMS,
                    "native input producer limit"
                );
                group.prompts.push((attempt.clone(), proven.clone()));
            }
        }
        if groups.is_empty() {
            return Ok(output);
        }
        let (sources, inventory_gaps) = self.input_source_inventory(&groups).await?;
        let mut budget = MAX_RECORDS as u64;
        let mut execution_budget = MAX_OBJECT_BYTES;
        for group in groups.values_mut() {
            group.gaps.extend(inventory_gaps.iter().cloned());
            group.gaps.extend(
                collection_gaps
                    .iter()
                    .filter(|gap| {
                        gap.starts_with("native_recovery_")
                            || gap.starts_with("native_spool_")
                            || gap.as_str() == "native_process_source_limit"
                    })
                    .cloned(),
            );
            if collection_gaps.contains("native_recovery_backlog")
                || collection_gaps.contains("native_spool_backlog")
            {
                group.gaps.insert("native_input_inventory_pending".into());
            }
            if let Err(error) = self
                .corroborate_inputs(group, &sources, &mut budget, &mut execution_budget)
                .await
            {
                tracing::warn!(%error, "native input evidence could not be verified");
                group.gaps.insert("native_input_evidence_invalid".into());
                group.bindings.clear();
            }
            for (attempt, proven) in &group.prompts {
                let selected = output
                    .get_mut(attempt)
                    .context("native input attempt missing")?;
                selected.gaps.extend(group.gaps.iter().cloned());
                for range in &group.inspected {
                    if !selected.inspected.contains(range) {
                        selected.inspected.push(range.clone());
                    }
                }
                let mut found = false;
                for binding in &group.bindings {
                    if binding.producer == proven.record {
                        found = true;
                        ensure!(
                            selected.bindings.len() < MAX_GRAPH_ITEMS,
                            "native attempt input limit"
                        );
                        reserve_details(binding, &mut execution_budget)?;
                        selected.bindings.push(binding.clone());
                    }
                }
                if !found {
                    selected.gaps.insert("native_input_unobserved".into());
                }
            }
        }
        Ok(output)
    }

    async fn input_source_inventory(
        &self,
        groups: &BTreeMap<String, Group>,
    ) -> Result<(Vec<RegisteredSource>, BTreeSet<String>)> {
        let mut after = None;
        let mut count = 0;
        let mut sources = Vec::new();
        let mut gaps = BTreeSet::new();
        loop {
            let rows = self.store.source_inventory(after.as_deref(), 16).await?;
            if rows.is_empty() {
                break;
            }
            for (id, source) in rows {
                if count == MAX_GRAPH_ITEMS {
                    gaps.insert("native_input_source_limit".into());
                    return Ok((sources, gaps));
                }
                count += 1;
                after = Some(id);
                match source {
                    Ok(source)
                        if groups
                            .values()
                            .any(|group| owns(group, &source) || claim_source(group, &source)) =>
                    {
                        sources.push(source)
                    }
                    Ok(_) => {}
                    Err(_) => {
                        gaps.insert("native_input_source_invalid".into());
                    }
                }
            }
        }
        Ok((sources, gaps))
    }

    async fn corroborate_inputs(
        &self,
        group: &mut Group,
        sources: &[RegisteredSource],
        budget: &mut u64,
        execution_budget: &mut usize,
    ) -> Result<()> {
        let targets: BTreeSet<String> = group
            .prompts
            .iter()
            .filter_map(|(_, proven)| {
                native_inputs::target(&proven.observation.event).map(str::to_owned)
            })
            .collect();
        // Check the original controller journal, including old attempts, before
        // associating any reused native identifier with the active prompt.
        let mut claims = BTreeMap::<(String, String), BTreeSet<String>>::new();
        for source in sources {
            if !claim_source(group, source) {
                continue;
            }
            self.recovered_root(source).await?;
            let end = source.cursor.next_sequence;
            ensure!(end <= *budget, "native producer inspection limit");
            *budget -= end;
            let mut start = 0;
            while start < end {
                let range = SourceRange {
                    source_id: source.id.clone(),
                    generation: "controller/1".into(),
                    start,
                    end: (start + RECORD_PAGE_SIZE).min(end),
                };
                let (page, gaps) = self.store.producer_claims(&range, &targets).await?;
                group.gaps.extend(gaps);
                for proven in page {
                    // Notifications can move journals while their immutable
                    // prompt origin remains in its original native profile.
                    if proven.observation.origin.request.range.source_id == group.source.id
                        && let Some(key) = claim_key(&proven.observation.event)
                    {
                        claims
                            .entry(key)
                            .or_default()
                            .insert(proven.observation.origin.request.record_id);
                    }
                }
                start = range.end;
            }
            group.inspected.push(SourceRange {
                source_id: source.id.clone(),
                generation: "controller/1".into(),
                start: 0,
                end,
            });
        }
        for source in sources {
            if !owns(group, source) {
                continue;
            }
            match self.store.spool_extent(source).await {
                Ok(Some(end)) if end == source.cursor.offset => {}
                Ok(Some(end)) if end > source.cursor.offset => {
                    group.gaps.insert("native_input_tail_uncollected".into());
                }
                Ok(None) => {
                    group.gaps.insert("native_input_extent_unknown".into());
                }
                _ => {
                    group.gaps.insert("native_input_extent_invalid".into());
                }
            }
            ensure!(
                source.cursor.generation == "spool/1" && source.cursor.next_sequence > 0,
                "native input source has no prefix"
            );
            let end = source.cursor.next_sequence;
            ensure!(end <= *budget, "native input inspection limit");
            *budget -= end;
            let mut scanner = Scanner::new(&source.descriptor, &targets);
            let mut start = 0;
            let mut header = None;
            while start < end {
                let range = SourceRange {
                    source_id: source.id.clone(),
                    generation: "spool/1".into(),
                    start,
                    end: (start + RECORD_PAGE_SIZE).min(end),
                };
                let records = self.store.records(&range).await?;
                ensure!(
                    records.len() as u64 == range.end - range.start,
                    "native input prefix has missing records"
                );
                for record in records {
                    ensure!(record.source_id == source.id, "native input source changed");
                    if record.input.sequence == 0 {
                        ensure!(
                            (record.input.raw["method"] == "runtime/started"
                                || (source.descriptor.format == SourceFormat::ClaudeCli
                                    && record.input.raw["method"] == "runtime/failed"
                                    && end == 1))
                                && record.input.raw["phase"] == "metadata",
                            "native input process has an invalid start or failed-only prefix"
                        );
                        // Claude's proxy writes one failed metadata record when
                        // spawn fails. The scanner verifies its process/scope;
                        // it supplies no input receipt and cannot precede input.
                        header = Some(RecordRef::from_record(&record)?);
                    }
                    scanner.push(&record)?;
                }
                start = range.end;
            }
            group.inspected.push(SourceRange {
                source_id: source.id.clone(),
                generation: "spool/1".into(),
                start: 0,
                end,
            });
            let (receipts, gaps) = scanner.finish();
            group.gaps.extend(gaps);
            if receipts.is_empty() {
                continue;
            }
            let path = Path::new(
                source
                    .descriptor
                    .locator
                    .strip_prefix("spool:")
                    .context("native spool missing")?,
            );
            let name = path
                .file_stem()
                .and_then(|name| name.to_str())
                .context("native process filename missing")?;
            let (process_id, configuration, session_response) =
                if source.descriptor.format == SourceFormat::ClaudeCli {
                    let process = self.process_binding(source.id.clone()).await;
                    ensure!(
                        process.gaps.is_empty(),
                        "native input process origin unverified"
                    );
                    (
                        process.process_id.context("native process id missing")?,
                        Some(
                            process
                                .configured_by
                                .context("native process configuration missing")?,
                        ),
                        Some(
                            process
                                .session_response
                                .context("native process lifecycle response missing")?,
                        ),
                    )
                } else {
                    ensure!(
                        uuid::Uuid::parse_str(name)?.to_string() == name,
                        "noncanonical native connection id"
                    );
                    (name.to_owned(), None, None)
                };
            for receipt in receipts {
                for (_, proven) in &group.prompts {
                    let observation = &proven.observation;
                    if !native_inputs::matches(&observation.event, &receipt) {
                        continue;
                    }
                    let key =
                        claim_key(&observation.event).context("native claim identity missing")?;
                    if claims.get(&key).is_none_or(|origins| {
                        origins.len() != 1
                            || !origins.contains(&observation.origin.request.record_id)
                    }) {
                        group.gaps.insert("native_input_producer_ambiguous".into());
                        continue;
                    }
                    if let (Some(config), Some(response)) = (&configuration, &session_response)
                        && (config.controller_id != observation.origin.controller_id
                            || response.error_code.is_some()
                            || response.acp_session_id.as_ref()
                                != Some(&observation.origin.session.session_id))
                    {
                        group.gaps.insert("native_input_attachment_conflict".into());
                        continue;
                    }
                    ensure!(
                        group.bindings.len() < MAX_GRAPH_ITEMS,
                        "native input binding limit"
                    );
                    reserve_parts(&receipt.execution, &receipt.codex_history, execution_budget)?;
                    let binding = NativeInputBinding {
                        origin: observation.origin.clone(),
                        producer: proven.record.clone(),
                        node: receipt.node.clone(),
                        native_id: receipt.native_id.clone(),
                        process_id: process_id.clone(),
                        input: receipt.input.clone(),
                        acknowledgements: receipt.acknowledgements.clone(),
                        process_header: header.clone().context("native process header missing")?,
                        controller_registration: group.registration.clone(),
                        configuration: configuration.clone(),
                        session_response: session_response.clone(),
                        execution: receipt.execution.clone(),
                        codex_history: receipt.codex_history.clone(),
                    };
                    group.bindings.push(binding);
                }
            }
        }
        // A Codex turn accepted by several requests is ambiguous even across
        // connections. Claude explicitly retains each process/input occurrence.
        let mut occurrences = BTreeMap::<(NodeKey, String), BTreeSet<String>>::new();
        for binding in &group.bindings {
            if binding.node.harness == Harness::Codex {
                occurrences
                    .entry((binding.node.clone(), binding.native_id.clone()))
                    .or_default()
                    .insert(binding.input.record_id.clone());
            }
        }
        group.bindings.retain(|binding| {
            if occurrences
                .get(&(binding.node.clone(), binding.native_id.clone()))
                .is_some_and(|inputs| inputs.len() > 1)
            {
                group.gaps.insert("native_input_turn_ambiguous".into());
                false
            } else {
                true
            }
        });
        self.corroborate_rollouts(group, budget, execution_budget)
            .await?;
        Ok(())
    }
}

fn reserve_details(binding: &NativeInputBinding, budget: &mut usize) -> Result<()> {
    reserve_parts(&binding.execution, &binding.codex_history, budget)
}

fn reserve_parts(
    execution: &crate::session_evidence::execution::input_runs::InputRun,
    history: &Option<crate::session_evidence::native_inputs::rollouts::CodexHistoryEvidence>,
    budget: &mut usize,
) -> Result<()> {
    // Reserve before each clone into controller groups and the final output.
    // The aggregate bound includes repeated producers and multiple sources.
    let bytes = serde_json::to_vec(execution)?.len() + serde_json::to_vec(history)?.len();
    ensure!(bytes <= *budget, "native execution materialization limit");
    *budget -= bytes;
    Ok(())
}

fn owns(group: &Group, source: &RegisteredSource) -> bool {
    let descriptor = &source.descriptor;
    descriptor.namespace == group.source.descriptor.namespace
        && descriptor.harness == group.source.descriptor.harness
        && descriptor.node.is_none()
        && matches!(
            (descriptor.harness, descriptor.format),
            (Harness::Codex, SourceFormat::CodexAppServer)
                | (Harness::ClaudeCode, SourceFormat::ClaudeCli)
        )
        && descriptor
            .locator
            .strip_prefix("spool:")
            .is_some_and(|path| Path::new(path).parent() == Some(group.spool.as_path()))
}

fn claim_source(group: &Group, source: &RegisteredSource) -> bool {
    source.descriptor.format == SourceFormat::Acp
        && source.descriptor.harness == group.source.descriptor.harness
        && source.descriptor.node.is_none()
        && source.descriptor.locator == group.source.descriptor.locator
}

fn claim_key(event: &Event) -> Option<(String, String)> {
    match event {
        Event::CodexAccepted {
            thread_id, turn_id, ..
        } => Some((thread_id.clone(), turn_id.clone())),
        Event::ClaudeEnqueued { command_id } => Some((String::new(), command_id.clone())),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
