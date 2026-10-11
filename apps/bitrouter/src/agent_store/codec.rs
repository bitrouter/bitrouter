//! Lossless row compression below the execution log: hashes, cursors, ACKs and
//! replay facts are unchanged. Public events stay queryable by their JSON tag.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use bitrouter_orchestrator::core::checkpoint::sha256;

const PREFIX: &str = "bro-zstd-v1:";
const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    bytes: usize,
    sha256: String,
    body: String,
}

pub(super) fn encode(payload: String) -> Result<String, String> {
    if payload.len() < 4096
        || payload.len() > MAX_BYTES
        || payload.starts_with("{\"record\":\"thread_event\",")
    {
        return Ok(payload);
    }
    // https://docs.rs/zstd/latest/zstd/bulk/index.html
    let compressed = zstd::bulk::compress(payload.as_bytes(), 3).map_err(|e| e.to_string())?;
    let envelope = Envelope {
        bytes: payload.len(),
        sha256: sha256(payload.as_bytes()),
        body: STANDARD.encode(compressed),
    };
    let encoded = format!(
        "{PREFIX}{}",
        serde_json::to_string(&envelope).map_err(|e| e.to_string())?
    );
    // Small or incompressible records never grow as a result of packing.
    Ok(if encoded.len() < payload.len() {
        encoded
    } else {
        payload
    })
}

pub(super) fn decode(payload: &str) -> Result<std::borrow::Cow<'_, str>, String> {
    let Some(encoded) = payload.strip_prefix(PREFIX) else {
        return Ok(std::borrow::Cow::Borrowed(payload));
    };
    if payload.len() > MAX_BYTES {
        return Err("compressed execution record exceeds storage bound".into());
    }
    let envelope: Envelope = serde_json::from_str(encoded).map_err(|e| e.to_string())?;
    if envelope.bytes == 0 || envelope.bytes > MAX_BYTES {
        return Err("compressed execution record exceeds decoded bound".into());
    }
    let compressed = STANDARD.decode(envelope.body).map_err(|e| e.to_string())?;
    // The decoder caps output before allocating it; the digest also proves
    // that the decoded bytes exactly match the original execution fact.
    // https://docs.rs/zstd/latest/zstd/bulk/fn.decompress.html
    let decoded = zstd::bulk::decompress(&compressed, envelope.bytes).map_err(|e| e.to_string())?;
    if decoded.len() != envelope.bytes || sha256(&decoded) != envelope.sha256 {
        return Err("compressed execution record size or digest mismatch".into());
    }
    String::from_utf8(decoded)
        .map(std::borrow::Cow::Owned)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_round_trip_exactly_and_reject_corruption_or_expansion() -> Result<(), String> {
        let original = format!(
            "{{\"record\":\"turn_record\",\"body\":\"{}\"}}",
            "retained evidence ".repeat(4000)
        );
        let packed = encode(original.clone())?;
        assert!(packed.len() < original.len() / 10);
        assert_eq!(decode(&packed)?, original);
        assert_eq!(decode(&original)?, original);
        let mut envelope: Envelope =
            serde_json::from_str(packed.trim_start_matches(PREFIX)).map_err(|e| e.to_string())?;
        envelope.bytes -= 1;
        assert!(
            decode(&format!(
                "{PREFIX}{}",
                serde_json::to_string(&envelope).map_err(|e| e.to_string())?
            ))
            .is_err()
        );
        envelope.bytes = MAX_BYTES + 1;
        assert!(
            decode(&format!(
                "{PREFIX}{}",
                serde_json::to_string(&envelope).map_err(|e| e.to_string())?
            ))
            .is_err()
        );
        envelope.bytes = original.len();
        envelope.sha256 = "0".repeat(64);
        assert!(
            decode(&format!(
                "{PREFIX}{}",
                serde_json::to_string(&envelope).map_err(|e| e.to_string())?
            ))
            .is_err()
        );
        let public = format!(
            "{{\"record\":\"thread_event\",\"text\":\"{}\"}}",
            "x".repeat(8000)
        );
        assert_eq!(encode(public.clone())?, public);
        Ok(())
    }
}
