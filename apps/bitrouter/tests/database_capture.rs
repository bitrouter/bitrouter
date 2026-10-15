//! Real database regression for ACP capture initialization and durable appends.
//!
//! SQLite uses a temporary file. The ignored server test requires an explicit
//! BITROUTER_TEST_DATABASE_URL naming a dedicated PostgreSQL or MySQL test
//! database and the matching db-postgres/db-mysql feature. It applies pending
//! migrations, uses UUID-scoped fixtures, and leaves server records intact;
//! there is no fallback, truncation, or external harness process.
//! Enable harness for this target. The portable exercise has a two-minute deadline.
//! MySQL snapshot races additionally require db-mysql and a MySQL URL; use
//! backend-specific feature profiles with --include-ignored so PostgreSQL builds
//! omit those tests. Their barrier, transaction release and completion are bounded.
//! The tombstone race deletes only its own UUID-scoped capture content.
//! ACP fixture messages follow the upstream session protocol and schema:
//! https://github.com/agentclientprotocol/agent-client-protocol

#![cfg(feature = "harness")]

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter::acp_trajectory::{CanonicalStore, RecordingScope, SessionTranscript};
use bitrouter::db;
use bitrouter::harness::acp::capture::CapturePort;
use bitrouter::storage::capture::{CaptureDirection, CaptureEvent, CaptureKind};
use bitrouter::storage::session_identity::SessionIdentity;
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test]
async fn sqlite_database_capture() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("database-capture.sqlite");
    exercise_bounded(&format!("sqlite://{}", path.display())).await?;
    ensure!(
        path.is_file(),
        "capture regression must use a real SQLite file"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL for a dedicated PostgreSQL or MySQL test database"]
async fn server_database_capture() -> Result<()> {
    let database_url = std::env::var("BITROUTER_TEST_DATABASE_URL").context(
        "set BITROUTER_TEST_DATABASE_URL to a dedicated test database; no fallback is used",
    )?;
    let parsed = url::Url::parse(&database_url)
        .context("BITROUTER_TEST_DATABASE_URL must be a valid server database URL")?;
    match parsed.scheme() {
        "postgres" | "postgresql" => {
            ensure!(
                cfg!(feature = "db-postgres"),
                "enable the db-postgres feature"
            );
        }
        "mysql" => ensure!(cfg!(feature = "db-mysql"), "enable the db-mysql feature"),
        _ => anyhow::bail!("BITROUTER_TEST_DATABASE_URL must select PostgreSQL or MySQL"),
    }
    ensure!(
        !parsed.path().trim_matches('/').is_empty(),
        "BITROUTER_TEST_DATABASE_URL must explicitly name a dedicated test database"
    );
    exercise_bounded(&database_url).await
}

async fn exercise_bounded(database_url: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(120), exercise(database_url))
        .await
        .context("capture database regression exceeded its two-minute deadline")?
}

async fn exercise(database_url: &str) -> Result<()> {
    let connection = db::connect(database_url).await?;
    let result = async {
        db::run_migrations(&connection).await?;
        let store = CanonicalStore::new(connection.clone());
        let snapshots = capture_scenario(&store).await?;
        check_readback(&store, &snapshots).await?;
        Ok::<_, anyhow::Error>(snapshots)
    }
    .await;
    let closed = connection.close().await;
    let snapshots = result?;
    closed?;

    // A fresh pool must read the same session metadata, events and connections.
    let reopened = db::connect(database_url).await?;
    let result = check_readback(&CanonicalStore::new(reopened.clone()), &snapshots).await;
    let closed = reopened.close().await;
    result?;
    closed?;
    Ok(())
}

