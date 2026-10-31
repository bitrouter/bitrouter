use super::*;
use anyhow::{Result as TestResult, ensure};
use axum::http::StatusCode;

#[tokio::test]
async fn json_limit_counts_actual_escaped_wire_bytes_before_terminal_authorization()
-> TestResult<()> {
    // Both inputs are the same logical size. Escaping the second one exceeds
    // the fixed wire buffer, so a character/string-length check is insufficient.
    let plain = serde_json::Value::String("x".repeat(BODY / 2));
    let response = json(&plain)?;
    ensure!(response.status() == StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), BODY).await?;
    ensure!(bytes.len() == BODY / 2 + 2);
    let escaped = serde_json::Value::String("\n".repeat(BODY / 2));
    ensure!(json(&escaped).is_err());
    Ok(())
}
