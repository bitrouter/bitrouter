use super::tests::{result, subject};
use super::*;
use bitrouter_ai::eval::EvalVerdict;
use sea_orm::IntoActiveModel;
use sea_orm::sea_query::Expr;

fn tenant(
    db: &DatabaseConnection,
    installation: &str,
    id: &str,
    creator: &str,
) -> Result<EvalStore> {
    let (scope, provenance) = Scope::for_test(
        installation.try_into()?,
        id.try_into()?,
        creator.try_into()?,
    );
    Ok(EvalStore {
        db: db.clone(),
        scope,
        provenance,
    })
}

async fn database() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    Ok(db)
}

async fn admitted(store: &EvalStore) -> Result<String> {
    store.insert_subject(&subject()?).await?;
    let value = store.insert_result(&result(EvalVerdict::Pass)).await?;
    store
        .append_admission_event(
            value.result_id(),
            AdmissionStatus::Admitted,
            "accepted",
            "evaluator",
        )
        .await?;
    Ok(value.result_id().to_owned())
}

const FROZEN: &str = "2026-10-25T00:00:00Z";

#[tokio::test]
async fn three_namespaces_share_logical_content_but_never_sql_identity() -> Result<()> {
    let db = database().await?;
    let legacy = EvalStore::new(db.clone());
    let a = tenant(&db, "installation", "a", "first")?;
    let second = tenant(&db, "installation", "a", "second")?;
    let b = tenant(&db, "installation", "b", "first")?;
    let other_installation = tenant(&db, "elsewhere", "a", "first")?;
    let logical = admitted(&legacy).await?;
    assert_eq!(admitted(&a).await?, logical);
    assert_eq!(admitted(&b).await?, logical);
    assert_eq!(admitted(&other_installation).await?, logical);
    assert_eq!(second.subject("eval-1").await?, Some(subject()?));
    assert_eq!(second.list_subjects().await?.len(), 1);
    assert!(matches!(
        second.insert_subject(&subject()?).await?,
        SubjectInsertOutcome::Duplicate
    ));
    assert!(
        matches!(second.insert_result(&result(EvalVerdict::Pass)).await?, ResultInsertOutcome::Duplicate { result_id } if result_id == logical)
    );
    let key = a.key(KeyDomain::EvalSubject, "eval-1")?;
    let row = subject_entity::Entity::find_by_id(&key)
        .one(&db)
        .await?
        .context("subject row")?;
    assert_eq!(row.creator_principal_id.as_deref(), Some("first"));
    assert_eq!(row.eval_id.len(), 64);
    assert_ne!(row.owner_user_id, "first");
    assert!(legacy.subject(&key).await?.is_none());
    assert!(a.subject("request:absent").await?.is_none());
    assert_eq!(legacy.list_subjects().await?.len(), 1);
    b.append_admission_event(&logical, AdmissionStatus::Disputed, "only b", "evaluator")
        .await?;
    assert_eq!(
        a.latest_admissions()
            .await?
            .get(&logical)
            .context("admission")?
            .status,
        AdmissionStatus::Admitted
    );
    assert_eq!(
        b.latest_admissions()
            .await?
            .get(&logical)
            .context("admission")?
            .status,
        AdmissionStatus::Disputed
    );
    assert_eq!(legacy.latest_admissions_for_owner("local").await?.len(), 1);
    let frozen = a.freeze_snapshot(FROZEN).await?;
    assert_eq!(frozen, second.freeze_snapshot(FROZEN).await?);
    assert_eq!(frozen.entries[0].result_id, logical);
    assert_eq!(frozen.entries[0].eval_id, "eval-1");
    assert!(b.freeze_snapshot(FROZEN).await?.entries.is_empty());
    let legacy_frozen = legacy.freeze_snapshot(FROZEN).await?;
    assert_eq!(
        legacy_frozen.evidence_root,
        canonical_digest(&("*", FROZEN, &legacy_frozen.entries))?
    );
    assert_ne!(frozen.evidence_root, legacy_frozen.evidence_root);
    assert!(
        crate::core::eval::compiler::EvalEvidenceSnapshot::from_manifest(&legacy, frozen.clone())
            .await
            .is_err()
    );
    assert!(
        crate::core::eval::compiler::EvalEvidenceSnapshot::from_manifest(&legacy, legacy_frozen)
            .await
            .is_ok()
    );
    let owned_legacy = legacy.freeze_snapshot_for_owner(FROZEN, "local").await?;
    assert!(
        crate::core::eval::compiler::EvalEvidenceSnapshot::from_manifest(&legacy, owned_legacy)
            .await
            .is_ok()
    );
    let sql = subject_entity::Entity::find_by_id("eval-1")
        .one(&db)
        .await?
        .context("legacy subject")?;
    assert_eq!(sql.subject_json, serde_json::to_string(&subject()?)?);
    assert_eq!(sql.owner_user_id, "local");
    assert!(
        sql.installation_id.is_none()
            && sql.tenant_id.is_none()
            && sql.creator_principal_id.is_none()
    );
    Ok(())
}

