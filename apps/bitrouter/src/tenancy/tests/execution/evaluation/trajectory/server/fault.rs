//! Real backend errors guarded by same-transaction predecessor assertions.
use super::*;

pub(super) const MARKER: &str = "trajectory_compound_terminal_fault";

#[derive(Clone, Copy, Debug)]
pub(super) enum Point {
    Outbox,
    Subject,
    Result,
    Admission,
    Delivered,
}

impl Point {
    pub(super) const ALL: [Self; 5] = [
        Self::Outbox,
        Self::Subject,
        Self::Result,
        Self::Admission,
        Self::Delivered,
    ];

    pub(super) fn preceding(self) -> [usize; 4] {
        match self {
            Self::Outbox => [0, 0, 0, 0],
            Self::Subject => [1, 0, 0, 0],
            Self::Result => [1, 1, 0, 0],
            Self::Admission => [1, 1, 1, 0],
            Self::Delivered => [1, 1, 1, 1],
        }
    }

    fn target(self) -> (&'static str, &'static str) {
        match self {
            Self::Outbox => ("INSERT", "trajectory_outbox"),
            Self::Subject => ("INSERT", "eval_subjects"),
            Self::Result => ("INSERT", "eval_results"),
            Self::Admission => ("INSERT", "eval_admission_events"),
            Self::Delivered => ("UPDATE", "trajectory_outbox"),
        }
    }
}

pub(super) async fn install(
    db: &DatabaseConnection,
    point: Point,
    started_events: usize,
) -> Result<()> {
    let backend = db.get_database_backend();
    let (operation, table) = point.target();
    let [outboxes, subjects, results, admissions] = point.preceding();
    let condition = if matches!(point, Point::Delivered) {
        "NEW.delivered_at IS NOT NULL"
    } else {
        "TRUE"
    };
    // These checks execute BEFORE the failing statement in its own transaction.
    // PostgreSQL then aborts that transaction; MySQL preserves earlier writes
    // until explicit rollback. Neither backend can pass by failing at BEGIN.
    let preconditions = format!(
        "
        (SELECT COUNT(*) FROM trajectory_requests WHERE status = 'failed'
            AND settlement_event_id IS NOT NULL AND settlement_outbox_id IS NOT NULL) = 1
        AND (SELECT COUNT(*) FROM trajectory_events WHERE kind = 'request_settled') = 1
        AND (SELECT COUNT(*) FROM trajectory_events) = {}
        AND (SELECT COUNT(*) FROM trajectory_outbox) = {outboxes}
        AND (SELECT COUNT(*) FROM trajectory_outbox WHERE delivered_at IS NULL) = {outboxes}
        AND (SELECT COUNT(*) FROM eval_subjects) = {subjects}
        AND (SELECT COUNT(*) FROM eval_results) = {results}
        AND (SELECT COUNT(*) FROM eval_admission_events) = {admissions}
        AND NEW.installation_id IS NOT NULL AND NEW.tenant_id IS NOT NULL
        AND NEW.creator_principal_id IS NOT NULL",
        started_events + 1
    );
    match backend {
        DatabaseBackend::Postgres => {
            // https://www.postgresql.org/docs/17/sql-createtrigger.html
            // https://www.postgresql.org/docs/17/plpgsql-errors-and-messages.html
            db.execute_unprepared(&format!(r#"
                CREATE FUNCTION trajectory_compound_fault() RETURNS trigger
                LANGUAGE plpgsql AS $fixture$
                BEGIN
                    IF {condition} THEN
                        IF NOT ({preconditions}) THEN
                            RAISE EXCEPTION USING ERRCODE = 'P0002', MESSAGE = 'trajectory_fault_precondition_missing';
                        END IF;
                        RAISE EXCEPTION USING ERRCODE = 'P0001', MESSAGE = '{MARKER}';
                    END IF;
                    RETURN NEW;
                END;
                $fixture$
            "#)).await?;
            db.execute_unprepared(&format!("CREATE TRIGGER trajectory_compound_fault BEFORE {operation} ON {table} FOR EACH ROW EXECUTE FUNCTION trajectory_compound_fault()" )).await?;
        }
        DatabaseBackend::MySql => {
            // Compound SQL is one statement; DELIMITER belongs to the CLI only.
            // https://dev.mysql.com/doc/refman/8.4/en/trigger-syntax.html
            // https://dev.mysql.com/doc/refman/8.4/en/signal.html
            db.execute_unprepared(&format!(r#"
                CREATE TRIGGER trajectory_compound_fault BEFORE {operation} ON {table}
                FOR EACH ROW BEGIN
                    IF {condition} THEN
                        IF NOT ({preconditions}) THEN
                            SIGNAL SQLSTATE '45001' SET MESSAGE_TEXT = 'trajectory_fault_precondition_missing';
                        END IF;
                        SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = '{MARKER}';
                    END IF;
                END
            "#)).await?;
        }
        _ => anyhow::bail!("trigger fixture requires PostgreSQL/MySQL"),
    }
    Ok(())
}

pub(super) async fn remove(db: &DatabaseConnection, point: Point) -> Result<()> {
    match db.get_database_backend() {
        DatabaseBackend::Postgres => {
            let (_, table) = point.target();
            db.execute_unprepared(&format!(
                "DROP TRIGGER trajectory_compound_fault ON {table}"
            ))
            .await?;
            db.execute_unprepared("DROP FUNCTION trajectory_compound_fault()")
                .await?;
        }
        DatabaseBackend::MySql => {
            db.execute_unprepared("DROP TRIGGER trajectory_compound_fault")
                .await?;
        }
        _ => anyhow::bail!("trigger fixture requires PostgreSQL/MySQL"),
    }
    Ok(())
}

pub(super) fn verify(error: &anyhow::Error, backend: DatabaseBackend) -> Result<()> {
    use sea_orm::{DbErr, RuntimeErr};
    // publish_trajectory_in wraps the SQL error in OwnedError. Follow sources
    // rather than asserting generic actor StorageUnavailable or a substring alone.
    let db = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<DbErr>())
        .context("SQL source missing")?;
    let runtime = match db {
        DbErr::Conn(runtime) | DbErr::Exec(runtime) | DbErr::Query(runtime) => runtime,
        _ => anyhow::bail!("unexpected database error: {db}"),
    };
    let code = match runtime {
        RuntimeErr::SqlxError(error) => error
            .as_database_error()
            .and_then(|e| e.code())
            .map(|c| c.into_owned()),
        RuntimeErr::Internal(_) => None,
    };
    let expected = match backend {
        DatabaseBackend::Postgres => "P0001",
        DatabaseBackend::MySql => "45000",
        _ => anyhow::bail!("unsupported backend"),
    };
    ensure!(
        code.as_deref() == Some(expected),
        "wrong terminal fault SQLSTATE: {error:#}"
    );
    ensure!(
        format!("{error:#}").contains(MARKER),
        "trigger predecessor assertions failed: {error:#}"
    );
    Ok(())
}