async fn capture_scenario(
    store: &CanonicalStore,
) -> Result<Vec<(SessionIdentity, SessionTranscript)>> {
    let scope = Uuid::new_v4().to_string();
    let parent = SessionIdentity {
        owner: format!("capture-{scope}-owner"),
        source: format!("capture-{scope}-source"),
        native_session_id: format!("capture-{scope}-parent"),
    };
    let child = SessionIdentity {
        native_session_id: format!("capture-{scope}-child"),
        ..parent.clone()
    };
    let recorder = store.recorder(recording_scope(&parent)).await?;
    let port = recorder.as_ref();
    let setup = record(
        port,
        CaptureKind::Request,
        Some(1),
        "session/new",
        json!({"cwd":"/workspace"}),
    )
    .await?;
    let created = record(
        port,
        CaptureKind::Response,
        Some(1),
        "session/new",
        json!({"result":{"sessionId":parent.native_session_id}}),
    )
    .await?;
    let mut parent_metadata = store.transcript(&parent).await?.session;
    ensure!(parent_metadata.head == 1 && parent_metadata.history_origin == "new");
    ensure!(
        !parent_metadata.deleted
            && parent_metadata.parent_key.is_none()
            && parent_metadata.parent_watermark.is_none()
    );

    // The fork request reinitializes the existing parent with default metadata;
    // the response creates a partial child with a non-default lineage boundary.
    let fork = record(
        port,
        CaptureKind::Request,
        Some(2),
        "session/fork",
        json!({"sessionId":parent.native_session_id}),
    )
    .await?;
    let forked = record(
        port,
        CaptureKind::Response,
        Some(2),
        "session/fork",
        json!({"result":{"sessionId":child.native_session_id}}),
    )
    .await?;
    let mut child_metadata = store.transcript(&child).await?.session;
    ensure!(child_metadata.head == 1 && child_metadata.history_origin == "partial");
    ensure!(
        child_metadata.parent_key == Some(parent.key()?)
            && child_metadata.parent_watermark == Some(1)
            && !child_metadata.deleted
    );
    let mut child_events = vec![forked];
    for text in ["first append", "second append"] {
        child_events.push(record(port, CaptureKind::Notification, None, "session/update", json!({
            "sessionId":child.native_session_id,
            "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}
        })).await?);
    }
    record(
        port,
        CaptureKind::Disconnected,
        None,
        "controller/disconnect",
        json!({"clean":true}),
    )
    .await?;

    let parent_transcript = store.transcript(&parent).await?;
    let child_transcript = store.transcript(&child).await?;
    parent_metadata.head = 2;
    child_metadata.head = 3;
    ensure!(
        parent_transcript.session == parent_metadata,
        "reinitialization reset parent metadata"
    );
    ensure!(
        child_transcript.session == child_metadata,
        "reinitialization reset fork metadata"
    );
    check_events(&parent_transcript, &[created, fork])?;
    check_events(&child_transcript, &child_events)?;
    ensure!(parent_transcript.gaps.is_empty());
    ensure!(parent_transcript.setup_evidence.len() == 1);
    ensure!(
        parent_transcript
            .setup_evidence
            .first()
            .context("missing setup evidence")?
            .event
            == setup
    );
    for transcript in [&parent_transcript, &child_transcript] {
        let connection = transcript
            .connections
            .first()
            .context("missing capture connection")?;
        ensure!(
            transcript.connections.len() == 1
                && connection.connection_id == recorder.connection_id()
        );
        ensure!(connection.head == 7 && connection.state == "complete");
    }
    let mut snapshots = vec![
        (parent.clone(), parent_transcript),
        (child, child_transcript),
    ];

    // The same native ID in another owner or source must start an independent
    // partial history, leaving the original parent's new-session origin intact.
    for identity in [
        SessionIdentity {
            owner: format!("{}-other", parent.owner),
            ..parent.clone()
        },
        SessionIdentity {
            source: format!("{}-other", parent.source),
            ..parent.clone()
        },
    ] {
        let recorder = store.recorder(recording_scope(&identity)).await?;
        let update = record(recorder.as_ref(), CaptureKind::Notification, None, "session/update", json!({
            "sessionId":identity.native_session_id,
            "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":identity.owner}}
        })).await?;
        record(
            recorder.as_ref(),
            CaptureKind::Disconnected,
            None,
            "controller/disconnect",
            json!({"clean":true}),
        )
        .await?;
        let transcript = store.transcript(&identity).await?;
        ensure!(
            transcript.session.history_origin == "partial"
                && transcript.session.parent_key.is_none()
        );
        check_events(&transcript, &[update])?;
        snapshots.push((identity, transcript));
    }
    Ok(snapshots)
}

fn recording_scope(identity: &SessionIdentity) -> RecordingScope {
    RecordingScope {
        owner: identity.owner.clone(),
        source: identity.source.clone(),
        controller_instance_id: None,
        route_scope_id: None,
    }
}