#[tokio::test]
async fn public_legacy_owner_overloads_reject_tenant_handles() -> Result<()> {
    let db = database().await?;
    let a = tenant(&db, "installation", "a", "first")?;
    let subject = subject()?;
    let result = result(EvalVerdict::Pass);
    assert!(a.insert_subject_owned(&subject, "first").await.is_err());
    assert!(a.insert_result_owned(&result, "first").await.is_err());
    assert!(a.subject_for_owner("eval-1", "first").await.is_err());
    assert!(a.list_subjects_for_owner("first").await.is_err());
    assert!(a.latest_admissions_for_owner("first").await.is_err());
    assert!(a.freeze_snapshot_for_owner(FROZEN, "first").await.is_err());
    assert!(
        a.freeze_snapshot_for_result_ids(FROZEN, "first", &["result".into()])
            .await
            .is_err()
    );
    assert!(a.snapshot_by_root_for_owner("root", "first").await.is_err());
    Ok(())
}

#[tokio::test]
async fn literal_names_are_exact_and_collections_order_logically() -> Result<()> {
    let db = database().await?;
    let a = tenant(&db, "installation", "a", "first")?;
    let ids = ["é", "case ", "Case", "e\u{301}", "case"];
    for id in ids {
        let mut item = subject()?;
        item.eval_id = id.into();
        a.insert_subject(&item).await?;
    }
    let mut expected = ids.map(str::to_owned).to_vec();
    expected.sort();
    assert_eq!(
        a.list_subjects()
            .await?
            .into_iter()
            .map(|value| value.eval_id)
            .collect::<Vec<_>>(),
        expected
    );
    for id in ids {
        assert_eq!(a.subject(id).await?.context("subject")?.eval_id, id);
    }
    Ok(())
}

