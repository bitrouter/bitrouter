use std::time::{SystemTime, UNIX_EPOCH};

/// Generates a hex-encoded timestamp-based ID.
pub(crate) fn generate_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}")
}
