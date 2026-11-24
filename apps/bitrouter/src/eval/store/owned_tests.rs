//! Atomic borrowed-transaction tests; actor custody is qualified separately.
use super::*;
use bitrouter_ai::eval::{EvalVerdict, EvaluatorKind};
use sea_orm::{DatabaseBackend, Statement};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn scoped(db: &DatabaseConnection) -> Result<EvalStore> {
    let (scope, provenance) = Scope::for_test(
        "installation".try_into()?,
        uuid::Uuid::new_v4().to_string().try_into()?,
        "principal".try_into()?,
    );
    Ok(EvalStore {
        db: db.clone(),
        scope,
        provenance,
        storage_budget: None,
    })
}

async fn fixture() -> Result<EvalStore> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = scoped(&db)?;
    store.insert_subject(&store.subject_fixture()?).await?;
    Ok(store)
}

fn result(verdict: EvalVerdict, key: &str) -> EvaluationResult {
    let mut result = tests::result(verdict);
    result.evaluator.kind = EvaluatorKind::Generic;
    result.evaluator.authority_id = "principal".into();
    result.idempotency_key = key.into();
    result
}

#[tokio::test]
async fn selected_subject_checks_format_and_namespace_without_reclassifying_sql_errors()
-> Result<()> {
    use bitrouter_ai::eval::EvalSubjectIdentity;
    let store = fixture().await?;
    let original = store.subject_fixture()?;
    for change in [
        "profile",
        "installation",
        "tenant",
        "missing-version",
        "old-version",
    ] {
        let mut subject = original.clone();
        match change {
            "profile" => subject.identity = EvalSubjectIdentity::GatewayGeneric {},
            "installation" => {
                let EvalSubjectIdentity::TenantGeneric {
                    installation_id, ..
                } = &mut subject.identity
                else {
                    anyhow::bail!("owned subject fixture profile");
                };
                *installation_id = "other-installation".try_into()?;
            }
            "tenant" => {
                let EvalSubjectIdentity::TenantGeneric { tenant_id, .. } = &mut subject.identity
                else {
                    anyhow::bail!("owned subject fixture profile");
                };
                *tenant_id = "other-tenant".try_into()?;
            }
            _ => {}
        }
        let mut json = serde_json::to_value(&subject)?;
        if change == "missing-version" {
            json.as_object_mut()
                .context("subject object")?
                .remove("spec_version");
        } else if change == "old-version" {
            json["spec_version"] = "eval/1.0.0-beta.0".into();
        }
        let tx = store.transaction(false).await?;
        tx.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "UPDATE eval_subjects SET subject_json=?,content_digest=?,policy_digest=? WHERE eval_id=?",
            [serde_json::to_string(&json)?.into(),canonical_digest(&subject)?.into(),
                crate::storage::evaluation::identity_digest(&subject.identity)?.into(),
                store.key(KeyDomain::EvalSubject,&original.eval_id)?.into()]
        )).await?;
        let row = subject_entity::Entity::find_by_id(
            store.key(KeyDomain::EvalSubject, &original.eval_id)?,
        )
        .one(&tx)
        .await?
        .context("selected malformed subject fixture")?;
        let malformed = store
            .decode_subject(&row)
            .err()
            .context("decoder accepted malformed subject")?;
        ensure!(
            matches!(OwnedError::classify(malformed), OwnedError::Integrity(_)),
            "decoder content failure lost its integrity classification"
        );
        let selected = store
            .subject_owned_in(&tx, &original.eval_id, &|| Ok(()))
            .await;
        tx.rollback().await?;
        ensure!(
            matches!(selected, Err(OwnedError::Integrity(_))),
            "malformed selected subject was accepted or misclassified: {change}"
        );
    }
    let tx = store.transaction(false).await?;
    tx.execute_unprepared("ALTER TABLE eval_subjects RENAME TO fixture_unreadable_eval_subjects")
        .await?;
    let selected = store
        .subject_owned_in(&tx, &original.eval_id, &|| Ok(()))
        .await;
    tx.rollback().await?;
    ensure!(
        matches!(selected, Err(OwnedError::Storage(_))),
        "raw SQL fault lost its storage classification"
    );
    let tx = store.transaction(false).await?;
    ensure!(
        store
            .subject_owned_in(&tx, &original.eval_id, &|| Ok(()))
            .await?
            == original
    );
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn owned_submission_and_freeze_use_atomic_closure_and_exact_replay() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let store = fixture().await?;
        let tx = store.transaction(false).await?;
        let pass = store
            .submit_owned_in(&tx, &result(EvalVerdict::Pass, "pass"), &|| Ok(()))
            .await?;
        tx.commit().await?;
        let tx = store.transaction(false).await?;
        let frozen = store
            .freeze_owned_in(&tx, "2026-10-25T00:00:00Z", &|| Ok(()))
            .await?;
        tx.commit().await?;
        assert_eq!(frozen.entries.len(), 1);
        let tx = store.transaction(false).await?;
        let failed = store
            .submit_owned_in(&tx, &result(EvalVerdict::Fail, "fail"), &|| Ok(()))
            .await?;
        tx.commit().await?;
        assert_eq!(failed.status, AdmissionStatus::Disputed);
        assert_eq!(
            store
                .latest_admissions()
                .await?
                .get(&pass.result_id)
                .context("prior result")?
                .status,
            AdmissionStatus::Disputed
        );
        let before = store.admissions().all(&store.db).await?;
        let tx = store.transaction(false).await?;
        let replay = store
            .submit_owned_in(&tx, &result(EvalVerdict::Pass, "pass"), &|| Ok(()))
            .await?;
        tx.rollback().await?;
        assert!(replay.duplicate);
        assert_eq!(replay.status, AdmissionStatus::Disputed);
        assert_eq!(store.admissions().all(&store.db).await?, before);
        assert_eq!(
            store.snapshot_by_root(&frozen.evidence_root).await?,
            Some(frozen)
        );
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn final_admission_failure_rolls_back_result_and_prior_dispute() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let store = fixture().await?;
        let tx = store.transaction(false).await?;
        let pass = store.submit_owned_in(&tx, &result(EvalVerdict::Pass, "pass"), &|| Ok(())).await?;
        tx.commit().await?;
        let failed = result(EvalVerdict::Fail, "fail");
        let key = store.key(KeyDomain::EvalResult, &crate::storage::evaluation::result_digest(&failed)?)?;
        // Digest keys contain only hexadecimal bytes, never user text.
        assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()));
        store.db.execute(Statement::from_string(DatabaseBackend::Sqlite, format!("CREATE TRIGGER reject_terminal BEFORE INSERT ON eval_admission_events WHEN NEW.result_id = '{key}' BEGIN SELECT RAISE(ABORT, 'fixture terminal failure'); END"))).await?;
        let before = store.admissions().all(&store.db).await?;
        let tx = store.transaction(false).await?;
        assert!(matches!(store.submit_owned_in(&tx, &failed, &|| Ok(())).await, Err(OwnedError::Storage(_))));
        tx.rollback().await?;
        assert_eq!(store.admissions().all(&store.db).await?, before);
        assert_eq!(store.results_for_subject("eval-1").await?.len(), 1);
        assert_eq!(store.latest_admissions().await?.get(&pass.result_id).context("prior")?.status, AdmissionStatus::Admitted);
        Ok::<_, anyhow::Error>(())
    }).await?
}

