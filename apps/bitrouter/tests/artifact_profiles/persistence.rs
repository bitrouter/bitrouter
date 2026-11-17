//! Refuse old datasets; preserve independently produced current BRO history.

use super::*;
use bitrouter::agent_store::DatabaseExecutionStore;
use bitrouter::client::agent_local::{self, Operation, ReplyError, ReplyResult};
use bitrouter::gateway::caller::CallerContext;
use bitrouter::harness::{
    agent::AgentConfigExt,
    storage::{AcceptedKey, ExecutionRecord, ExecutionStore, RUNTIME_FORMAT_VERSION},
};
use bitrouter_ai::harness::{
    agent::AgentConfig,
    runtime::{ErrorCode, OwnerClaim},
    thread::{PermissionProfile, ThreadChange, ThreadEvent, ThreadSnapshot, ThreadStatus},
};
use bitrouter_ai::types::{Message, Role};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use sha2::{Digest, Sha256};
use wiremock::MockServer;

const OLD_OWNER: &str = "46c9adc1-3b84-42b3-8495-24d9b26e9e5f";
const THREAD: &str = "current-profile-thread";
const OWNER: &str = "current-profile-fixture-owner";
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

impl StoredState {
    fn preserved(&self, original: &Self) -> bool {
        self.ledger == original.ledger
            && self.rows == original.rows
            && self.original_owner == original.original_owner
            && self.schema == original.schema
            && original
                .owners
                .iter()
                .all(|owner| self.owners.contains(owner))
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

fn snapshot(fixture: &Fixture, owner: &str) -> Result<StoredState> {
    let url = format!(
        "sqlite://{}?mode=ro",
        fixture.directory.path().join("profiles.db").display()
    );
    fixture.runtime.block_on(async {
        tokio::time::timeout(COMMAND_TIMEOUT, async {
            // The independent read-only history reader may inspect an old
            // dataset. It must never install a ledger or run migrations.
            let db = bitrouter::db::connect(&url).await?;
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

fn artifacts(fixture: &mut Fixture, binaries: [&Path; 3]) -> Result<[ArtifactIdentity; 3]> {
    let full = identity(fixture, binaries[0])?;
    let minimum = identity(fixture, binaries[1])?;
    let harness = identity(fixture, binaries[2])?;
    ensure!(full.profile == DistributionProfile::Full);
    ensure!(minimum.profile == DistributionProfile::Gateway && minimum.features.is_empty());
    ensure!(
        harness.profile == DistributionProfile::Custom && harness.features == ["harness", "mcp"]
    );
    ensure!(full.source_revision.is_some());
    for other in [&minimum, &harness] {
        ensure!(
            other.source_revision == full.source_revision
                && other.version == full.version
                && other.target == full.target
        );
        ensure!(other.handoff_build_id != full.handoff_build_id);
    }
    Ok([full, minimum, harness])
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
            !fixture.socket.exists() && !agent_local::socket_path(&fixture.socket).exists(),
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
            return output.rejected("store format");
        }
        ensure!(
            Instant::now() < deadline,
            "old dataset refusal timed out: {}",
            process.diagnostic()
        );
        std::thread::sleep(POLL);
    }
}

fn exercise_old_refusal(fixture: &mut Fixture, binaries: [&Path; 3]) -> Result<()> {
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
    let original = snapshot(fixture, OLD_OWNER)?;
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
            snapshot(fixture, OLD_OWNER)? == original,
            "refusal changed old ledger/history/ownership"
        );
        ensure!(
            !path.with_extension("db-wal").exists(),
            "refusal created a writable WAL"
        );
        ensure!(!fixture.socket.exists() && !agent_local::socket_path(&fixture.socket).exists());
        ensure!(fs::read_to_string(&fixture.config)? == config);
    }
    Ok(())
}

fn seed_current_history(fixture: &Fixture) -> Result<()> {
    let workspace = fixture.directory.path().join("ungranted-history");
    fs::create_dir(&workspace)?;
    let url = format!(
        "sqlite://{}",
        fixture.directory.path().join("profiles.db").display()
    );
    fixture.runtime.block_on(async {
        let db = bitrouter::db::connect(&url).await?;
        let store = DatabaseExecutionStore::new(db.clone());
        let OwnerClaim::Acquired { owner } =
            store.claim_owner(OWNER).await.map_err(anyhow::Error::msg)?
        else {
            bail!("current fixture could not claim the stopped full owner");
        };
        let snapshot = ThreadSnapshot {
            server_instance_id: OWNER.into(),
            thread_id: THREAD.into(),
            status: ThreadStatus::Idle,
            workspace,
            model: "fixture:profile-model".into(),
            permission_profile: PermissionProfile::ReadOnly,
            context_version: 0,
            cursor: 0,
            active_turn_id: None,
            queued: Vec::new(),
            pause_reason: None,
            waiting_for_capacity: false,
        };
        let mut checkpoint = snapshot.clone();
        checkpoint.cursor = 4;
        let records = vec![
            ExecutionRecord::ThreadCreated {
                caller: CallerContext::local(),
                snapshot,
                config: Box::new(AgentConfig::fixed("fixture:profile-model", None).read_only()),
                verification_command: None,
            },
            ExecutionRecord::AcceptedKey {
                entry: AcceptedKey {
                    scope: "current-profile-scope".into(),
                    key: "current-profile-key".into(),
                    fingerprint: "c".repeat(64),
                    thread_id: THREAD.into(),
                    turn_id: None,
                },
            },
            ExecutionRecord::ThreadCheckpoint {
                snapshot: checkpoint.clone(),
                messages: vec![Message::text(Role::User, "current profile history")],
            },
            ExecutionRecord::ThreadEvent {
                event: ThreadEvent {
                    server_instance_id: OWNER.into(),
                    thread_id: THREAD.into(),
                    seq: 4,
                    timestamp_ms: 1,
                    changes: vec![ThreadChange::Checkpoint {
                        snapshot: checkpoint,
                    }],
                },
            },
        ];
        ensure!(
            store
                .commit_owned(&owner, THREAD, 0, &records)
                .await
                .map_err(anyhow::Error::msg)?
                == 4
        );
        let stopped = store.stop_owner(&owner).await.map_err(anyhow::Error::msg)?;
        ensure!(stopped.stopped_at_ms.is_some());
        let stored = store
            .load(THREAD)
            .await
            .map_err(anyhow::Error::msg)?
            .context("current fixture history missing")?;
        ensure!(stored.format_version == RUNTIME_FORMAT_VERSION && stored.version == 4);
        ensure!(serde_json::to_value(&stored.records)? == serde_json::to_value(&records)?);
        drop(store);
        db.close().await?;
        Ok(())
    })
}

fn inspect_harness(fixture: &Fixture, native: bool) -> Result<()> {
    let socket = agent_local::socket_path(&fixture.socket);
    fixture.runtime.block_on(async {
        tokio::time::timeout(COMMAND_TIMEOUT, async {
            let ReplyResult::Capabilities {
                runtime,
                operations,
            } = agent_local::request(&socket, Operation::Capabilities).await?
            else {
                bail!("missing harness capabilities");
            };
            ensure!(runtime.native_execution == native);
            if native {
                let discovery = runtime
                    .startup_discovery
                    .context("missing current startup discovery")?;
                ensure!(discovery.complete && discovery.writer_fenced && discovery.error.is_none());
                ensure!(
                    discovery.inspected_roots == 1
                        && discovery.index_cutoff == 1
                        && discovery.scanned_records == 4
                        && discovery.blocked_workspaces == 0
                );
            } else {
                ensure!(
                    runtime.startup_discovery.is_none() && runtime.execution_ownership.is_none()
                );
                ensure!(!operations.iter().any(|operation| operation == "start_turn"));
            }
            let client = agent_local::ThreadClient::connect(&socket).await?;
            let denied = client
                .request(Operation::ReadThread {
                    thread_id: THREAD.into(),
                })
                .await
                .err()
                .context("cold history bypassed its absent workspace grant")?;
            ensure!(
                denied
                    .downcast_ref::<ReplyError>()
                    .is_some_and(|error| error.code == ErrorCode::Unauthorized)
            );
            if !native {
                let denied = client
                    .request(Operation::StartTurn {
                        thread_id: THREAD.into(),
                        prompt: "must not execute".into(),
                        idempotency_key: "profile-native-unavailable".into(),
                    })
                    .await
                    .err()
                    .context("harness-only profile admitted native work")?;
                ensure!(
                    denied
                        .downcast_ref::<ReplyError>()
                        .is_some_and(|error| error.code == ErrorCode::UnsupportedCapability)
                );
            }
            Ok(())
        })
        .await
        .context("current harness inspection timed out")?
    })
}

fn owner_head(state: &StoredState) -> Result<(u64, String, Option<u64>)> {
    ensure!(
        state.authority.len() == 1,
        "native authority singleton changed"
    );
    let (id, generation, instance, stopped): (u64, u64, Option<String>, Option<u64>) =
        serde_json::from_str(&state.authority[0])?;
    ensure!(id == 1 && generation > 0);
    Ok((
        generation,
        instance.context("native authority owner absent")?,
        stopped,
    ))
}

fn exercise_current_transitions(fixture: &mut Fixture, binaries: [&Path; 3]) -> Result<()> {
    let identities = artifacts(fixture, binaries)?;
    let provider = fixture.runtime.block_on(MockServer::start());
    // The actual full artifact creates the fresh dataset and initial owner.
    let (config, pid) = fixture.configure_and_start_full(binaries[0])?;
    let report = fixture.status(binaries[0], &identities[0], &identities[0], pid)?;
    fixture.health(&report)?;
    fixture.stop_resident(pid)?;
    // Append a small, known current local journal with the original writer and
    // real claim/stop APIs. No historical rows or fabricated owner SQL is used.
    seed_current_history(fixture)?;
    let config = format!("{config}{}", providers(&provider.uri()));
    fs::write(&fixture.config, &config)?;
    let original = snapshot(fixture, OWNER)?;
    ensure!(
        original.ledger.len() == 1 && original.ledger[0].0 == "m20261118_000001_model_policy_store"
    );
    for (rows, expected) in original.rows.iter().zip([1, 1, 4, 1, 0, 0]) {
        ensure!(rows.len() == expected);
    }
    for (step, index) in [0, 1, 0, 2, 0].into_iter().enumerate() {
        let previous = snapshot(fixture, OWNER)?;
        let pid = fixture.start(binaries[index])?;
        let report =
            fixture.status(binaries[index], &identities[index], &identities[index], pid)?;
        fixture.health(&report)?;
        if index == 1 {
            ensure!(
                !agent_local::socket_path(&fixture.socket).exists(),
                "minimum profile published a harness listener"
            );
        } else {
            inspect_harness(fixture, index == 0)?;
        }
        provider_idle(fixture, &provider)?;
        let running = snapshot(fixture, OWNER)?;
        ensure!(
            running.preserved(&original),
            "profile step {step} changed current history or its original owner"
        );
        fixture.stop_resident(pid)?;
        let after = snapshot(fixture, OWNER)?;
        ensure!(
            after.preserved(&original),
            "profile step {step} changed current history on shutdown"
        );
        if index != 0 {
            ensure!(
                running.owners == previous.owners
                    && running.authority == previous.authority
                    && after.owners == previous.owners
                    && after.authority == previous.authority,
                "core-free profile acquired or changed native ownership"
            );
        } else {
            let before_owner = owner_head(&previous)?;
            let live_owner = owner_head(&running)?;
            let stopped_owner = owner_head(&after)?;
            ensure!(
                before_owner.2.is_some()
                    && live_owner.0 == before_owner.0 + 1
                    && live_owner.1 != before_owner.1
                    && live_owner.2.is_none()
            );
            ensure!(
                stopped_owner.0 == live_owner.0
                    && stopped_owner.1 == live_owner.1
                    && stopped_owner.2.is_some()
            );
            ensure!(
                running.owners.len() == previous.owners.len() + 1
                    && after.owners.len() == running.owners.len()
            );
        }
        ensure!(
            fs::read_to_string(&fixture.config)? == config,
            "profile step {step} changed saved configuration"
        );
        provider_idle(fixture, &provider)?;
    }
    Ok(())
}

fn binaries() -> Result<[PathBuf; 3]> {
    let binaries = [
        artifact_path("BITROUTER_TEST_FULL_BINARY")?,
        artifact_path("BITROUTER_TEST_GATEWAY_BINARY")?,
        artifact_path("BITROUTER_TEST_HARNESS_BINARY")?,
    ];
    ensure!(binaries[0] != binaries[1] && binaries[0] != binaries[2] && binaries[1] != binaries[2]);
    Ok(binaries)
}

#[test]
#[ignore = "requires separately built full, minimum and harness-only artifacts"]
fn historical_dataset_is_refused_by_full_minimum_and_harness_artifacts() -> Result<()> {
    let binaries = binaries()?;
    let mut fixture = Fixture::new()?;
    let result = exercise_old_refusal(&mut fixture, [&binaries[0], &binaries[1], &binaries[2]]);
    let cleanup = fixture.cleanup();
    result.with_context(|| {
        format!(
            "historical dataset refusal failed\n{}",
            fixture.diagnostics()
        )
    })?;
    cleanup
}

#[test]
#[ignore = "requires separately built full, minimum and harness-only artifacts"]
fn current_records_survive_full_minimum_full_harness_full_transitions() -> Result<()> {
    let binaries = binaries()?;
    let mut fixture = Fixture::new()?;
    let result =
        exercise_current_transitions(&mut fixture, [&binaries[0], &binaries[1], &binaries[2]]);
    let cleanup = fixture.cleanup();
    result.with_context(|| {
        format!(
            "current profile transition failed\n{}",
            fixture.diagnostics()
        )
    })?;
    cleanup
}
