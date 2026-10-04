//! Bounded auxiliary outcomes. Count projection invalidates routing evidence;
//! it never turns rejected metadata into a verified input fit.

use super::native::{
    NativeEvidenceCommitment, NativeExecutionControl, NativeInputCount, NativeInputCountRejection,
    NativeInputCountReport, auxiliary_report_allowance,
};
use crate::{BitrouterError, Result};

pub(super) fn limit(control: &dyn NativeExecutionControl, request_id: &str) -> Result<Option<u64>> {
    let limit = control.auxiliary_report_byte_limit(request_id)?;
    if let Some(limit) = limit
        && limit < auxiliary_report_allowance(request_id)?
    {
        return Err(BitrouterError::bad_request(
            "auxiliary report limit cannot hold rejection evidence",
        ));
    }
    Ok(limit)
}

pub(super) fn count(report: &mut NativeInputCountReport, limit: Option<u64>) -> Result<()> {
    let Some(limit) = limit else { return Ok(()) };
    if super::native_output::fits(report, limit) {
        return Ok(());
    }
    report.report_rejection = Some(NativeInputCountRejection {
        version: 1,
        byte_limit: limit,
        original: NativeEvidenceCommitment::capture(report)?,
        input_tokens: match report.outcome {
            NativeInputCount::Counted { input_tokens, .. } => Some(input_tokens),
            NativeInputCount::Unavailable { .. } => None,
        },
    });
    report.outcome = NativeInputCount::Unavailable {
        reason: NativeInputCountRejection::REASON.into(),
    };
    if !super::native_output::fits(report, limit) {
        return Err(BitrouterError::internal(
            "input count rejection exceeds admitted envelope",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::native::{NativeAttemptReport, NativePlan, NativePlanAdmission};
    use super::super::native_preparation::{NativePreparationWork, NativePreparationWorkKind};
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Control(Option<u64>);

    #[async_trait::async_trait]
    impl NativeExecutionControl for Control {
        fn auxiliary_report_byte_limit(&self, _: &str) -> Result<Option<u64>> {
            Ok(self.0)
        }
        async fn plan(&self, _: NativePlan) -> Result<NativePlanAdmission> {
            Ok(NativePlanAdmission {
                route_indices: vec![0],
            })
        }
        async fn before_attempt(&self, _: &str, _: u32) -> Result<()> {
            Ok(())
        }
        async fn after_attempt(&self, _: NativeAttemptReport) {}
    }

    #[tokio::test]
    async fn insufficient_auxiliary_envelope_rejects_before_callback_execution() -> Result<()> {
        let request_id = "request\0\"".repeat(1024);
        let required = auxiliary_report_allowance(&request_id)?;
        for byte_limit in [None, Some(required - 1), Some(required)] {
            let called = AtomicBool::new(false);
            let outcome = super::super::native_preparation::observe(
                &Control(byte_limit),
                NativePreparationWork {
                    request_id: request_id.clone(),
                    kind: NativePreparationWorkKind::PreRequestHook,
                    work_index: u32::MAX,
                },
                async {
                    called.store(true, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await;
            let admitted = byte_limit.is_none_or(|limit| limit >= required);
            assert_eq!(outcome.is_ok(), admitted);
            assert_eq!(called.load(Ordering::SeqCst), admitted);
        }
        Ok(())
    }

    #[test]
    fn exact_count_limit_preserves_evidence_and_larger_metadata_cannot_prove_fit() -> Result<()> {
        for counted in [false, true] {
            let mut report = NativeInputCountReport {
                request_id: "request\0\"".repeat(1024),
                route_index: u32::MAX,
                elapsed_ms: u64::MAX,
                outcome: if counted {
                    NativeInputCount::Counted {
                        input_tokens: u64::MAX,
                        request_sha256: "a".repeat(64),
                        source: String::new(),
                    }
                } else {
                    NativeInputCount::Unavailable {
                        reason: String::new(),
                    }
                },
                report_rejection: None,
            };
            let limit = auxiliary_report_allowance(&report.request_id)?;
            let padding = limit - NativeEvidenceCommitment::capture(&report)?.bytes;
            let field = match &mut report.outcome {
                NativeInputCount::Counted { source, .. } => source,
                NativeInputCount::Unavailable { reason } => reason,
            };
            *field = "x".repeat(padding as usize);
            assert_eq!(NativeEvidenceCommitment::capture(&report)?.bytes, limit);
            let original = report.clone();
            count(&mut report, Some(limit))?;
            assert_eq!(report, original);
            let field = match &mut report.outcome {
                NativeInputCount::Counted { source, .. } => source,
                NativeInputCount::Unavailable { reason } => reason,
            };
            field.push('x');
            let expected = NativeEvidenceCommitment::capture(&report)?;
            count(&mut report, Some(limit))?;
            let rejection = report
                .report_rejection
                .as_ref()
                .ok_or_else(|| BitrouterError::internal("rejection missing"))?;
            assert_eq!(rejection.original, expected);
            assert_eq!(rejection.input_tokens, counted.then_some(u64::MAX));
            assert!(
                matches!(&report.outcome, NativeInputCount::Unavailable { reason } if reason == NativeInputCountRejection::REASON)
            );
            assert!(super::super::native_output::fits(&report, limit));
            let encoded = serde_json::to_vec(&report)
                .map_err(|error| BitrouterError::internal(error.to_string()))?;
            assert_eq!(
                serde_json::from_slice::<NativeInputCountReport>(&encoded)
                    .map_err(|error| BitrouterError::internal(error.to_string()))?,
                report
            );
        }
        Ok(())
    }
}
