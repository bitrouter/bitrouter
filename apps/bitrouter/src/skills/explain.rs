//! Original skill evidence from the selected database, without an App or daemon.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use bitrouter_orchestrator::core::skills::explain::Explanation;
use bitrouter_orchestrator::harness::managed::session::NativeSession;
use bitrouter_sdk::caller::CallerContext;
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use serde::{Deserialize, Serialize};

#[derive(Debug, clap::Args)]
#[command(group(clap::ArgGroup::new("runtime").args(["thread", "managed_session"]).required(true)))]
pub struct Options {
    /// Original native Thread identity, with its external Turn ID.
    #[arg(long, requires = "turn")]
    pub thread: Option<String>,
    #[arg(long, requires = "thread")]
    pub turn: Option<String>,
    /// Original managed native session; distinct from a Thread or ACP run.
    #[arg(long, requires = "request_id")]
    pub managed_session: Option<String>,
    #[arg(long, requires = "managed_session")]
    pub request_id: Option<String>,
    /// Narrow to an exact Core agent Turn within the original input.
    #[arg(long)]
    pub agent_turn: Option<String>,
    #[arg(short, long, conflicts_with = "database_url")]
    pub config: Option<PathBuf>,
    /// Explicit history database; never initializes a missing database.
    #[arg(long, conflicts_with = "config")]
    pub database_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub operation: &'static str,
    pub selector: Selector,
    pub explanation: Option<Explanation>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Selector {
    NativeThread {
        thread_id: String,
        turn_id: String,
        recorded_server_instance_id: String,
        cutoff: u64,
    },
    ManagedNative {
        managed_session: String,
        request_id: String,
        grant: bitrouter_orchestrator::core::protocol::OwnershipGrant,
        head: bitrouter_orchestrator::core::checkpoint::DurableHead,
        released: bool,
    },
}

pub async fn read(options: Options) -> Result<Report> {
    for id in [
        options.thread.as_deref(),
        options.turn.as_deref(),
        options.request_id.as_deref(),
        options.agent_turn.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        crate::control_output::validate_id(id)?;
    }
    if let Some(session) = &options.managed_session {
        crate::managed_store::validate_session_name(session)?;
    }
    ensure!(
        options.thread.is_some() != options.managed_session.is_some(),
        "select exactly one native runtime identity"
    );
    ensure!(
        options.thread.is_some() == options.turn.is_some()
            && options.managed_session.is_some() == options.request_id.is_some(),
        "supply the original Turn or managed input identity"
    );
    let db = database(&options).await?;
    let mut report = if let (Some(thread), Some(turn)) = (options.thread, options.turn) {
        let store = crate::agent_store::DatabaseExecutionStore::new(db);
        let inspection = bitrouter_orchestrator::service::skills::inspect(
            &store,
            &CallerContext::local(),
            &thread,
            &turn,
        )
        .await
        .map_err(anyhow::Error::msg)?;
        Report {
            operation: "explain",
            selector: Selector::NativeThread {
                thread_id: inspection.thread_id,
                turn_id: inspection.turn_id,
                recorded_server_instance_id: inspection.recorded_server_instance_id,
                cutoff: inspection.cutoff,
            },
            explanation: inspection.explanation,
        }
    } else {
        let session = options.managed_session.context("missing managed session")?;
        let request_id = options
            .request_id
            .context("missing original managed input")?;
        let store = crate::managed_store::DatabaseNativeStore::new(db, session.clone())?;
        let stored = NativeSession::inspect(&store)
            .await?
            .context("managed native session is not in the selected history database")?;
        let snapshot = stored
            .snapshot
            .context("managed session has no acknowledged Core checkpoint")?;
        let receipt = snapshot
            .operations
            .get(&request_id)
            .context("input is not in the selected managed session")?;
        let run_id = receipt
            .assigned_ids
            .get("run_id")
            .context("operation is not an accepted managed input")?;
        ensure!(
            receipt.error.is_none()
                && receipt.disposition
                    == bitrouter_orchestrator::core::protocol::OperationDisposition::Accepted
                && receipt.assigned_ids.get("agent_id") == Some(&snapshot.agent_id)
                && receipt.assigned_ids.contains_key("agent_turn_id"),
            "operation is not an accepted managed input"
        );
        let explanation =
            bitrouter_orchestrator::core::skills::explain::capture(&snapshot, run_id)?;
        Report {
            operation: "explain",
            selector: Selector::ManagedNative {
                managed_session: session,
                request_id,
                grant: stored.grant,
                head: stored.head,
                released: stored.released,
            },
            explanation: Some(explanation),
        }
    };
    if let Some(agent_turn) = options.agent_turn {
        let explanation = report
            .explanation
            .as_mut()
            .context("no native skill facts were recorded for this input")?;
        explanation
            .work_units
            .retain(|work| work.agent_turn_id == agent_turn);
        ensure!(
            !explanation.work_units.is_empty(),
            "agent Turn does not belong to this original input"
        );
    }
    Ok(report)
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct DatabaseLocation {
    database: bitrouter_sdk::config::DatabaseConfig,
}

async fn database(options: &Options) -> Result<DatabaseConnection> {
    let location = if let Some(url) = &options.database_url {
        crate::db::anchor_url(
            &bitrouter_sdk::config::substitute_env(url)?,
            &std::env::current_dir()?,
        )
    } else {
        let source = crate::paths::resolve_config(options.config.as_deref())?;
        // Inspect only the database address. Broken provider/MCP configuration
        // cannot require loading hosts or hide already committed history.
        let location: DatabaseLocation = match &source {
            crate::paths::ConfigSource::File(path) => {
                serde_saphyr::from_str(&tokio::fs::read_to_string(path).await?)?
            }
            crate::paths::ConfigSource::Default { .. } => DatabaseLocation::default(),
        };
        crate::db::anchor_url(
            &bitrouter_sdk::config::substitute_env(&location.database.url)?,
            source.home(),
        )
    };
    let location = if location.starts_with("sqlite:") {
        ensure!(
            !location.contains(":memory:"),
            "an in-memory database has no stored execution history"
        );
        let (path, query) = location.split_once('?').unwrap_or((&location, ""));
        let mut parameters = url::form_urlencoded::parse(query.as_bytes())
            .filter(|(key, _)| key != "mode")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        parameters.push(("mode".into(), "ro".into()));
        format!(
            "{path}?{}",
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(parameters)
                .finish()
        )
    } else {
        location
    };
    let mut connection = ConnectOptions::new(location);
    connection.sqlx_logging(false).max_connections(1);
    // No migrations, journal-mode writes, provider warmup or owner acquisition.
    Database::connect(connection).await.map_err(|_| {
        anyhow::anyhow!(
            "cannot open stored execution history; check the selected database location"
        )
    })
}

impl crate::output::CliReport for Report {
    fn render(&self, human: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        match &self.selector {
            Selector::NativeThread {
                thread_id, turn_id, ..
            } => human.line(&format!("Thread {thread_id} / Turn {turn_id}"))?,
            Selector::ManagedNative {
                managed_session,
                request_id,
                ..
            } => human.line(&format!(
                "Managed session {managed_session} / input {request_id}"
            ))?,
        }
        let Some(explanation) = &self.explanation else {
            return human.line("No native skill evidence recorded for this Turn.");
        };
        if explanation.work_units.is_empty() {
            human.line("No native skill selection recorded for this input.")?;
        }
        for work in &explanation.work_units {
            human.line(&format!(
                "agent Turn {} ({:?})",
                work.agent_turn_id, work.status
            ))?;
            let Some(selection) = &work.selection else {
                human.line("  Skill selection not recorded.")?;
                continue;
            };
            if let Some(error) = &selection.failure {
                human.line(&format!(
                    "  admission failed: {}",
                    error.message.escape_debug()
                ))?;
            }
            if selection.candidates.is_empty() {
                human.line("  No available skill candidates.")?;
            }
            for problem in &selection.catalog.mcp.excluded {
                human.line(&format!(
                    "  MCP {}: {}",
                    problem.server.escape_debug(),
                    problem.reason.escape_debug()
                ))?;
            }
            for problem in &selection.catalog.local.problems {
                human.line(&format!("  library: {}", problem.escape_debug()))?;
            }
            for candidate in &selection.candidates {
                let source = match &candidate.source {
                    bitrouter_orchestrator::core::skills::SkillSource::Local(binding) => {
                        format!("{:?}:{}", binding.scope, binding.directory_id)
                    }
                    bitrouter_orchestrator::core::skills::SkillSource::Mcp(held) => format!(
                        "mcp:{}:{}",
                        held.origin.server,
                        held.entry.uri.escape_debug()
                    ),
                };
                human.line(&format!(
                    "  {} [{source}]: {}; {}; {} prepared model requests; {}",
                    candidate.source.name(),
                    if candidate.selected {
                        "selected"
                    } else if !selection.selected && selection.failure.is_none() {
                        "selection pending"
                    } else {
                        "excluded"
                    },
                    if candidate.instructions.is_some() {
                        "verified instructions recorded"
                    } else {
                        "instructions not loaded"
                    },
                    candidate.prepared_model_requests.len(),
                    candidate.reason.escape_debug()
                ))?;
                if let Some(error) = &candidate.failure {
                    human.line(&format!("    {}", error.message.escape_debug()))?;
                }
                for file in &candidate.supporting_files {
                    human.line(&format!(
                        "    file {}: {} bytes; sha256={}; {} prepared model requests",
                        file.reference.escape_debug(),
                        file.bytes,
                        file.sha256,
                        candidate
                            .prepared_model_requests
                            .iter()
                            .filter(|request| request.supporting_files.contains(file))
                            .count()
                    ))?;
                }
            }
        }
        Ok(())
    }
}
