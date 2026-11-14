//! Bounded copies of actual receipt SQL outcomes; no retained runtime owner.
use super::{Disposition, Proof};
use crate::tenancy::receipts::store::Candidate;
use anyhow::Result;
#[cfg(unix)]
use anyhow::{Context, ensure};
#[cfg(unix)]
use bitrouter_ai::tenancy::identity::AttemptId;
use bitrouter_ai::tenancy::identity::{ExecutionId, InstallationId, TenantId};
use sea_orm::sqlx::error::DatabaseError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tenancy) struct Key {
    pub installation: InstallationId,
    pub tenant: TenantId,
    pub execution: ExecutionId,
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tenancy) struct Fingerprint {
    pub digest: String,
    pub bytes: u64,
    pub attempt_count: usize,
    pub first_attempt: Option<AttemptId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tenancy) struct SqlOutcome {
    pub ok: bool,
    pub sqlite_code: Option<i32>,
}

impl SqlOutcome {
    fn from_result<T>(result: &Result<T>) -> Self {
        // SQLx's boxed DatabaseError does not forward its source. Use the
        // typed accessor before reading SQLite's numeric extended code;
        // neither Display text nor SQLite's code proves a syscall errno.
        // https://docs.rs/sqlx/0.8.6/sqlx/enum.Error.html#method.as_database_error
        // https://docs.rs/sqlx/0.8.6/sqlx/error/trait.DatabaseError.html#method.try_downcast_ref
        Self {
            ok: result.is_ok(),
            sqlite_code: result.as_ref().err().and_then(|error| {
                error.chain().find_map(|cause| {
                    cause
                        .downcast_ref::<sea_orm::sqlx::Error>()
                        .and_then(sea_orm::sqlx::Error::as_database_error)
                        .and_then(|error| {
                            error.try_downcast_ref::<sea_orm::sqlx::sqlite::SqliteError>()
                        })
                        .or_else(|| cause.downcast_ref::<sea_orm::sqlx::sqlite::SqliteError>())
                        .and_then(|error| error.code())
                        .and_then(|code| code.parse().ok())
                })
            }),
        }
    }
}

#[derive(Clone, Debug)]
pub(in crate::tenancy) struct Fact {
    #[cfg(unix)]
    pub fingerprint: Fingerprint,
    pub begin: Option<SqlOutcome>,
    pub store: Option<SqlOutcome>,
    #[cfg(unix)]
    pub post_intent_query_only: Option<SqlOutcome>,
    pub inserted: Option<bool>,
    #[cfg(all(feature = "core", feature = "harness"))]
    pub native_checkpoint: Option<SqlOutcome>,
    pub commit: Option<SqlOutcome>,
    pub rollback: Option<SqlOutcome>,
    pub check_failed: bool,
    pub disposition: Disposition,
}

#[derive(Clone, Debug)]
pub(in crate::tenancy) struct Trace {
    pub key: Key,
    pub facts: [Option<Fact>; 2],
    pub overflow: bool,
    #[cfg(unix)]
    readonly_after_intent: bool,
}

#[derive(Clone, Copy)]
pub(in crate::tenancy) enum Step {
    Check,
    Begin,
    #[cfg(unix)]
    PostIntentQueryOnly,
    Store,
    Commit,
    Rollback,
    #[cfg(all(feature = "core", feature = "harness"))]
    NativeCheckpoint,
}

impl Proof {
    #[cfg(unix)]
    pub(in crate::tenancy) fn observe_receipt_for_test(&self, key: Key) -> Result<()> {
        let mut slot = self
            .receipt_observation
            .lock()
            .map_err(|_| anyhow::anyhow!("receipt observation poisoned"))?;
        ensure!(slot.is_none(), "receipt observer already installed");
        *slot = Some(Trace {
            key,
            facts: [None, None],
            overflow: false,
            readonly_after_intent: false,
        });
        Ok(())
    }

