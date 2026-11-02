//! Canonical precedence and closed-body regressions.
use super::*;
use anyhow::Result;
use serde_json::json;

fn defaults() -> Result<CanonicalGenerationDefaultsV1> {
    Ok(serde_json::from_value(
        json!({"schema_version":1,"temperature":0.7,"top_p":0.8,
        "max_tokens":23,"reasoning_effort":"high","top_k":40,"seed":7,
        "stop":["END"],"presence_penalty":0.2,"frequency_penalty":0.3}),
    )?)
}

#[test]
fn native_null_and_empty_stop_fill_but_explicit_zero_and_opaque_extra_win() -> Result<()> {
    for (protocol, body) in [
        (
            ApiProtocol::ChatCompletions,
            json!({"model":"r","messages":[],"temperature":null,"stop":[],"seed":0}),
        ),
        (
            ApiProtocol::Messages,
            json!({"model":"r","messages":[],"max_tokens":0,"temperature":null,"stop_sequences":[]}),
        ),
        (
            ApiProtocol::Responses,
            json!({"model":"r","input":"hello","temperature":null,"max_output_tokens":0,"reasoning":{"effort":null}}),
        ),
    ] {
        let mut input = crate::gateway::http::parse_input(&protocol, body)?;
        let params = &mut input
            .generation_prompt_mut()
            .ok_or_else(|| anyhow::anyhow!("generation"))?
            .params;
        apply(&defaults()?, params);
        assert_eq!(params.temperature, Some(0.7));
        assert_eq!(params.top_p, Some(0.8));
        assert_eq!(params.reasoning_effort, defaults()?.reasoning_effort);
        assert_eq!(params.stop, ["END"]);
        if protocol == ApiProtocol::ChatCompletions {
            assert_eq!(params.seed, Some(0));
        } else {
            assert_eq!(params.max_tokens, Some(0));
        }
    }
    let mut input = crate::gateway::http::parse_input(
        &ApiProtocol::ChatCompletions,
        json!({"model":"r","messages":[],"top_k":null,"temperature":0}),
    )?;
    let params = &mut input
        .generation_prompt_mut()
        .ok_or_else(|| anyhow::anyhow!("generation"))?
        .params;
    apply(&defaults()?, params);
    assert_eq!(params.top_k, None);
    assert_eq!(params.extra.get("top_k"), Some(&Value::Null));
    assert_eq!(params.temperature, Some(0.0));
    assert!(params.supplemental_extra.is_empty());
    Ok(())
}

#[test]
fn direct_body_decode_rejects_duplicate_reserved_and_inner_members_before_value_erasure()
-> Result<()> {
    for raw in [
        r#"{"model":"r","bitrouter_generation_defaults":{"schema_version":1},"bitrouter_generation_defaults":{"schema_version":1}}"#,
        r#"{"bitrouter_generation_defaults":{"schema_version":1,"seed":1,"seed":2}}"#,
        r#"{"bitrouter_generation_defaults":null}"#,
        r#"{"bitrouter_generation_defaults":{"schema_version":1,"seed":null}}"#,
        r#"{"bitrouter_generation_defaults":{"schema_version":1,"api_key":"hidden"}}"#,
    ] {
        assert!(serde_json::from_str::<OrderedBody>(raw).is_err(), "{raw}");
    }
    let decoded: OrderedBody = serde_json::from_str(
        r#"{"model":"r","bitrouter_generation_defaults":{"schema_version":1,"top_k":7}}"#,
    )?;
    assert_eq!(
        decoded.request,
        Map::from_iter([("model".into(), json!("r"))])
    );
    assert_eq!(
        decoded
            .defaults
            .ok_or_else(|| anyhow::anyhow!("defaults"))?
            .top_k,
        Some(7)
    );
    Ok(())
}
