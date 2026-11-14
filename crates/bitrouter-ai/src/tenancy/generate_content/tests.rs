//! Private ingress contract shape, capability scope and predecessor fences.
use super::*;
use serde_json::json;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn caps() -> serde_json::Value {
    json!({"contract_version":1,"installation_id":"installation","instance_id":"instance",
        "capability_revision":"revision","authority_revision":1,"enabled":true,
        "cached_content_enabled":false,"path":PATH,"request_media_type":MIME})
}
fn request() -> serde_json::Value {
    json!({"schema_version":1,"model":"root","stream":false,"request":{"contents":[{"parts":[{"text":"q"}]}]}})
}

// UT: Wire operation/mode and original native body occupy separate closed slots.
#[test]
fn ut_request_shape_and_competing_envelope_fields() -> TestResult {
    let value: OrderedGenerateContentRequestV1 = serde_json::from_value(request())?;
    value.validate()?;
    assert_eq!(serde_json::to_value(value)?, request());
    for key in [
        "model",
        "stream",
        "bitrouter_generation_defaults",
        "bitrouter_generation_defaults_envelope",
    ] {
        let mut raw = request();
        raw["request"][key] = json!("competing");
        assert!(
            serde_json::from_value::<OrderedGenerateContentRequestV1>(raw)?
                .validate()
                .is_err()
        );
    }
    let mut raw = request();
    raw["extra"] = json!(true);
    assert!(serde_json::from_value::<OrderedGenerateContentRequestV1>(raw).is_err());
    let mut raw = request();
    raw["model"] = json!("");
    assert!(
        serde_json::from_value::<OrderedGenerateContentRequestV1>(raw)?
            .validate()
            .is_err()
    );
    let mut raw = request();
    raw["stream"] = json!(null);
    assert!(serde_json::from_value::<OrderedGenerateContentRequestV1>(raw).is_err());
    Ok(())
}

// UT: Explicit version integers normalize exact decimal/exponent spellings;
// fractional and unsupported versions cannot widen this private operation.
#[test]
fn ut_request_version_exactness_and_unknown_version_refusal() -> TestResult {
    let raw = r#"{"schema_version":1e0,"model":"root","stream":false,"request":{}}"#;
    serde_json::from_str::<OrderedGenerateContentRequestV1>(raw)?.validate()?;
    assert!(
        serde_json::from_str::<OrderedGenerateContentRequestV1>(&raw.replace("1e0", "1.5"))
            .is_err()
    );
    assert!(
        serde_json::from_str::<OrderedGenerateContentRequestV1>(&raw.replace("1e0", "2"))?
            .validate()
            .is_err()
    );
    Ok(())
}

// UT: Protocol enablement never silently authorizes provider-owned cache state.
#[test]
fn ut_capabilities_exact_paths_and_independent_cache_gate() -> TestResult {
    let baseline = caps();
    serde_json::from_value::<OrderedGenerateContentCapabilitiesV1>(baseline.clone())?.validate()?;
    for (enabled, cached, valid) in [
        (false, false, true),
        (true, false, true),
        (true, true, true),
        (false, true, false),
    ] {
        let mut value = baseline.clone();
        value["enabled"] = json!(enabled);
        value["cached_content_enabled"] = json!(cached);
        assert_eq!(
            serde_json::from_value::<OrderedGenerateContentCapabilitiesV1>(value)?
                .validate()
                .is_ok(),
            valid
        );
    }
    for key in ["path", "request_media_type"] {
        let mut value = baseline.clone();
        value[key] = json!("ordinary-protocol");
        assert!(
            serde_json::from_value::<OrderedGenerateContentCapabilitiesV1>(value)?
                .validate()
                .is_err()
        );
    }
    let mut value = baseline.clone();
    value
        .as_object_mut()
        .ok_or("capability object")?
        .remove("cached_content_enabled");
    assert!(serde_json::from_value::<OrderedGenerateContentCapabilitiesV1>(value).is_err());
    let mut value = baseline;
    value["unsupported_authority"] = json!(true);
    assert!(serde_json::from_value::<OrderedGenerateContentCapabilitiesV1>(value).is_err());
    Ok(())
}

// UT: Native absence/null remains raw data, not a cache reference or an invented zero.
#[test]
fn ut_native_null_missingness_and_body_bound() -> TestResult {
    let mut raw = request();
    raw["request"]["cachedContent"] = serde_json::Value::Null;
    let decoded: OrderedGenerateContentRequestV1 = serde_json::from_value(raw.clone())?;
    decoded.validate()?;
    assert_eq!(serde_json::to_value(decoded)?, raw);
    let mut raw = request();
    raw["request"]["large"] = json!("x".repeat(MAX_REQUEST_BYTES));
    assert!(
        serde_json::from_value::<OrderedGenerateContentRequestV1>(raw)?
            .validate()
            .is_err()
    );
    Ok(())
}

