use bitrouter_ai::eval::{
    AdmissionOutcome, AdmissionStatus, ErrorEnvelope, FreezeRequest, MetricUnit,
    MetricValidationError, MetricValue, PutSubjectReport, StatusReport, SubjectInsertOutcome,
};
use serde_json::json;

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
