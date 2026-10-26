use super::*;
#[test]
fn reply_requires_expiry_and_rotated_refresh_material_with_explicit_bounds() -> anyhow::Result<()> {
    for value in [
        serde_json::json!({"access_token":"a","expires_in":1}),
        serde_json::json!({"access_token":"a","refresh_token":"r","expires_in":0}),
        serde_json::json!({"access_token":"a","refresh_token":"r","expires_in":MAX_EXPIRY_SECONDS+1}),
        serde_json::json!({"access_token":"a".repeat(profile::TOKEN_BYTES+1),"refresh_token":"r","expires_in":1}),
    ] {
        assert!(decode(&serde_json::to_vec(&value)?).is_err());
    }
    assert!(decode(&vec![b' '; RESPONSE_BYTES + 1]).is_err());
    let token=decode(br#"{"access_token":"a","refresh_token":"r","expires_in":3600,"untrusted_metadata":{"unused":true}}"#)?;
    assert_eq!(token.access_token, "a");
    assert_eq!(token.refresh_token.as_deref(), Some("r"));
    Ok(())
}
