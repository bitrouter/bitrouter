//! Native rollout names are distinct from both stable threads and file handles.

use super::*;
use crate::session_evidence::types::{Harness, MAX_RECORDS, NodeKey, RecordRef, SourceFormat};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RolloutIdentity {
    pub id: String,
    pub revision: u64,
    pub node: NodeKey,
    pub rollout_id: String,
    pub metadata: RecordRef,
    /// The basename observed on the handle's source, never a path to reopen.
    pub observed_name: String,
}

/// Native ordinary names end in the thread ID; revert adds a distinct rollout
/// ID. The timestamp is a locator and supplies no ordering or active selection.
/// https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/rollout/src/rollout_file_name.rs
pub(crate) fn rollout_id(name: &str, thread: &str) -> Option<String> {
    let core = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let matches_thread = |value: &str| value == thread || value.ends_with(&format!("-{thread}"));
    let Some((prefix, id)) = core.rsplit_once('_') else {
        return matches_thread(core).then(|| thread.into());
    };
    if matches_thread(prefix)
        && !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        Some(id.into())
    } else {
        None
    }
}

impl EvidenceStore {
    pub async fn rollout_identity(&self, source_id: &str) -> Result<Option<RolloutIdentity>> {
        let identity: Option<RolloutIdentity> = self
            .object(&self.db, "rollout_identity", source_id)
            .await?
            .map(decode_object)
            .transpose()?;
        if let Some(identity) = &identity {
            self.validate_rollout_identity(identity).await?;
        }
        Ok(identity)
    }

    pub async fn bind_rollout_identity(&self, identity: &RolloutIdentity) -> Result<()> {
        self.validate_rollout_identity(identity).await?;
        if let Some(existing) = self.rollout_identity(&identity.id).await? {
            // A move or a new file generation cannot rewrite native identity.
            ensure!(
                existing.node == identity.node && existing.rollout_id == identity.rollout_id,
                "native rollout identity conflict"
            );
            return Ok(());
        }
        self.insert_object(&self.db, "rollout_identity", &identity.id, 0, identity)
            .await
    }

    async fn validate_rollout_identity(&self, identity: &RolloutIdentity) -> Result<()> {
        identity.node.validate()?;
        identity.metadata.validate()?;
        ensure!(
            identity.revision == 0
                && identity.node.harness == Harness::Codex
                && identity.metadata.range.source_id == identity.id
                && identity.metadata.range.start == 0
                && identity.observed_name.len() <= 8192
                && !identity.observed_name.contains(['/', '\\'])
                && rollout_id(&identity.observed_name, &identity.node.native_id).as_deref()
                    == Some(identity.rollout_id.as_str()),
            "invalid native rollout identity"
        );
        let source = self
            .source(&identity.id)
            .await?
            .context("rollout source missing")?;
        ensure!(
            source.descriptor.format == SourceFormat::CodexRollout
                && source.descriptor.node.as_ref() == Some(&identity.node),
            "rollout source identity mismatch"
        );
        let records = self.records(&identity.metadata.range).await?;
        let record = records
            .first()
            .context("rollout identity metadata missing")?;
        ensure!(
            RecordRef::from_record(record)? == identity.metadata
                && record
                    .input
                    .raw
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    == Some("session_meta")
                && record
                    .input
                    .raw
                    .pointer("/payload/id")
                    .and_then(serde_json::Value::as_str)
                    == Some(identity.node.native_id.as_str()),
            "rollout identity metadata changed"
        );
        Ok(())
    }

    /// Inventory failures remain gaps while healthy sources stay inspectable.
    pub(crate) async fn rollout_inventory(
        &self,
        namespace: &str,
        node: Option<&NodeKey>,
        rollout: Option<&str>,
    ) -> Result<(Vec<RegisteredSource>, BTreeSet<String>)> {
        let mut after = None;
        let mut found = vec![];
        let mut gaps = BTreeSet::new();
        let mut scanned = 0;
        loop {
            let page = self.source_inventory(after.as_deref(), 128).await?;
            scanned += page.len();
            if scanned > MAX_RECORDS {
                gaps.insert("rollout_inventory_limit".into());
                break;
            }
            after = page.last().map(|(id, _)| id.clone());
            let done = page.len() < 128;
            for (_, candidate) in page {
                let source = match candidate {
                    Ok(source) => source,
                    Err(error) => {
                        tracing::warn!(%error, "rollout inventory registration invalid");
                        gaps.insert("rollout_inventory_registration_invalid".into());
                        continue;
                    }
                };
                if source.descriptor.namespace != namespace
                    || source.descriptor.format != SourceFormat::CodexRollout
                    || node.is_some_and(|node| source.descriptor.node.as_ref() != Some(node))
                {
                    continue;
                }
                if let Some(rollout) = rollout {
                    match self.rollout_identity(&source.id).await {
                        Ok(Some(identity)) if identity.rollout_id == rollout => {}
                        Ok(Some(_)) => continue,
                        Ok(None) => {
                            gaps.insert("rollout_inventory_identity_unavailable".into());
                            continue;
                        }
                        Err(error) => {
                            tracing::warn!(%error, "rollout inventory identity invalid");
                            gaps.insert("rollout_inventory_identity_invalid".into());
                            continue;
                        }
                    }
                }
                if found.len() >= MAX_GRAPH_ITEMS {
                    gaps.insert("rollout_inventory_limit".into());
                    return Ok((found, gaps));
                }
                found.push(source);
            }
            if done {
                break;
            }
        }
        Ok((found, gaps))
    }
}
