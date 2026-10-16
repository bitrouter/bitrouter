//! Reopen unchanged pre-refactor data through three real application profiles.

use super::*;
use bitrouter::client::agent_local::{self, Operation, ReplyError, ReplyResult};
use bitrouter_ai::harness::runtime::ErrorCode;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use sha2::{Digest, Sha256};

const THREAD: &str = "1149fbda-eb06-43b7-8d0e-108e857d9c2d";
const OWNER: &str = "46c9adc1-3b84-42b3-8495-24d9b26e9e5f";
const TABLES: [(&str, usize); 7] = [
    ("seaql_migrations", 26),
    ("bro_executions", 1),
    ("bro_execution_index", 1),
    ("bro_execution_records", 284),
    ("bro_acceptance_keys", 2),
    ("bro_managed_sessions", 0),
    ("requests", 2),
];

#[derive(Debug, PartialEq, Eq)]
struct StoredState {
    rows: Vec<Vec<String>>,
    original_owner: String,
    schema: Vec<String>,
    owners: Vec<String>,
    authority: Vec<String>,
}

impl StoredState {
    fn preserved(&self, original: &Self) -> bool {
        self.rows == original.rows
            && self.original_owner == original.original_owner
            && self.schema == original.schema
    }
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

async fn read_state(db: &DatabaseConnection) -> Result<StoredState> {
    let mut rows = Vec::new();
    for (table, count) in TABLES {
        let values = table_rows(db, table).await?;
        ensure!(values.len() == count, "baseline {table} row count changed");
        rows.push(values);
    }
    let owner = db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT json_array(generation,server_instance_id,stopped_at_ms) AS value FROM bro_runtime_owners WHERE server_instance_id = ?",
        [OWNER.into()],
    )).await?.context("baseline stopped owner missing")?;
    Ok(StoredState {
        rows,
        original_owner: owner.try_get("", "value")?,
        schema: strings(db, "SELECT json_array(type,name,tbl_name,sql) AS value FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*' ORDER BY value".into()).await?,
        owners: strings(db, "SELECT json_array(generation,server_instance_id,stopped_at_ms) AS value FROM bro_runtime_owners ORDER BY generation".into()).await?,
        authority: strings(db, "SELECT json_array(id,generation,server_instance_id,stopped_at_ms) AS value FROM bro_runtime_ownership ORDER BY id".into()).await?,
    })
}

fn snapshot(fixture: &Fixture) -> Result<StoredState> {
    let url = format!(
        "sqlite://{}",
        fixture.directory.path().join("profiles.db").display()
    );
    fixture.runtime.block_on(async {
        tokio::time::timeout(COMMAND_TIMEOUT, async {
            let db = bitrouter::db::connect(&url).await?;
            let result = read_state(&db).await;
            let closed = db.close().await;
            let state = result?;
            closed?;
            Ok(state)
        })
        .await
        .context("profile database snapshot timed out")?
    })
}

fn inspect_harness(fixture: &Fixture, native: bool) -> Result<()> {
    let socket = agent_local::socket_path(&fixture.socket);
    fixture.runtime.block_on(async {
        tokio::time::timeout(COMMAND_TIMEOUT, async {
            let reply = agent_local::request(&socket, Operation::Capabilities).await?;
            let ReplyResult::Capabilities {
                runtime,
                operations,
            } = reply
            else {
                bail!("missing harness capabilities");
            };
            ensure!(runtime.native_execution == native);
            if native {
                let discovery = runtime
                    .startup_discovery
                    .context("missing startup discovery")?;
                ensure!(discovery.complete && discovery.error.is_none());
                ensure!(discovery.inspected_roots == 1 && discovery.index_cutoff == 1);
            } else {
                ensure!(runtime.startup_discovery.is_none());
                ensure!(runtime.execution_ownership.is_none());
                ensure!(!operations.iter().any(|op| op == "start_turn"));
            }
            let client = agent_local::ThreadClient::connect(&socket).await?;
            // The historical workspace has no grant in this private host.
            // Opening its database must not implicitly authorize that path.
            // The separate baseline database/recovery tests decode its complete
            // journal offline; no recorded path is opened or rewritten here.
            let rejected = client
                .request(Operation::ReadThread {
                    thread_id: THREAD.into(),
                })
                .await
                .err()
                .context("cold inspection bypassed the workspace grant")?;
            ensure!(
                rejected
                    .downcast_ref::<ReplyError>()
                    .is_some_and(|error| error.code == ErrorCode::Unauthorized)
            );
            if !native {
                let rejected = client
                    .request(Operation::StartTurn {
                        thread_id: THREAD.into(),
                        prompt: "must not execute".into(),
                        idempotency_key: "profile-native-unavailable".into(),
                    })
                    .await
                    .err()
                    .context("harness-only profile admitted native execution")?;
                ensure!(
                    rejected
                        .downcast_ref::<ReplyError>()
                        .is_some_and(|error| error.code == ErrorCode::UnsupportedCapability)
                );
            }
            Ok(())
        })
        .await
        .context("baseline harness inspection timed out")?
    })
}

