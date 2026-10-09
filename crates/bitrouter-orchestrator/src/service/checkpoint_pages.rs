//! Store-page framing never changes the bytes acknowledged by Core. All parts
//! belong to the same fenced transaction; incomplete prefixes cannot restore.

use base64::{Engine, engine::general_purpose::STANDARD};

use crate::store::ExecutionRecord;

const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;

fn checkpoint(record: &ExecutionRecord) -> bool {
    matches!(record, ExecutionRecord::TurnRecord { fact, .. }
        if matches!(fact.as_ref(), ExecutionRecord::CoreCheckpoint { .. }))
}

pub(super) fn split(
    records: &[ExecutionRecord],
    page_bytes: usize,
) -> Result<Vec<ExecutionRecord>, String> {
    let mut result = Vec::new();
    for record in records {
        if !checkpoint(record) {
            result.push(record.clone());
            continue;
        }
        let encoded = serde_json::to_vec(record).map_err(|error| error.to_string())?;
        if encoded.len() <= page_bytes {
            result.push(record.clone());
            continue;
        }
        if encoded.len() > MAX_RECORD_BYTES || page_bytes <= 1024 {
            return Err("native checkpoint exceeds bounded record framing capacity".into());
        }
        let digest = crate::core::checkpoint::sha256(&encoded);
        let chunk_bytes = (page_bytes - 1024) / 4 * 3;
        for (index, chunk) in encoded.chunks(chunk_bytes).enumerate() {
            result.push(ExecutionRecord::CoreCheckpointPart {
                sha256: digest.clone(),
                bytes: encoded.len() as u64,
                offset: (index * chunk_bytes) as u64,
                content_base64: STANDARD.encode(chunk),
            });
        }
    }
    Ok(result)
}

#[derive(Default)]
pub(super) struct Reader {
    pending: Option<(String, u64, Vec<u8>)>,
}

impl Reader {
    pub(super) fn incomplete(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn consume(
        &mut self,
        record: ExecutionRecord,
    ) -> Result<Option<ExecutionRecord>, String> {
        let ExecutionRecord::CoreCheckpointPart {
            sha256,
            bytes,
            offset,
            content_base64,
        } = record
        else {
            if self.incomplete() {
                return Err("native checkpoint parts were interrupted".into());
            }
            return Ok(Some(record));
        };
        crate::core::checkpoint::validate_digest(&sha256).map_err(|error| error.message)?;
        if bytes == 0
            || bytes > MAX_RECORD_BYTES as u64
            || content_base64.len() > MAX_RECORD_BYTES * 2
        {
            return Err("native checkpoint parts exceed bounded recovery capacity".into());
        }
        let chunk = STANDARD
            .decode(content_base64)
            .map_err(|error| error.to_string())?;
        if self.pending.is_none() && offset == 0 {
            self.pending = Some((sha256.clone(), bytes, Vec::new()));
        }
        let (known_digest, known_bytes, body) = self
            .pending
            .as_mut()
            .ok_or("native checkpoint parts do not start at zero")?;
        if *known_digest != sha256
            || *known_bytes != bytes
            || offset != body.len() as u64
            || chunk.is_empty()
            || chunk.len() as u64 > bytes.saturating_sub(offset)
        {
            return Err("native checkpoint part identity, offset or size mismatch".into());
        }
        body.extend(chunk);
        if body.len() as u64 != bytes {
            return Ok(None);
        }
        if crate::core::checkpoint::sha256(body) != sha256 {
            return Err("native checkpoint part digest mismatch".into());
        }
        let decoded = serde_json::from_slice(body).map_err(|error| error.to_string())?;
        if !checkpoint(&decoded) {
            return Err("native checkpoint parts contain a different record type".into());
        }
        self.pending = None;
        Ok(Some(decoded))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_pages_preserve_exact_bytes_and_reject_gaps_or_corruption()
    -> Result<(), Box<dyn std::error::Error>> {
        let record = ExecutionRecord::TurnRecord {
            turn_id: "turn".into(),
            fact: Box::new(ExecutionRecord::CoreCheckpoint {
                batch: crate::core::checkpoint::CheckpointBatch {
                    identity: crate::core::checkpoint::BatchIdentity {
                        batch_id: "batch".into(),
                        session_id: "session".into(),
                        core_instance_id: "core".into(),
                        execution_epoch: 1,
                    },
                    payload_encoding: "base64".into(),
                    payload_bytes: STANDARD.encode(vec![b'x'; 200 * 1024]),
                    payload_sha256: "a".repeat(64),
                },
                limits: Default::default(),
            }),
        };
        let parts = split(std::slice::from_ref(&record), 64 * 1024)?;
        assert!(parts.len() > 2);
        let mut reader = Reader::default();
        let mut restored = None;
        for part in &parts {
            assert!(serde_json::to_vec(part)?.len() <= 64 * 1024);
            restored = reader.consume(part.clone())?;
        }
        assert!(!reader.incomplete());
        assert_eq!(
            serde_json::to_value(restored.ok_or("record missing")?)?,
            serde_json::to_value(&record)?
        );
        let mut reader = Reader::default();
        assert!(reader.consume(parts[1].clone()).is_err());
        let mut reader = Reader::default();
        assert!(reader.consume(parts[0].clone())?.is_none());
        assert!(reader.incomplete());
        assert!(reader.consume(parts[0].clone()).is_err());
        let mut corrupt = parts.clone();
        if let ExecutionRecord::CoreCheckpointPart { content_base64, .. } = &mut corrupt[0] {
            content_base64.replace_range(0..1, "A");
        }
        let mut reader = Reader::default();
        assert!(
            corrupt
                .into_iter()
                .try_for_each(|part| reader.consume(part).map(|_| ()))
                .is_err()
        );
        Ok(())
    }
}
