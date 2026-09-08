//! Prompt boundaries own their immutable source frontiers. Reconciliation is
//! bounded and any unfinished collection remains part of the checkpoint.

use super::*;
use crate::session_evidence::types::AcpSessionKey;

impl ControllerEvidence {
    pub(super) async fn capture_native_checkpoint(
        &self,
        observation: &SessionObservation,
        session: AcpSessionKey,
    ) -> Result<String> {
        let mut gaps = BTreeSet::new();
        match tokio::time::timeout(Duration::from_secs(30), self.reconcile()).await {
            Ok(Ok(snapshot)) => {
                gaps.extend(snapshot.gaps.into_iter().filter(|gap| {
                    !gap.starts_with("workspace_")
                        && !gap.starts_with("native_checkpoint_")
                        && !matches!(
                            gap.as_str(),
                            "native_attempt_membership_unavailable"
                                | "native_prompt_response_unobserved"
                                | "native_task_state_invalid"
                                | "native_task_session_limit"
                        )
                }));
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "native collection before checkpoint failed");
                gaps.insert("native_checkpoint_collection_failed".into());
            }
            Err(_) => {
                gaps.insert("native_checkpoint_collection_timed_out".into());
            }
        }
        self.store
            .capture_checkpoint(
                &self.controller_id,
                &observation.operation_id,
                &observation.phase,
                session,
                gaps,
            )
            .await
    }
}