async fn record(
    port: &dyn CapturePort,
    kind: CaptureKind,
    call_id: Option<u64>,
    method: &str,
    payload: Value,
) -> Result<CaptureEvent> {
    let event = CaptureEvent {
        direction: match kind {
            CaptureKind::Request => CaptureDirection::Client,
            CaptureKind::Response | CaptureKind::Notification => CaptureDirection::Agent,
            CaptureKind::Connected | CaptureKind::Disconnected => CaptureDirection::Controller,
        },
        kind,
        call_id,
        method: method.to_owned(),
        payload,
    };
    port.record(event.clone()).await?;
    Ok(event)
}

fn check_events(transcript: &SessionTranscript, expected: &[CaptureEvent]) -> Result<()> {
    ensure!(transcript.session.head == i64::try_from(expected.len())?);
    ensure!(
        transcript
            .events
            .iter()
            .map(|node| &node.event)
            .eq(expected)
    );
    ensure!(
        transcript
            .events
            .iter()
            .enumerate()
            .all(|(index, node)| node.sequence == index as i64 + 1)
    );
    ensure!(transcript.replay_events.is_empty());
    Ok(())
}

async fn check_readback(
    store: &CanonicalStore,
    snapshots: &[(SessionIdentity, SessionTranscript)],
) -> Result<()> {
    for (identity, expected) in snapshots {
        let transcript = store.transcript(identity).await?;
        ensure!(
            serde_json::to_value(&transcript)? == serde_json::to_value(expected)?,
            "capture changed during readback"
        );
        ensure!(
            transcript.session.owner == identity.owner
                && transcript.session.source == identity.source
                && transcript.session.native_session_id == identity.native_session_id
        );
        let listed = store.list(&identity.owner, &identity.source).await?;
        let scoped: Vec<_> = snapshots
            .iter()
            .filter(|(other, _)| other.owner == identity.owner && other.source == identity.source)
            .map(|(_, transcript)| &transcript.session)
            .collect();
        ensure!(
            listed.len() == scoped.len() && scoped.iter().all(|session| listed.contains(session)),
            "session listing crossed owner/source boundaries"
        );
    }
    Ok(())
}

#[cfg(feature = "db-mysql")]
mod mysql_snapshot_races {
    use super::*;
    use std::sync::Arc;