// UT: Three existing protocol wire values and native-control-v1 discovery stay
// unchanged; native protocol/end values occupy their own namespace.
#[test]
fn ut_protocol_namespace_and_predecessor_contracts() -> TestResult {
    use crate::tenancy::{
        native_controls::{CONTROLS, NativeControlV1},
        ordered::GenerationProtocol,
    };
    for (protocol, value) in [
        (GenerationProtocol::ChatCompletions, "chat_completions"),
        (GenerationProtocol::Messages, "messages"),
        (GenerationProtocol::Responses, "responses"),
        (GenerationProtocol::GenerateContent, "generate_content"),
    ] {
        assert_eq!(serde_json::to_value(protocol)?, json!(value));
    }
    assert_eq!(
        CONTROLS,
        [
            NativeControlV1::ResponsesPreviousResponseId,
            NativeControlV1::ResponsesConversation,
            NativeControlV1::MessagesContextManagement
        ]
    );
    assert_ne!(
        crate::stream::ProtocolEnd::GenerateContentFinished,
        crate::stream::ProtocolEnd::ChatDone
    );
    assert_ne!(PROTOCOL, BRIDGE_PROTOCOL);
    Ok(())
}

// UT: A programmatic native body cannot validate and then exceed the decoder's
// recursion budget. Exact-bound accepted requests remain transport roundtrippable.
#[test]
fn ut_programmatic_nesting_matches_transport_budget() -> TestResult {
    fn nested(depth: usize) -> serde_json::Value {
        let mut value = serde_json::Value::Null;
        for _ in 0..depth {
            value = serde_json::Value::Array(vec![value]);
        }
        value
    }
    let mut value: OrderedGenerateContentRequestV1 = serde_json::from_value(request())?;
    value.request.insert("extension".into(), nested(125));
    value.validate()?;
    let encoded = serde_json::to_vec(&value)?;
    let decoded: OrderedGenerateContentRequestV1 = serde_json::from_slice(&encoded)?;
    decoded.validate()?;
    for depth in [126, 256] {
        value.request.insert("extension".into(), nested(depth));
        assert!(value.validate().is_err());
    }
    Ok(())
}

#[test]
fn ut_native_dto_preserves_opaque_keys_numbers_and_presence() -> TestResult {
    let raw_string = r#"{"a":1,"a":2}"#;
    let expected = json!({
        "contents":[{"role":"model","parts":[{
            "functionCall":{"id":"call-1","name":"lookup","args":{
                "$serde_json::private::Number":"1.25","sibling":true
            }},"thoughtSignature":"proof"
        }]}],
        "vendor":{
            "raw":{"$serde_json::private::RawValue":raw_string},
            "nested":[{"$serde_json::private::Number":"0.25"}]
        },
        "generationConfig":{"temperature":0.25},
        "cachedContent":null
    });
    let original = OrderedGenerateContentRequestV1 {
        schema_version: 1,
        model: "root".into(),
        stream: true,
        request: expected
            .as_object()
            .ok_or("native object")?
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    };
    original.validate()?;
    let decoded: OrderedGenerateContentRequestV1 =
        serde_json::from_slice(&serde_json::to_vec(&original)?)?;
    decoded.validate()?;
    assert_eq!(decoded.request, original.request);
    assert_eq!(serde_json::to_value(&decoded.request)?, expected);
    let reopened: OrderedGenerateContentRequestV1 =
        serde_json::from_value(serde_json::to_value(&decoded)?)?;
    assert_eq!(reopened.request, original.request);
    assert!(!reopened.request.contains_key("absent"));
    Ok(())
}

#[test]
fn ut_native_dto_refuses_raw_and_escaped_nested_duplicates() {
    for request in [
        r#"{"x":1,"x":2}"#,
        r#"{"x":[{"a":1,"\u0061":2}]}"#,
        r#"{"contents":[{"parts":[{"functionCall":{"name":"lookup","args":{"x":1,"x":2}}}]}]}"#,
    ] {
        let body =
            format!(r#"{{"schema_version":1,"model":"root","stream":false,"request":{request}}}"#);
        assert!(serde_json::from_str::<OrderedGenerateContentRequestV1>(&body).is_err());
    }
}

#[test]
fn ut_native_dto_typed_token_depth_matches_programmatic_budget() -> TestResult {
    for object in [false, true] {
        for depth in [125, 126, 256] {
            let (open, close) = if object {
                (r#"{"n":"#, "}")
            } else {
                ("[", "]")
            };
            let nested = format!("{}null{}", open.repeat(depth), close.repeat(depth));
            let body = format!(
                r#"{{"schema_version":1,"model":"root","stream":false,"request":{{"extension":{nested}}}}}"#
            );
            let decoded = serde_json::from_str::<OrderedGenerateContentRequestV1>(&body);
            assert_eq!(
                decoded.is_ok(),
                depth == 125,
                "object={object}, depth={depth}"
            );
            if depth == 125 {
                decoded?.validate()?;
            }
            let mut value = serde_json::Value::Null;
            for _ in 0..depth {
                value = if object {
                    json!({"n":value})
                } else {
                    json!([value])
                };
            }
            let body = json!({"schema_version":1,"model":"root","stream":false,"request":{"extension":value}});
            assert_eq!(
                serde_json::from_value::<OrderedGenerateContentRequestV1>(body).is_ok(),
                depth == 125
            );
        }
    }
    Ok(())
}
