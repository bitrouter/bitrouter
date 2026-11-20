use bitrouter_ai::eval::{
    AdmissionOutcome, AdmissionStatus, ErrorEnvelope, FreezeRequest, MetricUnit,
    MetricValidationError, MetricValue, PutSubjectReport, StatusReport, SubjectInsertOutcome,
};
use serde_json::json;

fn subject(identity: serde_json::Value) -> serde_json::Value {
    json!({
        "spec_version":bitrouter_ai::eval::SPEC_VERSION,"identity":identity,"eval_id":"eval","scope":"task",
        "subject_id":"task","cohort":null,"holdout":false,"decisions":[],
        "requested_dimensions":[],"evidence":[],"evidence_digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "observed_at":"2026-11-18T00:00:00Z"
    })
}

fn model_identity() -> serde_json::Value {
    json!({"profile":"tenant_model_policy","installation_id":"installation","tenant_id":"tenant",
        "configuration_revision":"configuration-r1",
        "policy":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"policy","resource_id":"routing","revision_id":"policy-r1"},
        "name":"auto:cost"})
}

#[test]
fn subject_profiles_are_closed_and_identity_is_required() -> Result<(), Box<dyn std::error::Error>>
{
    use bitrouter_ai::eval::EvalSubject;
    for identity in [
        model_identity(),
        json!({"profile":"tenant_generic","installation_id":"installation","tenant_id":"tenant"}),
        json!({"profile":"gateway_generic"}),
    ] {
        let input = subject(identity);
        let decoded: EvalSubject = serde_json::from_value(input.clone())?;
        assert_eq!(serde_json::to_value(decoded)?, input);
    }
    for key in [
        "installation_id",
        "tenant_id",
        "configuration_revision",
        "policy",
        "name",
    ] {
        let mut identity = model_identity();
        identity
            .as_object_mut()
            .ok_or("identity object")?
            .remove(key);
        assert!(
            serde_json::from_value::<EvalSubject>(subject(identity)).is_err(),
            "missing {key}"
        );
    }
    for mut identity in [
        json!({"profile":"gateway_generic"}),
        json!({"profile":"tenant_generic","installation_id":"installation","tenant_id":"tenant"}),
    ] {
        identity["policy"] = model_identity()["policy"].clone();
        assert!(serde_json::from_value::<EvalSubject>(subject(identity)).is_err());
    }
    let mut identity = model_identity();
    identity["unexpected"] = true.into();
    assert!(serde_json::from_value::<EvalSubject>(subject(identity)).is_err());
    let mut input = subject(model_identity());
    input
        .as_object_mut()
        .ok_or("subject object")?
        .remove("identity");
    assert!(serde_json::from_value::<EvalSubject>(input).is_err());
    Ok(())
}

