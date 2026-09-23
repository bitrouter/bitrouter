use bitrouter_laya_extension::{
    LayaLocalFormat, MAX_CHOICE_OPTIONS, MAX_QUESTIONS, MAX_SCORE_LEVELS, PROVIDER_MODEL_ID,
    REVISION, register,
};
use bitrouter_sdk::evaluation::{EvaluationAnswer, EvaluationRequest, EvaluationRoutingTarget};
use bitrouter_sdk::extension::{EvaluationFormatAdapter, ExtensionApi};
use serde_json::{Value, json};

fn request() -> std::result::Result<EvaluationRequest, serde_json::Error> {
    serde_json::from_str(include_str!("fixtures/mixed_request.json"))
}

fn response() -> std::result::Result<Value, serde_json::Error> {
    serde_json::from_str(include_str!("fixtures/mixed_response.json"))
}

fn target() -> EvaluationRoutingTarget {
    EvaluationRoutingTarget {
        provider: "laya".to_owned(),
        provider_model_id: PROVIDER_MODEL_ID.to_owned(),
    }
}

#[test]
fn registration_is_explicit_and_revisioned() -> std::result::Result<(), Box<dyn std::error::Error>>
{
    let mut api = ExtensionApi::new();
    register(&mut api)?;
    assert_eq!(REVISION, 1);
    assert!(register(&mut api).is_err());
    Ok(())
}

#[test]
fn request_preserves_structure_and_exact_snapshot_mapping()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let wire = LayaLocalFormat.render_request(&request()?, &target())?;
    assert_eq!(wire["model"], PROVIDER_MODEL_ID);
    assert_eq!(wire["state"]["ticket"], "Synthetic duplicate charge");
    assert_eq!(
        wire["questions"]["team"]["criteria"]["billing"],
        Value::Null
    );
    assert!(wire.get("provider").is_none());
    Ok(())
}

#[test]
fn real_checkpoint_fixture_preserves_types_usage_and_version_without_action_head()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let result = LayaLocalFormat.parse_response(response()?, &request()?)?;
    assert_eq!(result.model, PROVIDER_MODEL_ID);
    assert_eq!(result.usage.input_tokens, 92);
    assert_eq!(result.usage.output_tokens, 0);
    assert_eq!(result.usage.cost, None);
    assert!(matches!(
        result.answers.get("billing"),
        Some(EvaluationAnswer::Noul { noul }) if *noul == 0.3868
    ));
    assert!(matches!(
        result.answers.get("team"),
        Some(EvaluationAnswer::Choice { choice, confidence: Some(confidence), .. })
            if choice == "billing" && *confidence == 0.1203
    ));
    assert!(matches!(
        result.answers.get("urgency"),
        Some(EvaluationAnswer::Score { score, confidence: Some(confidence), .. })
            if *score == 0.5665 && *confidence == 0.0128
    ));
    let public = serde_json::to_value(result)?;
    assert!(public["answers"]["team"].get("action").is_none());
    assert!(public["answers"]["billing"].get("confidence").is_none());
    Ok(())
}

#[test]
fn four_decimal_rounding_is_accepted_but_invalid_sums_are_not()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut rounded = response()?;
    rounded["answers"]["team"]["probabilities"]["technical"] = json!(0.2986);
    assert!(LayaLocalFormat.parse_response(rounded, &request()?).is_ok());
    let mut invalid = response()?;
    invalid["answers"]["team"]["probabilities"]["technical"] = json!(0.28);
    assert!(
        LayaLocalFormat
            .parse_response(invalid, &request()?)
            .is_err()
    );
    Ok(())
}

#[test]
fn malformed_keys_types_legend_version_and_usage_are_rejected()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let request = request()?;
    let mut cases = Vec::new();
    let mut wrong_model = response()?;
    wrong_model["model"] = json!("laya-rl-agent");
    cases.push(wrong_model);
    let mut missing_answer = response()?;
    if let Some(answers) = missing_answer["answers"].as_object_mut() {
        answers.remove("billing");
    }
    cases.push(missing_answer);
    let mut wrong_type = response()?;
    wrong_type["answers"]["billing"]["type"] = json!("choice");
    cases.push(wrong_type);
    let mut wrong_legend = response()?;
    wrong_legend["answers"]["urgency"]["legend"]["1"] = json!("not urgent");
    cases.push(wrong_legend);
    let mut negative_usage = response()?;
    negative_usage["usage"]["input_tokens"] = json!(-1);
    cases.push(negative_usage);
    let mut invented_price = response()?;
    invented_price["usage"]["cost"] = json!(0.0);
    cases.push(invented_price);
    let mut extra = response()?;
    extra["provider"] = json!("untrusted");
    cases.push(extra);
    for body in cases {
        assert!(LayaLocalFormat.parse_response(body, &request).is_err());
    }
    Ok(())
}

#[test]
fn local_resource_bounds_reject_before_transport()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut wrong_model = target();
    wrong_model.provider_model_id = "laya-latest".to_owned();
    assert!(
        LayaLocalFormat
            .render_request(&request()?, &wrong_model)
            .is_err()
    );

    let mut questions = json!({});
    for index in 0..=MAX_QUESTIONS {
        questions[format!("q{index}")] = json!({"type":"noul","instructions":"test"});
    }
    let many: EvaluationRequest = serde_json::from_value(json!({
        "model":"laya/typed-decisions-f9ab0b2", "state":"synthetic", "questions":questions
    }))?;
    assert!(LayaLocalFormat.render_request(&many, &target()).is_err());

    let mut choices = json!({});
    for index in 0..=MAX_CHOICE_OPTIONS {
        choices[format!("c{index}")] = Value::Null;
    }
    let large_choice: EvaluationRequest = serde_json::from_value(json!({
        "model":"laya/typed-decisions-f9ab0b2", "state":"synthetic", "questions":{
            "team":{"type":"choice","instructions":"test","criteria":choices}
        }
    }))?;
    assert!(
        LayaLocalFormat
            .render_request(&large_choice, &target())
            .is_err()
    );

    let levels = vec!["level"; MAX_SCORE_LEVELS + 1];
    let large_score: EvaluationRequest = serde_json::from_value(json!({
        "model":"laya/typed-decisions-f9ab0b2", "state":"synthetic", "questions":{
            "urgency":{"type":"score","instructions":"test","criteria":levels}
        }
    }))?;
    assert!(
        LayaLocalFormat
            .render_request(&large_score, &target())
            .is_err()
    );
    Ok(())
}
