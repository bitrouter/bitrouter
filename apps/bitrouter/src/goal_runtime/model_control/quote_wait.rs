//! Wait on releasable current-worker quotes without changing an attempt key.
use super::*;
use bitrouter_orchestrator::goal::{GoalError, GoalOperation, OperationKind};

type Commit = std::result::Result<GoalOperation, GoalError>;

impl ModelControl {
    pub(super) async fn commit_fact(
        &self,
        operation: &str,
        revision: u64,
        change: Change,
    ) -> Commit {
        let mut retry = 0;
        loop {
            let time = now().map_err(|error| GoalError::Storage {
                message: error.to_string(),
            })?;
            let result = self
                .goals
                .workflow_change(
                    WorkflowActor {
                        owner: "local",
                        coordinator_id: &self.coordinator_id,
                    },
                    &self.goal_id,
                    operation,
                    revision,
                    change.clone(),
                    time,
                )
                .await;
            if retry == 3
                || !matches!(
                    &result,
                    Err(GoalError::Conflict | GoalError::Storage { .. })
                )
            {
                return result;
            }
            // Repeat only the same durable fact, never preparation/provider I/O.
            tokio::time::sleep(Duration::from_millis(5 << retry)).await;
            retry += 1;
        }
    }

    pub(super) async fn wait_for_quote(
        &self,
        operation: &str,
        revision: u64,
        change: &Change,
        mut result: Commit,
    ) -> bitrouter_sdk::Result<Commit> {
        let Change::Accounting {
            event: Event::Begin { work },
        } = change
        else {
            return Ok(result);
        };
        if self.worker.is_none() || work.kind != WorkKind::ProviderAttempt {
            return Ok(result);
        }
        let wait_key = format!("{operation}/quote-wait");
        let mut waiting = false;
        let admitted = async {
            while matches!(&result, Err(GoalError::BudgetExceeded { .. })) {
                self.gate().await?;
                let GoalReply::Snapshot { goal } = self.goals.execute("local",
                    GoalCommand::Get { goal_id: self.goal_id.clone() }, now().map_err(|_| unavailable())?
                ).await.map_err(|_| unavailable())? else { return Err(unavailable()) };
                match bitrouter_orchestrator::goal::budget::waiting_on_workers(&goal, work.token_reservation.as_ref()) {
                    Ok(true) => {
                        if !waiting {
                            let intent = crate::goal_artifacts::put(&self.root, &serde_json::to_vec(
                                &serde_json::json!({"reason":"current_worker_quotes", "quote":work.token_reservation})
                            ).map_err(|_| unavailable())?).await.map_err(|_| unavailable())?;
                            self.wait_fact(&format!("{}-begin", key_id(&wait_key)), revision,
                                Change::Accounting { event: Event::Begin { work: Work {
                                    id: key_id(&wait_key), kind: WorkKind::ResourceWait,
                                    source: work.source.clone(), requirements_revision: revision,
                                    request_id: work.request_id.clone(), intent, token_reservation: None,
                                } } }
                            ).await?;
                            waiting = true;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Ok(false) => {}
                    Err(_) => break,
                }
                self.gate().await?;
                result = self.commit_fact(operation, revision, change.clone()).await;
            }
            Ok(())
        }.await;
        if waiting {
            self.finish_quote_wait(&wait_key, revision).await?;
        }
        admitted?;
        Ok(result)
    }

    async fn wait_fact(
        &self,
        operation: &str,
        revision: u64,
        change: Change,
    ) -> bitrouter_sdk::Result<()> {
        let result = self.commit_fact(operation, revision, change).await;
        if let Err(error) = &result {
            self.fail_ack(&format!("resource wait fact was not acknowledged: {error}"))
                .await;
        }
        result
            .map(|_| ())
            .map_err(|error| diagnostic(&error.to_string()))
    }

    async fn finish_quote_wait(&self, key: &str, revision: u64) -> bitrouter_sdk::Result<()> {
        let report = crate::goal_artifacts::put(&self.root, br#"{"waiting_finished":true}"#)
            .await
            .map_err(|_| unavailable())?;
        self.wait_fact(
            &format!("{}-outcome", key_id(key)),
            revision,
            Change::Accounting {
                event: Event::Observe {
                    id: key_id(key),
                    report,
                    estimated_model_tokens_micro_usd: None,
                    reservation_verified: false,
                },
            },
        )
        .await?;
        Ok(())
    }

    pub(super) async fn abort_admission(
        &self,
        request: &str,
        index: u32,
    ) -> bitrouter_sdk::Result<()> {
        let key = format!("{request}/attempt/{index}");
        let wait_key = format!("{}-begin/quote-wait", key_id(&key));
        if let Some(wait) = self.intent(&key_id(&wait_key)).await? {
            self.finish_quote_wait(&wait_key, wait.requirements_revision)
                .await?;
        }
        if let Some(work) = self.intent(&key_id(&key)).await? {
            if work.kind != WorkKind::ProviderAttempt
                || work.request_id.as_deref() != Some(request)
                || work.source.as_deref() != self.worker.as_ref()
            {
                return Err(diagnostic("undispatched admission source differs"));
            }
            // The SDK invokes this only before polling its executor, including
            // when cancellation followed a committed host reservation.
            self.observe_with_reservation(
                &key,
                serde_json::json!({"sdk_admission_aborted":true}),
                Some(0),
                true,
            )
            .await?;
        }
        Ok(())
    }

    pub(super) async fn plan_outcome(
        &self,
        key: &str,
    ) -> bitrouter_sdk::Result<Option<serde_json::Value>> {
        let operation = self
            .goals
            .execute(
                "local",
                GoalCommand::Operation {
                    operation_id: format!("{}-outcome", key_id(key)),
                },
                now().map_err(|_| unavailable())?,
            )
            .await;
        let operation = match operation {
            Ok(GoalReply::Accepted { operation }) => operation,
            Err(GoalError::NotFound) => return Ok(None),
            _ => return Err(unavailable()),
        };
        let OperationKind::Workflow { change } = operation.kind else {
            return Err(unavailable());
        };
        let Change::Accounting {
            event: Event::Observe { id, report, .. },
        } = *change
        else {
            return Err(unavailable());
        };
        if operation.goal_id != self.goal_id || id != key_id(key) {
            return Err(unavailable());
        }
        let bytes = crate::goal_artifacts::read(&self.root, &report)
            .await
            .map_err(|_| unavailable())?;
        Ok(Some(
            serde_json::from_slice(&bytes).map_err(|_| unavailable())?,
        ))
    }
}
