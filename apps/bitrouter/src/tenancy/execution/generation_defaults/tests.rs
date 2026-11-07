//! Canonical precedence and closed-body regressions.
use super::*;
use anyhow::{Context, Result, ensure};
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

#[test]
fn envelope_member_is_reserved_on_nonordered_protocol_parsing_only() -> Result<()> {
    for protocol in [
        ApiProtocol::ChatCompletions,
        ApiProtocol::Messages,
        ApiProtocol::Responses,
    ] {
        let body = match protocol {
            ApiProtocol::Responses => json!({"model":"r","input":"hello"}),
            _ => json!({"model":"r","messages":[],"max_tokens":25}),
        };
        crate::gateway::http::parse_input(&protocol, body.clone())?;
        let mut old = body.clone();
        old[BODY_MEMBER] = json!({"schema_version":1});
        crate::gateway::http::parse_input(&protocol, old)?;
        for value in [Value::Null, json!({"schema_version":1,"supplemental":{}})] {
            let mut new = body.clone();
            new[ENVELOPE_BODY_MEMBER] = value;
            assert!(crate::gateway::http::parse_input(&protocol, new).is_err());
        }
    }
    Ok(())
}

#[test]
fn versioned_raw_envelope_decoder_preserves_exact_numeric_tokens_and_depth() -> Result<()> {
    let raw = r#"{"bitrouter_generation_defaults_envelope":{"schema_version":2.0,"canonical":{"schema_version":1e0,"seed":9.223372036854775807e18},"supplemental":{},"configuration_defaults":"preserve_absence"}}"#;
    let decoded: OrderedBody = serde_json::from_str(raw)?;
    let Envelope::V2(value) = decoded.envelope.context("envelope")? else {
        anyhow::bail!("wrong version")
    };
    ensure!(value.canonical.as_ref().context("canonical")?.seed == Some(i64::MAX));
    value.digest()?;
    for bad in [
        raw.replace("2.0", "2.00000000000000001"),
        raw.replace("9.223372036854775807e18", "9.223372036854775808e18"),
        raw.replace(
            "\"supplemental\":{}",
            "\"supplemental\":{\"k\":1,\"\\u006b\":1}",
        ),
    ] {
        assert!(serde_json::from_str::<OrderedBody>(&bad).is_err());
    }
    for depth in [124, 125] {
        let raw = format!(
            r#"{{"bitrouter_generation_defaults_envelope":{{"schema_version":2,"supplemental":{{"k":{}null{}}},"configuration_defaults":"preserve_absence"}}}}"#,
            "[".repeat(depth),
            "]".repeat(depth)
        );
        let decoded: OrderedBody = serde_json::from_str(&raw)?;
        assert_eq!(
            decoded.envelope.context("envelope")?.digest().is_ok(),
            depth == 124
        );
    }
    Ok(())
}
