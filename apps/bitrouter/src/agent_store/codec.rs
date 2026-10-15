//! Lossless row compression below the execution log: hashes, cursors, ACKs and
//! replay facts are unchanged. Public events stay queryable by their JSON tag.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use crate::storage::checkpoint::sha256;

const PREFIX: &str = "bro-zstd-v2:";
pub(super) const LEGACY_PREFIX: &str = "bro-zstd-v1:";
const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    bytes: usize,
    sha256: String,
    body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base64_field: Option<Base64Field>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Base64Field {
    offset: usize,
    bytes: usize,
}

/// Remove one canonical Base64 layer before compression. Checkpoint parts wrap
/// already-encoded payloads; compressing both layers defeats repeated-byte
/// matching. The exact original JSON is restored before its digest is checked.
fn unpack_base64(payload: &str) -> Option<(Vec<u8>, Base64Field)> {
    for marker in ["\"payload_bytes\":\"", "\"content_base64\":\""] {
        let Some(offset) = payload.find(marker).map(|start| start + marker.len()) else {
            continue;
        };
        let end = offset + payload[offset..].find('"')?;
        let encoded = &payload[offset..end];
        if encoded.len() < 4096 {
            continue;
        }
        let decoded = STANDARD.decode(encoded).ok()?;
        if STANDARD.encode(&decoded) != encoded {
            continue;
        }
        let field = Base64Field {
            offset,
            bytes: decoded.len(),
        };
        let mut packed = Vec::with_capacity(payload.len() - encoded.len() + decoded.len());
        packed.extend_from_slice(&payload.as_bytes()[..offset]);
        packed.extend_from_slice(&decoded);
        packed.extend_from_slice(&payload.as_bytes()[end..]);
        return Some((packed, field));
    }
    None
}

pub(super) fn encode(payload: String) -> Result<String, String> {
    if payload.len() < 4096
        || payload.len() > MAX_BYTES
        || payload.starts_with("{\"record\":\"thread_event\",")
    {
        return Ok(payload);
    }
    // https://docs.rs/zstd/latest/zstd/bulk/index.html
    let unpacked = unpack_base64(&payload);
    let bytes = unpacked
        .as_ref()
        .map_or(payload.as_bytes(), |(bytes, _)| bytes.as_slice());
    let compressed = zstd::bulk::compress(bytes, 3).map_err(|e| e.to_string())?;
    let envelope = Envelope {
        bytes: payload.len(),
        sha256: sha256(payload.as_bytes()),
        body: STANDARD.encode(compressed),
        base64_field: unpacked.map(|(_, field)| field),
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
    let Some(encoded) = payload
        .strip_prefix(PREFIX)
        .or_else(|| payload.strip_prefix(LEGACY_PREFIX))
    else {
        return Ok(std::borrow::Cow::Borrowed(payload));
    };
    if payload.len() > MAX_BYTES {
        return Err("compressed execution record exceeds storage bound".into());
    }
    let envelope: Envelope = serde_json::from_str(encoded).map_err(|e| e.to_string())?;
    if payload.starts_with(LEGACY_PREFIX) && envelope.base64_field.is_some() {
        return Err("legacy execution record has unsupported encoding".into());
    }
    if envelope.bytes == 0 || envelope.bytes > MAX_BYTES {
        return Err("compressed execution record exceeds decoded bound".into());
    }
    let compressed = STANDARD.decode(envelope.body).map_err(|e| e.to_string())?;
    // The decoder caps output before allocating it; the digest also proves
    // that the decoded bytes exactly match the original execution fact.
    // https://docs.rs/zstd/latest/zstd/bulk/fn.decompress.html
    let mut decoded =
        zstd::bulk::decompress(&compressed, envelope.bytes).map_err(|e| e.to_string())?;
    if let Some(field) = envelope.base64_field {
        let end = field
            .offset
            .checked_add(field.bytes)
            .filter(|end| *end <= decoded.len())
            .ok_or("compressed execution record has invalid Base64 bounds")?;
        let encoded_bytes = field
            .bytes
            .checked_add(2)
            .and_then(|bytes| (bytes / 3).checked_mul(4))
            .ok_or("compressed execution record has invalid Base64 size")?;
        if decoded
            .len()
            .checked_sub(field.bytes)
            .and_then(|bytes| bytes.checked_add(encoded_bytes))
            != Some(envelope.bytes)
        {
            return Err("compressed execution record exceeds reconstructed bound".into());
        }
        let mut restored = Vec::with_capacity(envelope.bytes);
        restored.extend_from_slice(&decoded[..field.offset]);
        restored.extend_from_slice(STANDARD.encode(&decoded[field.offset..end]).as_bytes());
        restored.extend_from_slice(&decoded[end..]);
        decoded = restored;
    }
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
    fn base64_rows_preserve_exact_bytes_and_reject_invalid_reconstruction() -> Result<(), String> {
        let content = (0..16384)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        for field in ["payload_bytes", "content_base64"] {
            let original = format!(
                "{{\"record\":\"turn_record\",\"label\":\"雪\",\"{field}\":\"{}\", \"tail\": 1}}",
                STANDARD.encode(&content)
            );
            let packed = encode(original.clone())?;
            assert_eq!(decode(&packed)?, original);
            let mut envelope: Envelope = serde_json::from_str(packed.trim_start_matches(PREFIX))
                .map_err(|e| e.to_string())?;
            let field = envelope
                .base64_field
                .as_mut()
                .ok_or("Base64 field was not packed")?;
            field.offset = usize::MAX;
            assert!(
                decode(&format!(
                    "{PREFIX}{}",
                    serde_json::to_string(&envelope).map_err(|e| e.to_string())?
                ))
                .is_err()
            );
            envelope.base64_field = Some(Base64Field {
                offset: 0,
                bytes: 1,
            });
            assert!(
                decode(&format!(
                    "{PREFIX}{}",
                    serde_json::to_string(&envelope).map_err(|e| e.to_string())?
                ))
                .is_err()
            );
            envelope.base64_field = None;
            envelope.body = STANDARD
                .encode(zstd::bulk::compress(original.as_bytes(), 3).map_err(|e| e.to_string())?);
            let legacy = format!(
                "{LEGACY_PREFIX}{}",
                serde_json::to_string(&envelope).map_err(|e| e.to_string())?
            );
            assert_eq!(decode(&legacy)?, original);
        }
        Ok(())
    }

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
