//! Response limits and latest-state reads have independent work bounds.
use super::*;

#[tokio::test]
async fn oversized_owned_list_rolls_back_without_truncating_or_poisoning_reads() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let store = fixture().await?;
        let tx = store.transaction(false).await?;
        for index in 0..10 {
            let mut subject = tests::subject()?;
            subject.eval_id = format!("large-{index}");
            subject.cohort = Some("x".repeat(900 * 1024));
            assert!(serde_json::to_vec(&subject)?.len() < 1024 * 1024);
            store.put_owned_in(&tx, &subject, &|| Ok(())).await?;
        }
        tx.commit().await?;
        let tx = store.transaction(false).await?;
        assert!(matches!(
            store.subjects_owned_in(&tx, &|| Ok(())).await,
            Err(OwnedError::Rejected(ErrorCode::CapacityExceeded))
        ));
        tx.rollback().await?;
        // The bounded status response still sees the complete collection; the
        // rejected list neither deletes records nor returns a truncated success.
        let tx = store.transaction(false).await?;
        let status = store.status_owned_in(&tx, &|| Ok(())).await?;
        assert_eq!(status.subjects, 11);
        assert_eq!(status.results, 0);
        assert!(status.admission.is_empty());
        assert_eq!(
            store
                .subject_owned_in(&tx, "large-9", &|| Ok(()))
                .await?
                .cohort
                .as_ref()
                .map(String::len),
            Some(900 * 1024)
        );
        tx.rollback().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn owned_replay_and_freeze_seek_latest_after_more_than_4096_events() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let store = fixture().await?;
        let input = result(EvalVerdict::Pass, "long-history");
        let tx = store.transaction(false).await?;
        let first = store.submit_owned_in(&tx, &input, &|| Ok(())).await?;
        tx.commit().await?;
        let physical = store.key(KeyDomain::EvalResult, &first.result_id)?;
        // Populate valid append-only history without thousands of fixture
        // round trips. Each row retains the original namespace and provenance.
        store.db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "WITH RECURSIVE history(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM history WHERE n < 4096) INSERT INTO eval_admission_events (result_id, status, reason, authority_id, created_at, installation_id, tenant_id, creator_principal_id) SELECT original.result_id, 'disputed', 'historical dispute', original.authority_id, original.created_at, original.installation_id, original.tenant_id, original.creator_principal_id FROM eval_admission_events AS original CROSS JOIN history WHERE original.result_id = ?",
            [physical.clone().into()],
        )).await?;
        let count = store.db.query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS total FROM eval_admission_events WHERE result_id = ?",
            [physical.into()],
        )).await?.context("history count")?;
        assert_eq!(count.try_get::<i64>("", "total")?, 4097);
        let tx = store.transaction(false).await?;
        let replay = store.submit_owned_in(&tx, &input, &|| Ok(())).await?;
        assert!(replay.duplicate);
        assert_eq!(replay.result_id, first.result_id);
        assert_eq!(replay.status, AdmissionStatus::Disputed);
        assert_eq!(replay.reason, "historical dispute");
        let frozen = store.freeze_owned_in(&tx, "2026-10-25T00:00:00Z", &|| Ok(())).await?;
        assert!(frozen.entries.is_empty());
        let status = store.status_owned_in(&tx, &|| Ok(())).await?;
        assert_eq!(status.results, 1);
        assert_eq!(status.admission.get("disputed"), Some(&1));
        tx.rollback().await?;
        Ok::<_, anyhow::Error>(())
    }).await?
}