    #[cfg(unix)]
    pub(in crate::tenancy) fn receipt_trace_for_test(&self) -> Result<Trace> {
        self.receipt_observation
            .lock()
            .map_err(|_| anyhow::anyhow!("receipt observation poisoned"))?
            .clone()
            .context("receipt observer missing")
    }

    /// Arm only the already observed exact receipt, before any attempt starts.
    #[cfg(all(unix, any(feature = "otel-http", feature = "otel-grpc")))]
    pub(in crate::tenancy) fn readonly_receipt_after_intent_for_test(
        &self,
        key: &Key,
    ) -> Result<()> {
        let mut slot = self
            .receipt_observation
            .lock()
            .map_err(|_| anyhow::anyhow!("receipt observation poisoned"))?;
        let trace = slot.as_mut().context("receipt observer missing")?;
        ensure!(
            &trace.key == key
                && !trace.readonly_after_intent
                && !trace.overflow
                && trace.facts.iter().all(Option::is_none),
            "readonly receipt injection key or state differs"
        );
        trace.readonly_after_intent = true;
        Ok(())
    }

    // This entire module is cfg(test). A nonmatching receipt has no observation
    // index, so cannot consume the one-shot flag or modify any connection.
    #[cfg(unix)]
    pub(in crate::tenancy) async fn receipt_after_intent_for_test(
        &self,
        index: Option<usize>,
        transaction: &sea_orm::DatabaseTransaction,
    ) -> Result<()> {
        use sea_orm::ConnectionTrait;
        let Some(index) = index else {
            return Ok(());
        };
        {
            let mut slot = self
                .receipt_observation
                .lock()
                .map_err(|_| anyhow::anyhow!("receipt observation poisoned"))?;
            let Some(trace) = slot.as_mut() else {
                return Ok(());
            };
            if !trace.readonly_after_intent {
                return Ok(());
            }
            let fact = trace
                .facts
                .get(index)
                .and_then(Option::as_ref)
                .context("readonly receipt observation absent")?;
            ensure!(
                fact.begin.is_some_and(|outcome| outcome.ok)
                    && fact.store.is_none()
                    && fact.post_intent_query_only.is_none(),
                "readonly receipt injection precedes acknowledged write intent"
            );
            trace.readonly_after_intent = false;
        } // Never hold the observation mutex across actual SQL.
        ensure!(
            transaction.get_database_backend() == sea_orm::DatabaseBackend::Sqlite,
            "readonly receipt injection requires SQLite"
        );
        let result: Result<()> = transaction
            .execute_unprepared("PRAGMA query_only=ON")
            .await
            .map(|_| ())
            .map_err(Into::into);
        self.receipt_result_for_test(Some(index), Step::PostIntentQueryOnly, &result);
        result?;
        Ok(())
    }

    pub(in crate::tenancy) fn receipt_started_for_test(
        &self,
        candidate: &Candidate,
    ) -> Option<usize> {
        // Observation failure must never change the production outcome. The
        // snapshot fails on poison; missing/overflow facts fail the test.
        let Ok(mut slot) = self.receipt_observation.lock() else {
            return None;
        };
        let trace = slot.as_mut()?;
        if trace.key.installation != candidate.receipt.installation_id
            || trace.key.tenant != candidate.receipt.tenant_id
            || trace.key.execution != candidate.receipt.execution_id
        {
            return None;
        }
        let Some(index) = trace.facts.iter().position(Option::is_none) else {
            trace.overflow = true;
            return None;
        };
        trace.facts[index] = Some(Fact {
            #[cfg(unix)]
            fingerprint: candidate.fingerprint_for_test(),
            begin: None,
            store: None,
            #[cfg(unix)]
            post_intent_query_only: None,
            inserted: None,
            #[cfg(all(feature = "core", feature = "harness"))]
            native_checkpoint: None,
            commit: None,
            rollback: None,
            check_failed: false,
            disposition: self.disposition(),
        });
        Some(index)
    }

