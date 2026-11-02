//! Real public commands over dense and genuinely sparse, byte-identical SQLite files.

use super::*;
use crate::offline::{
    large,
    recovery::tests::fixture::scale::{FileWitness, observation::Observation, witness},
};
use bitrouter_ai::tenancy::configuration::{Activation, ConfigurationPut, ConfigurationRevision};
use bitrouter_sdk::tenancy::TenancyClient;
use sea_orm::sqlx::{
    ConnectOptions, Connection, SqliteConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode},
};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
};
use tokio::time::Instant;

mod process;

const GIB: u64 = 1024 * 1024 * 1024;
const FLOOR: u64 = 2 * GIB;
const SAMPLE_ROWS: i64 = 1_000_001;

async fn source(home: &Path) -> Result<(producer::Provision, ConfigurationRevision)> {
    let path = home.join("dense.sqlite");
    let live = producer::Live::open(&path, None, Some(producer::KEY_ENV)).await?;
    let made = producer::provision(&live).await;
    let closed = live.close(&path).await;
    closed?;
    let provision = made?;
    let manifest = producer::manifest(&provision.installation);
    let import = home.join("import.json");
    producer::private_json(&import, &manifest)?;
    let live = producer::Live::open(&path, Some(&import), Some(producer::KEY_ENV)).await?;
    let configured = async {
        let client = TenancyClient::new(&live.base,&provision.secret)?;
        let tenant = "tenant".try_into()?;
        let configuration = client.put_configuration(&tenant,&ConfigurationPut {
            operation_id:"draft".try_into()?,expected_authority_revision:provision.authority,
            installation_id:provision.installation.clone(),
            document:serde_json::from_value(json!({"routes":[manifest["resources"][1]["reference"]],"policy":null,
                "generation_defaults":{"max_output_tokens":128,"temperature":0.2},"learning":{"mode":"off","max_evaluations":0},
                "limits":{"active_executions":1,"queued_work":1,"learning_jobs":0}}))?,
        }).await?;
        let draft = client.operation(&provision.installation,&"draft".try_into()?).await?;
        client.activate(&tenant,&Activation {
            operation_id:"activate".try_into()?,expected_active_revision:None,
            revision_id:configuration.revision_id.clone(),expected_authority_revision:draft.authority_revision.context("draft authority")?,
            installation_id:provision.installation.clone(),
        }).await?;
        Ok::<_,anyhow::Error>(configuration)
    }.await;
    let closed = live.close(&path).await;
    closed?;
    Ok((provision, configured?))
}

async fn open_writer(
    path: &Path,
    cancel: &CancellationToken,
) -> Result<(crate::tenancy::ownership::Lease, SqliteConnection)> {
    let owner = crate::tenancy::ownership::acquire_path(path, false)?;
    let opened = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Delete)
        .row_buffer_size(1)
        .statement_cache_capacity(0)
        .pragma("cache_size", "-2048")
        .pragma("temp_store", "FILE")
        .pragma("mmap_size", "0")
        .disable_statement_logging()
        .connect()
        .await;
    let mut db = match opened {
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

async fn seed(path: &Path, cancel: &CancellationToken) -> Result<()> {
    large::probe(path.parent().context("source parent")?, FLOOR, GIB)?;
    let (owner, mut db) = open_writer(path, cancel).await?;
    let work = async {
        let vacuum:i64 = sea_orm::sqlx::query_scalar("PRAGMA auto_vacuum").fetch_one(&mut db).await?;
        ensure!(vacuum == 0,"sparse witness requires retained main-database freelist pages");
        sea_orm::sqlx::query("PRAGMA secure_delete=ON").execute(&mut db).await?;
        // The identical dense workload used by the engine qualification, not invented history.
        sea_orm::sqlx::query("CREATE TABLE samples(value BLOB)").execute(&mut db).await?;
        sea_orm::sqlx::query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1000001) INSERT INTO samples SELECT zeroblob(544) FROM n")
            .execute(&mut db).await?;
        // Main schema, not TEMP. Each value is 4 KiB; no 128 MiB client/SQLite cell.
        sea_orm::sqlx::query("CREATE TABLE main.offline_sparse_padding(value BLOB)").execute(&mut db).await?;
        sea_orm::sqlx::query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<32768) INSERT INTO main.offline_sparse_padding SELECT zeroblob(4096) FROM n")
            .execute(&mut db).await?;
        sea_orm::sqlx::query("DROP TABLE main.offline_sparse_padding").execute(&mut db).await?;
        let pages:i64 = sea_orm::sqlx::query_scalar("PRAGMA freelist_count").fetch_one(&mut db).await?;
        let page_bytes:i64 = sea_orm::sqlx::query_scalar("PRAGMA page_size").fetch_one(&mut db).await?;
        ensure!(pages.checked_mul(page_bytes).is_some_and(|bytes| bytes >= 128*1024*1024),"main DB did not retain actual free pages");
        eprintln!("sparse fixture main_freelist_pages={pages} page_bytes={page_bytes}");
        Ok::<_,anyhow::Error>(())
    }.await;
    let closed = close_writer(owner, db).await;
    closed?;
    work?;
    let metadata = std::fs::metadata(path)?;
    ensure!(
        metadata.len() > 512 * 1024 * 1024
            && metadata
                .blocks()
                .checked_mul(512)
                .is_some_and(|n| n > 512 * 1024 * 1024),
        "dense fixture lacks actual allocated data"
    );
    large::probe(path.parent().context("source parent")?, FLOOR, 0)?;
    inspect(path, cancel).await
}

