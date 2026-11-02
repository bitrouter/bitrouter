//! Complete historical credential scans and indexed intersections beyond one million rows.

use super::*;
use crate::offline::{
    engine::Connections,
    large,
    recovery::{
        plan::Target,
        tests::fixture::scale::{JsonLines, observation::Observation, witness},
        v2::{format::ProvisioningRecord, inputs::Prepared},
    },
};
use bitrouter_ai::tenancy::{
    authority::{CredentialCreate, GrantScope, Permission},
    identity::PrincipalId,
    operation::OperationStatus,
};
use sea_orm::sqlx::{
    ConnectOptions, Connection, Row, SqliteConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode},
};
use sha2::{Digest, Sha256};

const ROWS: i64 = 1_000_001;
const LAST: &str = "zz-historical-0001000000";
const FLOOR: u64 = 2 * 1024 * 1024 * 1024;

fn working(home: &Path) -> large::Options {
    large::Options {
        working_bytes: 64 * 1024 * 1024,
        scratch_directory: home.into(),
        min_free_bytes: FLOOR,
    }
}

async fn writer(
    path: &Path,
    cancel: &CancellationToken,
) -> Result<(crate::tenancy::ownership::Lease, SqliteConnection)> {
    let owner = crate::tenancy::ownership::acquire_path(path, false)?;
    let connection = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Delete)
        .row_buffer_size(1)
        .statement_cache_capacity(1)
        .pragma("cache_size", "-2048")
        .pragma("temp_store", "FILE")
        .pragma("mmap_size", "0")
        .disable_statement_logging()
        .connect()
        .await;
    let mut db = match connection {
        Ok(db) => db,
        Err(error) => {
            std::mem::forget(owner);
            return Err(error.into());
        }
    };
    let stop = cancel.clone();
    let installed = async {
        db.lock_handle()
            .await?
            .set_progress_handler(10_000, move || !stop.is_cancelled());
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = installed {
        close_writer(owner, db).await?;
        return Err(error);
    }
    Ok((owner, db))
}

async fn close_writer(owner: crate::tenancy::ownership::Lease, db: SqliteConnection) -> Result<()> {
    if let Err(error) = db.close().await {
        std::mem::forget(owner);
        return Err(error.into());
    }
    let path = owner.path.clone();
    let clean = large::Source::inspect(&path);
    drop(owner);
    drop(crate::tenancy::ownership::acquire_path(&path, false)?);
    clean?;
    Ok(())
}

async fn seed_source(path: &Path, cancel: &CancellationToken) -> Result<()> {
    let (owner, mut db) = writer(path, cancel).await?;
    let work = async {
        let template: String = sea_orm::sqlx::query_scalar("SELECT id FROM tenancy_credentials LIMIT 1")
            .fetch_one(&mut db).await?;
        let initial: i64 = sea_orm::sqlx::query_scalar("SELECT COUNT(*) FROM tenancy_credentials")
            .fetch_one(&mut db).await?;
        ensure!(initial == 1, "historical fixture must begin with one real credential");
        for (batch, first) in (1_i64..ROWS).step_by(128).enumerate() {
            ensure!(!cancel.is_cancelled(), "credential fixture cancelled");
            let end = (first + 128).min(ROWS);
            let mut tx = db.begin().await?;
            let inserted = async {
                // Each batch and bound parameter set is finite; the total is real rows.
                let mut sql = String::from("WITH seed(id,key_hash) AS (");
                for ordinal in first..end {
                    if ordinal != first { sql.push_str(" UNION ALL "); }
                    sql.push_str("SELECT ?,?");
                }
                sql.push_str(") INSERT INTO tenancy_credentials (id,tenant_id,principal_id,binding_id,revision,key_hash,state,audit_reader,permissions_json,scope_json,expires_at,created_at) SELECT seed.id,t.tenant_id,t.principal_id,t.binding_id,t.revision,seed.key_hash,'revoked',t.audit_reader,t.permissions_json,t.scope_json,t.expires_at,t.created_at FROM seed CROSS JOIN tenancy_credentials AS t WHERE t.id=?");
                let mut query = sea_orm::sqlx::query(&sql);
                for ordinal in first..end {
                    query = query.bind(format!("zz-historical-{ordinal:010}"))
                        .bind(hex::encode(Sha256::digest(format!("offline-source-credential-{ordinal}"))));
                }
                let changed = query.bind(&template).execute(&mut *tx).await?;
                ensure!(changed.rows_affected() == u64::try_from(end - first)?);
                Ok::<_, anyhow::Error>(())
            }.await;
            match inserted {
                Ok(()) => tx.commit().await?,
                Err(error) => { tx.rollback().await?; return Err(error); }
            }
            if batch.is_multiple_of(128) {
                large::probe(path.parent().context("source parent")?, FLOOR, 1024 * 1024)?;
            }
        }
        let count: i64 = sea_orm::sqlx::query_scalar("SELECT COUNT(*) FROM tenancy_credentials")
            .fetch_one(&mut db).await?;
        let preceding: i64 = sea_orm::sqlx::query_scalar("SELECT COUNT(*) FROM tenancy_credentials WHERE id<?")
            .bind(LAST).fetch_one(&mut db).await?;
        ensure!(count == ROWS && preceding == ROWS - 1, "source cutoff fixture is not real/last");
        let plan = sea_orm::sqlx::query("EXPLAIN QUERY PLAN SELECT id FROM tenancy_credentials WHERE key_hash=? LIMIT 1")
            .bind("0".repeat(64)).fetch_all(&mut db).await?;
        ensure!(plan.len() <= 8 && plan.iter().any(|row| row.try_get::<String,_>("detail")
            .is_ok_and(|detail| detail.contains("SEARCH") && detail.contains("INDEX") && detail.contains("key_hash"))),
            "historical hash lookup lost its index");
        eprintln!("credential source rows={count} late_predecessors={preceding} batch_rows=128");
        Ok::<_, anyhow::Error>(())
    }.await;
    let closed = close_writer(owner, db).await;
    closed?;
    work
}

