//! Receipt completions use the same writer and transaction proof as authority.
use super::{Control, ErrorCode, Failure, Worker};
use crate::telemetry::receipt_latency::{Event, Phase, ResultKind};
use crate::tenancy::receipts::{owner::Owner, store};
use std::sync::Arc;

use crate::tenancy::storage::storage_health_error;

impl Worker {
    pub(super) async fn execute_receipt(
        &self,
        owner: &Arc<Owner>,
        control: &Control,
    ) -> Result<(), Failure> {
        owner.observation.entered();
        // Origin is a private writer identity, independent of mutable caller grants.
        // A revoke stops sends, never the cleanup of already admitted work.
        if !Arc::ptr_eq(&owner.origin, &self.proof) {
            return Err(ErrorCode::Forbidden.into());
        }
        let installation = self
            .authority
            .read()
            .map_err(Failure::from)?
            .header()
            .installation
            .clone();
        if owner.installation() != &installation {
            return Err(ErrorCode::Forbidden.into());
        }
        let projection = self.admission.unknown_read(true)?;
        let preparation = owner.observation.span(Phase::Prepare);
        let prepared = owner.prepare().await;
        preparation.finish(if prepared.is_ok() {
            ResultKind::Observed
        } else {
            ResultKind::Failed
        });
        let candidate = prepared.map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        #[cfg(test)]
        let observation = candidate
            .v1()
            .ok()
            .and_then(|legacy| self.proof.receipt_started_for_test(legacy));
        #[cfg(test)]
        use crate::tenancy::transaction::receipt_observation::Step;
        let checked = control.check_read();
        #[cfg(test)]
        self.proof
            .receipt_result_for_test(observation, Step::Check, &checked);
        checked.map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        let durable_bytes = candidate
            .bytes()
            .checked_add(candidate.auxiliary_bytes())
            .ok_or(ErrorCode::CapacityExceeded)?;
        let encoded = usize::try_from(durable_bytes).map_err(|_| ErrorCode::CapacityExceeded)?;
        #[cfg(all(feature = "core", feature = "harness"))]
        let encoded = encoded.max(crate::storage::operation_budget::MAX_BYTES);
        control.reserve_write(encoded, true)?;
        let transaction_span = owner.observation.span(Phase::Transaction);
        let beginning = control.begin_write_receipt(&self.db).await;
        #[cfg(test)]
        self.proof
            .receipt_result_for_test(observation, Step::Begin, &beginning);
        let transaction = match beginning {
            Ok(transaction) => transaction,
            Err(_) => {
                transaction_span.finish(
                    if self.proof.disposition()
                        == crate::tenancy::transaction::Disposition::NoTransaction
                    {
                        ResultKind::Failed
                    } else {
                        ResultKind::Unconfirmed
                    },
                );
                return Err(ErrorCode::StorageUnavailable.into());
            }
        };
        let checkpoint = || {
            control
                .check_read()
                .map_err(|_| ErrorCode::AuthorityUnavailable)
        };
        let budget = crate::storage::operation_budget::OperationBudget::new(&checkpoint);
        let key = store::obligations::Key {
            tenant: candidate.facts().tenant.clone(),
            execution: candidate.facts().execution.clone(),
        };
        let write = async {
            #[cfg(all(feature = "core", feature = "harness"))]
            store::reserve_experiment_write(&budget)?;
            let checked = control.check_read();
            #[cfg(test)]
            self.proof
                .receipt_result_for_test(observation, Step::Check, &checked);
            checked?;
            #[cfg(all(test, unix))]
            self.proof
                .receipt_after_intent_for_test(observation, &transaction)
                .await?;
            let insertion = store::stage_dispatch_terminal(&transaction, &candidate).await;
            #[cfg(test)]
            {
                self.proof
                    .receipt_result_for_test(observation, Step::Store, &insertion);
                if let Ok(staged) = &insertion {
                    self.proof
                        .receipt_inserted_for_test(observation, staged.inserted());
                }
            }
            let staged = insertion?;
            let inserted = staged.inserted();
            if let store::format::Terminal::V1(legacy) = candidate.as_ref() {
                crate::tenancy::receipts::auxiliary_store::insert(&transaction, legacy).await?;
                #[cfg(all(feature = "core", feature = "harness"))]
                crate::tenancy::experiment::store::terminal::receipt(
                    &transaction,
                    owner,
                    legacy,
                    &budget,
                )
                .await?;
            }
            let change = staged
                .reconcile(&|rows, bytes| budget.charge(rows, bytes).map_err(Into::into))
                .await?;
            let checked = control.check_read();
            #[cfg(test)]
            self.proof
                .receipt_result_for_test(observation, Step::Check, &checked);
            checked?;
            Ok::<_, anyhow::Error>((inserted, change))
        }
        .await;
        let (inserted, change) = match write {
            Ok(value) => value,
            Err(error) => {
                if storage_health_error(&error) {
                    control.storage_failed();
                }
                let rollback = control.rollback(transaction).await;
                #[cfg(test)]
                self.proof
                    .receipt_result_for_test(observation, Step::Rollback, &rollback);
                transaction_span.finish(if rollback.is_ok() {
                    ResultKind::Failed
                } else {
                    ResultKind::Unconfirmed
                });
                if rollback.is_ok() {
                    owner.observation.event(Event::RollbackAck);
                }
                if rollback.is_err() {
                    self.proof.quarantine();
                }
                return Err(ErrorCode::StorageUnavailable.into());
            }
        };
        let bytes = if inserted { durable_bytes } else { 0 };
        let due = change
            .after
            .as_ref()
            .filter(|row| !row.escalated)
            .map(|row| row.due);
        let publication = match self.admission.prepare_unknown(
            &projection,
            vec![change],
            vec![crate::tenancy::admission::unknown::Terminal { key, bytes }],
            self.proof.clone(),
        ) {
            Ok(publication) => publication,
            Err(code) => {
                control
                    .rollback(transaction)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                return Err(code.into());
            }
        };
        let commit = control
            .commit_observed(transaction, Some(&owner.observation))
            .await;
        transaction_span.finish(if commit.is_ok() {
            ResultKind::Acknowledged
        } else {
            ResultKind::Unconfirmed
        });
        #[cfg(test)]
        self.proof
            .receipt_result_for_test(observation, Step::Commit, &commit);
        commit.map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        publication.publish().map_err(|_| {
            self.proof.quarantine();
            self.unavailable(ErrorCode::AuthorityUnavailable)
        })?;
        self.schedule_obligation(due)?;
        // Publish only after confirmed commit. A late acknowledgement still
        // releases exactly the original reserved completion, never another send.
        let publication = owner.committed(candidate, inserted);
        if publication.is_err() {
            owner.observation.event(Event::PublicationFailure);
        }
        publication.map_err(Failure::from)
    }
}

