//! Owned PostgreSQL catalog corruptions must refuse before incarnation writes.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde_json::json;

use crate::gateway::shared::{
    authority::Authority, engine::Engine, publication::Configuration, schema, store::Store,
    test_database::EmptyDatabase,
};
use crate::runtime_crypto::DeploymentKey;

struct Fixture {
    authority: Authority,
}

impl Fixture {
    async fn new(database: &mut EmptyDatabase) -> Result<Self> {
        Self::new_profile(database, false).await
    }

    async fn new_composed(database: &mut EmptyDatabase) -> Result<Self> {
        Self::new_profile(database, true).await
    }

    async fn new_profile(database: &mut EmptyDatabase, ordinary: bool) -> Result<Self> {
        let timeout = Duration::from_secs(5);
        let store = Store::new(
            database.connect(2, timeout).await?,
            &format!("schema-{}", uuid::Uuid::new_v4()),
            timeout,
        )?;
        if ordinary {
            schema::initialize_all(&store).await?;
        } else {
            schema::initialize(&store).await?;
        }
        let authority = Authority::new(store, DeploymentKey::from_bytes(rand::random())?);
        authority.initialize().await?;
        authority
            .publish(
                0,
                &Configuration {
                    format: 2,
                    routing: json!({"providers": {}, "routers": {}}),
                    policies: vec![],
                    model_policy: None,
                    credentials: vec![],
                    require_known_pricing: false,
                },
            )
            .await?;
        Ok(Self { authority })
    }

    async fn snapshot(&self) -> Result<Vec<String>> {
        let db = self.authority.store().database();
        let mut snapshot = Vec::new();
        for (table, order) in [
            ("gateway_shared_records", "id"),
            ("gateway_shared_migrations", "version"),
        ] {
            let rows = db
                .query_all(Statement::from_string(
                    DatabaseBackend::Postgres,
                    format!("SELECT to_jsonb(t)::text AS body FROM {table} t ORDER BY {order}"),
                ))
                .await?;
            for row in rows {
                snapshot.push(format!("{table}:{}", row.try_get::<String>("", "body")?));
            }
        }
        Ok(snapshot)
    }

    async fn composed_snapshot(&self) -> Result<Vec<String>> {
        let mut snapshot = self.snapshot().await?;
        let rows = self
            .authority
            .store()
            .database()
            .query_all(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT to_jsonb(t)::text AS body FROM seaql_migrations t ORDER BY version",
            ))
            .await?;
        for row in rows {
            snapshot.push(format!(
                "seaql_migrations:{}",
                row.try_get::<String>("", "body")?
            ));
        }
        Ok(snapshot)
    }

    async fn ordinary_refused(&self, before: &[String], reason: &str) -> Result<()> {
        let error = crate::db::run_migrations(self.authority.store().database())
            .await
            .err()
            .context("ordinary baseline accepted an incompatible composition")?;
        ensure!(
            format!("{error:#}").contains(reason),
            "ordinary baseline failed for an unrelated reason: {error:#}"
        );
        ensure!(
            self.composed_snapshot().await? == before,
            "ordinary baseline refusal changed authority or either migration marker"
        );
        Ok(())
    }

    async fn refused_without_registration(&self, before: &[String]) -> Result<()> {
        ensure!(
            schema::validated_tables(self.authority.store().database())
                .await
                .is_err(),
            "incompatible schema granted ordinary-store exclusions"
        );
        let error = Engine::join(self.authority.clone(), 30_000)
            .await
            .err()
            .context("incompatible schema registered an incarnation")?;
        ensure!(
            format!("{error:#}").contains("incompatible shared schema"),
            "join failed for an unrelated reason: {error:#}"
        );
        ensure!(
            self.snapshot().await? == before,
            "schema refusal changed authority, retained records or the migration marker"
        );
        Ok(())
    }

    async fn corruptions(&self, mutations: &[(&str, &str)]) -> Result<()> {
        let before = self.snapshot().await?;
        let db = self.authority.store().database();
        for (corrupt, repair) in mutations {
            db.execute_unprepared(corrupt).await?;
            self.refused_without_registration(&before)
                .await
                .with_context(|| format!("catalog corruption: {corrupt}"))?;
            db.execute_unprepared(repair).await?;
            ensure!(
                schema::current(db).await?,
                "restored current DDL was refused"
            );
            ensure!(
                self.snapshot().await? == before,
                "validation changed stored rows"
            );
        }
        Ok(())
    }
}

