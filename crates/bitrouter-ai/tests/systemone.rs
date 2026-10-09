//! Classifier wire fidelity, correlation, confidence and partial usage contracts.

use bitrouter_ai::client::{HttpTimeouts, ModelClient};
use bitrouter_ai::error::ModelError;
use bitrouter_ai::protocol::classifier::{admission, validate_result};
use bitrouter_ai::protocol::decisions::DecisionsCodec;
use bitrouter_ai::protocol::systemone::SystemOneCodec;
use bitrouter_ai::target::ModelTarget;
use bitrouter_ai::types::{ApiProtocol, AuthScheme, UsageNormalizationError};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn request() -> Value {
    json!({"model":"selector", "state":{"build":"passed", "count":3}, "questions":{
        "predicate/key":{"type":"noul", "instructions":{"question":"Did checks pass?"}, "criteria":{"true":"All checks pass","false":["Any check fails"]}},
        "choice":{"type":"choice", "instructions":"Choose a status.", "criteria":{"pass":null,"fail":{"when":"A check failed"}}},
        "score":{"type":"score", "instructions":"Rate quality.", "criteria":[{"level":"low"}, {"level":"high"}]}
    }})
}

fn response() -> Value {
    json!({"model":"jev-fixture", "answers":{
        "score":{"type":"score", "score":0.8, "probabilities":{"1":0.8,"0":0.2}, "confidence":0.6, "legend":{"0":"Low quality","1":"High quality"}, "future":"score metadata"},
        "choice":{"type":"choice", "choice":"pass", "probabilities":{"pass":0.9,"fail":0.1}, "confidence":0.8},
        "predicate/key":{"type":"noul", "noul":0.95, "future":{"ok":true}}
    }, "usage":{"input_tokens":100,"output_tokens":12,"future_usage":"preserved"}, "future_envelope":{"version":1}})
}

fn full_usage() -> Value {
    json!({"input_tokens":100,"output_tokens":0,"total_tokens":100,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},"output_tokens_details":{"reasoning_tokens":0}})
}

#[test]
fn structured_native_round_trip_and_partial_usage() -> TestResult {
    let request = SystemOneCodec::parse_request(request())?;
    assert_eq!(SystemOneCodec::render_request(&request)?, self::request());
    let result = SystemOneCodec::parse_response(response(), &request)?;
    assert_eq!(
        SystemOneCodec::render_response(&result, &request)?,
        response()
    );
    assert_eq!(result.usage.prompt_tokens, 100);
    assert_eq!(result.usage.completion_tokens, 12);
    assert_eq!(
        result.usage.normalized_buckets(),
        Err(UsageNormalizationError::BreakdownUnavailable)
    );
    let availability = result
        .usage
        .availability
        .as_ref()
        .ok_or("missing availability")?;
    assert!(!availability.cache_read && !availability.cache_write && !availability.reasoning);
    validate_result(&result, &request)?;
    Ok(())
}

#[test]
fn canonical_serialization_preserves_structured_messages_and_wire_identity() -> TestResult {
    let request = SystemOneCodec::parse_request(
        json!({"model":"selector","state":[{"role":"user","content":"A record, not a native message projection."}],"questions":{"original/key":{"type":"noul","instructions":null,"criteria":{"true":null}}}}),
    )?;
    let encoded = serde_json::to_value(&request)?;
    assert_eq!(encoded["input"]["kind"], "structured");
    let decoded: bitrouter_ai::classifier::ClassifierRequest = serde_json::from_value(encoded)?;
    assert_eq!(decoded, request);
    let wire = SystemOneCodec::render_request(&decoded)?;
    assert_eq!(
        wire["questions"]["original/key"]["instructions"],
        Value::Null
    );
    assert!(
        wire["questions"]["original/key"]["criteria"]
            .get("true")
            .is_some()
    );
    assert!(
        !wire["questions"]["original/key"]["criteria"]
            .as_object()
            .ok_or("missing criteria")?
            .contains_key("false")
    );
    Ok(())
}

