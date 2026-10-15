//! Opt-in regressions for MySQL's default REPEATABLE READ isolation.
//!
//! Requires BITROUTER_TEST_DATABASE_URL naming a dedicated MySQL test database.
//! Applies pending migrations, uses UUID-scoped fixtures, and retains them after
//! each run. Only the deletion scenario deletes its own captured session content.
//! No harness process, model, database truncation, or global fixture cleanup runs.
//! ACP fixture updates follow https://github.com/agentclientprotocol/agent-client-protocol.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use sea_orm::{ConnectOptions, Database, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use tokio::sync::{Mutex, oneshot};
use uuid::Uuid;

use super::*;
use crate::acp_trajectory::tests::{event, new_session};
use crate::acp_trajectory::{Recorder, RecordingScope};
use crate::harness::acp::capture::CapturePort;

// Serialize migration setup within this module, including parallel test runs.
static MIGRATIONS: Mutex<()> = Mutex::const_new(());
const RACE_TIMEOUT: Duration = Duration::from_secs(30);

async fn with_mysql<F, Fut>(scenario: F) -> Result<()>
where
    F: FnOnce(CanonicalStore) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let database_url = std::env::var("BITROUTER_TEST_DATABASE_URL").context(
        "set BITROUTER_TEST_DATABASE_URL to a dedicated MySQL test database; no fallback",
    )?;
    let parsed = url::Url::parse(&database_url)?;
    ensure!(
        parsed.scheme() == "mysql" && !parsed.path().trim_matches('/').is_empty(),
        "BITROUTER_TEST_DATABASE_URL must explicitly name a MySQL test database"
    );
    let mut options = ConnectOptions::new(database_url);
    options
        .min_connections(2)
        .max_connections(2)
        .connect_timeout(Duration::from_secs(10))
        .acquire_timeout(Duration::from_secs(10))
        .sqlx_logging(false);
    let db = tokio::time::timeout(RACE_TIMEOUT, Database::connect(options))
        .await
        .context("MySQL checkpoint test connection timed out")??;
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        {
            let _guard = MIGRATIONS.lock().await;
            crate::db::run_migrations(&db).await?;
        }
        scenario(CanonicalStore::new(db.clone())).await
    })
    .await
    .context("MySQL checkpoint regression exceeded its two-minute deadline")
    .and_then(|result| result);
    let closed = tokio::time::timeout(RACE_TIMEOUT, db.close())
        .await
        .context("MySQL checkpoint test pool close timed out");
    result?;
    closed??;
    Ok(())
}

async fn mysql_connection_id(db: &impl ConnectionTrait) -> Result<u64> {
    // Do not switch isolation to READ COMMITTED: these regressions require the
    // default RR read view to survive a subsequent current/locking read.
    // https://dev.mysql.com/doc/refman/8.4/en/innodb-transaction-isolation-levels.html
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::MySql,
            "SELECT CONNECTION_ID() AS id, @@session.transaction_isolation AS isolation_level",
        ))
        .await?
        .context("missing MySQL connection metadata")?;
    ensure!(
        row.try_get::<String>("", "isolation_level")? == "REPEATABLE-READ",
        "checkpoint snapshot regressions require MySQL's default REPEATABLE READ"
    );
    Ok(row.try_get("", "id")?)
}

async fn wait_for_commit(
    tx: &DatabaseTransaction,
    snapshot_ready: oneshot::Sender<u64>,
    committed: oneshot::Receiver<()>,
) -> Result<()> {
    // The caller has already performed an ordinary SELECT on an InnoDB table.
    // That fixes RR's read view; a no-op UPDATE or SELECT FOR UPDATE does not
    // advance the view used by later ordinary SELECTs.
    // https://dev.mysql.com/doc/refman/8.4/en/innodb-consistent-read.html
    ensure!(
        snapshot_ready.send(mysql_connection_id(tx).await?).is_ok(),
        "competing writer disappeared before the snapshot barrier"
    );
    tokio::time::timeout(RACE_TIMEOUT, committed)
        .await
        .context("timed out waiting for the competing commit")?
        .context("competing writer did not commit")?;
    Ok(())
}

async fn commit_after_snapshot<F, T>(
    db: &DatabaseConnection,
    snapshot_ready: oneshot::Receiver<u64>,
    committed: oneshot::Sender<()>,
    write: F,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::time::timeout(RACE_TIMEOUT, async {
        let reader_id = snapshot_ready
            .await
            .context("checkpoint operation did not establish its snapshot")?;
        // The reader holds one of this pool's two connections for its entire
        // transaction. All competing operations must use the other connection.
        ensure!(
            mysql_connection_id(db).await? != reader_id,
            "competing write must use a different MySQL connection"
        );
        let result = write.await?;
        ensure!(
            committed.send(()).is_ok(),
            "checkpoint operation disappeared before the competing commit"
        );
        Ok(result)
    })
    .await
    .context("competing checkpoint write exceeded its deadline")?
}