async fn target(home: &Path, source: &producer::Provision) -> Result<Target> {
    let path = home.join("target.sqlite");
    let live = producer::Live::open(&path, None, Some(producer::KEY_ENV)).await?;
    let seeded = async {
        let mut provision = producer::provision(&live).await?;
        let mut output = JsonLines::create(&home.join("provisioning.jsonl"))?;
        output.record(&json!({"format":format::PROVISIONING,"installation_id":provision.installation,
            "operation_count":262,"configuration_count":1,"active_count":1,"resource_count":2}))?;
        for operation in &provision.operations {
            output.record(&json!({"kind":"operation","principal_id":operation["principal_id"],"result":operation["result"]}))?;
        }
        let client = live.operator()?;
        let caller = PrincipalId::for_control_credential("recovery-operator")?;
        let expiry = (Utc::now() + chrono::Duration::minutes(59)).to_rfc3339_opts(chrono::SecondsFormat::Secs,true);
        for ordinal in 1..257 {
            let issued = client.issue_credential(&"tenant".try_into()?, &CredentialCreate {
                operation_id: format!("credential-{ordinal:03}").as_str().try_into()?,
                expected_authority_revision: provision.authority,
                principal_id: "owner".try_into()?,
                permissions: vec![Permission::ResourceRead, Permission::ResourceDraft, Permission::PolicyActivate],
                scope: GrantScope::Tenant { operator_pools: vec![] },
                expires_at: expiry.as_str().try_into()?, rotate_credential_id: None,
                binding_id: "binding".try_into()?, installation_id: provision.installation.clone(),
            }).await?;
            let retained = client.operation(&provision.installation, &issued.operation_id).await?;
            ensure!(retained.status == OperationStatus::Applied && retained.authority_revision == Some(issued.authority_revision));
            provision.authority = issued.authority_revision;
            output.record(&ProvisioningRecord::Operation { principal_id: caller.clone(), result: retained })?;
        }
        Ok::<_, anyhow::Error>((provision,output))
    }.await;
    let closed = live.close(&path).await;
    closed?;
    let (mut provision, mut output) = seeded?;
    let manifest = producer::manifest(&provision.installation);
    let import = home.join("import.json");
    producer::private_json(&import, &manifest)?;
    let live = producer::Live::open(&path, Some(&import), Some(producer::KEY_ENV)).await?;
    let configured = producer::configure(&live, &mut provision, source).await;
    let closed = live.close(&path).await;
    closed?;
    let observed = configured?;
    for operation in &provision.operations[4..] {
        output.record(&json!({"kind":"operation","principal_id":operation["principal_id"],"result":operation["result"]}))?;
    }
    output.record(&json!({"kind":"configuration","revision":observed["configurations"][0]}))?;
    output.record(&json!({"kind":"active","tenant_id":"tenant","revision_id":observed["active"][0]["revision"],
        "authority_revision":observed["active"][0]["authority_revision"]}))?;
    let mut approved = JsonLines::create(&home.join("approval.jsonl"))?;
    approved.record(&json!({"format":format::APPROVAL,"installation_id":provision.installation,"resource_count":2}))?;
    for resource in manifest["resources"]
        .as_array()
        .context("static resources")?
    {
        output.record(&json!({"kind":"resource","reference":resource["reference"]}))?;
        approved.record(&json!({"kind":"resource","reference":resource["reference"],"definition":resource["definition"]}))?;
    }
    Ok(Target {
        database: path,
        installation_id: provision.installation,
        provisioning: output.finish()?.0,
        wrapping_key_env: producer::KEY_ENV.into(),
        static_manifest: approved.finish()?.0,
    })
}