#[test]
fn duplicate_json_members_do_not_erase_question_or_usage_identity() -> TestResult {
    use bitrouter_ai::protocol::classifier::{parse_request_json, parse_response_json};
    let duplicate_request = br#"{"model":"test","state":"evidence","questions":{"q":{"type":"noul"},"q":{"type":"score","criteria":["low","high"]}}}"#;
    assert!(parse_request_json(&ApiProtocol::SystemOne, duplicate_request).is_err());
    let request = SystemOneCodec::parse_request(
        json!({"model":"test","state":"evidence","questions":{"q":{"type":"noul"}}}),
    )?;
    let duplicate_answer = br#"{"model":"jev","answers":{"q":{"type":"noul","noul":0.8},"q":{"type":"noul","noul":0.1}},"usage":{"input_tokens":12,"output_tokens":3}}"#;
    let error = parse_response_json(&ApiProtocol::SystemOne, duplicate_answer, &request)
        .err()
        .ok_or("duplicate answer accepted")?;
    assert_eq!(
        error.classifier_usage().map(|usage| usage.prompt_tokens),
        Some(12)
    );
    let duplicate_usage = br#"{"model":"jev","answers":{"q":{"type":"noul","noul":0.8}},"usage":{"input_tokens":12,"input_tokens":99,"output_tokens":3}}"#;
    let error = parse_response_json(&ApiProtocol::SystemOne, duplicate_usage, &request)
        .err()
        .ok_or("duplicate usage accepted")?;
    assert!(error.classifier_usage().is_none());
    let large = String::from_utf8(duplicate_answer.to_vec())?.replace(
        "\"output_tokens\":3",
        &format!(
            "\"output_tokens\":3,\"future\":\"{}\"",
            "x".repeat(70 * 1024)
        ),
    );
    let error = parse_response_json(&ApiProtocol::SystemOne, large.as_bytes(), &request)
        .err()
        .ok_or("duplicate answer accepted")?;
    let usage = error.classifier_usage().ok_or("independent usage lost")?;
    assert_eq!(usage.prompt_tokens, 12);
    assert!(serde_json::to_vec(&usage.raw)?.len() < 1024);
    Ok(())
}

#[test]
fn native_optional_instructions_and_structured_legend_follow_live_schema() -> TestResult {
    let body = json!({"model":"selector","state":"evidence","questions":{
        "missing":{"type":"noul"},
        "null":{"type":"noul","instructions":null,"criteria":null},
        "structured-score":{"type":"score","criteria":[{"level":"only","values":[1,2]}]}
    }});
    let request = SystemOneCodec::parse_request(body.clone())?;
    assert_eq!(SystemOneCodec::render_request(&request)?, body);
    let response = json!({"model":"jev","answers":{
        "missing":{"type":"noul","noul":0.5},
        "null":{"type":"noul","noul":0.8},
        "structured-score":{"type":"score","score":0.0,"confidence":1.0,"legend":{"0":{"level":"only","values":[1,2]}},"probabilities":{"0":1.0}}
    },"usage":{"input_tokens":20,"output_tokens":3}});
    let result = SystemOneCodec::parse_response(response.clone(), &request)?;
    assert_eq!(
        SystemOneCodec::render_response(&result, &request)?,
        response
    );
    assert!(
        !admission(&ApiProtocol::Decisions, &request)
            .issues
            .is_empty()
    );
    Ok(())
}

#[test]
fn structured_fields_are_checked_without_identity_becoming_evidence() -> TestResult {
    let request = SystemOneCodec::parse_request(request())?;
    let mut text = Vec::new();
    request.visit_text(|_, _, value| text.push(value.to_owned()));
    assert!(text.iter().any(|value| value == "count"));
    assert!(text.iter().any(|value| value == "3"));
    assert!(text.iter().any(|value| value == "Any check fails"));
    assert!(!text.iter().any(|value| value == "predicate/key"));
    assert!(
        !admission(&ApiProtocol::Decisions, &request)
            .issues
            .is_empty()
    );
    Ok(())
}

#[test]
fn malformed_complete_output_preserves_independent_usage() -> TestResult {
    let request = SystemOneCodec::parse_request(request())?;
    for invalid in [
        json!({"type":"noul","noul":1.5}),
        json!({"type":"choice","choice":"pass","probabilities":{"pass":1},"confidence":1}),
        json!({"type":"refusal"}),
    ] {
        let mut body = response();
        body["answers"]["predicate/key"] = invalid;
        let error = SystemOneCodec::parse_response(body, &request)
            .err()
            .ok_or("invalid answer accepted")?;
        assert!(error.is_completed_classifier_failure());
        assert_eq!(
            error.classifier_usage().map(|usage| usage.prompt_tokens),
            Some(100)
        );
    }
    Ok(())
}