async fn inspect(path: &Path, cancel: &CancellationToken) -> Result<()> {
    let owner = crate::tenancy::ownership::acquire_path(path, false)?;
    let source = large::Source::inspect(path)?;
    let mut connections = crate::offline::engine::Connections::new(cancel.clone(), Some(owner));
    let options = large::Options {
        working_bytes: 64 * 1024 * 1024,
        scratch_directory: path.parent().context("source parent")?.into(),
        min_free_bytes: FLOOR,
    };
    let workspace = Workspace::create(&options, cancel.clone())?;
    let result = async {
        connections.validation_budget().await?;
        connections
            .open_immutable_source_budgeted(path, &workspace)
            .await?;
        let db = connections
            .source
            .as_mut()
            .context("inspection connection")?;
        let rows: i64 = sea_orm::sqlx::query_scalar("SELECT COUNT(*) FROM samples")
            .fetch_one(&mut *db)
            .await?;
        let values: i64 = sea_orm::sqlx::query_scalar("SELECT SUM(length(value)) FROM samples")
            .fetch_one(&mut *db)
            .await?;
        let broken: i64 = sea_orm::sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pragma_integrity_check WHERE integrity_check<>'ok')",
        )
        .fetch_one(&mut *db)
        .await?;
        let free: i64 = sea_orm::sqlx::query_scalar("PRAGMA freelist_count")
            .fetch_one(&mut *db)
            .await?;
        let page: i64 = sea_orm::sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&mut *db)
            .await?;
        ensure!(
            rows == SAMPLE_ROWS
                && values == SAMPLE_ROWS * 544
                && values > 256 * 1024 * 1024
                && broken == 0
        );
        ensure!(
            free.checked_mul(page)
                .is_some_and(|bytes| bytes >= 128 * 1024 * 1024)
        );
        eprintln!(
            "readonly immutable integrity=ok actual_sample_rows={rows} typed_value_bytes={values}"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let closed = connections.close().await;
    closed?;
    source.check()?;
    drop(connections);
    workspace.finish()?;
    drop(crate::tenancy::ownership::acquire_path(path, false)?);
    result
}

