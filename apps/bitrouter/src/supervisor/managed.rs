//! Managed sessions have one controller: the shared Thread/Turn owner. The
//! supervisor retains protocol/process mechanics and original action receipts.
use super::*;
use bitrouter_orchestrator::service::worker::acp::{Action, Owner, Stop};

#[async_trait::async_trait]
pub(crate) trait DispatchGuard: Send + Sync {
    async fn authorize(&self) -> Result<()>;
}

impl Supervisor {
    async fn managed_run(&self, owner: &Owner, run_id: &str) -> Result<Arc<Run>> {
        let run = self.run(run_id).await?;
        anyhow::ensure!(
            run.snapshot().await.managed_by.as_ref() == Some(owner),
            "ACP run belongs to another Thread/Turn owner"
        );
        Ok(run)
    }

    pub(crate) async fn managed_prompt(
        &self,
        owner: &Owner,
        run_id: &str,
        key: &str,
        text: &str,
        control: &dyn DispatchGuard,
    ) -> Result<()> {
        let run = self.managed_run(owner, run_id).await?;
        let _gate = run.mutation_gate.lock().await;
        let fingerprint = action_fingerprint("managed_prompt", &text)?;
        if run
            .cached_action(key, &owner.thread_id, Some(0), &fingerprint)
            .await?
            .is_some()
        {
            return Ok(());
        }
        control.authorize().await?;
        anyhow::ensure!(
            !self.inner.shutting_down.load(Ordering::SeqCst),
            "supervisor is shutting down"
        );
        run.submit_prompt(text.into()).await?;
        run.remember_action(key, &owner.thread_id, 0, fingerprint, None)
            .await;
        Ok(())
    }

    pub(crate) async fn managed_permission(
        &self,
        owner: &Owner,
        run_id: &str,
        key: &str,
        request_id: &str,
        option_id: &str,
        control: &dyn DispatchGuard,
    ) -> Result<()> {
        let run = self.managed_run(owner, run_id).await?;
        let _gate = run.mutation_gate.lock().await;
        let fingerprint = action_fingerprint("managed_permission", &(request_id, option_id))?;
        if run
            .cached_action(key, &owner.thread_id, Some(0), &fingerprint)
            .await?
            .is_some()
        {
            return Ok(());
        }
        control.authorize().await?;
        run.resolve_permission(request_id, option_id, false).await?;
        run.remember_action(key, &owner.thread_id, 0, fingerprint, None)
            .await;
        Ok(())
    }

    pub(crate) async fn managed_observation(
        &self,
        owner: &Owner,
        run_id: &str,
        after: u64,
    ) -> Result<(RunSnapshot, ReplayBatch)> {
        let run = self.managed_run(owner, run_id).await?;
        let attachment = run.attachment(Some(after)).await;
        Ok((attachment.snapshot, attachment.replay))
    }

    pub(crate) async fn managed_stop(
        &self,
        owner: &Owner,
        run_id: Option<&str>,
        key: &str,
    ) -> Result<Stop> {
        let run = match run_id {
            Some(id) => self.managed_run(owner, id).await?,
            None => {
                // A start can fail before its mapping ACK reaches ThreadService.
                // Inspect the actual owned runs, not PID existence or elapsed time.
                let runs = self
                    .inner
                    .runs
                    .read()
                    .await
                    .values()
                    .cloned()
                    .collect::<Vec<_>>();
                let mut found = None;
                for run in runs {
                    if run.snapshot().await.managed_by.as_ref() == Some(owner) {
                        anyhow::ensure!(found.is_none(), "managed start has multiple owned runs");
                        found = Some(run);
                    }
                }
                match found {
                    Some(run) => run,
                    None => {
                        let starts = self.inner.starts.lock().await;
                        let known_failure = matches!(
                            starts.get(&Action::Start.key(&owner.turn_id)),
                            Some(StartState::Failed { .. })
                        );
                        return Ok(Stop {
                            run_id: None,
                            confirmed: known_failure,
                            session_never_started: known_failure,
                            detail: if known_failure {
                                "Original startup failed before any owned Run/session was created"
                            } else {
                                "Original start has no confirmed process/session mapping"
                            }
                            .into(),
                        });
                    }
                }
            }
        };
        let _gate = run.mutation_gate.lock().await;
        let fingerprint = action_fingerprint("managed_stop", &true)?;
        if run
            .cached_action(key, &owner.thread_id, Some(0), &fingerprint)
            .await?
            .is_none()
        {
            run.stop().await?;
            run.remember_action(key, &owner.thread_id, 0, fingerprint, None)
                .await;
        }
        let snapshot = run.snapshot().await;
        Ok(Stop {
            run_id: Some(snapshot.run_id.clone()),
            confirmed: snapshot.owned_process_terminated(),
            session_never_started: snapshot.confirmed_startup_abort(),
            detail: format!(
                "Owned ACP controller {:?}; external effects are separate evidence",
                snapshot.process
            ),
        })
    }
}