async fn full_positive(
    home: &Path,
    source: &Path,
    installation: &InstallationId,
    target: &Target,
) -> Result<()> {
    let bundle = home.join("source-bundle");
    let evidence = crate::offline::snapshot_with_options(source, &bundle, working(home)).await?;
    let evidence = serde_json::to_value(&evidence as &dyn crate::output::CliReport)?;
    let commit = crate::artifact::current()
        .source_revision
        .context("producer revision")?;
    let executable = witness(&std::env::current_exe()?)?;
    let attestation = producer::private_json(
        &home.join("producer.json"),
        &json!({
        "format":"bitrouter.offline.source-producer-attestation.v1","installation_id":installation,
        "commit":commit,"artifact_sha256":executable.sha256,"database_evidence_digest":evidence["evidence_digest"]}),
    )?;
    let mut retired = JsonLines::create(&home.join("retirement.jsonl"))?;
    retired.record(&json!({"format":format::RETIREMENT,"source_installation_id":installation,
        "database_evidence_digest":evidence["evidence_digest"],"fence_maintained":true,"target_ingress_fenced":true,
        "selection_exhaustive":true,"unresolved_effects_remain_unknown":true,"writer_count":1,"selected_file_count":0}))?;
    retired.record(&json!({"kind":"writer","id":"source","process_identity":format!("stopped-Service:{}",std::process::id()),
        "restart_source":"none","retired":true}))?;
    let tables = evidence["evidence"]["inventory"]["tables"]
        .as_array()
        .context("inventory tables")?;
    ensure!(tables.len() <= 256);
    let tables = tables.iter().map(|table| json!({"table":table["name"],"rows":table["rows"],
        "value_bytes":table["value_bytes"],"digest":table["digest"],"disposition":"retained_archive"})).collect::<Vec<_>>();
    let plan = json!({"format":format::PLAN,"source":{"bundle":bundle,"evidence_digest":evidence["evidence_digest"],
        "installation_id":installation,"producer":{"commit":commit,"artifact_sha256":executable.sha256,"evidence":attestation},"tables":tables},
        "target":{"database":target.database,"installation_id":target.installation_id,"provisioning":target.provisioning,
        "wrapping_key_env":target.wrapping_key_env,"static_manifest":target.static_manifest},"retirement":retired.finish()?.0});
    let input = home.join("plan.json");
    producer::private_json(&input, &plan)?;
    let before_source = witness(source)?;
    let before_target = witness(&target.database)?;
    let output = home.join("positive");
    let result = crate::offline::recovery::run_with_options(
        &input,
        &output,
        ValidationBudget::default(),
        working(home),
    )
    .await;
    // The real adapter joins its SQL/file owner before reporting either outcome.
    drop(crate::tenancy::ownership::acquire_path(
        &target.database,
        false,
    )?);
    drop(crate::tenancy::ownership::acquire_path(source, false)?);
    ensure!(witness(source)? == before_source && witness(&target.database)? == before_target);
    let report = serde_json::to_value(&result? as &dyn crate::output::CliReport)?;
    ensure!(
        report["fresh_provisioning_verified"] == true
            && report["static_closure_authenticated"] == true
    );
    ensure!(
        report["target"]["ordinary_operations"] == 262
            && report["source"]["ordinary_operations"] == 4
    );
    ensure!(home.join("positive/VERIFIED").is_file() && !home.join("positive/INCOMPLETE").exists());
    Ok(())
}

async fn rollback(c: &mut Connections) -> Result<()> {
    let db = c.source.as_mut().context("read connection")?;
    db.lock_handle().await?.remove_progress_handler();
    sea_orm::sqlx::query("ROLLBACK").execute(&mut *db).await?;
    c.uncertain = false;
    Ok(())
}

