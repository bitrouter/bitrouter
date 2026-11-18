//! Lossless row compression below the execution log: hashes, cursors, ACKs and
//! replay facts are unchanged. Public events stay queryable by their JSON tag.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;

use crate::storage::checkpoint::sha256;

const PREFIX: &str = "bro-zstd-v2:";
pub(super) const LEGACY_PREFIX: &str = "bro-zstd-v1:";
const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    bytes: usize,
    sha256: String,
    body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base64_field: Option<Base64Field>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Base64Field {
    offset: usize,
    bytes: usize,
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
