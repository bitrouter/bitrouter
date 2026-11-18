//! Refuse old datasets; preserve independently produced current BRO history.

use super::*;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use sha2::{Digest, Sha256};
use wiremock::MockServer;

const OLD_OWNER: &str = "46c9adc1-3b84-42b3-8495-24d9b26e9e5f";
const TABLES: [&str; 6] = [
    "bro_executions",
    "bro_execution_index",
    "bro_execution_records",
    "bro_acceptance_keys",
    "bro_managed_sessions",
    "requests",
];

#[derive(Debug, PartialEq, Eq)]
struct StoredState {
    ledger: Vec<(String, i64)>,
    rows: Vec<Vec<String>>,
    original_owner: String,
    schema: Vec<String>,
    owners: Vec<String>,
    authority: Vec<String>,
}

async fn strings(db: &DatabaseConnection, sql: String) -> Result<Vec<String>> {
    db.query_all(Statement::from_string(DbBackend::Sqlite, sql))
        .await?
        .into_iter()
        .map(|row| Ok(row.try_get("", "value")?))
        .collect()
}

fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(super) async fn table_rows(db: &DatabaseConnection, table: &str) -> Result<Vec<String>> {
    let columns = db
        .query_all(Statement::from_string(
            DbBackend::Sqlite,
            format!("PRAGMA table_info({})", identifier(table)),
        ))
        .await?
        .into_iter()
        .map(|row| row.try_get::<String>("", "name"))
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(!columns.is_empty(), "missing table {table}");
    strings(
        db,
        format!(
            "SELECT json_array({}) AS value FROM {} ORDER BY value",
            columns
                .iter()
                .map(|name| identifier(name))
                .collect::<Vec<_>>()
                .join(","),
            identifier(table),
        ),
    )
    .await
}

pub(super) fn identity(fixture: &mut Fixture, binary: &Path) -> Result<ArtifactIdentity> {
    let mut command = fixture.command(binary);
    command.args(["--json", "build-info"]);
    fixture.run(command)?.json()
}

async fn read_state(db: &DatabaseConnection, owner: &str) -> Result<StoredState> {
    let ledger = db
        .query_all(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT version,applied_at FROM seaql_migrations ORDER BY version",
        ))
        .await?
        .into_iter()
        .map(|row| Ok((row.try_get("", "version")?, row.try_get("", "applied_at")?)))
        .collect::<Result<Vec<_>, sea_orm::DbErr>>()?;
    let mut rows = Vec::new();
    for table in TABLES {
        rows.push(table_rows(db, table).await?);
    }
    let original_owner = db.query_one(Statement::from_sql_and_values(DbBackend::Sqlite,
        "SELECT json_array(generation,server_instance_id,stopped_at_ms) AS value FROM bro_runtime_owners WHERE server_instance_id=?", [owner.into()])).await?.context("original stopped owner missing")?.try_get("", "value")?;
    Ok(StoredState {
        ledger, rows, original_owner,
        schema: strings(db,"SELECT json_array(type,name,tbl_name,sql) AS value FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*' ORDER BY value".into()).await?,
        owners: strings(db,"SELECT json_array(generation,server_instance_id,stopped_at_ms) AS value FROM bro_runtime_owners ORDER BY generation".into()).await?,
        authority: strings(db,"SELECT json_array(id,generation,server_instance_id,stopped_at_ms) AS value FROM bro_runtime_ownership ORDER BY id".into()).await?,
    })
}

fn snapshot(fixture: &Fixture, owner: &str, query: &str) -> Result<StoredState> {
    let url = format!(
        "sqlite://{}?mode=ro{query}",
        fixture.directory.path().join("profiles.db").display()
    );
    fixture.runtime.block_on(async {
        tokio::time::timeout(COMMAND_TIMEOUT, async {
            // The independent read-only history reader may inspect an old
            // dataset. It must never install a ledger or run migrations.
            let db = sea_orm::Database::connect(&url).await?;
            let observed = read_state(&db, owner).await;
            let closed = db.close().await;
            let observed = observed?;
            closed?;
            Ok(observed)
        })
        .await
        .context("profile database observation timed out")?
    })
}

fn artifacts(fixture: &mut Fixture, binaries: [&Path; 2]) -> Result<[ArtifactIdentity; 2]> {
    let full = identity(fixture, binaries[0])?;
    let minimum = identity(fixture, binaries[1])?;
    ensure!(full.profile == DistributionProfile::Full);
    ensure!(minimum.profile == DistributionProfile::Gateway && minimum.features.is_empty());
    ensure!(full.source_revision.is_some());
    ensure!(
        minimum.source_revision == full.source_revision
            && minimum.version == full.version
            && minimum.target == full.target
    );
    ensure!(minimum.handoff_build_id != full.handoff_build_id);
    Ok([full, minimum])
}

