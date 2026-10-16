use super::*;
use crate::service::admission::{fingerprint, key_scope};
use crate::store::{AcceptedKey, ExecutionRecord};
use crate::turn::TurnStatus;

impl ThreadService {
    pub async fn find_acp_permission_answer(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        answer: &Approval,
    ) -> Result<Option<TurnReceipt>, ServiceError> {
        self.read_stored_thread_view(target, caller).await?;
        let scope = key_scope(
            caller,
            Some(&target.thread_id),
            "answer_acp_permission",
            &answer.idempotency_key,
        )?;
        let hash = fingerprint(&(&answer.turn_id, &answer.request_id, &answer.option_id))?;
        match self
            .accepted_key(&scope, &answer.idempotency_key, &hash)
            .await?
        {
            Some(entry) => self.existing_turn_receipt(&entry).await.map(Some),
            None => Ok(None),
        }
    }

    pub async fn answer_acp_permission(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        answer: Approval,
    ) -> Result<TurnReceipt, ServiceError> {
        self.ensure_instance(Some(&target.server_instance_id))?;
        let admission = self.inner.admission.lock().await;
        let scope = key_scope(
            caller,
            Some(&target.thread_id),
            "answer_acp_permission",
            &answer.idempotency_key,
        )?;
        let hash = fingerprint(&(&answer.turn_id, &answer.request_id, &answer.option_id))?;
        if let Some(key) = self
            .accepted_key(&scope, &answer.idempotency_key, &hash)
            .await?
        {
            drop(admission);
            return self.existing_turn_receipt(&key).await;
        }
        let gate = self.commit_gate(&answer.turn_id)?;
        let _guard = gate.lock().await;
        self.authorize_active_turn(target, caller, &answer.turn_id)?;
        {
            let state = self.lock_state();
            let turn = state
                .turns
                .get(&answer.turn_id)
                .ok_or_else(crate::service::unknown_turn)?;
            if turn.cancel.is_cancelled()
                || turn.fence.pending()
                || turn.snapshot.status != TurnStatus::WaitingForInput
                || turn.snapshot.acp_selected.contains_key(&answer.request_id)
                || turn.snapshot.acp_selected.len() >= 1024
                || turn.acp_replies.len() >= 16
            {
                return Err("ACP permission is closed, already selected or over capacity".into());
            }
            let permission = turn
                .snapshot
                .acp_permissions
                .iter()
                .find(|permission| permission.request_id == answer.request_id)
                .ok_or("ACP permission is not pending on this Turn")?;
            if !permission.options.iter().any(|option| {
                option.get("optionId").and_then(serde_json::Value::as_str)
                    == Some(answer.option_id.as_str())
            }) {
                return Err("ACP option was not offered by the original request".into());
            }
        }
        let key = AcceptedKey {
            scope,
            key: answer.idempotency_key.clone(),
            fingerprint: hash,
            thread_id: target.thread_id.clone(),
            turn_id: Some(answer.turn_id.clone()),
        };
        self.commit_serialized(
            &answer.turn_id,
            &[
                ExecutionRecord::AcceptedKey { entry: key.clone() },
                ExecutionRecord::AcpPermissionChoice {
                    request_id: answer.request_id.clone(),
                    option_id: answer.option_id.clone(),
                    action_id: answer.idempotency_key.clone(),
                },
            ],
        )
        .await?;
        self.inner.runtime_changed.notify_waiters();
        drop(_guard);
        drop(admission);
        self.existing_turn_receipt(&key).await
    }
}
