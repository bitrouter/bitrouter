//! Registered catalog scope and executable endpoint evidence.
use bitrouter_registry::openrouter;
use registry_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn feed() -> Value {
    json!({"id":"catalog","kind":"openrouter_models","writes":["models","metadata"],"request":{"url":openrouter::URL,"auth":{"kind":"none"}},"pagination":{"kind":"none"},"inventory":{"mode":"merge","scope":"public","complete":true,"allow_empty":false}})
}

fn model(id: &str) -> Value {
    json!({"id":id,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},"endpoints":["/api/v1/chat/completions"]})
}

#[test]
fn batch_variants_are_excluded_without_narrowing_the_identity_audit() -> TestResult {
    let mut embedding = model("org/embed:batch");
    embedding["architecture"]["output_modalities"] = json!(["embedding"]);
    let snapshot = json!({"data":[model("org/chat:batch"),embedding,model("org/batch-worker"),model("org/chat:free"),model("org/chat:batch:nitro")]});
    assert_eq!(openrouter::identities(&snapshot)?.len(), 5);
    let observation = openrouter::observation(&feed(), &snapshot)?;
    let imported = observation
        .models
        .iter()
        .map(|m| m.value["provider_model_id"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(imported, [Some("org/batch-worker"), Some("org/chat:free")]);
    Ok(())
}

#[test]
fn text_modalities_alone_never_prove_a_chat_endpoint() -> TestResult {
    for endpoints in [
        json!([]),
        json!(["/api/v1/responses"]),
        json!(["/future/invoke"]),
    ] {
        let mut row = model("org/model");
        row["endpoints"] = endpoints;
        let observation = openrouter::observation(&feed(), &json!({"data":[row]}))?;
        assert_eq!(observation.models[0].value["interfaces"], json!([]));
        assert!(observation.models[0].observed.contains("/interfaces"));
    }
    let observation = openrouter::observation(&feed(), &json!({"data":[model("org/model")]}))?;
    assert_eq!(
        observation.models[0].value["interfaces"][0]["protocol"],
        "chat_completions"
    );
    Ok(())
}

#[test]
fn missing_endpoint_evidence_is_unobserved_instead_of_a_withdrawal() -> TestResult {
    for endpoints in [None, Some(Value::Null)] {
        let mut row = model("org/model");
        let object = row.as_object_mut().ok_or("model object")?;
        object.remove("endpoints");
        if let Some(endpoints) = endpoints {
            object.insert("endpoints".into(), endpoints);
        }
        let observation = openrouter::observation(&feed(), &json!({"data":[row]}))?;
        assert_eq!(observation.models[0].value["interfaces"], json!([]));
        assert!(!observation.models[0].observed.contains("/interfaces"));
    }
    Ok(())
}

#[test]
fn malformed_endpoint_evidence_rejects_the_complete_observation() {
    for endpoints in [
        json!(true),
        json!([7]),
        json!([""]),
        json!(["/api/v1/chat/completions", "/api/v1/chat/completions"]),
    ] {
        let mut row = model("org/model");
        row["endpoints"] = endpoints;
        assert!(
            openrouter::observation(&feed(), &json!({"data":[model("org/good"),row]})).is_err()
        );
    }
}