    pub(in crate::tenancy) fn receipt_inserted_for_test(
        &self,
        index: Option<usize>,
        inserted: bool,
    ) {
        self.receipt_fact_for_test(index, |fact| fact.inserted = Some(inserted));
    }

    pub(in crate::tenancy) fn receipt_result_for_test<T>(
        &self,
        index: Option<usize>,
        step: Step,
        result: &Result<T>,
    ) {
        let outcome = SqlOutcome::from_result(result);
        self.receipt_fact_for_test(index, |fact| {
            match step {
                Step::Check => fact.check_failed |= !outcome.ok,
                Step::Begin => fact.begin = Some(outcome),
                #[cfg(unix)]
                Step::PostIntentQueryOnly => fact.post_intent_query_only = Some(outcome),
                Step::Store => fact.store = Some(outcome),
                Step::Commit => fact.commit = Some(outcome),
                Step::Rollback => fact.rollback = Some(outcome),
                #[cfg(all(feature = "core", feature = "harness"))]
                Step::NativeCheckpoint => fact.native_checkpoint = Some(outcome),
            }
            fact.disposition = self.disposition();
        });
    }

    fn receipt_fact_for_test(&self, index: Option<usize>, update: impl FnOnce(&mut Fact)) {
        let Some(index) = index else { return };
        let Ok(mut slot) = self.receipt_observation.lock() else {
            return;
        };
        if let Some(fact) = slot
            .as_mut()
            .and_then(|trace| trace.facts.get_mut(index))
            .and_then(Option::as_mut)
        {
            update(fact);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SqlOutcome;
    use anyhow::{Context, Result, ensure};
    use sea_orm::{ConnectOptions, ConnectionTrait, Database, DbErr, RuntimeErr, TransactionTrait};

    #[tokio::test]
    async fn consuming_commit_preserves_boxed_sqlite_extended_code() -> Result<()> {
        let mut options = ConnectOptions::new("sqlite::memory:");
        options.max_connections(1);
        let db = Database::connect(options).await?;
        let result: Result<()> = async {
            db.execute_unprepared("PRAGMA foreign_keys=ON").await?;
            db.execute_unprepared("CREATE TABLE parent (id INTEGER PRIMARY KEY)")
                .await?;
            db.execute_unprepared(
                "CREATE TABLE child (parent INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED)",
            )
            .await?;
            let tx = db.begin().await?;
            tx.execute_unprepared("INSERT INTO child(parent) VALUES (1)")
                .await?;
            tx.commit().await.context("actual consuming commit")
        }
        .await;
        db.close().await?;

        let error = result.as_ref().err().context("COMMIT must fail")?;
        let db_error = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<DbErr>())
            .context("SeaORM error missing")?;
        ensure!(matches!(
            db_error,
            DbErr::Query(RuntimeErr::SqlxError(sea_orm::sqlx::Error::Database(_)))
        ));
        // This is a real extended FK code from COMMIT, not injected FULL or
        // filesystem evidence. The ignored scratch test still requires FULL13
        // independently of the actual same-volume ENOSPC syscall.
        // https://www.sqlite.org/rescode.html#constraint_foreignkey
        assert_eq!(
            SqlOutcome::from_result(&result),
            SqlOutcome {
                ok: false,
                sqlite_code: Some(787),
            }
        );
        Ok(())
    }

    #[test]
    fn non_database_errors_and_success_have_no_sqlite_code() {
        let success: Result<()> = Ok(());
        assert_eq!(
            SqlOutcome::from_result(&success),
            SqlOutcome {
                ok: true,
                sqlite_code: None,
            }
        );
        for error in [
            anyhow::anyhow!("SQLITE_FULL (code: 13): database or disk is full"),
            anyhow::Error::new(sea_orm::sqlx::Error::PoolTimedOut)
                .context("SQLITE_FULL (code: 13)"),
        ] {
            let result: Result<()> = Err(error);
            assert_eq!(
                SqlOutcome::from_result(&result),
                SqlOutcome {
                    ok: false,
                    sqlite_code: None,
                }
            );
        }
    }
}
