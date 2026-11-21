//! A real backend error after the earlier dispute exists in the same transaction.
use super::*;

pub(super) const MARKER: &str = "owned_eval_terminal_after_dispute";

pub(super) async fn install(store: &EvalStore, prior: &str, next: &str) -> Result<()> {
    let prior = store.key(KeyDomain::EvalResult, prior)?;
    let next = store.key(KeyDomain::EvalResult, next)?;
    ensure!(
        [&prior, &next]
            .iter()
            .all(|key| key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit())),
        "fixture keys must be hex digests"
    );
    let backend = store.db.get_database_backend();
    match backend {
        DatabaseBackend::Postgres => {
            // BEFORE INSERT observes previous writes of this transaction.
            // P0002 proves a broken fixture; P0001 is the intended terminal fault.
            // https://www.postgresql.org/docs/17/sql-createtrigger.html
            // https://www.postgresql.org/docs/17/plpgsql-errors-and-messages.html
            store.db.execute_unprepared(&format!(r#"
                CREATE FUNCTION owned_eval_terminal_fault() RETURNS trigger
                LANGUAGE plpgsql AS $fixture$
                BEGIN
                    IF NEW.result_id = '{next}' THEN
                        IF (SELECT COUNT(*) FROM eval_admission_events
                            WHERE result_id = '{prior}' AND status = 'disputed') <> 1
                           OR (SELECT COUNT(*) FROM eval_results WHERE result_id = '{next}') <> 1 THEN
                            RAISE EXCEPTION USING ERRCODE = 'P0002', MESSAGE = 'owned_eval_fault_precondition_missing';
                        END IF;
                        RAISE EXCEPTION USING ERRCODE = 'P0001', MESSAGE = '{MARKER}';
                    END IF;
                    RETURN NEW;
                END;
                $fixture$
            "#)).await?;
            store.db.execute_unprepared("CREATE TRIGGER owned_eval_terminal_fault BEFORE INSERT ON eval_admission_events FOR EACH ROW EXECUTE FUNCTION owned_eval_terminal_fault()").await?;
        }
        DatabaseBackend::MySql => {
            // SIGNAL aborts the triggering statement, not earlier transaction
            // writes: the test must prove explicit rollback undoes those too.
            // Send the compound CREATE as one statement; DELIMITER is CLI syntax.
            // https://dev.mysql.com/doc/refman/8.4/en/trigger-syntax.html
            // https://dev.mysql.com/doc/refman/8.4/en/signal.html
            store.db.execute_unprepared(&format!(r#"
                CREATE TRIGGER owned_eval_terminal_fault BEFORE INSERT ON eval_admission_events
                FOR EACH ROW BEGIN
                    IF NEW.result_id = '{next}' THEN
                        IF (SELECT COUNT(*) FROM eval_admission_events
                            WHERE result_id = '{prior}' AND status = 'disputed') <> 1
                           OR (SELECT COUNT(*) FROM eval_results WHERE result_id = '{next}') <> 1 THEN
                            SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'owned_eval_fault_precondition_missing';
                        END IF;
                        SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = '{MARKER}';
                    END IF;
                END
            "#)).await?;
        }
        _ => anyhow::bail!("fault fixture requires PostgreSQL or MySQL"),
    }
    Ok(())
}

pub(super) async fn remove(store: &EvalStore) -> Result<()> {
    match store.db.get_database_backend() {
        DatabaseBackend::Postgres => {
            store
                .db
                .execute_unprepared(
                    "DROP TRIGGER owned_eval_terminal_fault ON eval_admission_events",
                )
                .await?;
            store
                .db
                .execute_unprepared("DROP FUNCTION owned_eval_terminal_fault()")
                .await?;
        }
        DatabaseBackend::MySql => {
            store
                .db
                .execute_unprepared("DROP TRIGGER owned_eval_terminal_fault")
                .await?;
        }
        _ => anyhow::bail!("fault fixture requires PostgreSQL or MySQL"),
    }
    Ok(())
}

pub(super) fn sql_code(error: &anyhow::Error) -> Option<String> {
    use sea_orm::{DbErr, RuntimeErr};
    let db = error.downcast_ref::<DbErr>()?;
    let runtime = match db {
        DbErr::Conn(runtime) | DbErr::Exec(runtime) | DbErr::Query(runtime) => runtime,
        _ => return None,
    };
    match runtime {
        RuntimeErr::SqlxError(error) => error
            .as_database_error()?
            .code()
            .map(|code| code.into_owned()),
        RuntimeErr::Internal(_) => None,
    }
}