#[tokio::test]
async fn checkpoint_between_preflight_and_payload_prevents_decode() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let store = fixture().await?;
        // Invalid JSON would produce Integrity if the payload were decoded.
        store
            .db
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE eval_subjects SET subject_json = ? WHERE eval_id = ?",
                [
                    "not json".into(),
                    store.key(KeyDomain::EvalSubject, "eval-1")?.into(),
                ],
            ))
            .await?;
        let calls = AtomicUsize::new(0);
        let check = || {
            if calls.fetch_add(1, Ordering::SeqCst) >= 2 {
                Err(ErrorCode::Forbidden)
            } else {
                Ok(())
            }
        };
        let tx = store.transaction(false).await?;
        assert!(matches!(
            store.subject_owned_in(&tx, "eval-1", &check).await,
            Err(OwnedError::Rejected(ErrorCode::Forbidden))
        ));
        tx.rollback().await?;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn owned_freeze_has_one_cumulative_budget_across_subject_closure() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let store = fixture().await?;
        for id in ["one", "two"] {
            let mut subject = store.subject_fixture()?;
            subject.eval_id = id.into();
            store.insert_subject(&subject).await?;
            let mut value = result(EvalVerdict::Pass, id);
            value.eval_id = id.into();
            let tx = store.transaction(false).await?;
            store.submit_owned_in(&tx, &value, &|| Ok(())).await?;
            tx.commit().await?;
        }
        let tx = store.transaction(false).await?;
        let prior = store.freeze_owned_in(&tx, "2026-10-25T00:00:00Z", &|| Ok(())).await?;
        tx.commit().await?;
        for id in ["one", "two"] {
            store.db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "UPDATE eval_subjects SET subject_json = subject_json || printf('%*s', ?, '') WHERE eval_id = ?",
                [(18_i64 * 1024 * 1024).into(), store.key(KeyDomain::EvalSubject, id)?.into()])).await?;
            assert!(store.subject(id).await?.is_some());
        }
        let tx = store.transaction(false).await?;
        assert!(matches!(store.freeze_owned_in(&tx, "2026-10-25T00:00:01Z", &|| Ok(())).await, Err(OwnedError::Rejected(ErrorCode::CapacityExceeded))));
        tx.rollback().await?;
        let manifests = store.snapshots().all(&store.db).await?;
        assert_eq!(manifests.len(), 1);
        assert_eq!(store.decode_snapshot(&manifests[0])?, prior);
        Ok::<_, anyhow::Error>(())
    }).await?
}