#[test]
fn all_eval_documents_require_the_exact_contract_version() -> Result<(), Box<dyn std::error::Error>>
{
    use bitrouter_ai::eval::{EvalSnapshot, EvalSubject, EvaluationResult, SPEC_VERSION};

    fn observe<T: serde::de::DeserializeOwned>(
        mut document: serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error>> {
        document["spec_version"] = json!(SPEC_VERSION);
        let _: T = serde_json::from_value(document.clone())?;
        for wrong in [
            json!("policy/1.0.0-beta.1"),
            json!("eval/1.0.0-beta.2"),
            json!(1),
            json!(null),
        ] {
            document["spec_version"] = wrong;
            assert!(serde_json::from_value::<T>(document.clone()).is_err());
        }
        document
            .as_object_mut()
            .ok_or("document")?
            .remove("spec_version");
        assert!(serde_json::from_value::<T>(document).is_err());
        Ok(())
    }

    observe::<EvalSubject>(subject(json!({"profile":"gateway_generic"})))?;
    observe::<EvaluationResult>(json!({
        "eval_id": "eval", "evidence_digest": "digest",
        "evaluator": {
            "authority_id": "owner", "evaluator_id": "reviewer", "kind": "human",
            "version": "1", "config_digest": "config"
        },
        "verdict": "pass", "idempotency_key": "submission", "submitted_at": "time"
    }))?;
    observe::<EvalSnapshot>(json!({"evidence_root": "root", "frozen_at": "time", "entries": []}))?;
    Ok(())
}

#[test]
fn eval_decisions_use_the_policy_route_key() -> Result<(), Box<dyn std::error::Error>> {
    use bitrouter_ai::eval::EvalDecisionRef;

    let mut decision = json!({
        "decision_id": "decision", "policy": "auto", "route_projection": "code:review|verify|normal",
        "request_key": "unknown|verify|normal", "selected_tier": "default",
        "baseline_tier": null, "policy_digest": "digest"
    });
    let decoded: EvalDecisionRef = serde_json::from_value(decision.clone())?;
    assert_eq!(
        decoded.route_projection.task_family,
        bitrouter_ai::policy::TaskFamily::CodeReview
    );
    assert_eq!(serde_json::to_value(decoded)?, decision);
    decision["request_key"] = json!("unknown|verify|context");
    assert!(serde_json::from_value::<EvalDecisionRef>(decision).is_err());
    Ok(())
}

#[test]
fn evaluation_receipts_preserve_the_existing_wire_vocabulary()
-> Result<(), Box<dyn std::error::Error>> {
    let receipt: PutSubjectReport =
        serde_json::from_value(json!({"eval_id":"subject", "outcome":"duplicate"}))?;
    assert_eq!(receipt.outcome, SubjectInsertOutcome::Duplicate);
    assert_eq!(
        serde_json::to_value(receipt)?,
        json!({"eval_id":"subject", "outcome":"duplicate"})
    );
    let admission =
        json!({"result_id":"result", "status":"held_out", "reason":"holdout", "duplicate":false});
    let result: AdmissionOutcome = serde_json::from_value(admission.clone())?;
    assert_eq!(result.status, AdmissionStatus::HeldOut);
    assert_eq!(serde_json::to_value(result)?, admission);
    let status = json!({"subjects":1, "results":1, "admission":{"heldout":1}});
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<StatusReport>(status.clone())?)?,
        status
    );
    assert_eq!(
        serde_json::from_value::<FreezeRequest>(json!({}))?,
        FreezeRequest::default()
    );
    Ok(())
}

#[test]
fn metric_validation_is_intrinsic_and_error_diagnostics_do_not_echo_payloads()
-> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        MetricValue::new(2, MetricUnit::Boolean).validate(),
        Err(MetricValidationError::Boolean)
    );
    assert_eq!(
        MetricValue::new(-1, MetricUnit::Ppm).validate(),
        Err(MetricValidationError::Ppm)
    );
    assert_eq!(
        MetricValue::new(-1, MetricUnit::Count).validate(),
        Err(MetricValidationError::Unsigned)
    );
    assert!(
        MetricValue::new(-1, MetricUnit::ScalarMicros)
            .validate()
            .is_ok()
    );
    let payload = json!({"error":"private peer diagnostic"});
    let error: ErrorEnvelope = serde_json::from_value(payload.clone())?;
    assert!(!format!("{error:?}").contains("private peer diagnostic"));
    assert_eq!(serde_json::to_value(error)?, payload);
    Ok(())
}

#[cfg(feature = "schema")]
#[test]
fn published_eval_schemas_match_the_contract() -> Result<(), Box<dyn std::error::Error>> {
    use bitrouter_ai::eval::{EvalSnapshot, EvalSubject, EvaluationResult};

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/specs/eval");
    for (file, schema) in [
        ("subject.schema.json", schemars::schema_for!(EvalSubject)),
        (
            "result.schema.json",
            schemars::schema_for!(EvaluationResult),
        ),
        ("snapshot.schema.json", schemars::schema_for!(EvalSnapshot)),
    ] {
        let published: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join(file))?)?;
        assert_eq!(published, serde_json::to_value(schema)?, "{file}");
    }
    Ok(())
}
