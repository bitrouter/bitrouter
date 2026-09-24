use bitrouter_sdk::error::Result;
use bitrouter_sdk::evaluation::{EvaluationAnswer, EvaluationRequest, EvaluationRoutingTarget};
use bitrouter_sdk::extension::ExtensionApi;
use bitrouter_sdk::extension::provider::EvaluationProvider;
use bitrouter_typesafe_provider::{MODEL_ID, PROVIDER_ID, TypeSafeProvider, register};
use serde_json::{Value, json};

fn request() -> std::result::Result<EvaluationRequest, serde_json::Error> {
    serde_json::from_str(include_str!("fixtures/mixed_request.json"))
}

fn response() -> std::result::Result<Value, serde_json::Error> {
    serde_json::from_str(include_str!("fixtures/mixed_response.json"))
}

#[test]
fn registration_is_provider_owned_and_exact() -> Result<()> {
    let mut api = ExtensionApi::new();
    register(&mut api)?;
    let descriptor = TypeSafeProvider.descriptor();
    assert_eq!(descriptor.provider_id, PROVIDER_ID);
    assert_eq!(descriptor.models[0].id, MODEL_ID);
    assert!(register(&mut api).is_err());
    assert!(api.into_registrations().is_err());
    Ok(())
}

#[test]
fn mixed_request_preserves_structure_nulls_and_provider_model_mapping()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let request = request()?;
    let wire = TypeSafeProvider.render_request(
        &request,
        &EvaluationRoutingTarget {
            provider_model_id: "jev-1.13.0".into(),
        },
    )?;
    assert_eq!(wire["model"], "jev-1.13.0");
    assert_eq!(wire["state"]["account"]["attempts"], json!([1, 2]));
    assert_eq!(
        wire["questions"]["department"]["criteria"]["sales"],
        Value::Null
    );
    assert!(wire.get("provider").is_none());
    assert!(wire.get("id").is_none());
    Ok(())
}

#[test]
fn mixed_response_preserves_all_answer_kinds_version_usage_and_confidence()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let result = TypeSafeProvider.parse_response(response()?, &request()?)?;
    assert_eq!(result.model, "jev-1.13.0");
    assert_eq!(result.usage.input_tokens, 318);
    assert_eq!(result.usage.output_tokens, 34);
    assert_eq!(result.usage.cost, None);
    assert!(matches!(
        result.answers.get("urgency"),
        Some(EvaluationAnswer::Noul { noul }) if *noul == 0.94
    ));
    assert!(matches!(
        result.answers.get("department"),
        Some(EvaluationAnswer::Choice { choice, confidence: Some(confidence), .. })
            if choice == "billing" && *confidence == 0.72
    ));
    assert!(matches!(
        result.answers.get("severity"),
        Some(EvaluationAnswer::Score { score, confidence: Some(confidence), .. })
            if *score == 1.0 && *confidence == 0.55
    ));
    Ok(())
}

#[test]
fn rounding_drift_is_accepted_but_invalid_distributions_are_rejected()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let request = request()?;
    let mut malformed = response()?;
    malformed["answers"]["severity"]["probabilities"]["2"] = json!(0.1);
    assert!(
        TypeSafeProvider
            .parse_response(malformed, &request)
            .is_err()
    );
    let valid = TypeSafeProvider.parse_response(response()?, &request)?;
    assert!(matches!(
        valid.answers.get("severity"),
        Some(EvaluationAnswer::Score { .. })
    ));
    Ok(())
}

#[test]
fn malformed_answer_keys_types_ranges_legend_and_usage_are_rejected()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let request = request()?;
    let mut cases = Vec::new();
    let mut missing = response()?;
    if let Some(answers) = missing["answers"].as_object_mut() {
        answers.remove("urgency");
    }
    cases.push(missing);
    let mut extra = response()?;
    extra["answers"]["extra"] = json!({"type": "noul", "noul": 0.5});
    cases.push(extra);
    let mut wrong_type = response()?;
    wrong_type["answers"]["urgency"] =
        json!({"type": "choice", "choice": "x", "probabilities": {"x": 1.0}});
    cases.push(wrong_type);
    let mut out_of_range = response()?;
    out_of_range["answers"]["urgency"]["noul"] = json!(1.2);
    cases.push(out_of_range);
    let mut wrong_keys = response()?;
    wrong_keys["answers"]["department"]["probabilities"] = json!({"billing": 0.8, "other": 0.2});
    cases.push(wrong_keys);
    let mut wrong_legend = response()?;
    wrong_legend["answers"]["severity"]["legend"]["2"] = json!("not the rubric");
    cases.push(wrong_legend);
    let mut negative_usage = response()?;
    negative_usage["usage"]["input_tokens"] = json!(-1);
    cases.push(negative_usage);
    for body in cases {
        assert!(TypeSafeProvider.parse_response(body, &request).is_err());
    }
    Ok(())
}