async fn hashes(target: &Target, cancel: &CancellationToken, home: &Path) -> Result<[String; 3]> {
    let workspace = Workspace::create(&working(home), cancel.clone())?;
    let owner = crate::tenancy::ownership::acquire_path(&target.database, false)?;
    let mut c = Connections::new(cancel.clone(), Some(owner));
    let result = async {
        c.open_immutable_source_budgeted(&target.database, &workspace)
            .await?;
        let db = c.source.as_mut().context("target")?;
        let count: i64 = sea_orm::sqlx::query_scalar("SELECT COUNT(*) FROM tenancy_credentials")
            .fetch_one(&mut *db)
            .await?;
        ensure!(count == 257);
        let mut selected = [String::new(), String::new(), String::new()];
        for (slot, rank) in selected.iter_mut().zip([0_i64, 128, 256]) {
            *slot = sea_orm::sqlx::query_scalar(
                "SELECT key_hash FROM tenancy_credentials ORDER BY key_hash LIMIT 1 OFFSET ?",
            )
            .bind(rank)
            .fetch_one(&mut *db)
            .await?;
        }
        Ok::<_, anyhow::Error>(selected)
    }
    .await;
    let closed = c.close().await;
    closed?;
    drop(c);
    workspace.finish()?;
    drop(crate::tenancy::ownership::acquire_path(
        &target.database,
        false,
    )?);
    result
}

async fn late_match(
    source: &Path,
    hash: &str,
    expired: bool,
    cancel: &CancellationToken,
) -> Result<()> {
    let (owner, mut db) = writer(source, cancel).await?;
    let changed = sea_orm::sqlx::query("UPDATE tenancy_credentials SET key_hash=?,state=?,created_at='2020-01-01T00:00:00Z',expires_at=? WHERE id=?")
        .bind(hash).bind(if expired {"expired"} else {"revoked"})
        .bind(if expired {"2021-01-01T00:00:00Z"} else {"2099-01-01T00:00:00Z"}).bind(LAST).execute(&mut db).await;
    let closed = close_writer(owner, db).await;
    closed?;
    ensure!(
        changed?.rows_affected() == 1,
        "late historical mutation missed the retained row"
    );
    Ok(())
}