fn provider_idle(fixture: &Fixture, provider: &MockServer) -> Result<()> {
    fixture.runtime.block_on(async {
        ensure!(
            provider
                .received_requests()
                .await
                .context("provider observation missing")?
                .is_empty(),
            "profile lifecycle dispatched a provider request"
        );
        Ok(())
    })
}

fn providers(upstream: &str) -> String {
    format!(
        "providers:\n  fixture:\n    api_base: '{upstream}'\n    api_key: profile-fixture-only\n    api_protocol: [{{'*': chat_completions}}]\n    models: [{{id: profile-model}}]\n"
    )
}

fn reject_old(fixture: &mut Fixture, binary: &Path, listen: std::net::SocketAddr) -> Result<()> {
    let mut command = fixture.selected_command(binary, "serve");
    command.arg("--managed-child");
    let index = fixture.spawn(command, true)?;
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        ensure!(
            !fixture.socket.exists(),
            "old dataset published a serving socket"
        );
        ensure!(
            !matches!(
                fixture.control(&DaemonCommand::Status),
                Ok(DaemonResponse::Status { .. })
            ),
            "old dataset reached readiness"
        );
        ensure!(
            std::net::TcpStream::connect_timeout(&listen, CONTROL_TIMEOUT).is_err(),
            "old dataset bound the inference listener"
        );
        let process = &mut fixture.processes[index];
        if let Some(status) = process.child.try_wait()? {
            let output = CliOutput {
                status,
                stdout: fs::read(&process.stdout)?,
                diagnostic: process.diagnostic(),
            };
            return output.rejected("format");
        }
        ensure!(
            Instant::now() < deadline,
            "old dataset refusal timed out: {}",
            process.diagnostic()
        );
        std::thread::sleep(POLL);
    }
}

fn exercise_old_refusal(fixture: &mut Fixture, binaries: [&Path; 2]) -> Result<()> {
    artifacts(fixture, binaries)?;
    let provider = fixture.runtime.block_on(MockServer::start());
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let listen = listener.local_addr()?;
    let config = format!(
        "inherit_defaults: false\nregistry: {{enabled: false}}\nserver:\n  listen: '{listen}'\n  control_socket: control.sock\n  skip_auth: true\ndatabase: {{url: 'sqlite://profiles.db?mode=rwc'}}\nagent_api: {{enabled: false}}\n{}",
        providers(&provider.uri())
    );
    fs::write(&fixture.config, &config)?;
    fs::set_permissions(&fixture.config, fs::Permissions::from_mode(0o600))?;
    drop(listener);
    let bytes = zstd::stream::decode_all(
        include_bytes!("../fixtures/baseline-runtime-v7/database.sqlite.zst").as_slice(),
    )?;
    ensure!(
        hex::encode(Sha256::digest(&bytes))
            == "28e61cdbf75ad2a918404ea30a185915d59b2cf58e67d5ac5e3b7dbc902064e9"
    );
    let path = fixture.directory.path().join("profiles.db");
    fs::write(&path, &bytes)?;
    // The historical fixture is a closed, checkpointed image. An ordinary
    // read-only WAL connection can create sidecars before the candidate starts.
    let original = snapshot(fixture, OLD_OWNER, "&immutable=true")?;
    ensure!(
        !path.with_extension("db-wal").exists(),
        "historical fixture observation created a WAL"
    );
    ensure!(original.ledger.len() == 26);
    for (rows, expected) in original.rows.iter().zip([1, 1, 284, 2, 0, 2]) {
        ensure!(rows.len() == expected);
    }
    for binary in binaries {
        reject_old(fixture, binary, listen)?;
        provider_idle(fixture, &provider)?;
        ensure!(
            fs::read(&path)? == bytes,
            "refusal rewrote the old database"
        );
        ensure!(
            !path.with_extension("db-wal").exists(),
            "refusal created a writable WAL"
        );
        ensure!(
            snapshot(fixture, OLD_OWNER, "&immutable=true")? == original,
            "refusal changed old ledger/history/ownership"
        );
        ensure!(!fixture.socket.exists());
        ensure!(fs::read_to_string(&fixture.config)? == config);
    }
    Ok(())
}

fn binaries() -> Result<[PathBuf; 2]> {
    let binaries = [
        artifact_path("BITROUTER_TEST_FULL_BINARY")?,
        artifact_path("BITROUTER_TEST_GATEWAY_BINARY")?,
    ];
    ensure!(binaries[0] != binaries[1]);
    Ok(binaries)
}

#[test]
#[ignore = "requires separately built full and minimum gateway artifacts"]
fn historical_dataset_is_refused_by_both_gateway_profiles() -> Result<()> {
    let binaries = binaries()?;
    let mut fixture = Fixture::new()?;
    let result = exercise_old_refusal(&mut fixture, [&binaries[0], &binaries[1]]);
    let cleanup = fixture.cleanup();
    result.with_context(|| {
        format!(
            "historical dataset refusal failed\n{}",
            fixture.diagnostics()
        )
    })?;
    cleanup
}
