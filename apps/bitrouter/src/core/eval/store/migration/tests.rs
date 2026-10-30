//! Actual Legacy producers supply the complete four-table graph; no owned grant is forged.
use super::*;
use bitrouter_ai::eval::EvalVerdict;
use sea_orm::{DatabaseBackend, Statement};

async fn typed(store: &EvalStore) -> Result<Tables> {
    let mut connection = store.db.get_sqlite_connection_pool().acquire().await?;
    sea_orm::sqlx::query("BEGIN")
        .execute(&mut *connection)
        .await?;
    let loaded = rows::load(&mut connection, &[37; 32]).await;
    let rollback = sea_orm::sqlx::query("ROLLBACK")
        .execute(&mut *connection)
        .await;
    rollback?;
    loaded
}

#[derive(Clone, Copy)]
enum Case {
    ExactPartition,
    DifferentScope,
    ReorderedScan,
    ForeignParent,
    MissingResult,
    MixedMetadata,
    ChangedPayload,
    Wildcard,
}

async fn check(case: Case) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("legacy.sqlite");
    let owner = crate::tenancy::ownership::acquire_path(&path, true)?;
    let db =
        crate::db::connect_tenancy(&format!("sqlite://{}", owner.path.display()), &owner).await?;
    crate::db::run_migrations(&db).await?;
    let store = EvalStore::new(db.clone());
    let checked = async {
        let mut exact = BTreeMap::new();
        for (ordinal, owner_name) in [(1, "alice"), (2, "ALICE")] {
            let mut subject = super::super::tests::subject()?;
            subject.eval_id = format!("evaluation-{ordinal}");
            subject.subject_id = format!("request-{ordinal}");
            store.insert_subject_owned(&subject, owner_name).await?;
            let mut result = super::super::tests::result(EvalVerdict::Pass);
            result.eval_id = subject.eval_id.clone();
            // Equal logical idempotency names in separate owners remain explicit;
            // no content/name inference merges them into one tenant.
            result.idempotency_key = format!("submission-{ordinal}");
            let result_id = store.insert_result_owned(&result, owner_name).await?.result_id().to_owned();
            store.append_admission_event(&result_id, AdmissionStatus::Admitted, "approved", "local").await?;
            let snapshot = store.freeze_snapshot_for_owner("2026-07-30T00:02:00+00:00", owner_name).await?;
            exact.insert(owner_name.to_owned(), (subject.eval_id, result_id, snapshot));
        }
        if matches!(case, Case::Wildcard) { store.freeze_snapshot("2026-07-30T00:03:00Z").await?; }
        if matches!(case, Case::MixedMetadata) {
            store.db.execute(Statement::from_string(DatabaseBackend::Sqlite,
                "UPDATE eval_snapshots SET installation_id='source' WHERE owner_user_id='ALICE'".to_owned())).await?;
            ensure!(typed(&store).await.is_err(), "last-table mixed metadata accepted");
            return Ok(());
        }
        let installation: InstallationId = "source-installation".try_into()?;
        let mut input = typed(&store).await?;
        match case {
            Case::ForeignParent => {
                let row = input.rows[1].first_mut().context("result row")?;
                row.cells[4] = rows::Cell::Text("foreign-owner".into());
                row.digest = rows::digest(rows::RESULTS, &row.cells, &[37; 32])?;
                ensure!(Graph::decode(input, &[37;32], &installation, "local_user").is_err());
                return Ok(());
            }
            Case::MissingResult => {
                input.rows[1].pop().context("retained result")?;
                input.row_count -= 1;
                ensure!(Graph::decode(input, &[37;32], &installation, "local_user").is_err());
                return Ok(());
            }
            Case::ChangedPayload => {
                let row = input.rows[3].first_mut().context("snapshot row")?;
                let json = row.text(5)?.replace("00:02:00+00:00", "00:02:01+00:00");
                ensure!(json != row.text(5)?);
                row.cells[5] = rows::Cell::Text(json);
                row.digest = rows::digest(rows::SNAPSHOTS, &row.cells, &[37;32])?;
                ensure!(Graph::decode(input, &[37;32], &installation, "local_user").is_err());
                return Ok(());
            }
            _ => {}
        }
        let graph = Graph::decode(input, &[37;32], &installation, "local_user")?;
        ensure!(graph.partitions().map(|p|p.source_count).sum::<u64>() == graph.rows as u64);
        for name in ["alice", "ALICE"] {
            let partition = graph.partitions.get(&Some(name.into())).context("exact owner partition")?;
            ensure!(partition.table_counts == [1,1,1,1] && partition.source_count == 4);
            let (subject, result, snapshot) = exact.get(name).context("original producer records")?;
            let node = graph.snapshots.iter().find(|node|node.owner.as_deref()==Some(name)).context("snapshot partition")?;
            ensure!(node.model.evidence_root == snapshot.evidence_root && node.row.text(3)? == snapshot.evidence_root);
            ensure!(serde_json::from_str::<EvalSnapshot>(node.row.text(5)?)? == *snapshot);
            ensure!(graph.subjects.iter().any(|n|n.model.eval_id==*subject && n.owner.as_deref()==Some(name)));
            ensure!(graph.results.iter().any(|n|n.model.result_id==*result && n.owner.as_deref()==Some(name)));
        }
        let summary = |g: &Graph| -> Result<Vec<u8>> {Ok(serde_json::to_vec(&g.partitions().collect::<Vec<_>>())?)};
        match case {
            Case::ExactPartition => {ensure!(graph.rows==8 && graph.partitions.len()==2);}
            Case::DifferentScope => {
                let other = Graph::decode(typed(&store).await?, &[37;32], &"different-installation".try_into()?, "local_user")?;
                let other_kind = Graph::decode(typed(&store).await?, &[37;32], &installation, "native_owner")?;
                ensure!(summary(&graph)? != summary(&other)? && summary(&graph)? != summary(&other_kind)?);
            }
            Case::ReorderedScan => {
                let mut shuffled = typed(&store).await?;
                for table in &mut shuffled.rows {table.reverse();}
                let replay = Graph::decode(shuffled, &[37;32], &installation, "local_user")?;
                ensure!(summary(&graph)?==summary(&replay)?);
            }
            Case::Wildcard => {
                let unresolved = graph.partitions.get(&None).context("wildcard must be externally quarantined")?;
                ensure!(unresolved.table_counts==[0,0,0,1] && graph.rows==9);
            }
            _ => anyhow::bail!("unhandled fixture case"),
        }
        Ok::<_, anyhow::Error>(())
    }.await;
    drop(store);
    let close = db.close().await;
    drop(owner);
    let lease = crate::tenancy::ownership::acquire_path(&path, false);
    close?;
    drop(lease?);
    checked
}
#[tokio::test]
async fn actual_four_table_closure_preserves_logical_ids_and_original_snapshot_bytes() -> Result<()>
{
    check(Case::ExactPartition).await
}
#[tokio::test]
async fn owner_partition_fingerprints_bind_installation_and_explicit_namespace() -> Result<()> {
    check(Case::DifferentScope).await
}
#[tokio::test]
async fn exact_typed_partition_does_not_depend_on_scan_order() -> Result<()> {
    check(Case::ReorderedScan).await
}
#[tokio::test]
async fn foreign_parent_and_missing_result_refuse_the_whole_graph() -> Result<()> {
    check(Case::ForeignParent).await?;
    check(Case::MissingResult).await
}
#[tokio::test]
async fn last_table_mixed_metadata_and_changed_snapshot_refuse_whole_graph() -> Result<()> {
    check(Case::MixedMetadata).await?;
    check(Case::ChangedPayload).await
}
#[tokio::test]
async fn wildcard_snapshot_remains_an_explicit_unresolved_partition() -> Result<()> {
    check(Case::Wildcard).await
}