pub(super) fn identity(fixture: &mut Fixture, binary: &Path) -> Result<ArtifactIdentity> {
    let mut command = fixture.command(binary);
    command.args(["--json", "build-info"]);
    fixture.run(command)?.json()
}

fn exercise_transitions(fixture: &mut Fixture, binaries: [&Path; 3]) -> Result<()> {
    let full = identity(fixture, binaries[0])?;
    let gateway = identity(fixture, binaries[1])?;
    let harness = identity(fixture, binaries[2])?;
    ensure!(full.profile == DistributionProfile::Full);
    ensure!(gateway.profile == DistributionProfile::Gateway && gateway.features.is_empty());
    ensure!(
        harness.profile == DistributionProfile::Custom && harness.features == ["harness", "mcp"]
    );
    ensure!(full.source_revision.is_some());
    for artifact in [&gateway, &harness] {
        ensure!(
            artifact.source_revision == full.source_revision
                && artifact.version == full.version
                && artifact.target == full.target
        );
        ensure!(artifact.handoff_build_id != full.handoff_build_id);
    }
    let bytes = zstd::stream::decode_all(
        include_bytes!("../fixtures/baseline-runtime-v7/database.sqlite.zst").as_slice(),
    )?;
    ensure!(
        hex::encode(Sha256::digest(&bytes))
            == "28e61cdbf75ad2a918404ea30a185915d59b2cf58e67d5ac5e3b7dbc902064e9"
    );
    fs::write(fixture.directory.path().join("profiles.db"), bytes)?;
    let before = snapshot(fixture)?;
    let (config, first_pid) = fixture.configure_and_start_full(binaries[0])?;
    // Two transitions share exactly the same historical database and config.
    // None submits new work or changes the old workspace paths/record payloads.
    for (step, index) in [0, 1, 0, 2, 0].into_iter().enumerate() {
        let artifact = [&full, &gateway, &harness][index];
        let previous = snapshot(fixture)?;
        let pid = if step == 0 {
            first_pid
        } else {
            fixture.start(binaries[index])?
        };
        let status = fixture.status(binaries[index], artifact, artifact, pid)?;
        fixture.health(&status)?;
        if index == 1 {
            ensure!(
                !agent_local::socket_path(&fixture.socket).exists(),
                "minimum profile created a harness listener"
            );
        } else {
            inspect_harness(fixture, index == 0)?;
        }
        let running = snapshot(fixture)?;
        ensure!(
            running.preserved(&before),
            "profile step {step} changed stored facts while running"
        );
        fixture.stop_resident(pid)?;
        let after = snapshot(fixture)?;
        ensure!(
            after.preserved(&before),
            "profile step {step} changed stored facts on shutdown"
        );
        if index != 0 {
            ensure!(
                running.owners == previous.owners
                    && running.authority == previous.authority
                    && after.owners == previous.owners
                    && after.authority == previous.authority,
                "core-free profile acquired execution ownership"
            );
        }
        ensure!(
            fs::read_to_string(&fixture.config)? == config,
            "profile step {step} changed saved configuration"
        );
    }
    Ok(())
}

#[test]
#[ignore = "requires separately built full, gateway and harness-only artifacts"]
fn baseline_records_survive_full_gateway_harness_transitions() -> Result<()> {
    let full = artifact_path("BITROUTER_TEST_FULL_BINARY")?;
    let gateway = artifact_path("BITROUTER_TEST_GATEWAY_BINARY")?;
    let harness = artifact_path("BITROUTER_TEST_HARNESS_BINARY")?;
    ensure!(full != gateway && full != harness && gateway != harness);
    let mut fixture = Fixture::new()?;
    let result = exercise_transitions(&mut fixture, [&full, &gateway, &harness]);
    let cleanup = fixture.cleanup();
    result.with_context(|| {
        format!(
            "baseline profile transition failed\n{}",
            fixture.diagnostics()
        )
    })?;
    cleanup.with_context(|| format!("baseline profile cleanup failed\n{}", fixture.diagnostics()))
}