/// UT for the exact current shared schema and its read-only ledger validation.
#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn ut_postgres_shared_schema_accepts_current_fixture() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        let fixture = Fixture::new(&mut database).await?;
        let before = fixture.snapshot().await?;
        let db = fixture.authority.store().database();
        let (tables, identities) = schema::validated_tables(db).await?;
        let catalog = db.query_all(Statement::from_string(DatabaseBackend::Postgres,
            "SELECT c.relname AS name FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND c.relkind='r'"
        )).await?;
        let actual = catalog.iter().map(|row| row.try_get::<String>("", "name"))
            .collect::<std::result::Result<std::collections::BTreeSet<_>, _>>()?;
        ensure!(tables.iter().copied().eq(actual.iter().map(String::as_str)),
            "validated table exclusions differ from the owned current catalog");
        ensure!(identities == [("gateway_shared_eval_admission_events", "sequence")].into(),
            "validated identity exclusions differ from the current DDL");
        ensure!(
            fixture.snapshot().await? == before,
            "validation mutated the current dataset"
        );
        let engine = Engine::join(fixture.authority.clone(), 30_000).await?;
        let rows = fixture.snapshot().await?;
        ensure!(
            rows.len() == before.len() + 1,
            "join did not register exactly one incarnation"
        );
        ensure!(
            before.iter().all(|row| rows.contains(row)),
            "join rewrote retained data or the marker"
        );
        engine.drain().await
    })
    .await
    .context("current shared schema fixture deadline exceeded")
    .and_then(|result| result);
    database.finish(result).await
}

/// UT for rejection of lost replay uniqueness before Engine::join writes.
#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn ut_postgres_shared_schema_refuses_corrupt_uniqueness() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        let fixture = Fixture::new(&mut database).await?;
        fixture.corruptions(&[
            ("ALTER TABLE gateway_shared_metering_requests DROP CONSTRAINT gateway_shared_metering_requests_namespace_event_id_key",
             "ALTER TABLE gateway_shared_metering_requests ADD CONSTRAINT gateway_shared_metering_requests_namespace_event_id_key UNIQUE (namespace,event_id)"),
            ("ALTER TABLE gateway_shared_metering_requests DROP CONSTRAINT gateway_shared_metering_requests_namespace_request_id_key",
             "ALTER TABLE gateway_shared_metering_requests ADD CONSTRAINT gateway_shared_metering_requests_namespace_request_id_key UNIQUE (namespace,request_id)"),
        ]).await
    }).await.context("shared schema uniqueness fixture deadline exceeded").and_then(|result| result);
    database.finish(result).await
}

/// UT for rejection of incompatible native continuation column types and nullability.
#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn ut_postgres_shared_schema_refuses_native_continuation_columns() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        let fixture = Fixture::new(&mut database).await?;
        fixture.corruptions(&[
            ("ALTER TABLE gateway_shared_native_continuations ALTER COLUMN body TYPE jsonb USING body::jsonb",
             "ALTER TABLE gateway_shared_native_continuations ALTER COLUMN body TYPE text USING body::text"),
            ("ALTER TABLE gateway_shared_native_continuations ALTER COLUMN expires_at TYPE varchar(64)",
             "ALTER TABLE gateway_shared_native_continuations ALTER COLUMN expires_at TYPE varchar(32)"),
            ("ALTER TABLE gateway_shared_native_continuations ALTER COLUMN body DROP NOT NULL",
             "ALTER TABLE gateway_shared_native_continuations ALTER COLUMN body SET NOT NULL"),
        ]).await
    }).await.context("shared native schema fixture deadline exceeded").and_then(|result| result);
    database.finish(result).await
}

/// UT for rejection of row filters, rewrite triggers, changed checks and index semantics.
#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn ut_postgres_shared_schema_refuses_filters_constraints_and_indexes() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let fixture = Fixture::new(&mut database).await?;
        fixture.corruptions(&[
            ("ALTER TABLE gateway_shared_records ENABLE ROW LEVEL SECURITY",
             "ALTER TABLE gateway_shared_records DISABLE ROW LEVEL SECURITY"),
            ("CREATE FUNCTION schema_test_trigger() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END'; CREATE TRIGGER schema_test BEFORE INSERT ON gateway_shared_records FOR EACH ROW EXECUTE FUNCTION schema_test_trigger()",
             "DROP TRIGGER schema_test ON gateway_shared_records; DROP FUNCTION schema_test_trigger()"),
            ("ALTER TABLE gateway_shared_native_continuations DROP CONSTRAINT gateway_shared_native_continuations_revision_check",
             "ALTER TABLE gateway_shared_native_continuations ADD CONSTRAINT gateway_shared_native_continuations_revision_check CHECK (revision>=0)"),
            ("ALTER TABLE gateway_shared_native_continuations ALTER COLUMN body TYPE text COLLATE \"C\"",
             "ALTER TABLE gateway_shared_native_continuations ALTER COLUMN body TYPE text COLLATE \"default\""),
            ("DROP INDEX gateway_shared_eval_admission_latest; CREATE INDEX gateway_shared_eval_admission_latest ON gateway_shared_eval_admission_events(namespace,result_id,sequence)",
             "DROP INDEX gateway_shared_eval_admission_latest; CREATE INDEX gateway_shared_eval_admission_latest ON gateway_shared_eval_admission_events(namespace,result_id,sequence DESC)"),
            ("CREATE INDEX schema_test_extra ON gateway_shared_records(namespace)",
             "DROP INDEX schema_test_extra"),
        ]).await
    }).await.context("shared catalog semantics fixture deadline exceeded").and_then(|result| result);
    database.finish(result).await
}