#[tokio::test]
async fn repeated_real_snapshot_parent_bytes_refuse_before_import() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("reader-budget.sqlite");
    let owner = crate::tenancy::ownership::acquire_path(&path, true)?;
    let db =
        crate::db::connect_tenancy(&format!("sqlite://{}", owner.path.display()), &owner).await?;
    crate::db::run_migrations(&db).await?;
    let store = EvalStore::new(db.clone());
    let checked = async {
        let subject = super::super::tests::subject()?;
        store.insert_subject_owned(&subject, "source-owner").await?;
        for n in 0..33 {
            let mut result = super::super::tests::result(EvalVerdict::Pass);
            result.idempotency_key = format!("budget-{n}");
            let result = store.insert_result_owned(&result, "source-owner").await?;
            store
                .append_admission_event(
                    result.result_id(),
                    AdmissionStatus::Admitted,
                    "original",
                    "local",
                )
                .await?;
        }
        store
            .freeze_snapshot_for_owner("2026-08-01T00:00:00Z", "source-owner")
            .await?;
        // Valid historical JSON spelling is a real stored byte charge each time
        // the ordinary snapshot reader loads this shared parent.
        let mut json = serde_json::to_string(&subject)?;
        ensure!(json.len() < 1024 * 1024);
        json.push_str(&" ".repeat(1024 * 1024 - json.len()));
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE eval_subjects SET subject_json=? WHERE eval_id=?",
            [json.into(), subject.eval_id.into()],
        ))
        .await?;
        let installation: InstallationId = "source-installation".try_into()?;
        let graph = Graph::decode(typed(&store).await?, &[37; 32], &installation, "local_user")?;
        ensure!(graph.rows < rows::MAX_ROWS && graph.bytes < rows::MAX_BYTES);
        let assignments = BTreeMap::from([(
            "source-owner".into(),
            Assignment {
                tenant: "target".try_into()?,
                principal: "creator".try_into()?,
            },
        )]);
        let error = graph
            .transfer(
                &"target-installation".try_into()?,
                &assignments,
                transfer::Anchors {
                    source: &installation,
                    kind: "local_user",
                    mapping: &"a".repeat(64),
                    database: &"b".repeat(64),
                    ownership: &"c".repeat(64),
                },
            )
            .err()
            .context("oversized normal-reader closure accepted")?;
        ensure!(
            error.to_string() == "import snapshot exceeds owned reader byte bound",
            "wrong refusal: {error:#}"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drop(store);
    let close = db.close().await;
    drop(owner);
    close?;
    drop(crate::tenancy::ownership::acquire_path(&path, false)?);
    checked
}