async fn rejected_source(
    home: &Path,
    source: &Path,
    installation: &InstallationId,
    target: &Target,
    cancel: &CancellationToken,
) -> Result<()> {
    let source_before = witness(source)?;
    let target_before = witness(&target.database)?;
    let source_witness = large::Source::inspect(source)?;
    let target_witness = large::Source::inspect(&target.database)?;
    let workspace = Workspace::create(&working(home), cancel.clone())?;
    let mut provisioning = Prepared::open(
        &target.provisioning,
        &[source.into(), target.database.clone()],
        &workspace,
    )?;
    let mut approval = Prepared::open(
        &target.static_manifest,
        &[source.into(), target.database.clone()],
        &workspace,
    )?;
    let mut t = Connections::new(
        cancel.clone(),
        Some(crate::tenancy::ownership::acquire_path(
            &target.database,
            false,
        )?),
    );
    let mut s = Connections::new(
        cancel.clone(),
        Some(crate::tenancy::ownership::acquire_path(source, false)?),
    );
    let result = async {
        t.validation_budget().await?;
        t.open_immutable_source_budgeted(&target.database, &workspace)
            .await?;
        t.uncertain = true;
        let db = t.source.as_mut().context("target connection")?;
        sea_orm::sqlx::query("BEGIN").execute(&mut *db).await?;
        let target_result = async {
            let inventory = large::inspect_in(db, &[71; 32], &workspace).await?;
            inspect_target_in(
                db,
                inventory.value(),
                target,
                &mut provisioning,
                &mut approval,
                ValidationBudget::default(),
                &workspace,
            )
            .await
        }
        .await;
        rollback(&mut t).await?;
        t.close().await?;
        let mut proof = target_result?;
        ensure!(
            proof.credentials.len() == 257,
            "fresh hash stream incomplete"
        );
        s.validation_budget().await?;
        s.open_immutable_source_budgeted(source, &workspace).await?;
        s.uncertain = true;
        let db = s.source.as_mut().context("source connection")?;
        sea_orm::sqlx::query("BEGIN").execute(&mut *db).await?;
        let outcome = no_copied_credentials_in(
            db,
            installation,
            &mut proof.credentials,
            ValidationBudget::default(),
            &workspace,
        )
        .await;
        rollback(&mut s).await?;
        s.close().await?;
        let error = outcome
            .err()
            .context("historical hash intersection accepted")?;
        ensure!(
            format!("{error:#}").contains("old ingress credential copied to target"),
            "wrong historical rejection: {error:#}"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    // Both owners remain retained on any unconfirmed SQL cleanup.
    let target_closed = t.close().await;
    let source_closed = s.close().await;
    target_closed?;
    source_closed?;
    source_witness.check()?;
    target_witness.check()?;
    provisioning.recheck(&workspace)?;
    approval.recheck(&workspace)?;
    drop(provisioning);
    drop(approval);
    drop(t);
    drop(s);
    workspace.finish()?;
    drop(crate::tenancy::ownership::acquire_path(source, false)?);
    drop(crate::tenancy::ownership::acquire_path(
        &target.database,
        false,
    )?);
    ensure!(witness(source)? == source_before && witness(&target.database)? == target_before);
    result
}

async fn run(home: PathBuf, cancel: CancellationToken) -> Result<()> {
    crate::runtime_crypto::DeploymentKey::from_environment(producer::KEY_ENV)?;
    let source = home.join("source.sqlite");
    let live = producer::Live::open(&source, None, None).await?;
    let made = producer::provision(&live).await;
    let closed = live.close(&source).await;
    closed?;
    let made = made?;
    seed_source(&source, &cancel).await?;
    let target = target(&home, &made).await?;
    full_positive(&home, &source, &made.installation, &target).await?;
    let selected = hashes(&target, &cancel, &home).await?;
    for expired in [false, true] {
        for (rank, hash) in [0, 128, 256].into_iter().zip(&selected) {
            ensure!(!cancel.is_cancelled(), "credential qualification cancelled");
            late_match(&source, hash, expired, &cancel).await?;
            rejected_source(&home, &source, &made.installation, &target, &cancel).await?;
            eprintln!(
                "historical intersection rejected: expired={expired} target_hash_rank={rank} source_predecessors=1000000"
            );
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "complete million-row credential qualification; requires private BITROUTER_C4_SCRATCH and recovery key"]
async fn million_row_source_checks_every_fresh_hash_and_late_historical_match() -> Result<()> {
    let started = tokio::time::Instant::now();
    let root = PathBuf::from(
        std::env::var_os("BITROUTER_C4_SCRATCH")
            .context("explicit qualification scratch required")?,
    )
    .canonicalize()?;
    large::probe(&root, 16 * 1024 * 1024 * 1024, 0)?;
    let directory = tempfile::Builder::new()
        .prefix("offline-credentials-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(&root)?;
    let home = directory.path().canonicalize()?;
    let observation = Observation::new(&home)?;
    observation.attach(Some(std::process::id()));
    let outcome = {
        let work = async {
            let cancel = CancellationToken::new();
            let mut task = tokio::spawn(run(home, cancel.clone()));
            let first =
                tokio::time::timeout_at(started + Duration::from_secs(3540), &mut task).await;
            let outcome = match first {
                Ok(outcome) => outcome,
                Err(_) => {
                    cancel.cancel();
                    match tokio::time::timeout_at(started + Duration::from_secs(3590), &mut task)
                        .await
                    {
                        Ok(joined) => {
                            // Joined error cleanup is still a timeout, never a successful retry.
                            std::mem::forget(directory);
                            let cleanup = joined?;
                            anyhow::bail!(
                                "credential qualification deadline exceeded after joined cleanup: {}",
                                cleanup.is_ok()
                            );
                        }
                        Err(_) => {
                            std::mem::forget(directory);
                            anyhow::bail!(
                                "credential qualification cleanup unconfirmed; retained owned directory"
                            );
                        }
                    }
                }
            };
            // Preserve diagnostics/SQL custody on failure; only a complete joined pass deletes.
            if !matches!(&outcome, Ok(Ok(()))) {
                std::mem::forget(directory);
                return outcome.context("credential qualification worker failed")?;
            }
            observation.sample();
            directory.close()?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::pin!(work);
        loop {
            tokio::select! {
                result = &mut work => break result,
                _ = observation.tick() => {}
            }
        }
    };
    let measured = observation.finish("K:parent", outcome.is_ok(), outcome.is_ok());
    outcome?;
    measured?;
    ensure!(
        started.elapsed() < Duration::from_secs(3590),
        "credential qualification exceeded its aggregate cleanup deadline"
    );
    eprintln!(
        "credential qualification elapsed_ms={}",
        started.elapsed().as_millis()
    );
    Ok(())
}