#[test]
fn invalid_custom_result_retains_provider_counters_instead_of_forged_totals() -> TestResult {
    let request = SystemOneCodec::parse_request(request())?;
    let mut result = SystemOneCodec::parse_response(response(), &request)?;
    result.usage.prompt_tokens = 1_000_000;
    let error = validate_result(&result, &request)
        .err()
        .ok_or("forged counters accepted")?;
    assert_eq!(
        error.classifier_usage().map(|usage| usage.prompt_tokens),
        Some(100)
    );
    assert_eq!(
        error
            .classifier_usage()
            .map(|usage| usage.completion_tokens),
        Some(12)
    );
    Ok(())
}

#[test]
fn usage_projection_gate_and_positional_question_keys() -> TestResult {
    let request =
        DecisionsCodec::parse_request(json!({"model":"selector","input":"evidence","questions":[
            {"type":"predicate","instructions":"A?","name":"same"},
            {"type":"predicate","instructions":"B?","name":"same"},
            {"type":"predicate","instructions":"C?"}
        ]}))?;
    let wire = SystemOneCodec::render_request(&request)?;
    assert!(wire["questions"].get("q0").is_some());
    assert!(wire["questions"].get("q1").is_some());
    assert!(wire["questions"].get("q2").is_some());
    assert!(
        !admission(&ApiProtocol::SystemOne, &request)
            .issues
            .is_empty()
    );
    let result = SystemOneCodec::parse_response(
        json!({"model":"jev","answers":{
        "q2":{"type":"noul","noul":0.3},"q0":{"type":"noul","noul":0.1},"q1":{"type":"noul","noul":0.2}
    },"usage":{"input_tokens":50,"output_tokens":3}}),
        &request,
    )?;
    assert_eq!(result.answers[0].name(), Some("same"));
    assert_eq!(result.answers[1].name(), Some("same"));
    assert_eq!(result.answers[2].name(), None);
    assert!(DecisionsCodec::render_response(&result, &request).is_err());
    Ok(())
}

#[test]
fn confidence_is_derived_for_destination_and_reported_value_is_retained() -> TestResult {
    let request =
        SystemOneCodec::parse_request(json!({"model":"selector","state":"evidence","questions":{
            "choose":{"type":"choice","instructions":"Choose.","criteria":{"a":null,"b":null}},
            "rate":{"type":"score","instructions":"Rate.","criteria":["low","middle","high"]}
        }}))?;
    let outbound = DecisionsCodec::render_request(&request)?;
    assert_eq!(outbound["questions"][0]["name"], Value::Null);
    let result = DecisionsCodec::parse_response(
        json!({"model":"openai-fixture","answers":[
        {"type":"choice","name":null,"choice":"a","probabilities":[{"value":"a","probability":0.6},{"value":"b","probability":0.4}],"confidence":0.91},
        {"type":"score","name":null,"score":1.43,"probabilities":[{"value":0,"label":"low","probability":0},{"value":1,"label":"middle","probability":0.57},{"value":2,"label":"high","probability":0.43}],"confidence":0.99}
    ],"usage":full_usage()}),
        &request,
    )?;
    let wire = SystemOneCodec::render_response(&result, &request)?;
    let choice = wire["answers"]["choose"]["confidence"]
        .as_f64()
        .ok_or("missing confidence")?;
    assert!((choice - 0.2).abs() < 1e-10);
    let score = wire["answers"]["rate"]["confidence"]
        .as_f64()
        .ok_or("missing confidence")?;
    assert!((score - (1.0 - 0.43 / (2.0 / 3.0))).abs() < 1e-10);
    assert_eq!(wire["answers"]["rate"]["legend"]["2"], "high");
    let native = DecisionsCodec::render_response(&result, &request)?;
    assert_eq!(native["answers"][0]["confidence"], 0.91);
    assert_eq!(native["answers"][1]["confidence"], 0.99);
    Ok(())
}

