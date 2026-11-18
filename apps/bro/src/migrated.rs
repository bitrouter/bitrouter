//! Read the public history projection and managed checkpoint evidence from the
//! split M0 store. Only offline-verified, sealed history can enter the local worker.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use bitrouter_ai::core::checkpoint::{CheckpointAck, DurableHead};
use bitrouter_ai::core::protocol::{OwnershipGrant, ToolExecute, ToolResult};
use bitrouter_ai::harness::agent::ToolMode;
use bitrouter_ai::harness::thread::ThreadEvent;
use clap::Subcommand;
use futures::TryStreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{ConnectOptions, SqliteConnection, sqlite::SqliteConnectOptions};

use crate::{Command, ThreadCommand, emit};

#[derive(Subcommand)]
pub(crate) enum ManagedCommand {
    /// List imported session row identities.
    List,
    /// Inspect the recorded checkpoint ACK, starts and confirmed tool outcomes.
    Show { id: String },
    /// Bind the sealed thread to a local workspace and explicitly selected endpoint.
    Restore {
        thread_id: uuid::Uuid,
        #[arg(long)]
        workspace: PathBuf,
    },
}

const LEDGER: &str = "m20261118_000004_bro_sqlite";

fn regular(path: &Path) -> Result<bool> {
    match path.symlink_metadata() {
        Ok(meta) => {
            ensure!(
                meta.is_file(),
                "invalid transferred store path {}",
                path.display()
            );
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn reject_gateway_home(root: &Path) -> Result<()> {
    for name in [
        "bitrouter.db",
        "bitrouter.db-wal",
        "bitrouter.db-journal",
        "bitrouter.db-shm",
        "bitrouter.yaml",
        "bitrouter.start.lock",
    ] {
        match root.join(name).symlink_metadata() {
            Ok(_) => bail!("gateway home cannot be used as BRO home ({name})"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) fn present(root: &Path) -> Result<bool> {
    reject_gateway_home(root)?;
    let store = regular(&root.join("store.sqlite"))?;
    ensure!(
        store || !regular(&root.join("ACTIVATED"))?,
        "activated BRO home is missing its transferred store"
    );
    Ok(store)
}

async fn verify_catalog(db: &mut SqliteConnection, sealed: bool) -> Result<()> {
    let tables = [
        "acp_assessment_revisions",
        "acp_canonical_sessions",
        "acp_capture_connections",
        "acp_capture_events",
        "acp_checkpoint_members",
        "acp_checkpoints",
        "acp_effective_assessments",
        "acp_resource_observations",
        "adequacy_exploration",
        "adequacy_pins",
        "adequacy_reliability_events",
        "adequacy_semantic_success",
        "bro_acceptance_keys",
        "bro_execution_index",
        "bro_execution_records",
        "bro_executions",
        "bro_managed_sessions",
        "bro_runtime_owners",
        "checkpoint_evolution",
    ];
    let mut expected = tables.to_vec();
    expected.extend(["bro_runtime_ownership", "seaql_migrations"]);
    expected.sort_unstable();
    let actual:Vec<String> = sqlx::query_scalar("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*' ORDER BY name").fetch_all(&mut *db).await?;
    ensure!(actual == expected, "unexpected transferred BRO catalog");
    if sealed {
        return Ok(());
    } // The activation marker binds all immutable rows by digest.
    for table in tables {
        let sql = format!("SELECT EXISTS(SELECT 1 FROM {table})");
        ensure!(
            !sqlx::query_scalar::<_, bool>(&sql)
                .fetch_one(&mut *db)
                .await?,
            "activated BRO store has retained {table} work; dispatch refused"
        );
    }
    let (owners,empty):(i64,i64) = sqlx::query_as("SELECT count(*),count(CASE WHEN id=1 AND generation=0 AND server_instance_id IS NULL AND stopped_at_ms IS NULL THEN 1 END) FROM bro_runtime_ownership").fetch_one(db).await?;
    ensure!(
        (owners, empty) == (1, 1),
        "activated BRO store has a retained owner"
    );
    Ok(())
}

// true means the transferred store handled the command. A fresh worker may
// open this home only after M4 published and verified the activated split.
pub(super) async fn dispatch(
    root: &Path,
    command: &Command,
    endpoint: Option<&str>,
) -> Result<bool> {
    let mut db = SqliteConnectOptions::new()
        .filename(root.join("store.sqlite"))
        .read_only(true)
        .immutable(true)
        .connect()
        .await?;
    let versions: Vec<String> =
        sqlx::query_scalar("SELECT version FROM seaql_migrations ORDER BY version")
            .fetch_all(&mut db)
            .await?;
    let db_seal = versions
        .first()
        .and_then(|v| v.strip_prefix(LEDGER))
        .and_then(|v| v.strip_prefix(":sealed-v1:"));
    ensure!(
        versions == [LEDGER]
            || (versions.len() == 1
                && db_seal
                    .is_some_and(|d| bitrouter_ai::core::checkpoint::validate_digest(d).is_ok())),
        "unsupported store owner or migration marker"
    );
    let application: i64 = sqlx::query_scalar("PRAGMA main.application_id")
        .fetch_one(&mut db)
        .await?;
    ensure!(
        matches!(application, 0 | 0x42525354),
        "unrecognized BRO SQLite application ID"
    );
    let marker = root.join("ACTIVATED");
    if regular(&marker)? {
        ensure!(
            application == 0,
            "staged inspection-only SQLite image cannot be activated by a marker"
        );
        for sidecar in ["store.sqlite-wal", "store.sqlite-journal"] {
            let path = root.join(sidecar);
            ensure!(
                !regular(&path)? || std::fs::metadata(path)?.len() == 0,
                "activated BRO image has an unresolved SQLite journal"
            );
        }
        ensure!(
            std::fs::metadata(&marker)?.len() <= 256,
            "oversized BRO activation marker"
        );
        let raw = std::fs::read_to_string(&marker)?;
        let rest = raw
            .strip_prefix(&format!("{LEDGER}\n"))
            .context("invalid BRO activation marker")?;
        let sealed = !rest.is_empty();
        ensure!(
            sealed == db_seal.is_some(),
            "M0 activation and database seal disagree"
        );
        let origins = if let Some(digest) = db_seal {
            verify_sealed(root, rest, digest)?
        } else {
            BTreeMap::new()
        };
        verify_catalog(&mut db, sealed).await?;
        if let Command::Managed {
            action:
                ManagedCommand::Restore {
                    thread_id,
                    workspace,
                },
        } = command
        {
            let origin = origins
                .get(thread_id)
                .context("no verified M0 transfer for thread")?;
            let home = crate::checkpoint::Home::open(root)?;
            let _guard = home.worker(*thread_id)?;
            let mut thread = home.load(*thread_id)?;
            verify_prefix(origin, &thread)?;
            let endpoint =
                endpoint.context("select a gateway endpoint for the transferred thread")?;
            crate::client(endpoint)?;
            let workspace = workspace.canonicalize()?;
            crate::tools::WorkspaceTools::new(&workspace, thread.tool_mode)?;
            if thread.endpoint.is_empty() {
                thread.workspace_id = Some(crate::workspace::WorkspaceId::read(&workspace)?);
                thread.workspace = workspace;
                thread.endpoint = endpoint.to_owned();
                home.admit(&thread)?;
            } else {
                ensure!(
                    thread.workspace == workspace && thread.endpoint == endpoint,
                    "thread already bound to another workspace or endpoint"
                );
                thread.verify_workspace()?;
            }
            crate::inspect(&thread, None, false)?;
            return Ok(true);
        }
        if !matches!(command, Command::Managed { .. }) {
            return Ok(false);
        }
    }
    match command {
        Command::Thread {
            action: ThreadCommand::List,
        } => {
            let mut rows = sqlx::query_as::<_, (String, i64, i64)>(
                "SELECT id,version,format_version FROM bro_executions ORDER BY id",
            )
            .fetch(&mut db);
            while let Some((id, version, format)) = rows.try_next().await? {
                emit(
                    &json!({"thread_id":id,"cursor":version,"format_version":format,"dispatch_allowed":false}),
                )?;
            }
            Ok(())
        }
        Command::Thread {
            action: ThreadCommand::History { thread_id },
        } => history(&mut db, &thread_id.to_string()).await,
        Command::Managed {
            action: ManagedCommand::List,
        } => {
            let mut rows = sqlx::query_as::<_, (String, i64, i64)>(
                "SELECT id,revision,payload_bytes FROM bro_managed_sessions ORDER BY id",
            )
            .fetch(&mut db);
            while let Some((id, revision, bytes)) = rows.try_next().await? {
                emit(
                    &json!({"id":id,"revision":revision,"payload_bytes":bytes,"dispatch_allowed":false}),
                )?;
            }
            Ok(())
        }
        Command::Managed {
            action: ManagedCommand::Show { id },
        } => managed(&mut db, id).await,
        _ => bail!(
            "staged M0 store is inspection-only; use thread history or managed show. Native execution ownership has not been recovered"
        ),
    }?;
    Ok(true)
}

fn verify_sealed(
    root: &Path,
    marker: &str,
    db_seal: &str,
) -> Result<BTreeMap<uuid::Uuid, crate::checkpoint::Thread>> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    let mut file = std::fs::File::open(root.join("store.sqlite"))?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = std::io::Read::read(&mut file, &mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    let path = root.join("sealed.json");
    ensure!(regular(&path)?, "missing sealed thread projections");
    let bytes = std::fs::read(path)?;
    ensure!(
        hex::encode(Sha256::digest(&bytes)) == db_seal
            && marker == format!("sealed-v1 {} {}\n", hex::encode(hash.finalize()), db_seal),
        "M0 transfer digest mismatch"
    );
    let origins: BTreeMap<uuid::Uuid, crate::checkpoint::Thread> = serde_json::from_slice(&bytes)?;
    ensure!(!origins.is_empty(), "missing sealed threads");
    let home = crate::checkpoint::Home::open(root)?;
    for (id, origin) in &origins {
        ensure!(
            *id == origin.thread_id && regular(&root.join("threads").join(format!("{id}.json")))?,
            "missing or misidentified transferred checkpoint"
        );
        verify_prefix(origin, &home.load(*id)?)?;
    }
    Ok(origins)
}

fn verify_prefix(origin: &crate::checkpoint::Thread, t: &crate::checkpoint::Thread) -> Result<()> {
    ensure!(
        t.thread_id == origin.thread_id
            && t.parent_id == origin.parent_id
            && t.managed() == origin.managed()
            && serde_json::to_value(&t.skills)? == serde_json::to_value(&origin.skills)?
            && t.model == origin.model
            && t.m0_instructions == origin.m0_instructions
            && t.tool_mode == origin.tool_mode
            && t.effort == origin.effort
            && t.max_output_tokens == origin.max_output_tokens
            && !origin.turns.is_empty()
            && t.turns.len() >= origin.turns.len()
            && t.turns[origin.turns.len()..]
                .iter()
                .all(|turn| turn.sealed_messages.is_empty())
            && (!t.endpoint.is_empty()
                || (t.workspace == origin.workspace && t.turns.len() == origin.turns.len()))
            && serde_json::to_value(&t.turns[..origin.turns.len()])?
                == serde_json::to_value(&origin.turns)?,
        "local thread identity or sealed M0 history differs from verified transfer"
    );
    Ok(())
}

#[derive(Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
enum PublicFact {
    ThreadEvent { event: ThreadEvent },
}

async fn history(db: &mut SqliteConnection, id: &str) -> Result<()> {
    let (head, format) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT version,format_version FROM bro_executions WHERE id=?",
    )
    .bind(id)
    .fetch_one(&mut *db)
    .await?;
    ensure!(format == 7 && head >= 0, "unsupported M0 execution format");
    let (count,first,last):(i64,Option<i64>,Option<i64>) = sqlx::query_as("SELECT count(*),min(sequence),max(sequence) FROM bro_execution_records WHERE execution_id=?").bind(id).fetch_one(&mut *db).await?;
    ensure!(
        count == head && (head == 0 || (first == Some(1) && last == Some(head))),
        "incomplete M0 execution journal"
    );
    // The M0 writer always leaves public ThreadEvent rows uncompressed; private
    // checkpoint chunks may use the bro-zstd envelope and are never dispatched.
    let mut rows = sqlx::query_as::<_,(i64,Option<String>)>(
        "SELECT sequence, CASE WHEN length(CAST(payload AS BLOB)) <= 4194304 THEN payload END FROM bro_execution_records WHERE execution_id=? AND substr(payload,1,length(?))=? ORDER BY sequence"
    ).bind(id).bind("{\"record\":\"thread_event\",").bind("{\"record\":\"thread_event\",").fetch(db);
    while let Some((seq, payload)) = rows.try_next().await? {
        let PublicFact::ThreadEvent { event } =
            serde_json::from_str(&payload.context("oversized M0 history event")?)?;
        ensure!(
            event.thread_id == id && i64::try_from(event.seq)? == seq,
            "M0 event identity or cursor mismatch"
        );
        emit(&json!({"type":"event","event":event}))?;
    }
    emit(&json!({"type":"history_end","thread_id":id,"cursor":head,"dispatch_allowed":false}))
}

#[derive(Deserialize)]
struct Journal {
    format: u32,
    workspace: PathBuf,
    mode: ToolMode,
    grant: OwnershipGrant,
    head: DurableHead,
    ack: Option<CheckpointAck>,
    checkpoint: Option<bitrouter_ai::core::checkpoint::CheckpointBatch>,
    released: bool,
    starts: BTreeMap<String, StartedTool>,
}

#[derive(Deserialize, serde::Serialize)]
struct StartedTool {
    command: ToolExecute,
    started: bool,
    result: Option<ToolResult>,
}

async fn managed(db: &mut SqliteConnection, id: &str) -> Result<()> {
    let (revision,actual,scoped,payload) = sqlx::query_as::<_,(i64,i64,bool,Option<String>)>(
        "SELECT revision,payload_bytes,installation_id IS NOT NULL,CASE WHEN payload_bytes BETWEEN 1 AND 134217728 AND payload_bytes=length(CAST(payload AS BLOB)) THEN payload END FROM bro_managed_sessions WHERE id=?"
    ).bind(id).fetch_one(db).await?;
    ensure!(revision > 0, "invalid managed revision");
    let payload = payload.context("invalid managed session payload length")?;
    let j: Journal = if scoped {
        let mut envelope: Value = serde_json::from_str(&payload)?;
        ensure!(
            matches!(
                (
                    envelope["format"].as_u64(),
                    envelope.get("recovery").is_some_and(|v| !v.is_null())
                ),
                (Some(1), false) | (Some(2), true)
            ),
            "unsupported owned session envelope"
        );
        let journal = envelope
            .get_mut("journal")
            .context("unsupported owned session envelope")?
            .take();
        serde_json::from_value(journal)?
    } else {
        serde_json::from_str(&payload)?
    };
    ensure!(j.format == 1, "unsupported managed journal format");
    j.grant.validate().map_err(|e| anyhow::anyhow!(e.message))?;
    j.head.validate().map_err(|e| anyhow::anyhow!(e.message))?;
    match (&j.ack, &j.checkpoint) {
        (Some(ack), Some(batch)) => ensure!(
            ack.head() == j.head
                && batch.identity.batch_id == ack.batch_id
                && batch.identity.session_id == j.grant.session_id
                && batch.payload_sha256 == ack.payload_sha256,
            "managed checkpoint and ACK disagree"
        ),
        (None, None) if j.head == DurableHead::default() && !j.released => {}
        _ => bail!("managed journal has an incomplete checkpoint/ACK"),
    }
    for start in j.starts.values() {
        if let Some(result) = &start.result {
            ensure!(
                result.invocation_id == start.command.invocation_id
                    && result.attempt_id == start.command.attempt_id,
                "managed tool result identity mismatch"
            );
        }
    }
    emit(
        &json!({"id":id,"revision":revision,"payload_bytes":actual,"session_id":j.grant.session_id,
        "workspace":j.workspace,"tool_mode":j.mode,"head":j.head,"ack":j.ack,"released":j.released,
        "starts":j.starts,"dispatch_allowed":false}),
    )
}