async fn sparse(source: &Path, destination: &Path, cancel: &CancellationToken) -> Result<()> {
    let source = source.to_owned();
    let destination = destination.to_owned();
    let stop = cancel.clone();
    let checked = destination.clone();
    tokio::task::spawn_blocking(move || {
        let owner = crate::tenancy::ownership::acquire_path(&source, false)?;
        let before = large::Source::inspect(&source)?;
        let expected = witness(&source)?;
        large::probe(
            destination.parent().context("sparse parent")?,
            FLOOR,
            expected.bytes,
        )?;
        let mut input = File::open(&source)?;
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&destination)?;
        let mut buffer = [0_u8; 4096];
        let mut position = 0_u64;
        let mut holes = 0_u64;
        while position < expected.bytes {
            ensure!(!stop.is_cancelled(), "sparse copy cancelled");
            let count = usize::try_from((expected.bytes - position).min(4096))?;
            input.read_exact(&mut buffer[..count])?;
            if count == 4096 && buffer.iter().all(|byte| *byte == 0) {
                output.seek(SeekFrom::Current(4096))?;
                holes = holes.checked_add(4096).context("sparse hole count")?;
            } else {
                output.write_all(&buffer[..count])?;
            }
            position = position
                .checked_add(u64::try_from(count)?)
                .context("sparse offset")?;
        }
        // Exact existing EOF only; never manufacture SQLite size with appended padding.
        output.set_len(expected.bytes)?;
        output.sync_all()?;
        drop(input);
        drop(output);
        ensure!(
            witness(&destination)? == expected && witness(&source)? == expected,
            "sparse copy changed SQLite bytes"
        );
        let metadata = std::fs::metadata(&destination)?;
        let allocated = metadata
            .blocks()
            .checked_mul(512)
            .context("allocated byte overflow")?;
        ensure!(
            holes >= 32 * 1024 * 1024
                && allocated
                    .checked_add(32 * 1024 * 1024)
                    .is_some_and(|n| n <= metadata.len()),
            "filesystem did not preserve real sparse holes"
        );
        before.check()?;
        large::Source::inspect(&destination)?;
        large::probe(destination.parent().context("sparse parent")?, FLOOR, 0)?;
        eprintln!(
            "sparse actual_bytes={} allocated_bytes={allocated} skipped_zero_bytes={holes}",
            metadata.len()
        );
        drop(owner);
        drop(crate::tenancy::ownership::acquire_path(&source, false)?);
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("sparse worker join")??;
    inspect(&checked, cancel).await
}

fn bundle_witness(bundle: &Path) -> Result<[FileWitness; 4]> {
    // A successful exclusive marker lock proves no CLI reader retains READY custody.
    let ready = File::open(bundle.join("READY"))?;
    ready.try_lock()?;
    Ok([
        witness(&bundle.join("snapshot.sqlite"))?,
        witness(&bundle.join("evidence.json"))?,
        witness(&bundle.join("inventory.key"))?,
        witness(&bundle.join("READY"))?,
    ])
}

async fn retained(
    path: &Path,
    provision: &producer::Provision,
    configuration: &ConfigurationRevision,
) -> Result<()> {
    let live = producer::Live::open(path, None, Some(producer::KEY_ENV)).await?;
    let checked = async {
        let client = TenancyClient::new(&live.base, &provision.secret)?;
        let tenant = "tenant".try_into()?;
        let current = client
            .configuration(&provision.installation, &tenant, &configuration.revision_id)
            .await?;
        ensure!(serde_json::to_value(&current)? == serde_json::to_value(configuration)?);
        let active = client
            .active_revision(&provision.installation, &tenant)
            .await?;
        ensure!(active.revision_id.as_ref() == Some(&configuration.revision_id));
        let credential = provision.operations[3]["result"]["resource_id"]
            .as_str()
            .context("credential identity")?
            .try_into()?;
        let status = live
            .operator()?
            .credential(&provision.installation, &tenant, &credential)
            .await?;
        ensure!(status.state == CredentialState::Active);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let closed = live.close(path).await;
    closed?;
    checked
}

async fn chain(
    home: &Path,
    source: &Path,
    name: &str,
    binary: &Path,
    authority: (&producer::Provision, &ConfigurationRevision),
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<()> {
    let (provision, configuration) = authority;
    let work_until = deadline - Duration::from_secs(110);
    let key = std::env::var(producer::KEY_ENV)?;
    let secrets = [provision.secret.as_str(), key.as_str()];
    let original = witness(source)?;
    let source_guard = large::Source::inspect(source)?;
    let bundle = home.join(format!("{name}-bundle"));
    large::probe(home, FLOOR, original.bytes)?;
    let mut command = process::command(binary, home, "snapshot")?;
    command
        .arg("--format")
        .arg("v2")
        .arg("--database")
        .arg(source)
        .arg("--bundle")
        .arg(&bundle);
    let report = process::invoke(
        command,
        &format!("L:{name}:snapshot"),
        work_until,
        deadline,
        cancel,
        &secrets,
    )
    .await?;
    ensure!(
        report["action"] == "snapshot"
            && report["evidence"]["format"] == "bitrouter.offline.sqlite-evidence.v2"
    );
    ensure!(
        report["database_inventory_verified"] == true
            && report["evidence"]["snapshot_bytes"].as_u64() == Some(original.bytes)
    );
    let tables = report["evidence"]["inventory"]["tables"]
        .as_array()
        .context("CLI inventory tables")?;
    let sample = tables
        .iter()
        .find(|table| table["name"] == "samples")
        .context("CLI omitted actual large table")?;
    ensure!(
        sample["rows"] == SAMPLE_ROWS
            && sample["value_bytes"]
                .as_u64()
                .is_some_and(|bytes| bytes > 256 * 1024 * 1024)
    );
    let anchor = report["evidence_digest"]
        .as_str()
        .context("CLI evidence anchor")?
        .to_owned();
    let before = bundle_witness(&bundle)?;
    ensure!(before[0] == original);
    drop(report);
    drop(crate::tenancy::ownership::acquire_path(source, false)?);
    source_guard.check()?;

    let mut command = process::command(binary, home, "verify")?;
    command
        .arg("--bundle")
        .arg(&bundle)
        .arg("--evidence-digest")
        .arg(&anchor);
    let verified = process::invoke(
        command,
        &format!("L:{name}:verify"),
        work_until,
        deadline,
        cancel,
        &secrets,
    )
    .await?;
    ensure!(
        verified["action"] == "verify"
            && verified["evidence_anchor_checked"] == true
            && verified["database_inventory_verified"] == true
    );
    drop(verified);
    ensure!(bundle_witness(&bundle)? == before && witness(source)? == original);

    let control = home.join(format!("{name}-control.json"));
    producer::private_json(
        &control,
        &json!({"credentials":[{"id":"audit","token_env":"OFFLINE_SCALE_AUDIT_TOKEN","scopes":["control:tenancy"]}]}),
    )?;
    let projection = home.join(format!("{name}-projection"));
    large::probe(home, FLOOR, original.bytes)?;
    let mut command = process::command(binary, home, "audit")?;
    command
        .arg("--archive")
        .arg(&bundle)
        .arg("--projection")
        .arg(&projection)
        .arg("--evidence-digest")
        .arg(&anchor)
        .arg("--control-config")
        .arg(&control)
        .args(["--listen", "127.0.0.1:0"]);
    process::audit(
        command,
        &format!("L:{name}:audit"),
        &provision.installation,
        work_until,
        deadline,
        cancel,
        &secrets,
    )
    .await?;
    let projection_path = projection.join("audit.sqlite");
    drop(crate::tenancy::ownership::acquire_path(
        &projection_path,
        false,
    )?);
    ensure!(bundle_witness(&bundle)? == before && witness(source)? == original);
    // Only remove the owned projection after real graceful process/SQL close.
    std::fs::remove_dir_all(&projection)?;

    let restored = home.join(format!("{name}-restored.sqlite"));
    large::probe(home, FLOOR, original.bytes)?;
    let mut command = process::command(binary, home, "restore")?;
    command
        .arg("--bundle")
        .arg(&bundle)
        .arg("--evidence-digest")
        .arg(&anchor)
        .arg("--database")
        .arg(&restored)
        .arg("--expected-installation-id")
        .arg(provision.installation.as_str())
        .arg("--wrapping-key-env")
        .arg(producer::KEY_ENV);
    let restored_report = process::invoke(
        command,
        &format!("L:{name}:restore"),
        work_until,
        deadline,
        cancel,
        &secrets,
    )
    .await?;
    ensure!(
        restored_report["action"] == "restore"
            && restored_report["database_inventory_verified"] == true
    );
    drop(restored_report);
    drop(crate::tenancy::ownership::acquire_path(&restored, false)?);
    large::Source::inspect(&restored)?;
    ensure!(witness(&restored)? == original && bundle_witness(&bundle)? == before);
    inspect(&restored, cancel).await?;
    retained(&restored, provision, configuration).await?;
    drop(crate::tenancy::ownership::acquire_path(&restored, false)?);
    // Owning startup may change SQLite journal headers; the restore was exact before it.
    ensure!(bundle_witness(&bundle)? == before && witness(source)? == original);
    source_guard.check()?;
    drop(crate::tenancy::ownership::acquire_path(source, false)?);
    std::fs::remove_file(&restored)?;
    std::fs::remove_dir_all(&bundle)?;
    large::probe(home, FLOOR, 0)?;
    eprintln!(
        "large CLI {name}: snapshot verify audit restore normal_startup PASS bytes={}",
        original.bytes
    );
    Ok(())
}

async fn run(home: PathBuf, deadline: Instant, cancel: CancellationToken) -> Result<()> {
    crate::runtime_crypto::DeploymentKey::from_environment(producer::KEY_ENV)?;
    let binary = PathBuf::from(
        std::env::var_os("BITROUTER_TEST_BRO")
            .context("explicit built bro required; no build/skip")?,
    );
    ensure!(binary.is_absolute());
    let binary = binary.canonicalize()?;
    ensure!(binary.is_file() && std::fs::metadata(&binary)?.permissions().mode() & 0o111 != 0);
    let identity = witness(&binary)?;
    eprintln!(
        "large CLI binary_sha256={} bytes={} working_bytes=67108864",
        identity.sha256, identity.bytes
    );
    let (provision, configuration) = source(&home).await?;
    let dense = home.join("dense.sqlite");
    seed(&dense, &cancel).await?;
    chain(
        &home,
        &dense,
        "dense",
        &binary,
        (&provision, &configuration),
        deadline,
        &cancel,
    )
    .await?;
    let sparse_path = home.join("sparse.sqlite");
    sparse(&dense, &sparse_path, &cancel).await?;
    chain(
        &home,
        &sparse_path,
        "sparse",
        &binary,
        (&provision, &configuration),
        deadline,
        &cancel,
    )
    .await?;
    ensure!(
        witness(&binary)? == identity,
        "CLI artifact changed during qualification"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "real dense/sparse CLI qualification; requires explicit bro, recovery key and private BITROUTER_C4_SCRATCH"]
async fn real_cli_dense_and_sparse_snapshot_verify_audit_restore() -> Result<()> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(1190);
    let root = PathBuf::from(
        std::env::var_os("BITROUTER_C4_SCRATCH")
            .context("explicit qualification scratch required")?,
    )
    .canonicalize()?;
    large::probe(&root, 16 * GIB, 0)?;
    let directory = tempfile::Builder::new()
        .prefix("offline-cli-scale-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(&root)?;
    let observation = Observation::new(directory.path())?;
    observation.attach(Some(std::process::id()));
    let outcome = {
        let work = async {
            let cancel = CancellationToken::new();
            let mut task = tokio::spawn(run(
                directory.path().canonicalize()?,
                deadline,
                cancel.clone(),
            ));
            let outcome =
                match tokio::time::timeout_at(deadline - Duration::from_secs(110), &mut task).await
                {
                    Ok(result) => result,
                    Err(_) => {
                        cancel.cancel();
                        let cleanup = tokio::time::timeout_at(deadline, &mut task).await;
                        std::mem::forget(directory);
                        match cleanup {
                            Ok(joined) => {
                                let result = joined?;
                                anyhow::bail!(
                                    "CLI qualification aggregate deadline after joined cleanup: {}",
                                    result.is_ok()
                                );
                            }
                            Err(_) => anyhow::bail!(
                                "CLI qualification cleanup unconfirmed; retained owned directory"
                            ),
                        }
                    }
                };
            if !matches!(&outcome, Ok(Ok(()))) {
                std::mem::forget(directory);
                return outcome.context("CLI qualification worker failed")?;
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
    let measured = observation.finish("L:parent", outcome.is_ok(), outcome.is_ok());
    outcome?;
    measured?;
    ensure!(
        Instant::now() < deadline,
        "CLI qualification exceeded its aggregate cleanup deadline"
    );
    eprintln!(
        "large CLI qualification elapsed_ms={}",
        started.elapsed().as_millis()
    );
    Ok(())
}
