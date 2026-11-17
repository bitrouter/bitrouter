use bitrouter_ai::eval::{
    AdmissionOutcome, AdmissionStatus, ErrorEnvelope, FreezeRequest, MetricUnit,
    MetricValidationError, MetricValue, PutSubjectReport, StatusReport, SubjectInsertOutcome,
};
use serde_json::json;

fn subject(identity: serde_json::Value) -> serde_json::Value {
    json!({
        "schema_version":2,"identity":identity,"eval_id":"eval","scope":"task",
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
fn subject_schema_two_profiles_are_closed_and_identity_is_required()
-> Result<(), Box<dyn std::error::Error>> {
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
fn subject_version_cutover_does_not_change_result_or_measurement_versions()
-> Result<(), Box<dyn std::error::Error>> {
    use bitrouter_ai::eval::{
        EVAL_RESULT_SCHEMA_VERSION, EVAL_SUBJECT_SCHEMA_VERSION, EvalSubject,
        ROUTE_MEASUREMENT_SCHEMA_VERSION,
    };
    assert_eq!(
        (
            EVAL_SUBJECT_SCHEMA_VERSION,
            EVAL_RESULT_SCHEMA_VERSION,
            ROUTE_MEASUREMENT_SCHEMA_VERSION
        ),
        (2, 1, 2)
    );
    for version in [0, 1, 3] {
        let mut input = subject(json!({"profile":"gateway_generic"}));
        input["schema_version"] = version.into();
        assert!(serde_json::from_value::<EvalSubject>(input).is_err());
    }
    for field in ["policy_digest", "preset"] {
        let mut input = subject(json!({"profile":"gateway_generic"}));
        input[field] = "old identity substitute".into();
        assert!(serde_json::from_value::<EvalSubject>(input).is_err());
    }
    Ok(())
}

#[test]
fn previous_subject_reader_and_current_writer_refuse_each_others_wire_shape()
-> Result<(), Box<dyn std::error::Error>> {
    // Frozen schema-1 reader fields, without a historical executable dependency.
    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PreviousSubject {
        schema_version: u32,
        eval_id: String,
        scope: bitrouter_ai::eval::EvalScope,
        subject_id: String,
        policy_digest: String,
        preset: Option<String>,
        cohort: Option<String>,
        holdout: bool,
        #[serde(default)]
        decisions: Vec<bitrouter_ai::eval::EvalDecisionRef>,
        #[serde(default)]
        requested_dimensions: std::collections::BTreeSet<String>,
        #[serde(default)]
        evidence: Vec<bitrouter_ai::eval::EvidenceItem>,
        evidence_digest: String,
        observed_at: String,
    }
    let current = subject(json!({"profile":"gateway_generic"}));
    assert!(serde_json::from_value::<PreviousSubject>(current.clone()).is_err());
    let mut old = current;
    old.as_object_mut()
        .ok_or("subject object")?
        .remove("identity");
    old["schema_version"] = 1.into();
    old["policy_digest"] =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
    old["preset"] = "auto".into();
    let accepted_old: PreviousSubject = serde_json::from_value(old.clone())?;
    assert_eq!(accepted_old.schema_version, 1);
    assert!(serde_json::from_value::<bitrouter_ai::eval::EvalSubject>(old).is_err());
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