async fn fixture(store: &CanonicalStore) -> Result<(SessionIdentity, Arc<Recorder>)> {
    let id = Uuid::new_v4().to_string();
    let identity = SessionIdentity {
        owner: format!("checkpoint-{id}-owner"),
        source: format!("checkpoint-{id}-source"),
        native_session_id: format!("checkpoint-{id}-native"),
    };
    let recorder = store
        .recorder(RecordingScope {
            owner: identity.owner.clone(),
            source: identity.source.clone(),
            controller_instance_id: Some(format!("checkpoint-{id}-controller")),
            route_scope_id: None,
        })
        .await?;
    new_session(recorder.as_ref(), &identity.native_session_id).await?;
    ensure!(session(&store.db, &identity.key()?).await?.head == 1);
    Ok((identity, recorder))
}

#[derive(Clone, Copy)]
enum SessionChange {
    Append,
    Delete,
}

#[tokio::test]
#[ignore = "requires db-mysql and BITROUTER_TEST_DATABASE_URL naming a dedicated MySQL database"]
async fn mysql_freeze_rejects_session_changed_after_snapshot() -> Result<()> {
    with_mysql(|store| freeze_after_snapshot(store, SessionChange::Append)).await
}

#[tokio::test]
#[ignore = "requires db-mysql and BITROUTER_TEST_DATABASE_URL naming a dedicated MySQL database"]
async fn mysql_freeze_rejects_session_deleted_after_snapshot() -> Result<()> {
    with_mysql(|store| freeze_after_snapshot(store, SessionChange::Delete)).await
}

async fn freeze_after_snapshot(store: CanonicalStore, change: SessionChange) -> Result<()> {
    let (identity, recorder) = fixture(&store).await?;
    let key = identity.key()?;
    let initial = session(&store.db, &key).await?;
    let old_session = initial.clone();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (committed_tx, committed_rx) = oneshot::channel();
    let frozen = store.freeze_checkpoint_checked(&identity, initial.head, move |tx| {
        Box::pin(async move {
            ensure!(session(tx, &old_session.session_key).await? == old_session);
            wait_for_commit(tx, ready_tx, committed_rx).await?;
            ensure!(
                session(tx, &old_session.session_key).await? == old_session,
                "freeze callback did not retain the old session snapshot"
            );
            Ok(())
        })
    });
    let writer = commit_after_snapshot(&store.db, ready_rx, committed_tx, async {
        match change {
            SessionChange::Append => {
                recorder
                    .record(event(
                        CaptureKind::Notification,
                        None,
                        "session/update",
                        json!({"sessionId":identity.native_session_id,"update":{
                            "sessionUpdate":"agent_message_chunk",
                            "content":{"type":"text","text":"committed after snapshot"}
                        }}),
                    ))
                    .await?;
                ensure!(session(&store.db, &key).await?.head == initial.head + 1);
            }
            SessionChange::Delete => {
                store.delete(&identity).await?;
                let deleted = sessions::Entity::find_by_id(&key)
                    .one(&store.db)
                    .await?
                    .context("deleted session tombstone missing")?;
                ensure!(deleted.deleted && deleted.head == initial.head);
            }
        }
        Ok(())
    });
    let (frozen, committed) = tokio::join!(frozen, writer);
    committed?;

    // A late resource/content error is insufficient: freeze must roll back its
    // checkpoint and membership writes, including after concurrent deletion.
    ensure!(
        checkpoints::Entity::find()
            .filter(checkpoints::Column::SessionKey.eq(&key))
            .all(&store.db)
            .await?
            .is_empty(),
        "obsolete freeze persisted a checkpoint after the competing session change"
    );
    ensure!(
        members::Entity::find()
            .filter(members::Column::SessionKey.eq(&key))
            .all(&store.db)
            .await?
            .is_empty(),
        "obsolete freeze persisted checkpoint membership"
    );
    let error = frozen
        .err()
        .context("obsolete freeze unexpectedly succeeded")?;
    let message = format!("{error:#}");
    match change {
        SessionChange::Append => {
            ensure!(
                message.contains("session changed"),
                "unexpected error: {message}"
            );
            let fresh = store.freeze_checkpoint(&identity, initial.head + 1).await?;
            ensure!(fresh.watermark == initial.head + 1);
            ensure!(
                store
                    .checkpoint_content(&identity, &fresh.checkpoint_id)
                    .await?
                    .events
                    .len()
                    == 2,
                "fresh freeze did not retain the committed append"
            );
        }
        SessionChange::Delete => {
            ensure!(
                message.contains("content was deleted") || message.contains("session changed"),
                "unexpected error: {message}"
            );
            ensure!(
                events::Entity::find()
                    .filter(events::Column::SessionKey.eq(&key))
                    .all(&store.db)
                    .await?
                    .is_empty(),
                "freeze recreated deleted capture content"
            );
        }
    }
    Ok(())
}

