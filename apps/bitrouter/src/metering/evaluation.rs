//! Host-owned, durable evaluation-attempt settlement.
//!
//! This records only routing, terminal, usage, and pricing evidence. Evaluated
//! state, questions, answers, credentials, and upstream error bodies never
//! enter the row.

use std::sync::Arc;

use async_trait::async_trait;
use bitrouter_sdk::error::{BitrouterError, Result};
use bitrouter_sdk::evaluation::pipeline::{
    EvaluationAttemptRecord, EvaluationAttemptRecorder, EvaluationAttemptTerminal,
};
use bitrouter_sdk::language_model::{Usage, UsageOrigin};
use sea_orm::{ActiveValue::Set, DatabaseConnection, EntityTrait};

use super::entities::evaluation_attempts;
use super::pricing::{
    PricingSource, PricingTable, calculate_charge_evidence, unavailable_charge_evidence,
};

/// The OSS host's append-only evaluation attempt and cost recorder.
pub struct MeteringEvaluationAttemptRecorder {
    db: DatabaseConnection,
    pricing: Arc<PricingTable>,
}

impl MeteringEvaluationAttemptRecorder {
    /// Share the migrated database and current provider-model pricing table.
    pub fn new(db: DatabaseConnection, pricing: Arc<PricingTable>) -> Self {
        Self { db, pricing }
    }
}

#[async_trait]
impl EvaluationAttemptRecorder for MeteringEvaluationAttemptRecorder {
    async fn record(&self, record: EvaluationAttemptRecord) -> Result<Option<f64>> {
        let evidence = record.usage.as_ref().map(|usage| {
            let normalized = Usage {
                prompt_tokens: usage.input_tokens,
                completion_tokens: usage.output_tokens,
                origin: UsageOrigin::ProviderReported,
                ..Usage::default()
            };
            match self
                .pricing
                .resolve(&record.provider, &record.provider_model_id)
            {
                Some(pricing) if !pricing.is_unconfigured() => {
                    calculate_charge_evidence(&normalized, &pricing, PricingSource::Configured)
                }
                _ => unavailable_charge_evidence(&normalized, "pricing_not_found"),
            }
        });
        let charge_micro_usd = evidence.as_ref().and_then(|value| value.charge_micro_usd);
        if charge_micro_usd.is_some_and(|value| value < 0) {
            return Err(BitrouterError::internal(
                "evaluation pricing returned a negative charge",
            ));
        }
        let charge_evidence_json = evidence
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| {
                BitrouterError::internal(format!("serializing evaluation charge evidence: {error}"))
            })?;
        let terminal = match record.terminal {
            EvaluationAttemptTerminal::Completed => "completed",
            EvaluationAttemptTerminal::Failed => "failed",
            EvaluationAttemptTerminal::TimedOut => "timed_out",
            EvaluationAttemptTerminal::UnknownRemoteCompletion => "unknown_remote_completion",
        };
        let row = evaluation_attempts::ActiveModel {
            attempt_id: Set(uuid::Uuid::new_v4().to_string()),
            request_id: Set(record.request_id),
            selector: Set(record.selector),
            provider: Set(record.provider),
            provider_model_id: Set(record.provider_model_id),
            reported_model: Set(record.reported_model),
            account_label: Set(record.account_label),
            attempt_index: Set(i64::try_from(record.attempt).unwrap_or(i64::MAX)),
            format: Set(record.format),
            duration_ms: Set(i64::try_from(record.duration_ms).unwrap_or(i64::MAX)),
            terminal: Set(terminal.to_owned()),
            error_code: Set(record.error_code.map(str::to_owned)),
            input_tokens: Set(record
                .usage
                .as_ref()
                .map(|value| value.input_tokens.to_string())),
            output_tokens: Set(record
                .usage
                .as_ref()
                .map(|value| value.output_tokens.to_string())),
            charge_status: Set(evidence
                .as_ref()
                .map_or("unknown", |value| value.status.as_str())
                .to_owned()),
            charge_micro_usd: Set(charge_micro_usd),
            charge_evidence_json: Set(charge_evidence_json),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
        };
        evaluation_attempts::Entity::insert(row)
            .exec(&self.db)
            .await
            .map_err(|error| {
                BitrouterError::internal(format!(
                    "persisting evaluation terminal evidence: {error}"
                ))
            })?;
        Ok(charge_micro_usd.map(|value| value as f64 / 1_000_000.0))
    }
}