mod bounds;
mod server;

#[tokio::test]
async fn owned_collection_limit_is_exact_and_does_not_truncate() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let store = fixture().await?;
        let mut models = Vec::new();
        for index in 0..1023 {
            let mut subject = store.subject_fixture()?;
            subject.eval_id = format!("bounded-{index}");
            let metadata = store.scope.metadata(&store.provenance)?;
            models.push(subject_entity::ActiveModel {
                installation_id: Set(metadata.installation),
                tenant_id: Set(metadata.tenant),
                creator_principal_id: Set(metadata.creator),
                eval_id: Set(store.key(KeyDomain::EvalSubject, &subject.eval_id)?),
                owner_user_id: Set(store.owner("local")?),
                subject_id: Set(subject.subject_id.clone()),
                scope: Set(scope_name(subject.scope).into()),
                identity_digest: Set(crate::storage::evaluation::identity_digest(
                    &subject.identity,
                )?),
                holdout: Set(subject.holdout),
                content_digest: Set(crate::storage::evaluation::subject_digest(&subject)?),
                subject_json: Set(serde_json::to_string(&subject)?),
                created_at: Set("2026-10-25T00:00:00Z".into()),
            });
        }
        subject_entity::Entity::insert_many(models)
            .exec(&store.db)
            .await?;
        let tx = store.transaction(false).await?;
        assert_eq!(store.subjects_owned_in(&tx, &|| Ok(())).await?.len(), 1024);
        tx.rollback().await?;
        let mut overflow = store.subject_fixture()?;
        overflow.eval_id = "overflow".into();
        store.insert_subject(&overflow).await?;
        let tx = store.transaction(false).await?;
        assert!(matches!(
            store.subjects_owned_in(&tx, &|| Ok(())).await,
            Err(OwnedError::Rejected(ErrorCode::CapacityExceeded))
        ));
        tx.rollback().await?;
        // The existing public-store profile keeps its independent 4,096 row cap.
        assert_eq!(store.list_subjects().await?.len(), 1025);
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
