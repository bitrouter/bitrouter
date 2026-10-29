use super::{chat_completions, messages, responses};

/// Generate the JsonSchema for `T`, pretty-print it, and compare against the
/// fixture at `snapshots/<name>.json`. `BITROUTER_BLESS=1` rewrites the fixture.
fn assert_schema_snapshot<T: schemars::JsonSchema>(
    name: &str,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let schema = schemars::schema_for!(T);
    let actual = serde_json::to_string_pretty(&schema)?;
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/protocol/snapshots")
        .join(format!("{name}.json"));

    if std::env::var_os("BITROUTER_BLESS").is_some() {
        std::fs::write(&path, format!("{actual}\n"))?;
        return Ok(());
    }

    let expected = std::fs::read_to_string(&path)?;
    // Compare *semantically* by parsing both sides to `serde_json::Value`
    // rather than as strings. JSON object key order is not part of the schema
    // contract, and it is sensitive to whether `serde_json`'s `preserve_order`
    // feature is unified into this build — any workspace dependency can toggle
    // that, which reorders the properties schemars emits.
    // `Value` equality is order-insensitive for
    // object keys while staying order-sensitive for arrays (`required`,
    // `oneOf`, …), which *are* meaningful. Parsing also subsumes the old
    // CRLF/whitespace normalisation. Stored snapshots stay human-readable.
    let expected_json: serde_json::Value = serde_json::from_str(&expected)?;
    let actual_json: serde_json::Value = serde_json::from_str(&actual)?;
    assert_eq!(
        expected_json, actual_json,
        "schema snapshot for `{name}` drifted; re-run with BITROUTER_BLESS=1 to update.\n\
         actual schema:\n{actual}"
    );
    Ok(())
}

#[test]
fn messages_request_schema_is_stable() -> std::result::Result<(), Box<dyn std::error::Error>> {
    assert_schema_snapshot::<messages::MessagesRequest>("messages_request")
}

#[test]
fn chat_completions_request_schema_is_stable() -> std::result::Result<(), Box<dyn std::error::Error>>
{
    assert_schema_snapshot::<chat_completions::ChatRequest>("chat_completions_request")
}

#[test]
fn responses_request_schema_is_stable() -> std::result::Result<(), Box<dyn std::error::Error>> {
    assert_schema_snapshot::<responses::ResponsesRequest>("responses_request")
}

#[test]
fn extra_passthrough_field_is_not_in_schema() -> std::result::Result<(), Box<dyn std::error::Error>>
{
    let s = serde_json::to_value(schemars::schema_for!(messages::MessagesRequest))?;
    assert!(
        s.get("properties").and_then(|p| p.get("extra")).is_none(),
        "MessagesRequest schema must not expose `extra` (pass-through field)",
    );
    let s = serde_json::to_value(schemars::schema_for!(chat_completions::ChatRequest))?;
    assert!(
        s.get("properties").and_then(|p| p.get("extra")).is_none(),
        "Chat CompletionsRequest schema must not expose `extra` (pass-through field)",
    );
    let s = serde_json::to_value(schemars::schema_for!(responses::ResponsesRequest))?;
    assert!(
        s.get("properties").and_then(|p| p.get("extra")).is_none(),
        "ResponsesRequest schema must not expose `extra` (pass-through field)",
    );
    Ok(())
}
