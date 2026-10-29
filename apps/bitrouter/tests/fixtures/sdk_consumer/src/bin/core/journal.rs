//! A deliberately small harness journal, independent of application codecs.
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use bitrouter_ai::core::checkpoint::{
    CheckpointAck, CheckpointBatch, CheckpointPayload, DurableHead,
};
use bitrouter_ai::core::protocol::{OwnershipGrant, VERSION};
use sha2::{Digest, Sha256};

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn decode(batch: &CheckpointBatch) -> Result<CheckpointPayload> {
    ensure!(batch.payload_encoding == "base64-json");
    ensure!(batch.payload_bytes.len() <= 16 * 1024 * 1024);
    let bytes = STANDARD.decode(&batch.payload_bytes)?;
    ensure!(
        sha256(&bytes) == batch.payload_sha256,
        "checkpoint byte digest mismatch"
    );
    let payload: CheckpointPayload = serde_json::from_slice(&bytes)?;
    ensure!(payload.identity == batch.identity);
    ensure!(payload.checkpoint.schema_version == VERSION);
    ensure!(
        payload.checkpoint.artifact_refs.is_empty(),
        "fixture does not admit external artifacts"
    );
    Ok(payload)
}

pub struct Journal {
    path: PathBuf,
    pub head: DurableHead,
    pub batches: Vec<CheckpointBatch>,
}

impl Journal {
    pub fn new(directory: &Path) -> Result<Self> {
        ensure!(directory.is_dir());
        let path = directory.join("checkpoints.jsonl");
        // Refuse accidental reuse of another harness's journal.
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?
            .sync_all()?;
        Ok(Self {
            path,
            head: DurableHead::default(),
            batches: Vec::new(),
        })
    }

    pub fn persist(
        &mut self,
        grant: &OwnershipGrant,
        batch: CheckpointBatch,
    ) -> Result<CheckpointAck> {
        let payload = decode(&batch)?;
        ensure!(
            batch.identity.session_id == grant.session_id
                && batch.identity.core_instance_id == grant.core_instance_id
                && batch.identity.execution_epoch == grant.execution_epoch,
            "foreign checkpoint"
        );
        let last = payload.events.last().context("empty checkpoint events")?;
        let ack = CheckpointAck {
            batch_id: batch.identity.batch_id.clone(),
            payload_sha256: batch.payload_sha256.clone(),
            through_event_seq: last.event_seq,
            state_revision: payload.checkpoint.state_revision,
            execution_epoch: batch.identity.execution_epoch,
        };
        if let Some(retained) = self
            .batches
            .iter()
            .find(|b| b.identity.batch_id == batch.identity.batch_id)
        {
            ensure!(*retained == batch, "duplicate ID changed checkpoint bytes");
            ensure!(
                ack.head() == self.head,
                "fixture only re-ACKs its current durable head"
            );
        } else {
            ensure!(
                payload.base_event_seq == self.head.event_seq
                    && payload.base_state_revision == self.head.state_revision,
                "noncontiguous journal append"
            );
            ensure!(payload.checkpoint.state_revision == self.head.state_revision + 1);
            for (index, event) in payload.events.iter().enumerate() {
                ensure!(event.event_seq == self.head.event_seq + index as u64 + 1);
            }
            // Fences are retained in the original exact-byte batch, in the same append.
            let mut line = serde_json::to_vec(&batch)?;
            line.push(b'\n');
            let mut file = OpenOptions::new().append(true).open(&self.path)?;
            file.write_all(&line)?;
            file.sync_all()?;
            self.batches.push(batch);
            self.head = ack.head();
        }
        // Reopen the persisted file before returning an ACK to the caller.
        let retained: Vec<CheckpointBatch> =
            std::io::BufRead::lines(std::io::BufReader::new(File::open(&self.path)?))
                .map(|line| Ok(serde_json::from_str(&line?)?))
                .collect::<Result<_>>()?;
        ensure!(
            retained == self.batches,
            "journal readback differs before ACK"
        );
        Ok(ack)
    }

    pub fn event_count(&self, kind: &str) -> Result<usize> {
        Ok(self
            .batches
            .iter()
            .map(decode)
            .collect::<Result<Vec<_>>>()?
            .iter()
            .flat_map(|p| &p.events)
            .filter(|e| e.kind == kind)
            .count())
    }
}