impl Worker {
    pub(super) async fn read_receipt(
        &self,
        read: &crate::tenancy::receipts::read::Read,
        control: &Control,
    ) -> Result<Vec<u8>, Failure> {
        use crate::tenancy::receipts::read::authorize;
        let check = || {
            control
                .check_read()
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
            self.authority
                .read()
                .map_err(Failure::from)?
                .check(&read.lease, |view| {
                    let state = view.facts();
                    if state.installation != read.installation {
                        return Err(ErrorCode::CapabilityMismatch);
                    }
                    authorize(state, &read.caller, &read.tenant)
                })
                .map_err(Failure::from)
        };
        check()?;
        let transaction = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let body = async {
            check()?;
            let bytes = crate::tenancy::receipts::read::body(
                &transaction,
                &read.installation,
                &read.tenant,
                &read.request,
                || self.unavailable(ErrorCode::StorageUnavailable),
            )
            .await?;
            check()?;
            Ok(bytes)
        }
        .await;
        // Read-only transactions also stay in actor custody through rollback.
        control
            .rollback(transaction)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        check()?;
        body
    }
}

#[cfg(test)]
mod storage_health_tests {
    use super::storage_health_error;
    use anyhow::{Context, Result, ensure};
    use sea_orm::sqlx::error::DatabaseError;
    use sea_orm::{ConnectionTrait, DbErr, RuntimeErr};

    #[tokio::test]
    async fn real_full_is_storage_failure_but_trigger_message_is_not() -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let result: Result<()> = async {
            db.execute_unprepared("CREATE TABLE health_test (payload BLOB)").await?;
            let pages: i64 = db.query_one(sea_orm::Statement::from_string(
                db.get_database_backend(), "PRAGMA page_count",
            )).await?.context("page count")?.try_get_by_index(0)?;
            db.execute_unprepared(&format!("PRAGMA max_page_count={}", pages + 1)).await?;
            let full = db.execute_unprepared("INSERT INTO health_test VALUES (zeroblob(65536))")
                .await.err().context("bounded allocation must exceed page limit")?;
            let sqlite = match &full {
                DbErr::Exec(RuntimeErr::SqlxError(sea_orm::sqlx::Error::Database(error)))
                | DbErr::Query(RuntimeErr::SqlxError(sea_orm::sqlx::Error::Database(error))) =>
                    error.try_downcast_ref::<sea_orm::sqlx::sqlite::SqliteError>(),
                _ => None,
            }.context("actual SQLite driver error")?;
            ensure!(sqlite.code().as_deref() == Some("13"));
            let full = anyhow::Error::new(full).context("original write with typed driver cause");
            ensure!(storage_health_error(&full));
            // Use SQLite's real boxed driver error, including the extended
            // trigger constraint code, with deliberately misleading text.
            db.execute_unprepared("PRAGMA max_page_count=1024").await?;
            db.execute_unprepared("CREATE TRIGGER health_refusal BEFORE INSERT ON health_test BEGIN SELECT RAISE(ABORT, 'SQLITE_FULL IOERR code 13'); END").await?;
            let refusal = db.execute_unprepared("INSERT INTO health_test VALUES (NULL)")
                .await.err().context("actual trigger refusal")?;
            ensure!(!storage_health_error(&anyhow::Error::new(refusal).context("receipt write")));
            Ok(())
        }.await;
        let closed = db.close().await;
        result?;
        closed?;
        Ok(())
    }

    #[test]
    fn wrapped_driver_io_is_distinct_from_non_storage_errors() -> Result<()> {
        let io = DbErr::Exec(RuntimeErr::SqlxError(sea_orm::sqlx::Error::Io(
            std::io::Error::other("typed test I/O failure"),
        )));
        assert!(storage_health_error(
            &anyhow::Error::new(io).context("receipt write")
        ));
        let decode = serde_json::from_str::<bool>("not JSON")
            .err()
            .context("invalid JSON")?;
        for error in [
            anyhow::anyhow!("SQLITE_FULL IOERR code 13"),
            anyhow::Error::new(DbErr::Custom("SQLITE_FULL IOERR code 13".into())),
            anyhow::Error::new(sea_orm::sqlx::Error::PoolTimedOut).context("SQLITE_FULL"),
            anyhow::Error::new(DbErr::ConnectionAcquire(sea_orm::ConnAcquireErr::Timeout)),
            anyhow::Error::new(decode),
        ] {
            assert!(!storage_health_error(&error));
        }
        Ok(())
    }
}