#[test]
fn refusal_is_a_completed_output_error_for_systemone() -> TestResult {
    let request = SystemOneCodec::parse_request(
        json!({"model":"selector","state":"evidence","questions":{"check":{"type":"noul","instructions":"Check?"}}}),
    )?;
    let result = DecisionsCodec::parse_response(
        json!({"model":"openai-fixture","answers":[{"type":"refusal","name":null}],"usage":full_usage()}),
        &request,
    )?;
    let error = SystemOneCodec::render_response(&result, &request)
        .err()
        .ok_or("refusal was fabricated")?;
    assert!(error.is_completed_classifier_failure());
    assert_eq!(
        error.classifier_usage().map(|usage| usage.prompt_tokens),
        Some(100)
    );
    Ok(())
}

#[tokio::test]
async fn selected_systemone_invocation_uses_native_model_and_explicit_credential() -> TestResult {
    let server = MockServer::start().await;
    let request = SystemOneCodec::parse_request(request())?;
    let target = ModelTarget {
        provider_name: "fixture".into(),
        service_id: "jev-fixture".into(),
        api_protocol: ApiProtocol::SystemOne,
        api_base: server.uri(),
        api_key: "explicit-test-key".into(),
        credential_priority: Default::default(),
        account_label: None,
        auth_scheme: AuthScheme::Bearer,
        compatibility: Default::default(),
    };
    let mut expected = self::request();
    expected["model"] = json!("jev-fixture");
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .and(header("authorization", "Bearer explicit-test-key"))
        .and(body_json(expected))
        .respond_with(ResponseTemplate::new(200).set_body_json(response()))
        .expect(1)
        .mount(&server)
        .await;
    let client = ModelClient::new(HttpTimeouts::default())?;
    let result = client
        .classify(&target, &request, &CancellationToken::new())
        .await?;
    assert_eq!(result.protocol, ApiProtocol::SystemOne);
    assert_eq!(result.model, "jev-fixture");
    assert_eq!(request.model, "selector");
    assert!(!format!("{target:?}").contains("explicit-test-key"));
    Ok(())
}

#[test]
fn semantic_values_read_legacy_decisions_without_renaming_the_wire() -> TestResult {
    use bitrouter_ai::types::ModelOperation;
    assert_eq!(
        serde_json::from_value::<ModelOperation>(json!("decisions"))?,
        ModelOperation::Classification
    );
    assert_eq!(
        serde_json::to_value(ModelOperation::Classification)?,
        json!("classification")
    );
    assert_eq!(
        serde_json::to_value(ApiProtocol::Decisions)?,
        json!("decisions")
    );
    assert_eq!(
        serde_json::from_value::<ApiProtocol>(json!("systemone"))?,
        ApiProtocol::SystemOne
    );
    assert_eq!(
        serde_json::from_value::<bitrouter_ai::catalog::types::RegistryProtocol>(json!(
            "systemone"
        ))?
        .to_api_protocol(),
        ApiProtocol::SystemOne
    );
    assert!(matches!(
        admission(
            &ApiProtocol::Responses,
            &SystemOneCodec::parse_request(request())?
        )
        .require_admitted(),
        Err(ModelError::Incompatible { .. })
    ));
    Ok(())
}

#[test]
fn native_extensions_are_bounded_and_cannot_overwrite_typed_fields() -> TestResult {
    let request = SystemOneCodec::parse_request(request())?;
    let mut result = SystemOneCodec::parse_response(response(), &request)?;
    if let Some(bitrouter_ai::classifier::ClassifierAnswer::Score { extensions, .. }) =
        result.answers.iter_mut().find(|answer| {
            matches!(
                answer,
                bitrouter_ai::classifier::ClassifierAnswer::Score { .. }
            )
        })
    {
        extensions.insert("legend".into(), json!({"0":"forged"}));
    }
    assert!(validate_result(&result, &request).is_err());
    let mut oversized = response();
    oversized["usage"]["input_tokens_details"] = json!({"cached_tokens":"x".repeat(70 * 1024)});
    let error = SystemOneCodec::parse_response(oversized, &request)
        .err()
        .ok_or("oversized native usage accepted")?;
    let usage = error.classifier_usage().ok_or("usable totals lost")?;
    assert_eq!(usage.prompt_tokens, 100);
    assert!(serde_json::to_vec(&usage.raw)?.len() < 1024);
    Ok(())
}

