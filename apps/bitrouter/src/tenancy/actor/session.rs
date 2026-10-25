//! Native journal I/O shares H's actual transaction and quarantine disposition.
use super::*;
use crate::tenancy::session::{Job, Rejected, Reply, Request};

impl Worker {
    pub(super) async fn execute_native_journal(
        &self,
        job: &Job,
        control: &Control,
    ) -> Result<Reply, Failure> {
        let authority = match &job.request {
            Request::Load(authority) => authority,
            Request::Save { binding, .. } | Request::Reconcile(binding) => binding.authority(),
        };
        let cleanup = matches!(
            &job.request,
            Request::Save { cleanup: true, .. } | Request::Reconcile(_)
        );
        let check = || -> anyhow::Result<()> {
            if Instant::now() >= job.deadline || control.check_read().is_err() {
                return Err(Rejected(ErrorCode::AuthorityUnavailable).into());
            }
            if !authority.origin(&self.proof) || !self.live.0.load(Ordering::Acquire) {
                return Err(Rejected(ErrorCode::AuthorityUnavailable).into());
            }
            if !cleanup {
                if !*self
                    .accepting
                    .lock()
                    .map_err(|_| Rejected(ErrorCode::AuthorityUnavailable))?
                {
                    return Err(Rejected(ErrorCode::AuthorityUnavailable).into());
                }
                authority
                    .check()
                    .map_err(|failure| Rejected(failure.0.code))?;
            }
            Ok(())
        };
        let map = |error: anyhow::Error| {
            if let Some(Rejected(code)) = error.downcast_ref::<Rejected>() {
                Failure::from(*code)
            } else {
                self.unavailable(ErrorCode::StorageUnavailable)
            }
        };
        check().map_err(map)?;
        let tx = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let result = async {
            check()?;
            let reply = match &job.request {
                Request::Load(authority) => Reply::Loaded(
                    crate::managed_store::owned::load_in(&tx, authority, &check)
                        .await?
                        .map(Box::new),
                ),
                Request::Reconcile(binding) => Reply::Loaded(
                    crate::managed_store::owned::load_in(&tx, binding.authority(), &check)
                        .await?
                        .map(Box::new),
                ),
                Request::Save {
                    binding,
                    expected,
                    bytes,
                    ..
                } => {
                    crate::managed_store::owned::save_in(&tx, binding, *expected, bytes, &check)
                        .await?;
                    Reply::Saved
                }
            };
            check()?;
            Ok::<_, anyhow::Error>(reply)
        }
        .await;
        let reply = match result {
            Ok(reply) => reply,
            Err(error) => {
                control
                    .rollback(tx)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                return Err(map(error));
            }
        };
        if matches!(reply, Reply::Saved) {
            control
                .commit(tx)
                .await
                .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        } else {
            control
                .rollback(tx)
                .await
                .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        }
        // Completed COMMIT is durable even when post-commit authority denies delivery.
        if let Err(error) = check() {
            if self.proof.disposition() == Disposition::Committed
                && (Instant::now() >= job.deadline || control.check_read().is_err())
            {
                self.proof.fenced();
            }
            return Err(map(error));
        }
        Ok(reply)
    }
}