#[tokio::test]
async fn crafted_legacy_primary_and_secondary_collisions_are_not_duplicates() -> Result<()> {
    let db = database().await?;
    let legacy = EvalStore::new(db.clone());
    let a = tenant(&db, "installation", "a", "first")?;
    let mut crafted = subject()?;
    crafted.eval_id = a.key(KeyDomain::EvalSubject, "eval-1")?;
    legacy.insert_subject(&crafted).await?;
    assert!(a.insert_subject(&subject()?).await.is_err());
    assert!(a.subject("eval-1").await?.is_none());
    assert_eq!(
        legacy.subject(&crafted.eval_id).await?,
        Some(crafted.clone())
    );
    let b = tenant(&db, "installation", "b", "first")?;
    b.insert_subject(&subject()?).await?;
    let compatibility = b.owner("local")?;
    legacy
        .insert_subject_owned(&subject()?, &compatibility)
        .await?;
    let mut collision = result(EvalVerdict::Pass);
    collision.idempotency_key = b.key(KeyDomain::EvalIdempotency, "submission-1")?;
    let original = legacy
        .insert_result_owned(&collision, &compatibility)
        .await?;
    assert!(b.insert_result(&result(EvalVerdict::Pass)).await.is_err());
    assert_eq!(
        legacy
            .result(original.result_id())
            .await?
            .context("legacy result")?
            .result,
        collision
    );
    assert!(b.results_for_subject("eval-1").await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn projections_and_parent_scope_tampering_fail_closed() -> Result<()> {
    let db = database().await?;
    let a = tenant(&db, "installation", "a", "first")?;
    let b = tenant(&db, "installation", "b", "first")?;
    let logical = admitted(&a).await?;
    admitted(&b).await?;
    let key = a.key(KeyDomain::EvalSubject, "eval-1")?;
    let row = subject_entity::Entity::find_by_id(&key)
        .one(&db)
        .await?
        .context("subject")?;
    for column in [
        subject_entity::Column::SubjectId,
        subject_entity::Column::Scope,
        subject_entity::Column::PolicyDigest,
        subject_entity::Column::ContentDigest,
        subject_entity::Column::OwnerUserId,
    ] {
        subject_entity::Entity::update_many()
            .col_expr(column, Expr::value("tampered"))
            .filter(subject_entity::Column::EvalId.eq(&key))
            .exec(&db)
            .await?;
        assert!(a.subject("eval-1").await.is_err());
        row.clone()
            .into_active_model()
            .reset_all()
            .update(&db)
            .await?;
    }
    subject_entity::Entity::update_many()
        .col_expr(
            subject_entity::Column::CreatorPrincipalId,
            Expr::value(Option::<String>::None),
        )
        .filter(subject_entity::Column::EvalId.eq(&key))
        .exec(&db)
        .await?;
    assert!(a.list_subjects().await.is_err());
    row.into_active_model().reset_all().update(&db).await?;
    let result_key = a.key(KeyDomain::EvalResult, &logical)?;
    result_entity::Entity::update_many()
        .col_expr(
            result_entity::Column::EvalId,
            Expr::value(b.key(KeyDomain::EvalSubject, "eval-1")?),
        )
        .filter(result_entity::Column::ResultId.eq(&result_key))
        .exec(&db)
        .await?;
    assert!(a.result(&logical).await.is_err());
    assert!(a.latest_admissions().await.is_err());
    result_entity::Entity::update_many()
        .col_expr(result_entity::Column::EvalId, Expr::value(&key))
        .filter(result_entity::Column::ResultId.eq(&result_key))
        .exec(&db)
        .await?;
    admission_entity::Entity::update_many()
        .col_expr(
            admission_entity::Column::ResultId,
            Expr::value(b.key(KeyDomain::EvalResult, &logical)?),
        )
        .filter(a.scope.predicate(
            admission_entity::Column::InstallationId,
            admission_entity::Column::TenantId,
        ))
        .exec(&db)
        .await?;
    assert!(a.latest_admissions().await.is_err());
    assert_eq!(b.latest_admissions().await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn frozen_roots_bind_namespace_and_survive_later_admission_changes() -> Result<()> {
    let db = database().await?;
    let a = tenant(&db, "installation", "a", "first")?;
    let b = tenant(&db, "installation", "b", "first")?;
    let logical = admitted(&a).await?;
    admitted(&b).await?;
    let snapshot = a.freeze_snapshot(FROZEN).await?;
    assert!(b.persist_snapshot(&snapshot).await.is_err());
    assert!(
        crate::core::eval::compiler::EvalEvidenceSnapshot::from_manifest(&b, snapshot.clone())
            .await
            .is_err()
    );
    let mut tampered = snapshot.clone();
    tampered.entries[0].eval_id = "foreign".into();
    assert!(
        crate::core::eval::compiler::EvalEvidenceSnapshot::from_manifest(&a, tampered)
            .await
            .is_err()
    );
    a.append_admission_event(&logical, AdmissionStatus::Disputed, "later", "evaluator")
        .await?;
    assert_eq!(a.persist_snapshot(&snapshot).await?, snapshot);
    assert_eq!(
        a.snapshot_by_root(&snapshot.evidence_root).await?,
        Some(snapshot.clone())
    );
    assert_eq!(
        crate::core::eval::compiler::EvalEvidenceSnapshot::from_manifest(&a, snapshot)
            .await?
            .records
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn missing_closure_and_forged_physical_projections_are_rejected() -> Result<()> {
    let db = database().await?;
    let a = tenant(&db, "installation", "a", "first")?;
    let b = tenant(&db, "installation", "b", "first")?;
    admitted(&b).await?;
    let foreign = b.freeze_snapshot(FROZEN).await?;
    let claimed = EvalSnapshot {
        evidence_root: a.snapshot_root(None, FROZEN, &foreign.entries)?,
        frozen_at: FROZEN.into(),
        entries: foreign.entries,
    };
    assert!(a.persist_snapshot(&claimed).await.is_err());
    let item = subject()?;
    a.insert_subject(&item).await?;
    let key = a.key(KeyDomain::EvalSubject, &item.eval_id)?;
    let mut forged = item;
    forged.eval_id = "different".into();
    subject_entity::Entity::update_many()
        .col_expr(
            subject_entity::Column::SubjectJson,
            Expr::value(serde_json::to_string(&forged)?),
        )
        .col_expr(
            subject_entity::Column::ContentDigest,
            Expr::value(crate::storage::evaluation::subject_digest(&forged)?),
        )
        .filter(subject_entity::Column::EvalId.eq(key))
        .exec(&db)
        .await?;
    assert!(a.subject("eval-1").await.is_err());
    Ok(())
}

#[tokio::test]
async fn row_limit_rejects_overflow_without_truncation_or_cross_namespace_counting() -> Result<()> {
    let db = database().await?;
    let a = tenant(&db, "installation", "a", "first")?;
    let b = tenant(&db, "installation", "b", "first")?;
    b.insert_subject(&subject()?).await?;
    let metadata = a.scope.metadata(&a.provenance)?;
    let mut batch = Vec::new();
    for index in 0..=io::MAX_ROWS {
        let mut value = subject()?;
        value.eval_id = format!("row-{index:04}");
        batch.push(subject_entity::ActiveModel {
            installation_id: Set(metadata.installation.clone()),
            tenant_id: Set(metadata.tenant.clone()),
            creator_principal_id: Set(metadata.creator.clone()),
            eval_id: Set(a.key(KeyDomain::EvalSubject, &value.eval_id)?),
            owner_user_id: Set(a.owner("local")?),
            subject_id: Set(value.subject_id.clone()),
            scope: Set(scope_name(value.scope).into()),
            policy_digest: Set(value.policy_digest.clone()),
            holdout: Set(value.holdout),
            content_digest: Set(crate::storage::evaluation::subject_digest(&value)?),
            subject_json: Set(serde_json::to_string(&value)?),
            created_at: Set(FROZEN.into()),
        });
        if batch.len() == 128 {
            subject_entity::Entity::insert_many(std::mem::take(&mut batch))
                .exec(&db)
                .await?;
        }
    }
    if !batch.is_empty() {
        subject_entity::Entity::insert_many(batch).exec(&db).await?;
    }
    assert!(
        a.list_subjects()
            .await
            .err()
            .context("missing row bound")?
            .to_string()
            .contains("row bound")
    );
    assert_eq!(b.list_subjects().await?.len(), 1);
    subject_entity::Entity::delete_by_id(a.key(KeyDomain::EvalSubject, "row-4096")?)
        .exec(&db)
        .await?;
    let rows = a.list_subjects().await?;
    assert_eq!(rows.len(), io::MAX_ROWS);
    assert_eq!(rows[0].eval_id, "row-0000");
    Ok(())
}

#[tokio::test]
async fn oversized_sql_cells_fail_before_decode_and_do_not_affect_other_tenants() -> Result<()> {
    let db = database().await?;
    let a = tenant(&db, "installation", "a", "first")?;
    let b = tenant(&db, "installation", "b", "first")?;
    a.insert_subject(&subject()?).await?;
    b.insert_subject(&subject()?).await?;
    // Construct the oversized value in SQLite, so the regression exercises the
    // SQL preflight rather than only a Rust deserialization limit.
    db.execute(sea_orm::Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Sqlite,
        "UPDATE eval_subjects SET subject_json = zeroblob(?) WHERE eval_id = ?",
        [
            ((io::MAX_BYTES + 1) as i64).into(),
            a.key(KeyDomain::EvalSubject, "eval-1")?.into(),
        ],
    ))
    .await?;
    assert!(
        a.subject("eval-1")
            .await
            .err()
            .context("missing byte bound")?
            .to_string()
            .contains("byte bound")
    );
    assert_eq!(b.subject("eval-1").await?, Some(subject()?));
    Ok(())
}

#[tokio::test]
async fn immutable_conflicts_leave_caller_transaction_usable() -> Result<()> {
    conflict_transaction(database().await?).await
}

async fn conflict_transaction(db: DatabaseConnection) -> Result<()> {
    let id = format!("fixture-{}", uuid::Uuid::new_v4());
    let a = tenant(&db, "installation", &id, "first")?;
    let second = tenant(&db, "installation", &id, "second")?;
    a.insert_subject(&subject()?).await?;
    a.insert_result(&result(EvalVerdict::Pass)).await?;
    let tx = second.transaction(true).await?;
    let owner = second.owner("local")?;
    assert!(matches!(
        second
            .insert_subject_in(&tx, &subject()?, &owner, &mut Budget::new(&second))
            .await?,
        SubjectInsertOutcome::Duplicate
    ));
    let mut conflicting = subject()?;
    conflicting.subject_id = "changed".into();
    assert!(
        second
            .insert_subject_in(&tx, &conflicting, &owner, &mut Budget::new(&second))
            .await
            .is_err()
    );
    assert!(matches!(
        second
            .insert_result_in(
                &tx,
                &result(EvalVerdict::Pass),
                &owner,
                &mut Budget::new(&second)
            )
            .await?,
        ResultInsertOutcome::Duplicate { .. }
    ));
    assert!(
        second
            .insert_result_in(
                &tx,
                &result(EvalVerdict::Fail),
                &owner,
                &mut Budget::new(&second)
            )
            .await
            .is_err()
    );
    let mut next = subject()?;
    next.eval_id = "after-conflicts".into();
    second
        .insert_subject_in(&tx, &next, &owner, &mut Budget::new(&second))
        .await?;
    tx.commit().await?;
    assert_eq!(a.subject(&next.eval_id).await?, Some(next));
    Ok(())
}

#[cfg(any(feature = "db-postgres", feature = "db-mysql"))]
#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL pointing at a dedicated PostgreSQL or MySQL database"]
async fn server_immutable_conflicts_preserve_transaction_and_first_creator() -> Result<()> {
    let url = std::env::var("BITROUTER_TEST_DATABASE_URL")?;
    let parsed = url::Url::parse(&url)?;
    ensure!(
        matches!(parsed.scheme(), "postgres" | "postgresql" | "mysql")
            && !parsed.path().trim_matches('/').is_empty(),
        "dedicated server database required"
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    conflict_transaction(db.clone()).await?;
    admission_queries_are_scoped(db).await
}

fn query_log(db: &mut DatabaseConnection) -> std::sync::mpsc::Receiver<String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    db.set_metric_callback(move |info| {
        let _ = sender.send(
            info.statement
                .sql
                .replace(['"', '`'], "")
                .to_ascii_lowercase(),
        );
    });
    receiver
}

#[tokio::test]
async fn admission_insert_reloads_only_through_scoped_bounded_queries() -> Result<()> {
    admission_queries_are_scoped(database().await?).await
}

async fn admission_queries_are_scoped(mut db: DatabaseConnection) -> Result<()> {
    let queries = query_log(&mut db);
    let id = format!("admission-{}", uuid::Uuid::new_v4());
    let a = tenant(&db, "installation", &id, "first")?;
    let b = tenant(&db, "elsewhere", &id, "second")?;
    let logical = admitted(&b).await?;
    a.insert_subject(&subject()?).await?;
    assert_eq!(
        a.insert_result(&result(EvalVerdict::Pass))
            .await?
            .result_id(),
        logical
    );
    queries.try_iter().for_each(drop);

    let event = a
        .append_admission_event(&logical, AdmissionStatus::Disputed, "only a", "reviewer")
        .await?;
    assert_eq!(event.result_id, logical);
    assert_eq!(event.reason, "only a");
    assert_eq!(event.authority_id, "reviewer");
    let queries = queries.try_iter().collect::<Vec<_>>();
    let reads = queries
        .iter()
        .filter(|sql| sql.starts_with("select "))
        .collect::<Vec<_>>();
    assert!(!reads.is_empty());
    for sql in &reads {
        let (_, predicate) = sql.split_once(" where ").context("unscoped eval read")?;
        assert!(predicate.contains("installation_id ="), "{sql}");
        assert!(predicate.contains("tenant_id ="), "{sql}");
    }
    let admission_reads = reads
        .iter()
        .filter(|sql| sql.contains("from eval_admission_events"))
        .collect::<Vec<_>>();
    assert_eq!(
        admission_reads.len(),
        2,
        "one preflight and one payload read: {queries:?}"
    );
    assert_eq!(
        admission_reads
            .iter()
            .filter(|sql| sql.contains("row_count"))
            .count(),
        1
    );
    let insert = queries
        .iter()
        .find(|sql| sql.starts_with("insert into eval_admission_events"))
        .context("missing admission insert")?;
    if let Some((_, returning)) = insert.split_once(" returning ") {
        assert!(returning.contains("sequence"));
        assert!(
            !returning.contains("reason"),
            "insert returned a full payload"
        );
    }
    assert_eq!(a.latest_admissions().await?.get(&logical), Some(&event));
    assert_eq!(
        b.latest_admissions()
            .await?
            .get(&logical)
            .context("other admission")?
            .status,
        AdmissionStatus::Admitted
    );
    Ok(())
}

async fn pad_subject_json(store: &EvalStore, bytes: i64) -> Result<()> {
    store.db.execute(sea_orm::Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Sqlite,
        "UPDATE eval_subjects SET subject_json = subject_json || printf('%*s', ?, '') WHERE eval_id = ?",
        [bytes.into(), store.key(KeyDomain::EvalSubject, "eval-1")?.into()],
    )).await?;
    Ok(())
}

#[tokio::test]
async fn compiler_load_reads_closure_once_and_bounds_manifest_plus_records() -> Result<()> {
    use crate::core::eval::compiler::EvalEvidenceSnapshot;

    let mut db = database().await?;
    let queries = query_log(&mut db);
    let a = tenant(&db, "installation", "a", "first")?;
    admitted(&a).await?;
    let snapshot = a.freeze_snapshot(FROZEN).await?;
    pad_subject_json(&a, 18 * 1024 * 1024).await?;
    queries.try_iter().for_each(drop);

    // One closure fits. Loading it twice would transfer over 36 MiB, even
    // though independent 32 MiB budgets would each permit the operation.
    let loaded = EvalEvidenceSnapshot::load(&a, &snapshot.evidence_root).await?;
    assert_eq!(loaded.evidence_root, snapshot.evidence_root);
    assert_eq!(loaded.records.len(), 1);
    assert_eq!(loaded.records[0].subject, subject()?);
    let reads = queries
        .try_iter()
        .filter(|sql| sql.starts_with("select "))
        .collect::<Vec<_>>();
    for table in ["eval_snapshots", "eval_results", "eval_subjects"] {
        let payloads = reads
            .iter()
            .filter(|sql| sql.contains(&format!("from {table}")) && !sql.contains("row_count"))
            .count();
        assert_eq!(payloads, 1, "repeated {table} payload: {reads:?}");
    }

    // Preserve the exact semantic root while making the stored manifest and
    // referenced subject individually fit but jointly exceed the read budget.
    db.execute(sea_orm::Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Sqlite,
        "UPDATE eval_snapshots SET manifest_json = manifest_json || printf('%*s', ?, '') WHERE evidence_root = ?",
        [(18_i64 * 1024 * 1024).into(), a.key(KeyDomain::EvalSnapshot, &snapshot.evidence_root)?.into()],
    )).await?;
    let tx = a.transaction(false).await?;
    let row = io::one(
        &tx,
        a.snapshots().filter(
            snapshot_entity::Column::EvidenceRoot
                .eq(a.key(KeyDomain::EvalSnapshot, &snapshot.evidence_root)?),
        ),
        &mut Budget::new(&a),
    )
    .await?
    .context("stored snapshot")?;
    assert_eq!(a.decode_snapshot(&row)?, snapshot);
    tx.commit().await?;
    assert_eq!(
        EvalEvidenceSnapshot::from_manifest(&a, snapshot.clone()).await?,
        loaded
    );
    let error = EvalEvidenceSnapshot::load(&a, &snapshot.evidence_root)
        .await
        .err()
        .context("compiler load reset its budget")?;
    assert!(error.to_string().contains("byte bound"));
    let error = a
        .persist_snapshot(&snapshot)
        .await
        .err()
        .context("persist reset its budget")?;
    assert!(error.to_string().contains("byte bound"));
    Ok(())
}

#[tokio::test]
async fn freeze_shares_materialization_and_persistence_budget() -> Result<()> {
    let db = database().await?;
    let a = tenant(&db, "installation", "a", "first")?;
    admitted(&a).await?;
    let original = a.freeze_snapshot(FROZEN).await?;
    pad_subject_json(&a, 12 * 1024 * 1024).await?;
    assert_eq!(a.materialize_snapshot(FROZEN).await?, original);
    assert_eq!(a.persist_snapshot(&original).await?, original);
    // Materialization reads the subject twice; persistence reads it once.
    // Each separate operation fits, but freeze must reject their combined work.
    let next_time = "2026-10-25T00:01:00Z";
    let next_root = a.snapshot_root(None, next_time, &original.entries)?;
    let error = a
        .freeze_snapshot(next_time)
        .await
        .err()
        .context("freeze reset its budget")?;
    assert!(error.to_string().contains("byte bound"));
    assert!(a.snapshot_by_root(&next_root).await?.is_none());
    assert_eq!(
        a.snapshot_by_root(&original.evidence_root).await?,
        Some(original)
    );
    Ok(())
}

#[tokio::test]
async fn compiler_load_preserves_legacy_wildcard_owned_and_empty_roots() -> Result<()> {
    use crate::core::eval::compiler::EvalEvidenceSnapshot;

    let mut db = database().await?;
    let queries = query_log(&mut db);
    let legacy = EvalStore::new(db);
    admitted(&legacy).await?;
    let snapshots = [
        legacy.freeze_snapshot(FROZEN).await?,
        legacy.freeze_snapshot_for_owner(FROZEN, "local").await?,
        legacy
            .freeze_snapshot_for_owner(FROZEN, "empty-owner")
            .await?,
    ];
    for snapshot in snapshots {
        queries.try_iter().for_each(drop);
        let loaded = EvalEvidenceSnapshot::load(&legacy, &snapshot.evidence_root).await?;
        assert_eq!(loaded.evidence_root, snapshot.evidence_root);
        assert_eq!(loaded.records.len(), snapshot.entries.len());
        assert_eq!(
            queries
                .try_iter()
                .filter(|sql| { sql.starts_with("select ") && sql.contains("from eval_snapshots") })
                .count(),
            1
        );
        assert_eq!(
            EvalEvidenceSnapshot::from_manifest(&legacy, snapshot).await?,
            loaded
        );
    }
    Ok(())
}

#[tokio::test]
async fn compiler_closure_has_one_cumulative_byte_budget() -> Result<()> {
    let db = database().await?;
    let a = tenant(&db, "installation", "a", "first")?;
    for id in ["one", "two"] {
        let mut item = subject()?;
        item.eval_id = id.into();
        a.insert_subject(&item).await?;
        let mut verdict = result(EvalVerdict::Pass);
        verdict.eval_id = id.into();
        verdict.idempotency_key = id.into();
        let stored = a.insert_result(&verdict).await?;
        a.append_admission_event(
            stored.result_id(),
            AdmissionStatus::Admitted,
            "accepted",
            "evaluator",
        )
        .await?;
    }
    let snapshot = a.freeze_snapshot(FROZEN).await?;
    // Each record remains parseable, under the point-read cap and semantically
    // unchanged; only the combined compiler closure exceeds its byte budget.
    for id in ["one", "two"] {
        db.execute(sea_orm::Statement::from_sql_and_values(sea_orm::DatabaseBackend::Sqlite,
            "UPDATE eval_subjects SET subject_json = subject_json || printf('%*s', ?, '') WHERE eval_id = ?",
            [(18_i64 * 1024 * 1024).into(), a.key(KeyDomain::EvalSubject, id)?.into()])).await?;
        assert!(a.subject(id).await?.is_some());
    }
    let error = crate::core::eval::compiler::EvalEvidenceSnapshot::from_manifest(&a, snapshot)
        .await
        .err()
        .context("compiler accepted oversized closure")?;
    assert!(error.to_string().contains("byte bound"));
    Ok(())
}

#[tokio::test]
async fn concurrent_identical_creators_replay_one_immutable_winner() -> Result<()> {
    let db = database().await?;
    let first = tenant(&db, "installation", "a", "first")?;
    let second = tenant(&db, "installation", "a", "second")?;
    let value = subject()?;
    let (a, b) = tokio::join!(first.insert_subject(&value), second.insert_subject(&value));
    assert!(matches!(
        (a?, b?),
        (
            SubjectInsertOutcome::Inserted,
            SubjectInsertOutcome::Duplicate
        ) | (
            SubjectInsertOutcome::Duplicate,
            SubjectInsertOutcome::Inserted
        )
    ));
    let key = first.key(KeyDomain::EvalSubject, &value.eval_id)?;
    let original = subject_entity::Entity::find_by_id(&key)
        .one(&db)
        .await?
        .context("winner")?;
    first.insert_subject(&value).await?;
    second.insert_subject(&value).await?;
    assert_eq!(
        subject_entity::Entity::find_by_id(key).one(&db).await?,
        Some(original)
    );
    Ok(())
}

#[tokio::test]
async fn snapshot_transaction_keeps_admissions_stable_across_other_writer() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!("sqlite://{}/eval.db?mode=rwc", directory.path().display());
    let left_db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&left_db).await?;
    let right_db = crate::db::connect(&url).await?;
    let left = tenant(&left_db, "installation", "a", "first")?;
    let right = tenant(&right_db, "installation", "a", "second")?;
    let logical = admitted(&left).await?;
    let tx = left.transaction(false).await?;
    let before = left.latest_in(&tx, None, &mut Budget::new(&left)).await?;
    right
        .append_admission_event(
            &logical,
            AdmissionStatus::Disputed,
            "new version",
            "evaluator",
        )
        .await?;
    let after = left.latest_in(&tx, None, &mut Budget::new(&left)).await?;
    assert_eq!(before, after);
    assert_eq!(
        after.get(&logical).context("original admission")?.status,
        AdmissionStatus::Admitted
    );
    tx.commit().await?;
    assert_eq!(
        left.latest_admissions()
            .await?
            .get(&logical)
            .context("new admission")?
            .status,
        AdmissionStatus::Disputed
    );
    left_db.close().await?;
    right_db.close().await?;
    Ok(())
}

#[tokio::test]
async fn metadata_shape_is_validated_on_every_eval_record_family() -> Result<()> {
    let db = database().await?;
    let a = tenant(&db, "installation", "a", "first")?;
    let logical = admitted(&a).await?;
    let snapshot = a.freeze_snapshot(FROZEN).await?;
    for table in [
        "eval_subjects",
        "eval_results",
        "eval_admission_events",
        "eval_snapshots",
    ] {
        let statement = sea_orm::sea_query::Query::update()
            .table(sea_orm::sea_query::Alias::new(table))
            .value(
                sea_orm::sea_query::Alias::new("creator_principal_id"),
                Option::<String>::None,
            )
            .cond_where(a.scope.predicate(
                sea_orm::sea_query::Alias::new("installation_id"),
                sea_orm::sea_query::Alias::new("tenant_id"),
            ))
            .to_owned();
        db.execute(db.get_database_backend().build(&statement))
            .await?;
        let rejected = match table {
            "eval_subjects" => a.subject("eval-1").await.is_err(),
            "eval_results" => a.result(&logical).await.is_err(),
            "eval_admission_events" => a.latest_admissions().await.is_err(),
            "eval_snapshots" => a.snapshot_by_root(&snapshot.evidence_root).await.is_err(),
            _ => anyhow::bail!("unexpected fixture table"),
        };
        assert!(rejected, "accepted missing creator for {table}");
        let statement = sea_orm::sea_query::Query::update()
            .table(sea_orm::sea_query::Alias::new(table))
            .value(
                sea_orm::sea_query::Alias::new("creator_principal_id"),
                "first",
            )
            .cond_where(a.scope.predicate(
                sea_orm::sea_query::Alias::new("installation_id"),
                sea_orm::sea_query::Alias::new("tenant_id"),
            ))
            .to_owned();
        db.execute(db.get_database_backend().build(&statement))
            .await?;
    }
    // A partial namespace never falls back into the Legacy reader.
    subject_entity::Entity::update_many()
        .col_expr(
            subject_entity::Column::InstallationId,
            Expr::value(Option::<String>::None),
        )
        .filter(a.scope.predicate(
            subject_entity::Column::InstallationId,
            subject_entity::Column::TenantId,
        ))
        .exec(&db)
        .await?;
    assert!(a.subject("eval-1").await?.is_none());
    assert!(EvalStore::new(db).list_subjects().await?.is_empty());
    Ok(())
}