#[test]
fn custom_native_systemone_result_must_obey_confidence_range() -> TestResult {
    let request = SystemOneCodec::parse_request(request())?;
    let mut result = SystemOneCodec::parse_response(response(), &request)?;
    for answer in &mut result.answers {
        match answer {
            bitrouter_ai::classifier::ClassifierAnswer::Choice { confidence, .. }
            | bitrouter_ai::classifier::ClassifierAnswer::Score { confidence, .. } => {
                *confidence = 1.5
            }
            _ => {}
        }
    }
    let error = validate_result(&result, &request)
        .err()
        .ok_or("invalid custom confidence accepted")?;
    assert_eq!(
        error.classifier_usage().map(|usage| usage.prompt_tokens),
        Some(100)
    );
    Ok(())
}

#[test]
fn conversion_report_derives_confidence_only_for_answers_that_have_it() -> TestResult {
    use bitrouter_ai::conversion::ConversionReason;
    for (question, derived) in [
        (json!({"type":"noul","instructions":"Check?"}), false),
        (
            json!({"type":"choice","instructions":"Choose.","criteria":{"yes":null,"no":null}}),
            true,
        ),
        (
            json!({"type":"score","instructions":"Rate.","criteria":["low","high"]}),
            true,
        ),
    ] {
        let request = SystemOneCodec::parse_request(
            json!({"model":"test","state":"evidence","questions":{"q":question}}),
        )?;
        let report = admission(&ApiProtocol::Decisions, &request);
        report.require_admitted()?;
        assert_eq!(
            report
                .admitted
                .iter()
                .any(|issue| issue.reason == ConversionReason::ClassifierConfidenceDerived),
            derived
        );
    }
    Ok(())
}

#[test]
fn equivalent_native_requests_share_the_three_primitive_semantics() -> TestResult {
    use bitrouter_ai::classifier::ClassifierQuestion;
    let decisions = DecisionsCodec::parse_request(
        json!({"model":"test","input":"evidence","questions":[
            {"type":"predicate","instructions":"Check?"},
            {"type":"choice","instructions":"Choose.","choices":[{"value":"a"},{"value":"b","description":"Second option"}]},
            {"type":"score","instructions":"Rate.","levels":[{"label":"low"},{"label":"high"}]}
        ]}),
    )?;
    let mut systemone = SystemOneCodec::parse_request(
        json!({"model":"test","state":"evidence","questions":{
            "q0":{"type":"noul","instructions":"Check?"},
            "q1":{"type":"choice","instructions":"Choose.","criteria":{"a":null,"b":"Second option"}},
            "q2":{"type":"score","instructions":"Rate.","criteria":["low","high"]}
        }}),
    )?;
    assert_eq!(
        SystemOneCodec::render_request(&decisions)?,
        SystemOneCodec::render_request(&systemone)?
    );
    assert_eq!(
        DecisionsCodec::render_request(&systemone)?,
        DecisionsCodec::render_request(&decisions)?
    );
    // Wire provenance and client correlation keys are independent of the task.
    systemone.source_protocol = decisions.source_protocol.clone();
    for question in &mut systemone.questions {
        match question {
            ClassifierQuestion::Predicate { key, .. }
            | ClassifierQuestion::Choice { key, .. }
            | ClassifierQuestion::Score { key, .. } => *key = None,
        }
    }
    assert_eq!(systemone, decisions);
    Ok(())
}

#[test]
fn decisions_response_renderer_requires_native_string_labels() -> TestResult {
    use bitrouter_ai::classifier::{ClassifierAnswer, ClassifierText};
    let request = SystemOneCodec::parse_request(
        json!({"model":"test","state":"evidence","questions":{"q":{"type":"score","instructions":"Rate.","criteria":[{"level":"low"},{"level":"high"}]}}}),
    )?;
    let mut result = DecisionsCodec::parse_response(
        json!({"model":"test","answers":[{"type":"score","name":null,"score":0.7,"confidence":0.5,"probabilities":[{"value":0,"label":"low","probability":0.3},{"value":1,"label":"high","probability":0.7}]}],"usage":full_usage()}),
        &request,
    )?;
    if let Some(ClassifierAnswer::Score { probabilities, .. }) = result.answers.first_mut()
        && let Some(level) = probabilities.first_mut()
    {
        level.label = ClassifierText::Structured(json!({"level":"low"}));
    }
    let error = DecisionsCodec::render_response(&result, &request)
        .err()
        .ok_or("non-string Decisions label rendered")?;
    assert_eq!(
        error.classifier_usage().map(|usage| usage.prompt_tokens),
        Some(100)
    );
    Ok(())
}