/// UT for initialization and restart of the composed ordinary/shared baseline.
#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn ut_postgres_shared_schema_composition_restarts_ordinary_baseline() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let fixture = Fixture::new_composed(&mut database).await?;
        let before = fixture.composed_snapshot().await?;
        let timeout = Duration::from_secs(5);
        let reopened = database.connect(2, timeout).await?;
        crate::db::run_migrations(&reopened).await?;
        let restarted = fixture.authority.with_database(reopened)?;
        schema::initialize_all(restarted.store()).await?;
        ensure!(
            fixture.composed_snapshot().await? == before,
            "baseline restart changed authority, retained data or either ledger"
        );
        let engine = Engine::join(restarted, 30_000).await?;
        let after = fixture.composed_snapshot().await?;
        ensure!(
            after.len() == before.len() + 1 && before.iter().all(|row| after.contains(row)),
            "composed restart did not preserve history and register exactly one incarnation"
        );
        engine.drain().await
    })
    .await
    .context("composed baseline restart deadline exceeded")
    .and_then(|result| result);
    database.finish(result).await
}

/// UT for exact composition exclusions, including unreviewed prefixed sequences.
#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn ut_postgres_shared_schema_composition_refuses_foreign_objects() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let fixture = Fixture::new_composed(&mut database).await?;
        let before = fixture.composed_snapshot().await?;
        let db = fixture.authority.store().database();
        db.execute_unprepared("CREATE TABLE unrelated_private_rows (payload text NOT NULL); INSERT INTO unrelated_private_rows VALUES ('preserved')").await?;
        fixture.ordinary_refused(&before, "foreign table outside validated composition").await?;
        let row = db.query_one(Statement::from_string(DatabaseBackend::Postgres,
            "SELECT payload FROM unrelated_private_rows")).await?.context("foreign payload disappeared")?;
        ensure!(row.try_get::<String>("", "payload")? == "preserved");
        db.execute_unprepared("DROP TABLE unrelated_private_rows").await?;
        crate::db::run_migrations(db).await?;

        db.execute_unprepared("CREATE SEQUENCE gateway_shared_unreviewed_sequence").await?;
        fixture.ordinary_refused(&before, "incompatible current server schema").await?;
        db.execute_unprepared("DROP SEQUENCE gateway_shared_unreviewed_sequence").await?;
        crate::db::run_migrations(db).await?;
        ensure!(fixture.composed_snapshot().await? == before);
        Ok(())
    }).await.context("foreign composition refusal deadline exceeded").and_then(|result| result);
    database.finish(result).await
}

/// UT for ordinary baseline refusal of mutated shared uniqueness and native storage.
#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn ut_postgres_shared_schema_composition_refuses_mutated_shared_layout() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let fixture = Fixture::new_composed(&mut database).await?;
        let before = fixture.composed_snapshot().await?;
        let db = fixture.authority.store().database();
        for (corrupt, repair) in [
            ("ALTER TABLE gateway_shared_metering_requests DROP CONSTRAINT gateway_shared_metering_requests_namespace_event_id_key",
             "ALTER TABLE gateway_shared_metering_requests ADD CONSTRAINT gateway_shared_metering_requests_namespace_event_id_key UNIQUE (namespace,event_id)"),
            ("ALTER TABLE gateway_shared_native_continuations ALTER COLUMN body TYPE jsonb USING body::jsonb",
             "ALTER TABLE gateway_shared_native_continuations ALTER COLUMN body TYPE text USING body::text"),
        ] {
            db.execute_unprepared(corrupt).await?;
            fixture.ordinary_refused(&before, "incompatible shared schema").await?;
            db.execute_unprepared(repair).await?;
            crate::db::run_migrations(db).await?;
            ensure!(fixture.composed_snapshot().await? == before);
        }
        Ok(())
    }).await.context("mutated shared composition refusal deadline exceeded").and_then(|result| result);
    database.finish(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn ut_postgres_shared_refuses_mixed_ordinary_role_before_registration() -> Result<()> {
    use sea_orm_migration::MigratorTrait;
    let mut database = EmptyDatabase::create().await?;
    let result = async {
        let timeout = Duration::from_secs(5);
        let db = database.connect(2, timeout).await?;
        // Reproduce both independent ledgers installed by the historical binary.
        crate::db::migration::m0_fixture::install(&db).await?;
        schema::SharedMigrator::up(&db, None).await?;
        let authority = Authority::new(
            Store::new(db, "mixed", timeout)?,
            DeploymentKey::from_bytes(rand::random())?,
        );
        authority.initialize().await?;
        let fixture = Fixture { authority };
        let before = fixture.composed_snapshot().await?;
        let err = Engine::join(fixture.authority.clone(), 30_000)
            .await
            .err()
            .context("mixed store joined")?;
        ensure!(format!("{err:#}").contains("new empty database"), "{err:#}");
        let err = schema::initialize_all(fixture.authority.store())
            .await
            .err()
            .context("mixed store bootstrapped")?;
        ensure!(format!("{err:#}").contains("new empty database"), "{err:#}");
        ensure!(fixture.composed_snapshot().await? == before);
        Ok(())
    }
    .await;
    database.finish(result).await
}
