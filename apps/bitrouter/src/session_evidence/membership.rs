//! Attempt-scoped native executions. Membership never proves complete coverage,
//! quiescence, coding quality or ownership of an entire native conversation.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::execution::rollout_runs::{ExecutionState, RolloutRun};
use super::native_inputs::{NativeInputBinding, NativeInputEvidence};
use super::types::{
    AcpSessionKey, MAX_GRAPH_ITEMS, MAX_OBJECT_BYTES, MAX_RECORDS, NodeKey, PARSER_VERSION,
    RecordRef, SourceRange,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescendantExecution {
    pub node: NodeKey,
    /// The exact accepted controller input, not a session-tree grouping key.
    pub root_input: RecordRef,
    /// Child-to-root metadata records proving actual spawn ancestry.
    pub ancestry: Vec<RecordRef>,
    pub execution: RolloutRun,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectedPrefix {
    pub range: SourceRange,
    pub digest: String,
}

/// An immutable observation at explicit source cuts. Later observations replace
/// the attempt's pointer, never this object or a previously frozen evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptExecutions {
    pub revision: u64,
    pub parser_version: String,
    pub attempt_id: String,
    pub attempt_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovered_from: Option<String>,
    pub session: AcpSessionKey,
    pub inputs: NativeInputEvidence,
    pub descendants: Vec<DescendantExecution>,
    /// Every inspected variant of a selected descendant's ancestry nodes.
    pub descendant_sources: Vec<SourceRange>,
    pub prefixes: Vec<InspectedPrefix>,
    pub gaps: BTreeSet<String>,
}

impl AttemptExecutions {
    pub(crate) fn new(
        attempt_id: String,
        attempt_revision: u64,
        session: AcpSessionKey,
        inputs: NativeInputEvidence,
    ) -> Self {
        Self {
            revision: 0,
            parser_version: PARSER_VERSION.into(),
            attempt_id,
            attempt_revision,
            recovered_from: None,
            session,
            inputs,
            descendants: vec![],
            descendant_sources: vec![],
            prefixes: vec![],
            gaps: BTreeSet::from(["native_attempt_execution_coverage_incomplete".into()]),
        }
    }

    pub fn members(&self) -> BTreeSet<NodeKey> {
        self.inputs
            .bindings
            .iter()
            .map(|input| input.node.clone())
            .chain(self.descendants.iter().map(|child| child.node.clone()))
            .collect()
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.session.validate()?;
        super::types::digest_identifier(&self.attempt_id)?;
        ensure!(
            self.revision == 0 && self.parser_version == PARSER_VERSION,
            "unsupported execution membership version"
        );
        ensure!(
            self.inputs.bindings.len() + self.descendants.len() <= MAX_GRAPH_ITEMS,
            "attempt execution limit"
        );
        ensure!(
            self.prefixes.len() <= MAX_GRAPH_ITEMS
                && self.descendant_sources.len() <= MAX_GRAPH_ITEMS,
            "execution prefix limit"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_OBJECT_BYTES,
            "execution membership size limit"
        );
        let mut inputs = BTreeMap::new();
        for input in &self.inputs.bindings {
            input.node.validate()?;
            ensure!(
                input.origin.session == self.session
                    && input.node.namespace == self.session.namespace
                    && input.node.harness == self.session.harness,
                "execution input belongs to another task scope"
            );
            ensure!(
                inputs
                    .insert(
                        (
                            input.node.clone(),
                            input.process_id.clone(),
                            input.input.record_id.clone()
                        ),
                        input
                    )
                    .is_none(),
                "duplicate execution input"
            );
        }
        let mut children = BTreeSet::new();
        for child in &self.descendants {
            child.node.validate()?;
            let roots: Vec<_> = inputs
                .values()
                .filter(|input| input.input == child.root_input)
                .collect();
            ensure!(
                roots.len() == 1,
                "descendant root input missing or ambiguous"
            );
            let root = roots[0];
            ensure!(
                root.codex_history
                    .as_ref()
                    .is_some_and(|history| history.source.is_some()
                        && history.gaps.is_empty()
                        && history
                            .execution
                            .as_ref()
                            .is_some_and(|run| run.root_turn_id.as_ref() == Some(&root.native_id))),
                "descendant root input is not independently selected"
            );
            ensure!(
                root.input == child.root_input
                    && root.node.harness == super::types::Harness::Codex
                    && child.node.harness == root.node.harness
                    && child.node.namespace == root.node.namespace
                    && child.node != root.node,
                "descendant root scope mismatch"
            );
            ensure!(
                !child.ancestry.is_empty()
                    && child.ancestry.len() <= 64
                    && child.execution.root_turn_id.as_ref() == Some(&root.native_id)
                    && child.execution.execution_state().is_some(),
                "descendant execution attribution incomplete"
            );
            ensure!(
                children.insert((child.node.clone(), child.execution.turn_id.clone())),
                "duplicate descendant execution"
            );
        }
        ensure!(
            self.gaps.contains("native_attempt_descendant_unfinished")
                == self.descendants.iter().any(|child| {
                    child.execution.execution_state() == Some(ExecutionState::AwaitingTerminal)
                }),
            "unfinished descendant status mismatch"
        );
        self.references()?;
        Ok(())
    }

    pub(crate) fn references(&self) -> Result<BTreeMap<String, RecordRef>> {
        let mut references = BTreeMap::new();
        let mut add = |reference: &RecordRef| -> Result<()> {
            reference.validate()?;
            if let Some(old) = references.get(&reference.record_id) {
                ensure!(old == reference, "execution reference conflict");
            } else {
                ensure!(references.len() < MAX_RECORDS, "execution reference limit");
                references.insert(reference.record_id.clone(), reference.clone());
            }
            Ok(())
        };
        for input in &self.inputs.bindings {
            input_references(input, &mut add)?;
        }
        for child in &self.descendants {
            add(&child.root_input)?;
            for metadata in &child.ancestry {
                add(metadata)?;
            }
            run_references(&child.execution, &mut add)?;
        }
        Ok(references)
    }
}

fn run_references(run: &RolloutRun, add: &mut impl FnMut(&RecordRef) -> Result<()>) -> Result<()> {
    for record in run.records.iter().chain(&run.starts) {
        add(record)?;
    }
    for end in &run.terminations {
        add(&end.record)?;
    }
    for context in &run.contexts {
        add(&context.record)?;
    }
    Ok(())
}

fn input_references(
    input: &NativeInputBinding,
    add: &mut impl FnMut(&RecordRef) -> Result<()>,
) -> Result<()> {
    for reference in [
        &input.origin.request,
        &input.producer,
        &input.input,
        &input.process_header,
        &input.controller_registration,
    ] {
        add(reference)?;
    }
    for acknowledgement in &input.acknowledgements {
        add(&acknowledgement.record)?;
    }
    if let Some(config) = &input.configuration {
        add(&config.configuration)?;
        add(&config.request)?;
    }
    if let Some(response) = &input.session_response {
        add(&response.record)?;
    }
    for reference in input
        .execution
        .records
        .iter()
        .chain(&input.execution.starts)
    {
        add(reference)?;
    }
    for end in &input.execution.terminations {
        add(&end.record)?;
    }
    for result in &input.execution.results {
        add(&result.record)?;
    }
    for call in &input.execution.agent_calls {
        add(call
            .record
            .as_ref()
            .context("execution call provenance missing")?)?;
    }
    if let Some(history) = &input.codex_history {
        if let Some(lifecycle) = &history.lifecycle {
            if let Some(request) = &lifecycle.request {
                add(request)?;
            }
            add(&lifecycle.response)?;
        }
        if let Some(source) = &history.source {
            add(&source.metadata)?;
        }
        for reference in &history.turn_contexts {
            add(reference)?;
        }
        if let Some(run) = &history.execution {
            run_references(run, add)?;
        }
    }
    Ok(())
}
