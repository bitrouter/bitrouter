//! Actual fork fan-out and a failure after dependent job deletion in its transaction.
use super::*;
use crate::core::evolution::jobs::{JobStatus, JudgeJob};
use crate::core::evolution::judge::{JUDGE_VERSION, JudgeOutput};
use crate::core::evolution::rubric::{
    Applicability, RUBRIC_VERSION, RubricEvaluation, RubricItem, library,
};

#[tokio::test]
async fn fork_delete_rolls_back_cached_jobs_then_removes_only_legacy_dependents() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("delete.sqlite").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let canonical = CanonicalStore::new(db.clone());
    let recorder = recorded(&db, "parent").await?;
    // ACP fork request/response semantics: https://github.com/agentclientprotocol/agent-client-protocol
    recorder
        .record(event(
            CaptureKind::Request,
            Some(42),
            "session/fork",
            json!({"sessionId":"parent"}),
        ))
        .await?;
    recorder
        .record(event(
            CaptureKind::Response,
            Some(42),
            "session/fork",
            json!({"result":{"sessionId":"child"}}),
        ))
        .await?;
    let store = EvolutionStore::new(db.clone(), "fixture-owner")?;
    let jobs = JudgeJobs::new(store.clone());
    let mut checkpoint_ids = Vec::new();
    for name in ["parent", "child"] {
        let id = identity(name);
        let row = sessions::Entity::find_by_id(id.key()?)
            .one(&db)
            .await?
            .context("session")?;
        let cp = canonical.freeze_checkpoint(&id, row.head).await?;
        if name == "child" {
            ensure!(cp.segments.len() == 2 && cp.segments[0].identity == identity("parent"));
        }
        canonical
            .submit_assessment(&id, label(name, &cp.checkpoint_id))
            .await?;
        canonical
            .observe_checkpoint_resources(&id, &cp.checkpoint_id)
            .await?;
        let job = jobs
            .enqueue(&id, &cp.checkpoint_id, "fixture/judge", None)
            .await?;
        // Seed a typed completed response, without claiming any provider execution.
        store
            .update("judge_job", &job.job_id, 0, |job: &mut JudgeJob| {
                job.status = JobStatus::ResponseStored;
                job.request_ids.push(format!("cached-{name}"));
                job.cached_response = Some(JudgeOutput {
                    request_id: format!("cached-{name}"),
                    model: job.model.clone(),
                    judge_version: JUDGE_VERSION.into(),
                    input_digest: job.input_digest.clone(),
                    usage: None,
                    evaluation: RubricEvaluation {
                        rubric_version: RUBRIC_VERSION.into(),
                        items: library()
                            .into_iter()
                            .map(|criterion| RubricItem {
                                criterion_id: criterion.id.into(),
                                applicability: Applicability::Unknown,
                                selection_reason: "fixture".into(),
                                score: CriterionScore::Unknown,
                                evidence: vec![],
                                explanation: "fixture".into(),
                            })
                            .collect(),
                        diagnostics: vec![],
                        severe_violation: false,
                        violation_evidence: vec![],
                        summary: "cached text quoting deleted parent content".into(),
                    },
                });
                Ok(())
            })
            .await?;
        ensure!(jobs.get(&job.job_id).await?.cached_response.is_some());
        checkpoint_ids.push(cp.checkpoint_id);
    }
    store
        .initialize(
            "judge_job",
            "foreign-child-job",
            Some(identity("child").key()?),
            &json!({"foreign":"retain"}),
        )
        .await?;
    let foreign_id = store.id("judge_job", "foreign-child-job")?;
    mark(&db, "checkpoint_evolution", "record_id", &foreign_id, true).await?;
    poison(
        &db,
        "checkpoint_evolution",
        "record_id",
        &foreign_id,
        "body",
    )
    .await?;
    let before_jobs = records::Entity::find()
        .filter(records::Column::Kind.eq("judge_job"))
        .order_by_asc(records::Column::RecordId)
        .all(&db)
        .await?;
    ensure!(before_jobs.len() == 3);
    let before_cp = checkpoints::Entity::find()
        .order_by_asc(checkpoints::Column::CheckpointId)
        .all(&db)
        .await?;
    let before_revisions = revisions::Entity::find()
        .order_by_asc(revisions::Column::RevisionId)
        .all(&db)
        .await?;
    let before_resources = resources::Entity::find()
        .order_by_asc(resources::Column::ObservationId)
        .all(&db)
        .await?;
    let before_heads = heads::Entity::find()
        .order_by_asc(heads::Column::SessionKey)
        .all(&db)
        .await?;
    let before_sessions = sessions::Entity::find()
        .order_by_asc(sessions::Column::SessionKey)
        .all(&db)
        .await?;
    let before_events = events::Entity::find()
        .order_by_asc(events::Column::Sequence)
        .all(&db)
        .await?;
    // SQLite ABORT rolls back this statement, not earlier statements; only the
    // caller's transaction rollback restores the already deleted cached jobs.
    // https://www.sqlite.org/lang_createtrigger.html#the_raise_function
    db.execute_unprepared("CREATE TRIGGER fixture_delete_failure BEFORE UPDATE ON acp_checkpoints BEGIN SELECT CASE WHEN EXISTS (SELECT 1 FROM checkpoint_evolution WHERE kind = 'judge_job' AND session_key = OLD.session_key AND installation_id IS NULL AND tenant_id IS NULL AND creator_principal_id IS NULL) THEN RAISE(ABORT, 'delete ordering changed') ELSE RAISE(ABORT, 'after legacy job removal') END; END").await?;
    let error = canonical
        .delete(&identity("parent"))
        .await
        .err()
        .context("fault did not fire")?;
    ensure!(format!("{error:#}").contains("after legacy job removal"));
    drop((canonical, recorder, jobs, store));
    db.close().await?;
    // A fresh pool verifies durable rollback, rather than the failed transaction's view.
    let db = crate::db::connect(&url).await?;
    ensure!(
        records::Entity::find()
            .filter(records::Column::Kind.eq("judge_job"))
            .order_by_asc(records::Column::RecordId)
            .all(&db)
            .await?
            == before_jobs
    );
    ensure!(
        checkpoints::Entity::find()
            .order_by_asc(checkpoints::Column::CheckpointId)
            .all(&db)
            .await?
            == before_cp
    );
    ensure!(
        revisions::Entity::find()
            .order_by_asc(revisions::Column::RevisionId)
            .all(&db)
            .await?
            == before_revisions
    );
    ensure!(
        resources::Entity::find()
            .order_by_asc(resources::Column::ObservationId)
            .all(&db)
            .await?
            == before_resources
    );
    ensure!(
        heads::Entity::find()
            .order_by_asc(heads::Column::SessionKey)
            .all(&db)
            .await?
            == before_heads
    );
    ensure!(
        sessions::Entity::find()
            .order_by_asc(sessions::Column::SessionKey)
            .all(&db)
            .await?
            == before_sessions
    );
    ensure!(
        events::Entity::find()
            .order_by_asc(events::Column::Sequence)
            .all(&db)
            .await?
            == before_events
    );
    db.execute_unprepared("DROP TRIGGER fixture_delete_failure")
        .await?;
    let canonical = CanonicalStore::new(db.clone());
    canonical.delete(&identity("parent")).await?;
    for checkpoint in checkpoint_ids {
        let row = checkpoints::Entity::find_by_id(checkpoint)
            .one(&db)
            .await?
            .context("checkpoint tombstone")?;
        ensure!(row.deleted && row.manifest_json == "{}");
    }
    ensure!(revisions::Entity::find().all(&db).await?.is_empty());
    ensure!(resources::Entity::find().all(&db).await?.is_empty());
    ensure!(
        heads::Entity::find()
            .all(&db)
            .await?
            .iter()
            .all(|row| row.revision_id.is_none())
    );
    let remaining = records::Entity::find()
        .filter(records::Column::Kind.eq("judge_job"))
        .all(&db)
        .await?;
    ensure!(remaining.len() == 1 && remaining[0].record_id == foreign_id);
    ensure!(before_jobs.iter().any(|row| row == &remaining[0]));
    db.close().await?;
    Ok(())
}
