//! Immutable observation frontiers. A source cut is not native execution
//! membership, producer quiescence, or permission to evaluate an attempt.

use std::collections::BTreeSet;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::types::{
    AcpSessionKey, MAX_GRAPH_ITEMS, MAX_OBJECT_BYTES, RecordRef, RegisteredSource, SourceCursor,
    digest_identifier, identifier,
};
use crate::eval::types::canonical_digest;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFrontier {
    pub source: RegisteredSource,
    pub last_record: Option<RecordRef>,
}

impl SourceFrontier {
    pub fn validate(&self) -> Result<()> {
        self.source.descriptor.validate()?;
        digest_identifier(&self.source.id)?;
        ensure!(self.source.revision >= 0, "negative source revision");
        let cursor = &self.source.cursor;
        if cursor == &SourceCursor::default() {
            ensure!(
                self.source.revision == 0 && self.last_record.is_none(),
                "invalid uninitialized frontier"
            );
            return Ok(());
        }
        identifier(&cursor.generation)?;
        digest_identifier(&cursor.anchor_digest)?;
        ensure!(
            cursor.next_sequence <= i64::MAX as u64,
            "frontier sequence overflow"
        );
        if let Some(last) = &self.last_record {
            last.validate()?;
            ensure!(
                last.range.source_id == self.source.id
                    && last.range.generation == cursor.generation
                    && last.range.end == cursor.next_sequence,
                "frontier does not end at its original record"
            );
        } else {
            ensure!(
                cursor.next_sequence == 0,
                "frontier terminal record missing"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCheckpoint {
    pub schema_version: u32,
    pub parser_version: String,
    pub controller_id: String,
    pub operation_id: String,
    pub phase: String,
    pub session: AcpSessionKey,
    pub sources: Vec<SourceFrontier>,
    pub gaps: BTreeSet<String>,
}

impl NativeCheckpoint {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "unknown native checkpoint schema");
        identifier(&self.parser_version)?;
        identifier(&self.controller_id)?;
        identifier(&self.operation_id)?;
        ensure!(
            matches!(self.phase.as_str(), "request" | "response"),
            "invalid checkpoint phase"
        );
        self.session.validate()?;
        ensure!(
            self.sources.len() <= MAX_GRAPH_ITEMS && self.gaps.len() <= MAX_GRAPH_ITEMS,
            "native checkpoint exceeds limits"
        );
        let mut ids = BTreeSet::new();
        for frontier in &self.sources {
            frontier.validate()?;
            ensure!(
                frontier.source.descriptor.namespace == self.session.namespace
                    && frontier.source.descriptor.harness == self.session.harness
                    && ids.insert(&frontier.source.id),
                "checkpoint source scope mismatch"
            );
        }
        for gap in &self.gaps {
            identifier(gap)?;
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_OBJECT_BYTES,
            "native checkpoint is too large"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        canonical_digest(self)
    }
}

/// References selected through the attempt's original prompt records. Later
/// prompts may change latest_prompt_result, but never the referenced objects.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeCheckpointEvidence {
    pub baseline: Option<String>,
    pub latest_prompt_result: Option<String>,
    pub gaps: BTreeSet<String>,
}