    use bitrouter::acp_trajectory::Recorder;
    use bitrouter::harness::acp::capture::CaptureError;
    use bitrouter::storage::capture::{connections, events, sessions};
    use sea_orm::sea_query::Expr;
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, ConnectOptions, ConnectionTrait, Database, DatabaseBackend,
        DatabaseConnection, DatabaseTransaction, EntityName, EntityTrait, QueryFilter, Set,
        Statement, TransactionTrait,
    };
    use tokio::task::JoinSet;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Race {
        Creation,
        Tombstone,
        NativeIdReuse,
    }

    #[tokio::test]
    #[ignore = "requires harness,db-mysql and BITROUTER_TEST_DATABASE_URL naming a dedicated MySQL database"]
    async fn mysql_append_sees_concurrent_session_creation() -> Result<()> {
        exercise_race(Race::Creation).await
    }

    #[tokio::test]
    #[ignore = "requires harness,db-mysql and BITROUTER_TEST_DATABASE_URL naming a dedicated MySQL database"]
    async fn mysql_append_cannot_recreate_tombstoned_content() -> Result<()> {
        exercise_race(Race::Tombstone).await
    }

    #[tokio::test]
    #[ignore = "requires harness,db-mysql and BITROUTER_TEST_DATABASE_URL naming a dedicated MySQL database"]
    async fn mysql_new_session_sees_concurrent_prior_event() -> Result<()> {
        exercise_race(Race::NativeIdReuse).await
    }

    async fn exercise_race(race: Race) -> Result<()> {
        let database_url = std::env::var("BITROUTER_TEST_DATABASE_URL").context(
            "set BITROUTER_TEST_DATABASE_URL to a dedicated MySQL database; no fallback",
        )?;
        let parsed = url::Url::parse(&database_url)?;
        ensure!(
            parsed.scheme() == "mysql" && !parsed.path().trim_matches('/').is_empty(),
            "MySQL snapshot races require an explicit MySQL test database; use backend-specific feature profiles"
        );
        let observer = db::connect(&database_url).await?;
        let result = async {
            tokio::time::timeout(Duration::from_secs(30), db::run_migrations(&observer))
                .await
                .context("MySQL race migrations timed out")??;
            // Pin every recorder operation to the connection whose ID we observe.
            let mut options = ConnectOptions::new(database_url);
            options
                .min_connections(1)
                .max_connections(1)
                .connect_timeout(Duration::from_secs(10))
                .acquire_timeout(Duration::from_secs(10))
                .sqlx_logging(false);
            let writer = Database::connect(options).await?;
            let result =
                async {
                    writer
                        .execute(Statement::from_string(
                            DatabaseBackend::MySql,
                            "SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ",
                        ))
                        .await?;
                    writer
                        .execute(Statement::from_string(
                            DatabaseBackend::MySql,
                            "SET SESSION innodb_lock_wait_timeout = 15",
                        ))
                        .await?;
                    let row = writer.query_one(Statement::from_string(DatabaseBackend::MySql,
                    "SELECT CONNECTION_ID() AS id, @@transaction_isolation AS isolation_level"))
                    .await?.context("missing recorder connection ID")?;
                    ensure!(row.try_get::<String>("", "isolation_level")? == "REPEATABLE-READ");
                    race_scenario(&observer, &writer, row.try_get("", "id")?, race).await
                }
                .await;
            let closed = writer.close().await;
            result?;
            closed?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        let closed = observer.close().await;
        result?;
        closed?;
        Ok(())
    }

    async fn race_scenario(
        observer: &DatabaseConnection,
        writer: &DatabaseConnection,
        writer_id: u64,
        race: Race,
    ) -> Result<()> {
        let scope = Uuid::new_v4().to_string();
        let identity = SessionIdentity {
            owner: format!("race-{scope}-owner"),
            source: format!("race-{scope}-source"),
            native_session_id: format!("race-{scope}-native"),
        };
        let key = identity.key()?;
        let store = CanonicalStore::new(writer.clone());
        let recorder = store.recorder(recording_scope(&identity)).await?;
        let prior = CaptureEvent {
            direction: CaptureDirection::Agent,
            kind: CaptureKind::Notification,
            call_id: None,
            method: "session/update".into(),
            payload: json!({"sessionId":identity.native_session_id,"update":{
                "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"prior"}
            }}),
        };
        let initial = sessions::ActiveModel {
            session_key: Set(key.clone()),
            owner: Set(identity.owner.clone()),
            source: Set(identity.source.clone()),
            native_session_id: Set(identity.native_session_id.clone()),
            head: Set(0),
            deleted: Set(false),
            history_origin: Set("partial".into()),
            parent_key: Set(None),
            parent_watermark: Set(None),
        };
        let winner = CanonicalStore::new(observer.clone())
            .recorder(recording_scope(&identity))
            .await?;
        if race == Race::Tombstone {
            winner.record(prior.clone()).await?;
        } else if race == Race::NativeIdReuse {
            initial.clone().insert(observer).await?;
            record(
                recorder.as_ref(),
                CaptureKind::Request,
                Some(1),
                "session/new",
                json!({"cwd":"/workspace"}),
            )
            .await?;
        }

        // No checkpoint/dependent fixtures exist here. These scoped mutations
        // reproduce the persisted states protected by append/delete's row lock.
        let blocker = observer.begin().await?;
        match race {
            Race::Creation => {
                initial.insert(&blocker).await?;
            }
            Race::Tombstone => {
                sessions::Entity::update_many()
                    .col_expr(sessions::Column::Deleted, Expr::value(true))
                    .filter(sessions::Column::SessionKey.eq(&key))
                    .exec(&blocker)
                    .await?;
                events::Entity::delete_many()
                    .filter(events::Column::SessionKey.eq(&key))
                    .exec(&blocker)
                    .await?;
            }
            Race::NativeIdReuse => {
                sessions::Entity::update_many()
                    .col_expr(sessions::Column::Head, Expr::value(1_i64))
                    .filter(sessions::Column::SessionKey.eq(&key))
                    .exec(&blocker)
                    .await?;
                connections::Entity::update_many()
                    .col_expr(connections::Column::Head, Expr::value(1_i64))
                    .filter(connections::Column::ConnectionId.eq(winner.connection_id()))
                    .exec(&blocker)
                    .await?;
                events::ActiveModel {
                    connection_id: Set(winner.connection_id().to_owned()),
                    sequence: Set(1),
                    session_key: Set(Some(key.clone())),
                    session_sequence: Set(Some(1)),
                    replay: Set(false),
                    event_json: Set(serde_json::to_string(&prior)?),
                    captured_at: Set(chrono::Utc::now().to_rfc3339()),
                }
                .insert(&blocker)
                .await?;
            }
        }
        let pending = if race == Race::NativeIdReuse {
            CaptureEvent {
                direction: CaptureDirection::Agent,
                kind: CaptureKind::Response,
                call_id: Some(1),
                method: "session/new".into(),
                payload: json!({"result":{"sessionId":identity.native_session_id}}),
            }
        } else {
            let mut event = prior.clone();
            event.payload["update"]["content"]["text"] = json!("pending append");
            event
        };
        let appended = commit_after_pending_insert(
            observer,
            writer_id,
            blocker,
            Arc::clone(&recorder),
            pending.clone(),
        )
        .await?;
        let session = sessions::Entity::find_by_id(&key)
            .one(observer)
            .await?
            .context("missing session")?;
        if race == Race::Tombstone {
            // Transcript rejection alone would miss content resurrected beneath
            // a still-deleted session. Inspect the raw content and cursors too.
            ensure!(
                events::Entity::find()
                    .filter(events::Column::SessionKey.eq(&key))
                    .all(observer)
                    .await?
                    .is_empty(),
                "append recreated tombstoned capture content"
            );
            ensure!(
                session.deleted && session.head == 1,
                "append changed the tombstone head"
            );
            ensure!(
                appended.is_err(),
                "append acknowledged a tombstoned session"
            );
            let connection = connections::Entity::find_by_id(recorder.connection_id())
                .one(observer)
                .await?
                .context("missing recorder connection")?;
            ensure!(connection.head == 0 && connection.state == "interrupted");
        } else {
            appended.context("append must see the concurrently committed session")?;
            let transcript = store.transcript(&identity).await?;
            if race == Race::Creation {
                ensure!(session.history_origin == "partial" && !session.deleted);
                check_events(&transcript, &[pending])?;
            } else {
                ensure!(
                    session.history_origin == "native_id_reused",
                    "session/new missed a prior event committed after its snapshot"
                );
                check_events(&transcript, &[prior, pending])?;
            }
        }
        Ok(())
    }

    async fn commit_after_pending_insert(
        observer: &DatabaseConnection,
        writer_id: u64,
        blocker: DatabaseTransaction,
        recorder: Arc<Recorder>,
        pending: CaptureEvent,
    ) -> Result<Result<(), CaptureError>> {
        let mut tasks = JoinSet::new();
        tasks.spawn(async move { recorder.record(pending).await });
        // RR's first nonlocking read fixes the snapshot. Seeing this recorder's
        // INSERT proves its earlier connection SELECT has already completed.
        // https://dev.mysql.com/doc/refman/8.4/en/innodb-consistent-read.html
        // PROCESSLIST exposes a user's own connections without PROCESS privilege;
        // match the connection ID and table, not prepared-statement parameter text.
        // https://dev.mysql.com/doc/refman/8.4/en/information-schema-processlist-table.html
        let barrier = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(completed) = tasks.try_join_next() {
                    anyhow::bail!("recorder finished before the INSERT barrier: {completed:?}");
                }
                let row = observer.query_one(Statement::from_sql_and_values(
                    DatabaseBackend::MySql,
                    "SELECT INFO FROM information_schema.PROCESSLIST WHERE ID = ? AND DB = DATABASE() AND COMMAND IN ('Execute', 'Query')",
                    [writer_id.into()],
                )).await?;
                if let Some(row) = row
                    && let Some(sql) = row.try_get::<Option<String>>("", "INFO")?
                    && sql.trim_start().starts_with("INSERT INTO")
                    && sql.contains(sessions::Entity.table_name())
                {
                    break Ok::<(), anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.context("timed out observing the recorder's pending session INSERT")
            .and_then(|result| result);
        let released = tokio::time::timeout(Duration::from_secs(10), async {
            if barrier.is_ok() {
                blocker.commit().await
            } else {
                blocker.rollback().await
            }
        })
        .await
        .context("timed out releasing the session blocker")
        .and_then(|result| result.context("releasing the session blocker"));
        if barrier.is_err() || released.is_err() {
            // shutdown aborts AND joins; do not detach an append on failure.
            tasks.shutdown().await;
            barrier?;
            released?;
        }
        let completed = tokio::time::timeout(Duration::from_secs(10), tasks.join_next()).await;
        tasks.shutdown().await;
        completed
            .context("recorder did not finish after releasing the session blocker")?
            .context("missing recorder task")?
            .context("recorder task failed")
    }
}