fn assessment_input(
    cp: &Checkpoint,
    expected_revision: Option<&str>,
    source: AssessmentSource,
) -> RevisionInput {
    RevisionInput {
        submission_id: Uuid::new_v4().to_string(),
        checkpoint_id: cp.checkpoint_id.clone(),
        expected_revision: expected_revision.map(str::to_owned),
        source,
        evaluator_id: "mysql-checkpoint-regression".into(),
        evaluator_version: "1".into(),
        reason: "snapshot regression".into(),
        assessment: Some(AssessmentContent {
            pipeline_config_digest: "a".repeat(64),
            selection_digest: "b".repeat(64),
            scores: BTreeMap::from([(
                "correctness".into(),
                CriterionScore::Scored { value_ppm: 500_000 },
            )]),
            evidence: Vec::new(),
            explanation: "recorded fixture assessment".into(),
        }),
    }
}

#[tokio::test]
#[ignore = "requires db-mysql and BITROUTER_TEST_DATABASE_URL naming a dedicated MySQL database"]
async fn mysql_stale_assessment_cannot_overwrite_human_after_snapshot() -> Result<()> {
    with_mysql(|store| async move {
        let (identity, _recorder) = fixture(&store).await?;
        let key = identity.key()?;
        let cp = store.freeze_checkpoint(&identity, 1).await?;
        let original = store
            .submit_assessment(
                &identity,
                assessment_input(&cp, None, AssessmentSource::Agentic),
            )
            .await?;
        ensure!(original.selected_on_submission);
        let old_head = heads::Entity::find_by_id(&key)
            .one(&store.db)
            .await?
            .context("initial assessment head missing")?;
        ensure!(old_head.revision_id.as_deref() == Some(original.revision_id.as_str()));
        let stale_input =
            assessment_input(&cp, Some(&original.revision_id), AssessmentSource::Agentic);
        let stale_id = digest(&(&key, &stale_input.submission_id))?;
        let mut human_input =
            assessment_input(&cp, Some(&original.revision_id), AssessmentSource::Human);
        human_input.reason = "human correction committed while automatic work was pending".into();
        human_input
            .assessment
            .as_mut()
            .context("human assessment missing")?
            .scores
            .insert(
                "correctness".into(),
                CriterionScore::Scored { value_ppm: 0 },
            );
        let expected_human_input = serde_json::to_value(&human_input)?;
        let (ready_tx, ready_rx) = oneshot::channel();
        let (committed_tx, committed_rx) = oneshot::channel();
        let stale = store.submit_assessment_checked(&identity, stale_input, move |tx| {
            Box::pin(async move {
                ensure!(
                    heads::Entity::find_by_id(&old_head.session_key)
                        .one(tx)
                        .await?
                        == Some(old_head.clone())
                );
                wait_for_commit(tx, ready_tx, committed_rx).await?;
                ensure!(
                    heads::Entity::find_by_id(&old_head.session_key)
                        .one(tx)
                        .await?
                        == Some(old_head),
                    "assessment callback did not retain the old head snapshot"
                );
                // A current read of the session later in lock_session does not
                // refresh this read view. The assessment head itself needs a
                // current read/CAS before selecting a replacement revision.
                Ok(())
            })
        });
        let writer = commit_after_snapshot(&store.db, ready_rx, committed_tx, async {
            let human = store.submit_assessment(&identity, human_input).await?;
            ensure!(human.selected_on_submission);
            ensure!(human.supersedes.as_deref() == Some(original.revision_id.as_str()));
            ensure!(
                heads::Entity::find_by_id(&key)
                    .one(&store.db)
                    .await?
                    .context("committed human head missing")?
                    .revision_id
                    .as_deref()
                    == Some(human.revision_id.as_str())
            );
            Ok(human)
        });
        let (stale, human) = tokio::join!(stale, writer);
        let human = human?;
        let view = store.effective_assessment(&identity).await?;
        ensure!(
            view.current_revision.as_deref() == Some(human.revision_id.as_str()),
            "stale expected_revision overwrote the committed human correction"
        );
        ensure!(
            serde_json::to_value(view.assessment.context("human correction missing")?.input)?
                == expected_human_input,
            "selected human correction changed"
        );
        ensure!(
            revisions::Entity::find_by_id(stale_id)
                .one(&store.db)
                .await?
                .is_none(),
            "rejected stale submission persisted an assessment revision"
        );
        ensure!(store.assessment_history(&identity).await?.len() == 2);
        let error = stale
            .err()
            .context("stale expected_revision was accepted")?;
        ensure!(
            format!("{error:#}").contains("effective revision changed"),
            "stale submission failed for an unrelated reason: {error:#}"
        );
        Ok(())
    })
    .await
}
