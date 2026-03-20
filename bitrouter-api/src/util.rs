#[cfg(any(feature = "openai", feature = "anthropic", feature = "google"))]
use std::time::{SystemTime, UNIX_EPOCH};

/// Generates a hex-encoded timestamp-based ID.
#[cfg(any(feature = "openai", feature = "anthropic", feature = "google"))]
pub(crate) fn generate_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}")
}

/// Returns the current Unix timestamp in seconds.
#[cfg(feature = "openai")]
pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
