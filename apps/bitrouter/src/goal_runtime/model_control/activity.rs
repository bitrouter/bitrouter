//! Received content observations, persisted inside the existing attempt report.
use super::*;
use bitrouter_ai::types::StreamPart;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::goal_runtime) struct StreamActivity {
    pub version: u32,
    pub clock_id: String,
    pub first_received_tick_ns: u64,
    pub last_received_tick_ns: u64,
    pub first_observed_at_ms: u64,
    pub last_observed_at_ms: u64,
    pub content_parts: u64,
    pub clock_regressed: bool,
}

impl StreamActivity {
    pub(in crate::goal_runtime) fn valid(&self) -> bool {
        self.version == 1
            && !self.clock_id.is_empty()
            && self.clock_id.len() <= 128
            && self.first_received_tick_ns <= self.last_received_tick_ns
            && self.content_parts > 0
    }
}

impl ModelControl {
    pub(super) async fn record_content(&self, request_id: &str, part: &StreamPart) {
        let content = match part {
            StreamPart::TextDelta { text } | StreamPart::ReasoningDelta { text, .. } => {
                !text.is_empty()
            }
            StreamPart::ToolCallDelta { arguments, .. } => !arguments.is_empty(),
            _ => false,
        };
        if !content {
            return;
        }
        let Ok(time) = now() else { return };
        // Only intervals from this same process clock can be compared. Wall
        // clock adjustments never establish cross-process overlap.
        static CLOCK: std::sync::OnceLock<(String, std::time::Instant)> =
            std::sync::OnceLock::new();
        let (clock_id, origin) =
            CLOCK.get_or_init(|| (uuid::Uuid::new_v4().to_string(), std::time::Instant::now()));
        let Ok(tick) = u64::try_from(origin.elapsed().as_nanos()) else {
            return;
        };
        let mut activity = self.activity.lock().await;
        // Omitted observations stay unknown. This buffer has no scheduling,
        // permission or result authority and retains no model content.
        if !activity.contains_key(request_id) && activity.len() >= 32 {
            activity.pop_first();
        }
        let current = activity.entry(request_id.into()).or_insert(StreamActivity {
            version: 1,
            clock_id: clock_id.clone(),
            first_received_tick_ns: tick,
            last_received_tick_ns: tick,
            first_observed_at_ms: time,
            last_observed_at_ms: time,
            content_parts: 0,
            clock_regressed: false,
        });
        current.clock_regressed |= time < current.last_observed_at_ms;
        current.last_observed_at_ms = time;
        current.last_received_tick_ns = tick;
        current.content_parts = current.content_parts.saturating_add(1);
    }

    /// Replayed acknowledgements must retain the original bytes, activity and
    /// reservation verdict even after the in-memory observation buffer is gone.
    pub(super) async fn replay_attempt(
        &self,
        report: &NativeAttemptReport,
    ) -> bitrouter_sdk::Result<bool> {
        let id = key_id(&format!(
            "{}/attempt/{}",
            report.request_id, report.attempt_index
        ));
        let operation_id = format!("{id}-outcome");
        let reply = self
            .goals
            .execute(
                "local",
                GoalCommand::Operation {
                    operation_id: operation_id.clone(),
                },
                now().map_err(|_| unavailable())?,
            )
            .await;
        let operation = match reply {
            Err(bitrouter_orchestrator::goal::GoalError::NotFound) => return Ok(false),
            Ok(GoalReply::Accepted { operation }) if operation.goal_id == self.goal_id => operation,
            _ => return Err(unavailable()),
        };
        let bitrouter_orchestrator::goal::OperationKind::Workflow { change } = operation.kind
        else {
            return Err(unavailable());
        };
        let Change::Accounting { event } = *change else {
            return Err(unavailable());
        };
        let Event::Observe {
            id: observed_id,
            report: reference,
            ..
        } = &event
        else {
            return Err(unavailable());
        };
        let intent = self.intent(&id).await?.ok_or_else(unavailable)?;
        if observed_id != &id
            || intent.kind != WorkKind::ProviderAttempt
            || intent.source.as_deref() != self.worker.as_ref()
        {
            return Err(unavailable());
        }
        let bytes = crate::goal_artifacts::read(&self.root, reference)
            .await
            .map_err(|_| unavailable())?;
        let mut original: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        let Some(object) = original.as_object_mut() else {
            return Err(unavailable());
        };
        object.remove("bro_stream_activity");
        if original != serde_json::to_value(report).map_err(|_| unavailable())? {
            return Err(unavailable());
        }
        self.commit(&operation_id, event).await?;
        self.activity.lock().await.remove(&report.request_id);
        Ok(true)
    }
}
