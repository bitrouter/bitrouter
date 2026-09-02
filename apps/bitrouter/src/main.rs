//! `bitrouter` CLI entry point — a thin shell over the `bitrouter` lib.
//!
//! Subcommand surface: `serve` / `start` / `stop` / `restart` /
//! `reload` / `status` / `route` / `init` / `key sign` / `models` / `tools` /
//! `policy create` / `providers (list|login|logout)` / `agents` /
//! `spawn` / `cloud` / `skills` / `mcp (serve|install|search|list|add)`.
//! Cloud-account sign-in lives under
//! `cloud (login|logout|whoami)`; per-provider credentials under
//! `providers (login|logout)`. Daemon control runs over a local IPC endpoint
//! (a Unix domain socket, or a Windows named pipe) — `start` spawns `serve`
//! detached; the client subcommands send one [`DaemonCommand`] each.
//!
//! OWS wallet integration is out of scope for v1.0 (it lives in the `ows`
//! workspace); a commented-out `Wallet` variant in `Command` reserves the
//! name for a future integration without shipping a non-functional command.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use bitrouter::commands;
use bitrouter::daemon::{self, DaemonCommand, DaemonResponse, RouteHop};
use bitrouter::output::reports::admin::{
    KeySignReport, PolicyCreateReport, ProviderLoginReport, ProviderLogoutReport,
};
use bitrouter::output::reports::agents::{
    AgentCheckRow, AgentInstallReport, AgentRegistryRow, AgentRow, AgentsCheckReport,
    AgentsListReport,
};
use bitrouter::output::reports::config::{UnsetVar, ValidateReport};
use bitrouter::output::reports::daemon::{
    DaemonActionReport, RouteHopView, RouteReport, StatusReport,
};
use bitrouter::output::reports::eval::EvalReport;
use bitrouter::output::reports::mcp::{McpAddReport, McpRegistryReport, McpRegistryRow};
use bitrouter::output::reports::observe::ObserveStatusReport;
use bitrouter::output::reports::optimization::{
    ArmReport, OptimizationControllerReport, TreatmentReport,
};
use bitrouter::output::reports::policy::PolicyReport;
use bitrouter::output::reports::routing::{ModelRow, ModelsReport, ProviderRow, ProvidersReport};
use bitrouter::output::reports::tools::{
    ServerStatusView, ServerToolsView, ToolInfo, ToolsDiscoverReport, ToolsListReport,
    ToolsStatusReport,
};
use bitrouter::output::reports::trajectory::{
    TrajectoryPruneReport, inspect_report as trajectory_inspect_report,
    replay_report as trajectory_replay_report,
};
use bitrouter::output::{CliReport, Output};
use bitrouter_sdk::config;

async fn supervise_http_shutdown<Http, Control, Hup, Term>(
    http: Http,
    control: Control,
    hup: Hup,
    term: Term,
    shutdown: tokio::sync::oneshot::Sender<()>,
) -> Result<()>
where
    Http: std::future::Future<Output = Result<()>> + Send,
    Control: std::future::Future<Output = Result<()>> + Send,
    Hup: std::future::Future<Output = Result<()>> + Send,
    Term: std::future::Future<Output = Result<()>> + Send,
{
    let mut http = Box::pin(http);
    let mut control = Box::pin(control);
    let mut hup = Box::pin(hup);
    let mut term = Box::pin(term);
    let mut hup_open = true;
    let trigger_result = loop {
        tokio::select! {
            result = &mut http => return result,
            result = &mut control => break result,
            result = &mut term => {
                if result.is_err() {
                    tracing::warn!(
                        reason = "termination_signal_listener_unavailable",
                        "termination-signal listener unavailable"
                    );
                }
                break Ok(());
            }
            result = &mut hup, if hup_open => {
                if result.is_err() {
                    tracing::warn!(
                        reason = "sighup_listener_unavailable",
                        "SIGHUP listener unavailable"
                    );
                }
                hup_open = false;
            }
        }
    };

    drop(control);
    drop(hup);
    drop(term);
    let _ = shutdown.send(());
    http.await?;
    trigger_result
}

/// BitRouter — an LLM API router.
#[derive(Parser)]
#[command(name = "bitrouter", version, about)]
struct Cli {
    /// Force JSON output (the default; agent-native). Conflicts with `--human`.
    #[arg(short = 'j', long, global = true, conflicts_with = "human")]
    json: bool,
    /// Render the human-readable view to stdout instead of JSON.
    #[arg(long, global = true)]
    human: bool,
    /// Compatibility spelling for `--human` when placed before the subcommand.
    #[arg(short = 'H', hide = true, conflicts_with = "json")]
    human_short: bool,
    /// No subcommand dispatches to the onboarding entry (`onboarding::entry`):
    /// the wizard when unconfigured, a one-line status + `bitrouter launch`
    /// hint when configured.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Load a config, run migrations, and serve HTTP + control socket
    /// **in the foreground**.
    Serve {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Spawn `bitrouter serve` as a detached background process.
    Start {
        /// Path to `bitrouter.yaml` (passed through to the child).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Path to redirect the daemon's stdout/stderr to. Defaults to
        /// `bitrouter.log` inside the config file's directory (e.g.
        /// `~/.bitrouter/bitrouter.log`) so it lives alongside the
        /// socket and pid file rather than in the launcher's CWD.
        #[arg(long)]
        log: Option<PathBuf>,
    },
    /// Send a `stop` command to a running daemon.
    Stop {
        /// Path to `bitrouter.yaml` (used to locate the control socket).
        /// Resolves via the standard chain: `./bitrouter.yaml` →
        /// `$BITROUTER_HOME/bitrouter.yaml` → `~/.bitrouter/bitrouter.yaml`.
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit control socket path. Overrides the config-derived path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// `stop` then `start` — config path is passed through.
    Restart {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit control socket path. Overrides the config-derived path.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Path to redirect the new daemon's stdout/stderr to. Defaults
        /// to `bitrouter.log` next to the config file.
        #[arg(long)]
        log: Option<PathBuf>,
    },
    /// Hot-reload the running daemon's config / routing table.
    Reload {
        /// Path to `bitrouter.yaml` (used to locate the control socket).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit control socket path. Overrides the config-derived path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Report a running daemon's status (pid, listen address, model count).
    /// Prints `running: no` when no daemon is reachable.
    ///
    /// With `--requests`, prints what the router has actually done instead:
    /// a table of settled requests (time, model, the provider that actually
    /// served, tokens, cost, latency, status) under a daemon-state line and
    /// over a spend rollup. Read straight from the metering store, so it works
    /// with no daemon running. Identical piped or not — repeat it with
    /// `watch -n1` for a live view.
    Status {
        /// Path to `bitrouter.yaml` (used to locate the control socket).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit control socket path. Overrides the config-derived path.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Print the settled-request table instead of one status line.
        #[arg(short, long)]
        requests: bool,
    },
    /// Resolve a model name through the routing table. Uses the running
    /// daemon if reachable, otherwise loads the config and resolves locally.
    Route {
        /// The model name to resolve.
        model: String,
        /// Path to `bitrouter.yaml` (used as the standalone fallback and
        /// to locate the control socket).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit control socket path. Overrides the config-derived path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Guided onboarding wizard that sequences credential, harness, and finish
    /// steps to first value. Interactive by default; `--yes` (or no TTY) runs
    /// it headlessly, emitting the JSON result envelope and never blocking on a
    /// human. Every prompt has a flag equivalent (below) so an agent can drive
    /// the whole thing. With `--yes` this also reproduces the classic
    /// starter-`bitrouter.yaml` scaffold (refusing to overwrite unless
    /// `--force`).
    Init {
        /// Path for the starter `bitrouter.yaml` write.
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
        /// Run non-interactively: process the flags below, never block, emit
        /// the JSON envelope, and scaffold the starter config.
        #[arg(short = 'y', long)]
        yes: bool,
        /// Allow overwriting an existing `bitrouter.yaml` when scaffolding.
        #[arg(long)]
        force: bool,
        /// Clear stored onboarding credentials (cloud session always; provider
        /// credentials after a confirm, or unconditionally under `--yes`) before
        /// running.
        #[arg(long)]
        reset: bool,
        /// (Step 1) Sign in to BitRouter Cloud via device-flow OAuth. Skipped
        /// and reported under `--yes` (a machine can't complete the device flow).
        #[arg(long)]
        cloud_login: bool,
        /// (Step 1) Seed the cloud credential from a `brk_` API key
        /// (non-interactive).
        #[arg(long, value_name = "BRK_KEY")]
        api_key: Option<String>,
        /// (Step 1) Log in to an upstream provider by id (repeatable). A paired
        /// `--provider-api-key` seeds it non-interactively; otherwise it is
        /// reported-and-skipped under `--yes`.
        #[arg(long = "provider", value_name = "ID")]
        providers: Vec<String>,
        /// (Step 1) API key for the `--provider` at the same position
        /// (repeatable).
        #[arg(long = "provider-api-key", value_name = "KEY")]
        provider_api_keys: Vec<String>,
        /// (Step 1) Accept the auto-detected credential(s) without prompting.
        #[arg(long)]
        use_detected: bool,
        /// (Step 2) Harness to drive: `claude` or `codex` (repeatable).
        #[arg(long = "harness", value_enum)]
        harnesses: Vec<bitrouter::spawn::SpawnAgent>,
        /// (Step 2) Never install a missing harness.
        #[arg(long)]
        no_install: bool,
        /// (Step 3) What to do at the end: `launch` | `serve` | `exit`.
        #[arg(long, value_enum)]
        after: Option<bitrouter::onboarding::AfterAction>,
        /// (Step 3) Model handed to the harness for this session only (not
        /// persisted).
        #[arg(long, value_name = "ID")]
        model: Option<String>,
        /// (Step 3) Write a starter `bitrouter.yaml` (the one sanctioned config
        /// write).
        #[arg(long)]
        write_config: bool,
    },
    /// Configuration tooling (validation against the published schema).
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Virtual-key management.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// List routable models for a config, optionally filtered by provider.
    Models {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Show only models declared by this provider.
        #[arg(short, long)]
        provider: Option<String>,
    },
    /// MCP server introspection — list/status/discover against the upstreams
    /// declared under `mcp_servers` in `bitrouter.yaml`. v1.0 does not maintain
    /// a global tool registry; these are one-shot queries.
    Tools {
        #[command(subcommand)]
        action: ToolsAction,
    },
    /// Observability inspection (OTel exporter state, cardinality usage).
    Observe {
        #[command(subcommand)]
        action: ObserveAction,
    },
    /// Policy management.
    Policy {
        #[command(subcommand)]
        action: PolicyAction,
    },
    /// Evaluator-neutral evidence exchange.
    Eval {
        #[command(subcommand)]
        action: EvalAction,
    },
    /// Advance or inspect history-driven routing optimization.
    Optimize {
        #[command(subcommand)]
        action: OptimizeAction,
    },
    /// Inspect, replay, and retain durable trajectory history in the local database.
    Trajectory {
        /// Path to `bitrouter.yaml`. Uses the standard config resolution chain
        /// when omitted.
        #[arg(short, long, global = true)]
        config: Option<PathBuf>,
        #[command(subcommand)]
        action: TrajectoryAction,
    },
    /// Provider management.
    Providers {
        #[command(subcommand)]
        action: ProviderAction,
    },
    // Reserved for a future OWS wallet integration (delivered by the `ows`
    // workspace, not bitrouter). Intentionally commented out so v1.0 ships no
    // non-functional `wallet` command; uncomment this variant AND restore its
    // match arm in `run` when wiring OWS in.
    // Wallet,
    /// ACP agent lifecycle — list the catalog, check configured agents,
    /// print install stubs.
    Agents {
        #[command(subcommand)]
        action: AgentsAction,
    },
    /// Launch a coding-agent harness as an interactive native-TUI child. Routed
    /// harnesses are pointed at the local BitRouter daemon; own-auth harnesses
    /// launch directly and are not redirected. The human drives the harness's
    /// own TUI directly (use `bitrouter spawn` for headless ACP sub-agents).
    /// Follows `cargo run`'s separator convention: bitrouter options come
    /// before `--`, everything after `--` is forwarded to the agent verbatim,
    /// e.g. `bitrouter launch -a codex -- --search`.
    ///
    /// Harnesses that route by env/args (claude, codex) are launched without
    /// touching any config file. Those that route by synthesized config
    /// (opencode, pi, hermes, openclaw) get it under `.bitrouter/launch/` —
    /// your own agent config is still never modified.
    ///
    /// The agent authenticates to BitRouter with `BITROUTER_API_KEY` when it is
    /// set; otherwise a local placeholder is used (fine under the `skip_auth`
    /// default written by `bitrouter init`). A missing `claude` / `codex`
    /// binary is offered for install via its official native installer; other
    /// harnesses report their own install command instead.
    Launch {
        /// Which agent harness to launch: any catalog harness with an
        /// interactive binary (`claude`, `codex`, `opencode`, `pi`, `hermes`,
        /// `openclaw`, `grok`, or `agy`; catalog ids such as `claude-acp`,
        /// `codex-acp`, `pi-acp`, and `hermes-acp` also resolve). Own-auth
        /// harnesses such as `grok` and `agy` launch direct and are not
        /// redirected.
        #[arg(short, long, value_name = "ID")]
        agent: String,
        /// Pin the harness's model to a daemon-routable id (e.g. the explicit
        /// `provider/model` form). Applied through whatever mechanism the
        /// harness has — a model env var, a `-c model=` override, the
        /// synthesized config's default, or the harness's own flag for the
        /// own-auth clients (grok, agy).
        #[arg(long, value_name = "ID")]
        model: Option<String>,
        /// Path to `bitrouter.yaml` (used to derive the daemon base URL).
        /// When omitted, the binary resolves in this order: `./bitrouter.yaml`
        /// → `$BITROUTER_HOME/bitrouter.yaml` → `~/.bitrouter/bitrouter.yaml`
        /// → zero-config in-memory defaults.
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Override the BitRouter daemon base URL used by routed harnesses
        /// instead of deriving it from `server.listen`. Own-auth harnesses are
        /// not redirected.
        #[arg(long)]
        base_url: Option<String>,
        /// Never offer to install a missing agent — fail with the install
        /// command instead. (Auto-implied when stdin is not a TTY.)
        #[arg(long)]
        no_install: bool,
        /// Never auto-start a local BitRouter daemon when none is running —
        /// just warn. (A `--base-url` or non-local target is never auto-started
        /// regardless.)
        #[arg(long)]
        no_start: bool,
        /// Check the agent binary, BitRouter base URL, and route compatibility
        /// without launching the agent.
        #[arg(long)]
        check: bool,
        /// Arguments forwarded verbatim to the agent binary. Everything after
        /// `--` lands here.
        #[arg(last = true, allow_hyphen_values = true)]
        agent_args: Vec<String>,
    },
    /// Spawn an ACP-compatible harness as a headless *sub-agent*. Routing is
    /// attempted by default when the harness supports headless redirection;
    /// config-synthesis-only catalog agents warn and run direct. Pick a mode:
    /// `-p "<text>"` streams one prompt as NDJSON then exits; `--serve`
    /// speaks ACP over stdio for a GUI/manager; `--check` preflights the route.
    /// Pass `--direct` to bypass daemon routing. (For an interactive native TUI
    /// use `bitrouter launch`.)
    Spawn {
        /// ACP agent id: a bundled-catalog id (`claude-acp`, `codex-acp`,
        /// `gemini-cli`, `opencode`, `pi-acp`, `hermes-acp`, `openclaw`) or a
        /// configured `agents:` entry. A catalog id needs no config entry; run
        /// `--check` to see whether it will route or run direct in headless mode.
        agent: Option<String>,
        /// Send one prompt, stream NDJSON to stdout, then exit.
        #[arg(short = 'p', long, value_name = "TEXT")]
        prompt: Option<String>,
        /// Serve the session as a vanilla ACP Agent over stdio (GUI/manager).
        #[arg(long, conflicts_with = "prompt")]
        serve: bool,
        /// Preflight the harness + route without launching anything.
        #[arg(long, conflicts_with_all = ["prompt", "serve"])]
        check: bool,
        /// Do NOT route through the daemon — let the harness use its own
        /// provider auth. Routing is attempted by default when the harness
        /// supports headless redirection.
        #[arg(long)]
        direct: bool,
        /// Pin the harness's model (via its model env var / `-c model=`).
        #[arg(long)]
        model: Option<String>,
        /// Override the gateway base URL (else derived from `server.listen`).
        #[arg(long)]
        base_url: Option<String>,
        /// Never auto-start a local daemon when none is running — fail fast.
        #[arg(long)]
        no_start: bool,
        /// Per-turn deadline in seconds.
        #[arg(long, value_name = "SECS")]
        turn_timeout: Option<u64>,
        /// (with `-p`) Return immediately after submitting the prompt.
        #[arg(long, requires = "prompt")]
        no_wait: bool,
        /// (with `-p`) JSON Schema — inline JSON or `@path` — the subagent's
        /// final reply must satisfy. The schema rides the prompt; the terminal
        /// NDJSON `result` line gains `result`/`schema_ok` fields, with one
        /// repair re-prompt on invalid output (then `schema_ok:false` + raw).
        #[arg(
            long,
            value_name = "JSON|@PATH",
            requires = "prompt",
            conflicts_with = "no_wait"
        )]
        result_schema: Option<String>,
        /// Path to `bitrouter.yaml`. Resolves via the standard chain when
        /// omitted.
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Deprecated: the interactive form `spawn --agent <claude|codex>`
        /// (also `-a`) moved to `bitrouter launch`. Kept as a migration alias.
        #[arg(long = "agent", short = 'a', hide = true, value_enum)]
        legacy_agent: Option<bitrouter::spawn::SpawnAgent>,
        /// Deprecated (`--agent` path only): forwarded to `launch`.
        #[arg(long, hide = true)]
        no_install: bool,
        /// Forwarded verbatim to the interactive agent in the deprecated
        /// `--agent` path (everything after `--`).
        #[arg(last = true, allow_hyphen_values = true)]
        agent_args: Vec<String>,
    },
    /// Manage your BitRouter Cloud account — sign in/out, namespaces, keys,
    /// usage, requests, billing, policies, budgets, presets, and BYOK. Start
    /// with `cloud login`.
    Cloud {
        #[command(subcommand)]
        action: bitrouter::cloud::cli::CloudAction,
    },
    /// Inspect installed Agent Skills and scaffold a local `SKILL.md`.
    Skills {
        #[command(subcommand)]
        action: bitrouter::skills::cli::SkillsAction,
    },
    /// Run or install BitRouter's origin MCP server, and discover upstream
    /// MCP servers from the official registry (`mcp search` / `list` /
    /// `add`).
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// Workflow-state trace/replay utilities.
    WorkflowState {
        #[command(subcommand)]
        action: WorkflowStateAction,
    },
    /// Update the installed `bitrouter` binary in place to the latest release.
    /// Follows prereleases by default while pre-1.0. For Homebrew / `cargo
    /// install` installs it prints the right upgrade command instead.
    Update {
        /// Report whether a newer version exists, then exit without changing
        /// anything.
        #[arg(long)]
        check: bool,
        /// Update (or downgrade) to a specific release tag, e.g.
        /// `1.0.0-alpha.18`. Named `--tag` to avoid clashing with the global
        /// `--version` flag.
        #[arg(long)]
        tag: Option<String>,
        /// Only consider stable (non-prerelease) releases.
        #[arg(long)]
        stable: bool,
        /// After a successful update, restart a running daemon so it serves the
        /// new binary.
        #[arg(long)]
        restart: bool,
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Per-session ACP substrate — headless agent session management.
    ///
    /// `serve` exposes one agent session as a vanilla ACP Agent over stdio.
    /// `prompt` launches a session, sends one prompt, and streams NDJSON output.
    Acp {
        #[command(subcommand)]
        cmd: AcpCmd,
    },
    /// Chat with an ACP agent in your terminal, routed through BitRouter.
    ///
    /// The interactive counterpart to `acp serve`: instead of exposing the
    /// session to a manager over stdio, this renders it for you — messages,
    /// tool calls, permission prompts, and what the turn cost.
    ///
    /// Output lands in your terminal's real scrollback, so search, selection,
    /// and copy keep working. `Ctrl-C` leaves a readable transcript behind.
    Chat {
        /// Agent id — a bundled-catalog id (`claude-acp`, `codex-acp`,
        /// `gemini-cli`, `opencode`, `pi-acp`, `hermes-acp`, `openclaw`)
        /// or an entry under `agents:` in the config. A catalog id needs no
        /// config entry.
        agent: String,
        /// Per-turn deadline in seconds. On elapse the agent is asked to
        /// cancel cooperatively; a turn that still doesn't finish errors.
        #[arg(long, value_name = "SECS")]
        turn_timeout: Option<u64>,
        #[command(flatten)]
        routing: bitrouter::acp_cli::RoutingOptions,
        /// Path to `bitrouter.yaml`. Resolves via the standard chain when
        /// omitted: `./bitrouter.yaml` → `$BITROUTER_HOME` →
        /// `~/.bitrouter/bitrouter.yaml` → zero-config defaults.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Validate a config file: structure, provider `derives` resolution, and
    /// upstream-URL (SSRF) safety. Exits non-zero on an invalid config — safe
    /// to run in CI. Unset `${VAR}` references are substituted with a
    /// placeholder and reported as warnings, so secrets need not be present.
    Validate {
        /// Path to `bitrouter.yaml` / `bitrouter.json`. When omitted, the
        /// standard resolution chain applies (`./bitrouter.yaml` →
        /// `$BITROUTER_HOME` → `~/.bitrouter`).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

fn parse_unit_interval_ppm(value: &str) -> std::result::Result<u32, String> {
    let value = value.trim();
    let (whole, fractional) = value.split_once('.').unwrap_or((value, ""));
    if !matches!(whole, "0" | "1")
        || fractional.len() > 6
        || !fractional.bytes().all(|byte| byte.is_ascii_digit())
        || (whole == "1" && fractional.bytes().any(|byte| byte != b'0'))
    {
        return Err(format!(
            "expected a decimal between 0 and 1 with at most six fractional digits, got {value}"
        ));
    }
    let fractional_value = if fractional.is_empty() {
        0
    } else {
        fractional
            .parse::<u32>()
            .map_err(|error| format!("expected a decimal between 0 and 1, got {value}: {error}"))?
    };
    let scale = 10_u32.pow(
        u32::try_from(6_usize.saturating_sub(fractional.len()))
            .map_err(|error| format!("could not scale unit interval {value}: {error}"))?,
    );
    Ok(if whole == "1" {
        1_000_000
    } else {
        fractional_value.saturating_mul(scale)
    })
}

#[derive(Subcommand)]
enum WorkflowStateAction {
    /// Build a deterministic benchmark trace bundle.
    Bundle {
        /// Run label stored in `run-artifact.json`.
        #[arg(long)]
        run_label: String,
        /// Daemon workflow trace JSONL.
        #[arg(long)]
        traces: PathBuf,
        /// BitRouter Cloud usage snapshot JSONL.
        #[arg(long)]
        cloud_usage: PathBuf,
        /// Optional request-scoped benchmark outcome JSONL. Omit when task or
        /// episode outcomes will be submitted through the Eval Exchange.
        #[arg(long)]
        outcomes: Option<PathBuf>,
        /// Optional policy routing decision JSONL from BITROUTER_POLICY_DECISION_JSONL.
        #[arg(long)]
        policy_decisions: Option<PathBuf>,
        /// Output directory for traces/cloud usage/outcomes/artifacts.
        #[arg(long)]
        output_dir: PathBuf,
    },
    /// Export daemon metering rows as usage JSONL for benchmark bundles.
    MeteringUsage {
        /// Database URL for the daemon metering DB, for example sqlite:///path/bitrouter.db.
        #[arg(long)]
        database_url: String,
        /// Output usage JSONL path.
        #[arg(long)]
        output: PathBuf,
        /// Impute charges as provider:model=uncached,cache_read,cache_write,output.
        /// Legacy input,output is accepted only for records with no cache usage.
        #[arg(long = "impute-price")]
        impute_prices: Vec<String>,
        /// Inclusive RFC3339 lower bound. Defaults to the current UTC month.
        #[arg(long)]
        since: Option<String>,
        /// Exclusive RFC3339 upper bound. Only used with --since; defaults to now.
        #[arg(long)]
        until: Option<String>,
    },
    /// Replay persisted provider reliability state into a deterministic JSON report.
    ReliabilityReport {
        /// Database URL for the daemon policy DB.
        #[arg(long)]
        database_url: String,
        /// Frozen BitRouter config that defines the reliability thresholds.
        #[arg(long)]
        config: PathBuf,
        /// Output JSON report path.
        #[arg(long)]
        output: PathBuf,
    },
    /// Estimate policy cost coverage and savings without changing live routing.
    PolicyOracle {
        /// Protocol-native daemon workflow trace JSONL from the baseline run.
        #[arg(long)]
        traces: PathBuf,
        /// Request-settled usage JSONL for the same baseline run.
        #[arg(long)]
        cloud_usage: PathBuf,
        /// Policy lock containing the candidate routes to replay.
        #[arg(long)]
        policy_lock: PathBuf,
        /// Named policy within the lock.
        #[arg(long)]
        policy: String,
        /// Candidate effective cost divided by baseline cost, including any
        /// expected token, retry, or turn inflation (for example 0.24).
        #[arg(long = "effective-cost-factor", value_parser = parse_unit_interval_ppm)]
        effective_cost_factor_ppm: u32,
        /// Desired end-to-end savings fraction. Repeat for multiple targets.
        #[arg(long = "target-savings", required = true, value_parser = parse_unit_interval_ppm)]
        target_savings_ppm: Vec<u32>,
        /// Output JSON report path.
        #[arg(long)]
        output: PathBuf,
    },
    /// Reconcile selected metering rows against request-scoped receipts.
    ReconcileMetering {
        /// Database URL for the daemon metering DB.
        #[arg(long)]
        database_url: String,
        /// Inference API root ending in `/v1`.
        #[arg(long, default_value = "https://api.bitrouter.ai/v1")]
        api_base: String,
        /// Environment variable containing the inference key.
        #[arg(long, default_value = "BITROUTER_API_KEY")]
        api_key_env: String,
        /// Protected BitRouter Cloud credential file containing a static API
        /// key. OAuth credentials are never refreshed for settlement.
        #[arg(long)]
        credentials_file: Option<PathBuf>,
        /// Exact request id to reconcile. Repeat for every selected row.
        #[arg(long = "request-id", required = true)]
        request_ids: Vec<String>,
        /// Frozen price as provider:model=uncached,cache_read,cache_write,output.
        /// Repeat a provider/model pair for alternative schedules; settlement
        /// accepts only one distinct candidate that reconstructs the receipt.
        #[arg(long = "price")]
        prices: Vec<String>,
        /// Maximum durable receipt fetches per request.
        #[arg(long, default_value_t = 12)]
        max_attempts: u32,
        /// Delay between pending-receipt polls.
        #[arg(long, default_value_t = 1_000)]
        poll_interval_ms: u64,
    },
    /// Apply task rewards to cheap replacement transitions before the next round.
    ApplyRewardFeedback {
        /// Database URL for the policy daemon DB, for example sqlite:///path/bitrouter.db.
        #[arg(long)]
        database_url: String,
        /// Daemon workflow trace JSONL for the just-finished benchmark group.
        #[arg(long)]
        traces: PathBuf,
        /// Exact, reconciled usage JSONL for the same benchmark group.
        #[arg(long)]
        cloud_usage: PathBuf,
        /// Benchmark outcome JSONL for the just-finished benchmark group.
        #[arg(long)]
        outcomes: PathBuf,
        /// Policy routing decision JSONL from BITROUTER_POLICY_DECISION_JSONL.
        #[arg(long)]
        policy_decisions: PathBuf,
    },
}

#[derive(Subcommand)]
enum McpAction {
    /// Serve the MCP server (stdio by default).
    Serve {
        /// `stdio` (local daemon) or `http` (cloud).
        #[arg(long, value_enum, default_value_t = McpTransport::Stdio)]
        transport: McpTransport,
        /// `local`, `cloud`, or `skills`. Defaults: stdio→local, http→cloud.
        #[arg(long, value_enum)]
        backend: Option<McpBackend>,
        /// Local daemon root.
        #[arg(long, default_value = "http://127.0.0.1:4356")]
        local_url: String,
        /// Cloud root.
        #[arg(long, default_value = "https://api.bitrouter.ai")]
        cloud_url: String,
        /// Cloud bearer token (else `BITROUTER_TOKEN`).
        #[arg(long)]
        token: Option<String>,
        /// HTTP bind address.
        #[arg(long, default_value = "127.0.0.1:4357")]
        bind: String,
    },
    /// Write/print the client config block.
    Install {
        /// `claude` or `cursor`.
        #[arg(long, value_enum, default_value_t = McpClient::Claude)]
        client: McpClient,
        /// Config file to merge into; omit to print to stdout.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Search the official MCP registry (registry.modelcontextprotocol.io)
    /// for upstream MCP servers. Rows carry an install-support column:
    /// `remote` (zero-install), `npx`/`uvx` (stub-able), `manual`
    /// (other package types).
    Search {
        /// Search text (matched server-side against registry names).
        query: String,
        /// Maximum rows to print.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// List servers from the official MCP registry with an install-support
    /// column. Responses are cached for 24h under
    /// `$XDG_CACHE_HOME/bitrouter/mcp-registry/`.
    List {
        /// Maximum rows to print.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Print a YAML stub for a registry server (paste under `mcp_servers:`
    /// in `bitrouter.yaml`). Prefers a zero-install `streamable-http` remote
    /// when the entry publishes one; otherwise stubs `npx`/`uvx` stdio
    /// packages with required env vars as placeholders. Entries without a
    /// pinned npm/PyPI stdio package are refused with a manual-install
    /// pointer.
    Add {
        /// Registry name, e.g. `com.pulsemcp/remote-filesystem` (see
        /// `bitrouter mcp search` / `bitrouter mcp list`).
        name: String,
    },
}

/// Wire transport for `bitrouter mcp serve`.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum McpTransport {
    /// Newline-delimited JSON-RPC over stdio (local clients launch this).
    Stdio,
    /// Streamable HTTP, mounted at `/mcp-control`.
    Http,
}

/// Backend the MCP tools route to.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum McpBackend {
    /// The local BYOK daemon at `127.0.0.1:4356`.
    Local,
    /// BitRouter Cloud at `api.bitrouter.ai`.
    Cloud,
    /// The origin AgentSkills server: `skills_search`/`skills_get` over the
    /// installed-skills root (the `bitrouter_skills` gateway server every
    /// launched harness gets). Stdio only.
    Skills,
}

/// MCP client targeted by `bitrouter mcp install`.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum McpClient {
    Claude,
    Cursor,
}

impl From<McpTransport> for bitrouter_mcp::Transport {
    fn from(t: McpTransport) -> Self {
        match t {
            McpTransport::Stdio => bitrouter_mcp::Transport::Stdio,
            McpTransport::Http => bitrouter_mcp::Transport::Http,
        }
    }
}

impl From<McpClient> for bitrouter_mcp::install::Client {
    fn from(c: McpClient) -> Self {
        match c {
            McpClient::Claude => bitrouter_mcp::install::Client::Claude,
            McpClient::Cursor => bitrouter_mcp::install::Client::Cursor,
        }
    }
}

#[derive(Subcommand)]
enum AgentsAction {
    /// Show the bundled catalog of well-known agents and which of them are
    /// present under `agents:` in the loaded config. With `--remote`, also
    /// fetch and list the official ACP agent registry.
    List {
        /// Also fetch the ACP agent registry
        /// (cdn.agentclientprotocol.com) and list its agents.
        #[arg(long)]
        remote: bool,
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Spawn each configured agent and verify it answers `initialize`.
    Check {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Print a YAML stub for an agent (paste under `agents:` in
    /// `bitrouter.yaml`). Resolves from the bundled catalog first, then the
    /// ACP registry (`npx`/`uvx` distributions only).
    Install {
        /// Agent id (see `bitrouter agents list` / `list --remote`).
        id: String,
    },
}

#[derive(Subcommand)]
enum ObserveAction {
    /// Report the OTel exporter's current state (endpoint, sampler,
    /// cardinality usage, in-flight spans). Queries the running daemon
    /// over the control socket; reports "stopped" + the compile-time
    /// `OTEL_ENABLED` flag when no daemon is reachable.
    Status {
        /// Path to `bitrouter.yaml` (used to locate the control socket).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit control socket path. Overrides the config-derived path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ToolsAction {
    /// List tools advertised by every configured MCP server.
    List {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Health-check every configured MCP server with a `tools/list` round-trip.
    Status {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Connect to one MCP server and print a YAML stub suitable for pasting
    /// into `mcp_servers:`.
    Discover {
        /// Server id (must exist under `mcp_servers` in the config).
        server: String,
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum KeyAction {
    /// Mint a new `brvk_` virtual key for a user. v1 does not sign a JWT — it
    /// creates a DB-backed virtual key and prints the plaintext once.
    Sign {
        /// The owning user id.
        #[arg(short, long)]
        user: String,
        /// Database URL — any backend sea-orm supports
        /// (`sqlite://…`, `postgres://…`, `mysql://…`).
        #[arg(short, long, default_value = "sqlite://./bitrouter.db")]
        db: String,
        /// Optional policy id to bind to the key (the `policy_id` column).
        #[arg(long)]
        policy: Option<String>,
    },
}

#[derive(Subcommand)]
enum PolicyAction {
    /// Write a starter access-control policy file to the policy dir.
    Create {
        /// Policy id (becomes the file stem and the `id:` field).
        id: String,
        /// Policy directory. Default matches the assembly default.
        #[arg(long, default_value = "./policies")]
        dir: PathBuf,
    },
    /// Create a routing policy lock and bind it to a preset.
    Init {
        /// Policy name written under `policies:`.
        name: String,
        /// Preset users select as `@preset` or `@preset:variant`.
        #[arg(long)]
        preset: String,
        /// Strong base model. Inferred from an existing preset when omitted.
        #[arg(long)]
        strong: Option<String>,
        /// Exact reasoning effort owned by the strong target.
        #[arg(long, requires = "strong")]
        strong_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
        /// Economy model explored as a replacement.
        #[arg(long)]
        economy: String,
        /// Exact reasoning effort owned by the economy target.
        #[arg(long)]
        economy_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
        /// Path to `bitrouter.yaml`.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Parse and cross-validate `bitrouter.yaml` and its policy lock.
    Check {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Verify the lock's compiled evidence root against the local ledger.
    Verify {
        #[arg(long)]
        evidence: bool,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Show policy path, digest, runtime mode, and preset bindings.
    Status {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Show one named policy after validation.
    Show {
        name: String,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Hot-reload the policy lock through the daemon control socket.
    Reload {
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Compile a deterministic v3 candidate without changing the active lock.
    Compile {
        /// Candidate output path.
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
        /// Frozen evidence snapshot time in Unix milliseconds.
        #[arg(long, value_name = "UNIX_MS")]
        snapshot_time: Option<i64>,
        /// Immutable admitted-evidence root from `eval snapshot freeze`.
        #[arg(long, value_name = "SHA256")]
        eval_snapshot: Option<String>,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Compare explicit routes in two policy lock artifacts.
    Diff { active: PathBuf, candidate: PathBuf },
    /// Publish one already-compiled candidate after lineage validation.
    Publish {
        candidate: PathBuf,
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Project qualified database evidence into a deterministic policy lock.
    Evolve {
        /// Publish the candidate. Without this flag, print a dry-run report.
        #[arg(long, conflicts_with = "output")]
        apply: bool,
        /// Export the candidate without changing the active policy lock.
        #[arg(long, value_name = "FILE", conflicts_with = "apply")]
        output: Option<PathBuf>,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Restore an exact lock snapshot from local promotion history.
    Rollback {
        digest: String,
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum EvalAction {
    /// Create, inspect, and list eval subjects.
    Subject {
        #[command(subcommand)]
        action: EvalSubjectAction,
    },
    /// Submit an evaluator result through authority admission.
    Result {
        #[command(subcommand)]
        action: EvalResultAction,
    },
    /// Freeze or inspect an immutable admitted-evidence snapshot.
    Snapshot {
        #[command(subcommand)]
        action: EvalSnapshotAction,
    },
    /// Summarize local exchange state.
    Status {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum OptimizeAction {
    /// Perform one deterministic controller transition from admitted Eval history.
    Run {
        #[arg(long, default_value = "auto")]
        policy: String,
        /// Challenger tier; defaults to the policy's adequacy explore tier.
        #[arg(long)]
        candidate_tier: Option<String>,
        #[arg(long, default_value_t = 100_000)]
        exploration_ppm: u32,
        #[arg(long, default_value_t = 3)]
        minimum_tasks: u32,
        #[arg(long, default_value_t = 20)]
        maximum_tasks: u32,
        #[arg(long, default_value_t = 900_000)]
        minimum_pass_rate_ppm: u32,
        #[arg(long)]
        evaluator_config_digest: Option<String>,
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Inspect the current controller state without changing files or the database.
    Status {
        #[arg(long, default_value = "auto")]
        policy: String,
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum TrajectoryAction {
    /// Inspect one episode's structural health, route intents, and event digests.
    Inspect {
        /// Globally unique trajectory episode id.
        episode_id: String,
    },
    /// Verify one episode and compare persisted live checkpoint evidence with replay.
    Replay {
        /// Globally unique trajectory episode id.
        episode_id: String,
    },
    /// Prune delivered outbox rows and retention-expired terminal episodes.
    Prune {
        /// Exclusive RFC3339 cutoff.
        #[arg(long, value_parser = parse_rfc3339_argument)]
        before: String,
        /// Report exact eligible counts without mutating the database.
        #[arg(long)]
        dry_run: bool,
    },
}

fn parse_rfc3339_argument(value: &str) -> std::result::Result<String, String> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|_| value.to_owned())
        .map_err(|error| format!("expected an RFC3339 timestamp: {error}"))
}

#[derive(Subcommand)]
enum EvalSubjectAction {
    /// Calculate the canonical evidence digest and write a validated JSON subject.
    Seal {
        /// Draft JSON or YAML subject with redacted evidence items.
        draft: PathBuf,
        /// Destination for the deterministic sealed JSON subject.
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Insert an immutable subject from JSON or YAML.
    Put {
        file: PathBuf,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Get one subject by eval id.
    Get {
        eval_id: String,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// List subjects.
    List {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum EvalResultAction {
    /// Submit an immutable result from JSON or YAML as the local operator.
    Submit {
        file: PathBuf,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum EvalSnapshotAction {
    /// Freeze all currently admitted results into a content-addressed manifest.
    Freeze {
        #[arg(long)]
        at: Option<String>,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Get a frozen manifest by evidence root.
    Get {
        evidence_root: String,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ProviderAction {
    /// List every configured provider.
    List {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Log in to an upstream provider — interactive credential setup.
    ///
    /// Per-provider methods are auto-derived from the catalog: `claude-code`
    /// adopts the live Claude Code session; `anthropic` accepts an API-key
    /// paste; `openai-codex` runs the ChatGPT PKCE flow; `github-copilot` the
    /// GitHub device flow; everything else accepts a pasted API key. Logging
    /// in to the built-in `bitrouter` provider runs the same cloud sign-in as
    /// `bitrouter cloud login`.
    Login {
        /// Provider id (e.g. `claude-code`, `openai-codex`, `bitrouter`).
        provider: String,
        /// Account label this credential is stored under (default `default`).
        /// Ignored for the `bitrouter` provider (it uses the cloud credential).
        #[arg(short, long, default_value = "default")]
        label: String,
        /// Import an existing vendor CLI session without prompting for a
        /// browser sign-in. Currently supported by openai-codex.
        #[arg(long)]
        import_existing: bool,
        /// Do not run a browser-based provider OAuth flow.
        #[arg(long)]
        no_browser: bool,
        /// Seed a BYOK provider non-interactively from this API key — skips the
        /// method menu and the stdin paste. The provider must accept a pasted
        /// key (OAuth-only backends reject it).
        #[arg(long, value_name = "KEY", conflicts_with_all = ["import_existing", "no_browser", "key_stdin"])]
        api_key: Option<String>,
        /// Read the API key from stdin (one line) instead of prompting — for
        /// pipelines, e.g. `printf %s "$KEY" | bitrouter providers login openai
        /// --key-stdin`.
        #[arg(long, conflicts_with_all = ["import_existing", "no_browser"])]
        key_stdin: bool,
    },
    /// Log out of an upstream provider — clears every stored credential for
    /// it. For the built-in `bitrouter` provider this is `cloud logout`.
    Logout {
        /// Provider id whose stored credentials should be removed.
        provider: String,
    },
}

#[derive(Subcommand)]
enum AcpCmd {
    /// Serve one agent session as a vanilla ACP Agent over **stdio** until the
    /// manager disconnects. Intended for GUIs and orchestrating agents that
    /// speak ACP directly.
    Serve {
        /// Agent id — a bundled-catalog id (`claude-acp`, `codex-acp`,
        /// `gemini-cli`, `opencode`, `pi-acp`, `hermes-acp`, `openclaw`)
        /// or an entry under `agents:` in the config. A catalog id needs no
        /// config entry; `bitrouter spawn <agent> --check` previews whether it
        /// will route or run direct.
        #[arg(long)]
        agent: String,
        /// Per-turn deadline in seconds. On elapse the agent is asked to
        /// cancel cooperatively; a turn that still doesn't finish errors.
        #[arg(long, value_name = "SECS")]
        turn_timeout: Option<u64>,
        #[command(flatten)]
        routing: bitrouter::acp_cli::RoutingOptions,
        /// Path to `bitrouter.yaml`. Resolves via the standard chain when
        /// omitted: `./bitrouter.yaml` → `$BITROUTER_HOME` →
        /// `~/.bitrouter/bitrouter.yaml` → zero-config defaults.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Launch a session, send one prompt, and stream NDJSON output to stdout.
    ///
    /// Each streamed agent update is emitted as one JSON object per line with
    /// a `type` field (e.g. `message_chunk`, `tool_call`). The final line has
    /// `type: result` with a `stop_reason` field.
    Prompt {
        /// Agent id — a bundled-catalog id (`claude-acp`, `codex-acp`,
        /// `gemini-cli`, `opencode`, `pi-acp`, `hermes-acp`, `openclaw`)
        /// or an entry under `agents:` in the config. A catalog id needs no
        /// config entry; `bitrouter spawn <agent> --check` previews whether it
        /// will route or run direct.
        #[arg(long)]
        agent: String,
        /// Per-turn deadline in seconds. On elapse the agent is asked to
        /// cancel cooperatively; a turn that still doesn't finish errors.
        #[arg(long, value_name = "SECS")]
        turn_timeout: Option<u64>,
        /// Return immediately after submitting the prompt (emit
        /// `{"type":"submitted"}`). The session is torn down after ack.
        #[arg(long)]
        no_wait: bool,
        #[command(flatten)]
        routing: bitrouter::acp_cli::RoutingOptions,
        /// Path to `bitrouter.yaml`. Resolves via the standard chain when
        /// omitted: `./bitrouter.yaml` → `$BITROUTER_HOME` →
        /// `~/.bitrouter/bitrouter.yaml` → zero-config defaults.
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// The prompt text to send.
        text: String,
    },
}

const CLI_MAIN_STACK_SIZE: usize = 8 * 1024 * 1024;

fn main() {
    let worker = std::thread::Builder::new()
        .name("bitrouter-main".to_owned())
        .stack_size(CLI_MAIN_STACK_SIZE)
        .spawn(async_main);

    match worker {
        Ok(handle) => {
            if handle.join().is_err() {
                // The panic hook already rendered the original failure. Match
                // Rust's normal main-thread panic exit status without hiding it.
                std::process::exit(101);
            }
        }
        Err(error) => {
            eprintln!("error: failed to start BitRouter CLI runtime: {error}");
            std::process::exit(1);
        }
    }
}

#[tokio::main]
async fn async_main() {
    // Parse once here so the global `--json` / `--human` flags are available to
    // render the *result* — a success report or the error envelope — through the
    // single `Output` driver. Diagnostics during execution go to stderr; the
    // result (this match) goes to stdout in the selected format, so
    // `bitrouter <cmd> 2>/dev/null | jq` always sees one clean JSON value.
    let cli = Cli::parse();
    let raw_cloud_api = matches!(
        &cli.command,
        Some(Command::Cloud {
            action: bitrouter::cloud::cli::CloudAction::Api(_)
        })
    );
    let output = bitrouter::output::Output::from_flags(cli.json, cli.human || cli.human_short);
    // Box the dispatch future onto the heap. `run` is a large `async fn` whose
    // state machine inlines the biggest per-command futures (the onboarding
    // wizard, `spawn`, …). Before `async_main` moved to its dedicated stack,
    // `#[tokio::main]` polled that state on Windows' ~1 MiB native main thread.
    // Keeping it off the dedicated stack still leaves more headroom for deep
    // synchronous call chains such as rustls/reqwest on the `cloud` path.
    match Box::pin(run(cli, &output)).await {
        Ok(()) => {}
        Err(e) => {
            // A pre-ACP routing failure is reported on **stderr**, never as a
            // stdout envelope: `acp serve` owns stdout for JSON-RPC and `acp
            // prompt` for NDJSON, and `prompt` has already put the structured
            // `error` line on that stream itself. Keyed off the error's type
            // rather than the command shape, so it cannot misclassify a
            // sibling mode (`spawn --check` still gets its JSON report).
            if raw_cloud_api {
                eprintln!("error: {e:#}");
            } else if let Some(routing) = e.downcast_ref::<bitrouter::acp_cli::RoutingError>() {
                eprintln!("error: {routing}");
            } else {
                let _ = output.emit(&bitrouter::output::error::envelope_from_anyhow(&e));
            }
            std::process::exit(1);
        }
    }
}

async fn run(cli: Cli, output: &bitrouter::output::Output) -> Result<()> {
    // Subscriber init splits by command: the long-running `serve` defers
    // its init until after the OTel exporter has installed a real tracer
    // provider globally (see `serve` below). Every other command — and
    // the foreground supervisor in `start` — gets a basic fmt subscriber
    // here so config-loading errors surface as log lines.
    //
    // `tracing-opentelemetry`'s bridge layer captures its tracer at
    // construction, so registering it before the exporter exists would
    // lock the bridge to the default no-op and silently drop every later
    // span. The two-stage init is the simplest way around that.
    //
    // Both `acp` subcommands keep stdout exclusively for their machine-readable
    // protocol — JSON-RPC for `acp serve`, NDJSON for `acp prompt` — so their
    // logging must go to stderr instead of the default (stdout) writer, or it
    // would interleave with and corrupt that stream. The exclusion of
    // `Command::Serve` mirrors how it defers its subscriber init to after the
    // OTel exporter is available.
    // `chat` shares this rule with the `acp` verbs, and goes further: it owns
    // the terminal, stderr included. Its renderer paints rows in place against
    // its own model of where they are, so a log line arriving between frames
    // does not merely interleave — it scrolls the screen out from under the
    // renderer and every row below it lands one line off. So chat's logs go to
    // the session file **only** (see TUI_RENDERER_SPEC §4.6).
    let is_acp = matches!(
        &cli.command,
        Some(Command::Acp { .. } | Command::Chat { .. })
    );
    let owns_the_terminal = matches!(&cli.command, Some(Command::Chat { .. }));
    if matches!(cli.command, Some(Command::Serve { .. })) {
        // `Command::Serve` defers its init — handled inside `serve()`.
    } else if is_acp {
        // Any `acp` subcommand: stderr so stdout stays pristine, plus a
        // per-session file so the substrate's log and the agent child's
        // captured stderr can be read back interleaved.
        if let Some(path) = init_session_log_tracing_subscriber(!owns_the_terminal) {
            tracing::debug!(log = %path.display(), "session log");
            // Remembered so a session that dies badly can show the end of it
            // and say where the rest is. Set once, at init, because that is
            // the only place the path exists.
            bitrouter::chat::session::remember_session_log(path);
        }
    } else {
        init_basic_tracing_subscriber();
    }

    let Some(command) = cli.command else {
        // Bare `bitrouter` — the onboarding front door (wizard when
        // unconfigured; status + hint when configured). Never re-onboards a
        // configured user, never silently spawns a daemon/harness.
        return bitrouter::onboarding::entry(output).await;
    };

    match command {
        Command::Serve { config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            serve(&source).await
        }
        Command::Start { config, log } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let log_path = resolve_log_path(source.home(), log.as_deref());
            output.emit(&start(&source, &log_path, "start").await?)?;
            Ok(())
        }
        Command::Stop { config, socket } => {
            let socket = resolve_client_socket(config.as_deref(), socket.as_deref()).await?;
            output.emit(&stop(&socket).await?)?;
            Ok(())
        }
        Command::Restart {
            config,
            socket,
            log,
        } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let socket = resolve_client_socket_from(&source, socket.as_deref()).await?;
            let log_path = resolve_log_path(source.home(), log.as_deref());
            output.emit(&restart(&source, &socket, &log_path).await?)?;
            Ok(())
        }
        Command::Reload { config, socket } => {
            let socket = resolve_client_socket(config.as_deref(), socket.as_deref()).await?;
            output.emit(&reload(&socket).await?)?;
            Ok(())
        }
        Command::Status {
            config,
            socket,
            requests,
        } => {
            let socket = resolve_client_socket(config.as_deref(), socket.as_deref()).await?;
            if requests {
                return request_table(config.as_deref(), &socket).await;
            }
            output.emit(&status(&socket).await?)?;
            Ok(())
        }
        Command::Route {
            model,
            config,
            socket,
        } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let socket = resolve_client_socket_from(&source, socket.as_deref()).await?;
            output.emit(&route(&model, &source, &socket).await?)?;
            Ok(())
        }
        Command::Init {
            config,
            yes,
            force,
            reset,
            cloud_login,
            api_key,
            providers,
            provider_api_keys,
            use_detected,
            harnesses,
            no_install,
            after,
            model,
            write_config,
        } => {
            let flags = bitrouter::onboarding::OnboardingFlags {
                config,
                yes,
                force,
                reset,
                cloud_login,
                api_key,
                providers,
                provider_api_keys,
                use_detected,
                harnesses,
                no_install,
                after,
                model,
                write_config,
            };
            bitrouter::onboarding::run(flags, output).await
        }
        Command::Config { action } => {
            let report = config_cmd(action).await?;
            output.emit(&report)?;
            if report.valid {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
        Command::Key { action } => {
            output.emit(&key(action).await?)?;
            Ok(())
        }
        Command::Models { config, provider } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            output.emit(&models(&source, provider.as_deref()).await?)?;
            Ok(())
        }
        Command::Tools { action } => tools(action, output).await,
        Command::Observe { action } => observe(action, output).await,
        Command::Policy { action } => policy(action, output).await,
        Command::Eval { action } => eval(action, output).await,
        Command::Optimize { action } => optimize(action, output).await,
        Command::Trajectory { config, action } => {
            trajectory(config.as_deref(), action, output).await
        }
        Command::Providers { action } => providers(action, output).await,
        Command::Agents { action } => agents_cmd(action, output).await,
        Command::Launch {
            agent,
            model,
            config,
            base_url,
            no_install,
            no_start,
            check,
            agent_args,
        } => {
            let opts = bitrouter::spawn::SpawnOptions {
                agent: bitrouter::spawn::resolve_launch_agent(&agent)?,
                model,
                agent_args,
                base_url,
                no_install,
                no_start,
                check,
            };
            run_launch(config.as_deref(), opts, output).await
        }
        Command::Spawn {
            agent,
            prompt,
            serve,
            check,
            direct,
            model,
            base_url,
            no_start,
            turn_timeout,
            no_wait,
            result_schema,
            config,
            legacy_agent,
            no_install,
            agent_args,
        } => {
            // Deprecated interactive alias: `spawn --agent <claude|codex>` (or
            // `-a`) → `launch`. Kept working for one or two alpha releases.
            if let Some(legacy) = legacy_agent {
                // The interactive alias and the ACP sub-agent modes are
                // mutually exclusive — reject the mix rather than silently
                // dropping the ACP args and launching an interactive TUI.
                if agent.is_some() || prompt.is_some() || serve {
                    anyhow::bail!(
                        "`--agent` selects the deprecated interactive launcher; it cannot be \
                         combined with a positional agent id, `-p`, or `--serve`. Use \
                         `bitrouter launch --agent {}` for the TUI, or drop `--agent` to spawn \
                         an ACP sub-agent.",
                        legacy.spec().id
                    );
                }
                eprintln!(
                    "note: `bitrouter spawn --agent` is deprecated — use \
                     `bitrouter launch --agent {}` (this alias will be removed).",
                    legacy.spec().id
                );
                let opts = bitrouter::spawn::SpawnOptions {
                    agent: bitrouter::spawn::resolve_launch_agent(legacy.spec().id)?,
                    model: model.clone(),
                    agent_args,
                    base_url,
                    no_install,
                    no_start,
                    check,
                };
                return run_launch(config.as_deref(), opts, output).await;
            }

            let Some(agent) = agent else {
                anyhow::bail!(
                    "spawn: provide an agent id and a mode, e.g. \
                     `bitrouter spawn claude-acp -p \"summarize README\"`, \
                     `bitrouter spawn codex-acp --serve`, or `--check`."
                );
            };
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            let routing = bitrouter::acp_cli::RoutingOptions {
                direct,
                base_url,
                model,
                no_start,
            };

            if check {
                let report = bitrouter::acp_cli::spawn_check(cfg, &agent, &routing).await?;
                output.emit(&report)?;
                if report.exit_code() == 0 {
                    Ok(())
                } else {
                    std::process::exit(report.exit_code());
                }
            } else if serve {
                let options = bitrouter::acp_cli::launch_options(turn_timeout);
                let ctx = bitrouter::acp_cli::SpawnContext {
                    source: &source,
                    config: cfg,
                    agent_id: &agent,
                    options,
                    routing,
                };
                bitrouter::acp_cli::serve(ctx).await
            } else if let Some(text) = prompt {
                let options = bitrouter::acp_cli::launch_options(turn_timeout);
                // A malformed schema fails fast, before any session side effect.
                let contract = result_schema
                    .as_deref()
                    .map(bitrouter::result_contract::ResultContract::from_flag)
                    .transpose()?;
                let mut stdout = tokio::io::stdout();
                let ctx = bitrouter::acp_cli::SpawnContext {
                    source: &source,
                    config: cfg,
                    agent_id: &agent,
                    options,
                    routing,
                };
                bitrouter::acp_cli::prompt(ctx, &text, no_wait, contract, &mut stdout).await
            } else {
                anyhow::bail!(
                    "spawn: choose a mode — `-p \"<prompt>\"` (NDJSON), \
                     `--serve` (ACP over stdio), or `--check` (preflight)."
                );
            }
        }
        Command::Cloud { action } => bitrouter::cloud::cli::run(action, output.format()).await,
        Command::Skills { action } => bitrouter::skills::cli::run(action, output),
        Command::Mcp { action } => mcp_cmd(action, output).await,
        Command::WorkflowState { action } => workflow_state_cmd(action).await,
        Command::Acp { cmd } => acp_cmd(cmd).await,
        Command::Chat {
            agent,
            turn_timeout,
            routing,
            config,
        } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            let options = bitrouter::acp_cli::launch_options(turn_timeout);
            let ctx = bitrouter::acp_cli::SpawnContext {
                source: &source,
                config: cfg,
                agent_id: &agent,
                options,
                routing,
            };
            bitrouter::acp_cli::chat(ctx).await
        }
        Command::Update {
            check,
            tag,
            stable,
            restart: restart_after,
            yes,
        } => {
            let source = bitrouter::paths::resolve_config(None)?;
            let socket = resolve_client_socket_from(&source, None).await?;
            let opts = bitrouter::update::UpdateOptions {
                check,
                tag,
                stable,
                restart: restart_after,
                yes,
            };
            let outcome = bitrouter::update::run(opts, &socket).await?;
            if outcome.restart_needed {
                // Bring the daemon onto the new binary before emitting, so a
                // restart failure surfaces as the error envelope. The restart's
                // own report is folded into `outcome.report.daemon`.
                let log_path = resolve_log_path(source.home(), None);
                restart(&source, &socket, &log_path).await?;
            }
            output.emit(&outcome.report)?;
            Ok(())
        }
    }
}

// ===== `bitrouter config …` (config tooling) =====

async fn config_cmd(action: ConfigAction) -> Result<ValidateReport> {
    match action {
        ConfigAction::Validate { config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            validate_config(&source).await
        }
    }
}

async fn settlement_api_key(
    explicit_api_key: Option<&str>,
    credentials_file: Option<&Path>,
    api_base: &str,
) -> Result<String> {
    if let Some(api_key) = explicit_api_key.filter(|api_key| !api_key.is_empty()) {
        return Ok(api_key.to_owned());
    }

    let credentials_file = credentials_file.ok_or_else(|| {
        anyhow::anyhow!("settlement requires a static BitRouter API key or credentials file")
    })?;
    let manager =
        bitrouter_providers::hosted::account::manager::CredentialManager::new(credentials_file)
            .context("build settlement credential manager")?;
    manager
        .resolve_api_key(None, Some(api_base))
        .await
        .map(|credential| credential.secret().to_owned())
        .map_err(anyhow::Error::from)
}

async fn workflow_state_cmd(action: WorkflowStateAction) -> Result<()> {
    match action {
        WorkflowStateAction::Bundle {
            run_label,
            traces,
            cloud_usage,
            outcomes,
            policy_decisions,
            output_dir,
        } => {
            use bitrouter::workflow_state::archive::{
                CloudUsageRecord, TraceArchive, WorkflowRunArtifact,
            };
            use bitrouter::workflow_state::decision::PolicyDecisionRecord;
            use bitrouter::workflow_state::real_trace::TraceSanitizer;
            use bitrouter::workflow_state::reward::BenchmarkOutcomeRecord;

            let traces = TraceArchive::read_jsonl(&traces)
                .with_context(|| format!("read workflow traces {}", traces.display()))?;
            let usage = CloudUsageRecord::load_snapshot_jsonl(&cloud_usage)
                .with_context(|| format!("read cloud usage {}", cloud_usage.display()))?;
            let outcomes = match outcomes {
                Some(path) => BenchmarkOutcomeRecord::load_jsonl(&path)
                    .with_context(|| format!("read benchmark outcomes {}", path.display()))?,
                None => Vec::new(),
            };
            let decisions = match policy_decisions {
                Some(path) => PolicyDecisionRecord::load_jsonl(&path)
                    .with_context(|| format!("read policy decisions {}", path.display()))?,
                None => Vec::new(),
            };
            let artifact = WorkflowRunArtifact::write_bundle_with_decisions(
                run_label,
                &output_dir,
                &traces,
                &usage,
                &outcomes,
                &decisions,
                &TraceSanitizer::default(),
            )
            .with_context(|| format!("write workflow bundle {}", output_dir.display()))?;
            println!(
                "✓ wrote workflow bundle to {} (traces: {}, reward matches: {})",
                output_dir.display(),
                artifact.trace_count,
                artifact.reward_join.matched_trace_count
            );
            Ok(())
        }
        WorkflowStateAction::MeteringUsage {
            database_url,
            output,
            impute_prices,
            since,
            until,
        } => {
            use bitrouter::metering::{
                MeteringStore, MeteringUsageRecord, TimeWindow, UsagePriceOverride,
            };

            let impute_prices = impute_prices
                .iter()
                .map(|value| UsagePriceOverride::parse(value))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(anyhow::Error::from)?;
            let window = match since {
                Some(since) => {
                    let start = parse_rfc3339_utc(&since, "--since")?;
                    let end = match until {
                        Some(until) => parse_rfc3339_utc(&until, "--until")?,
                        None => chrono::Utc::now(),
                    };
                    TimeWindow::Custom { start, end }
                }
                None => TimeWindow::ThisMonth,
            };
            let db = bitrouter::db::connect(&database_url)
                .await
                .with_context(|| format!("connect metering database {database_url}"))?;
            let mut records = MeteringStore::new(db)
                .export_usage(window)
                .await
                .with_context(|| format!("export metering usage from {database_url}"))?;
            MeteringUsageRecord::apply_price_overrides(&mut records, &impute_prices);
            MeteringUsageRecord::write_jsonl(&output, &records)
                .with_context(|| format!("write metering usage {}", output.display()))?;
            println!(
                "✓ wrote {} metering usage records to {}",
                records.len(),
                output.display()
            );
            Ok(())
        }
        WorkflowStateAction::ReliabilityReport {
            database_url,
            config,
            output,
        } => {
            use bitrouter::adequacy::report::ReliabilityReport;
            use bitrouter::adequacy::store::AdequacyStore;

            let config_document = bitrouter_sdk::config::load(&config)
                .await
                .with_context(|| format!("load reliability config {}", config.display()))?;
            let db = bitrouter::db::connect(&database_url)
                .await
                .with_context(|| format!("connect reliability database {database_url}"))?;
            let rows = AdequacyStore::new(db)
                .load_reliability_events()
                .await
                .with_context(|| format!("load reliability events from {database_url}"))?;
            let report = ReliabilityReport::build(&config_document.policy_table.adequacy, &rows)
                .context("replay provider reliability events")?;
            report
                .write(&output)
                .with_context(|| format!("write reliability report {}", output.display()))?;
            println!(
                "✓ wrote {} reliability events to {}",
                report.event_count,
                output.display()
            );
            Ok(())
        }
        WorkflowStateAction::PolicyOracle {
            traces,
            cloud_usage,
            policy_lock,
            policy,
            effective_cost_factor_ppm,
            target_savings_ppm,
            output,
        } => {
            use bitrouter::workflow_state::archive::{CloudUsageRecord, TraceArchive};
            use bitrouter::workflow_state::counterfactual::analyze_policy_counterfactual;

            let traces = TraceArchive::read_jsonl(&traces)
                .with_context(|| format!("read workflow traces {}", traces.display()))?;
            let usage = CloudUsageRecord::load_snapshot_jsonl(&cloud_usage)
                .with_context(|| format!("read cloud usage {}", cloud_usage.display()))?;
            let lock = bitrouter::policy_lock::load(&policy_lock)
                .await
                .with_context(|| format!("load policy lock {}", policy_lock.display()))?;
            let definition = lock.document.policies.get(&policy).ok_or_else(|| {
                anyhow::anyhow!(
                    "policy {policy} does not exist in {}",
                    policy_lock.display()
                )
            })?;
            let report = analyze_policy_counterfactual(
                &traces,
                &usage,
                definition,
                effective_cost_factor_ppm,
                &target_savings_ppm,
            )
            .context("analyze policy counterfactual")?;
            let encoded = serde_json::to_vec_pretty(&report)
                .context("serialize policy counterfactual report")?;
            std::fs::write(&output, encoded)
                .with_context(|| format!("write policy oracle report {}", output.display()))?;
            println!(
                "✓ analyzed {} requests: {:.1}% cost coverage, {:.1}% projected savings -> {}",
                report.trace_count,
                f64::from(report.cost_weighted_coverage_ppm) / 10_000.0,
                f64::from(report.projected_savings_ppm) / 10_000.0,
                output.display()
            );
            Ok(())
        }
        WorkflowStateAction::ReconcileMetering {
            database_url,
            api_base,
            api_key_env,
            credentials_file,
            request_ids,
            prices,
            max_attempts,
            poll_interval_ms,
        } => {
            use std::time::Duration;

            use bitrouter::cloud::settlement::SettlementClient;
            use bitrouter::metering::{MeteringStore, UsagePriceOverride, reconcile_requests};

            let prices = prices
                .iter()
                .map(|value| UsagePriceOverride::parse(value))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(anyhow::Error::from)?;
            let explicit_api_key = std::env::var(&api_key_env)
                .ok()
                .filter(|api_key| !api_key.is_empty());
            let api_key = match settlement_api_key(
                explicit_api_key.as_deref(),
                credentials_file.as_deref(),
                &api_base,
            )
            .await
            {
                Ok(api_key) => api_key,
                Err(error) if credentials_file.is_none() => {
                    return Err(error.context(format!(
                        "provide a non-empty {api_key_env} environment variable or a credentials file containing a static BitRouter API key"
                    )));
                }
                Err(error) => return Err(error),
            };
            let client = SettlementClient::new(&api_base, api_key)
                .context("build request settlement client")?;
            let db = bitrouter::db::connect(&database_url)
                .await
                .with_context(|| format!("connect metering database {database_url}"))?;
            bitrouter::db::run_migrations(&db)
                .await
                .context("run metering reconciliation migrations")?;
            let summary = reconcile_requests(
                &MeteringStore::new(db),
                &client,
                &request_ids,
                &prices,
                max_attempts,
                Duration::from_millis(poll_interval_ms),
            )
            .await
            .context("reconcile selected metering requests")?;
            if !summary.accepted() {
                anyhow::bail!(
                    "metering reconciliation failed closed: requested={}, computed={}, \
                     not_charged={}, unknown={}, attempts={}",
                    summary.requested,
                    summary.computed,
                    summary.not_charged,
                    summary.unknown,
                    summary.attempts
                );
            }
            println!(
                "✓ reconciled {} metering requests (computed: {}, not charged: {}, attempts: {})",
                summary.requested, summary.computed, summary.not_charged, summary.attempts
            );
            Ok(())
        }
        WorkflowStateAction::ApplyRewardFeedback {
            database_url,
            traces,
            cloud_usage,
            outcomes,
            policy_decisions,
        } => {
            use bitrouter::eval::EvalService;
            use bitrouter::eval::store::EvalStore;
            use bitrouter::workflow_state::archive::{
                CloudUsageRecord, TraceArchive, WorkflowRunArtifact,
            };
            use bitrouter::workflow_state::decision::PolicyDecisionRecord;
            use bitrouter::workflow_state::reward::BenchmarkOutcomeRecord;
            use bitrouter::workflow_state::reward_feedback::import_semantic_reward_feedback;

            let traces = TraceArchive::read_jsonl(&traces)
                .with_context(|| format!("read workflow traces {}", traces.display()))?;
            let outcomes = BenchmarkOutcomeRecord::load_jsonl(&outcomes)
                .with_context(|| format!("read benchmark outcomes {}", outcomes.display()))?;
            let usage = CloudUsageRecord::load_snapshot_jsonl(&cloud_usage)
                .with_context(|| format!("read cloud usage {}", cloud_usage.display()))?;
            let decisions = PolicyDecisionRecord::load_jsonl(&policy_decisions)
                .with_context(|| format!("read policy decisions {}", policy_decisions.display()))?;
            WorkflowRunArtifact::validate_reward_feedback_integrity(
                &traces, &usage, &outcomes, &decisions,
            )
            .context("validate reward feedback admission")?;
            let artifact = WorkflowRunArtifact::build_with_decisions(
                "reward-feedback",
                &traces,
                &usage,
                &outcomes,
                &decisions,
            )
            .context("build workflow artifact for reward feedback")?;
            let db = bitrouter::db::connect(&database_url)
                .await
                .with_context(|| format!("connect eval exchange database {database_url}"))?;
            bitrouter::db::run_migrations(&db).await?;
            let summary = import_semantic_reward_feedback(
                &EvalService::new(EvalStore::new(db), Default::default()),
                &artifact.semantic_policy_transition_candidates,
            )
            .await
            .context("import reward feedback into generic eval exchange")?;
            println!(
                "✓ imported reward feedback: {} candidates, {} admitted eval results, {} skipped",
                summary.candidate_count, summary.admitted_count, summary.skipped_count
            );
            for eval_id in summary.eval_ids {
                println!("  admitted {eval_id}");
            }
            Ok(())
        }
    }
}

fn parse_rfc3339_utc(value: &str, flag: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .with_context(|| format!("{flag} must be RFC3339, got {value:?}"))
}

/// Validate a config file and print a short summary. Returns `Err` (→ exit 1)
/// on a malformed or unsafe config, so the command is CI-safe.
///
/// Validation runs the real parse path — deserialization, `${VAR}`
/// substitution, `derives` resolution, and the upstream-URL (SSRF) gate. It
/// does **not** load the JSON Schema (that artifact is for IDE autocomplete and
/// the generated-dist drift check); structural validation here is what `serde` +
/// `serde-saphyr` enforce.
///
/// To validate without secrets present, any *unset* `${VAR}` is substituted
/// with a reserved `.invalid` URL placeholder. Caveat: a value that embeds an
/// unset variable *mid-string* (e.g. `api_base: https://${REGION}.host`) is
/// checked against that placeholder, so the SSRF/structure verdict for such a
/// value is **not authoritative** — it must be re-checked at runtime once the
/// real value is known. Whole-value `${VAR}` (the common case) is unaffected.
/// Unresolved variables are listed as warnings.
async fn validate_config(source: &bitrouter::paths::ConfigSource) -> Result<ValidateReport> {
    use bitrouter::paths::ConfigSource;
    let path = match source {
        ConfigSource::File(p) => p,
        ConfigSource::Default { .. } => anyhow::bail!(
            "no config file found to validate — looked in ./bitrouter.yaml, \
             $BITROUTER_HOME, and ~/.bitrouter. Pass --config <path>."
        ),
    };
    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;

    // `parse_with` takes an `Fn` lookup, so the missing-var set needs interior
    // mutability.
    let missing: std::cell::RefCell<std::collections::BTreeSet<String>> =
        std::cell::RefCell::new(std::collections::BTreeSet::new());
    let parsed = config::parse_with(&raw, |name| {
        Some(config::env_lookup(name).unwrap_or_else(|| {
            missing.borrow_mut().insert(name.to_string());
            "https://env-placeholder.invalid".to_string()
        }))
    });
    let missing = missing.into_inner();

    match parsed {
        Ok(cfg) => match bitrouter::policy_lock::load_for_config(&cfg, Some(path)).await {
            Ok(_) => Ok(ValidateReport::valid(
                path.display().to_string(),
                cfg.providers.len(),
                cfg.models.len(),
                cfg.presets.len(),
                cfg.variants.len(),
                missing
                    .into_iter()
                    .map(|name| UnsetVar { unset_env: name })
                    .collect(),
            )),
            Err(error) => Ok(ValidateReport::invalid(
                path.display().to_string(),
                error.to_string(),
            )),
        },
        Err(e) => Ok(ValidateReport::invalid(
            path.display().to_string(),
            e.to_string(),
        )),
    }
}

// ===== `bitrouter mcp …` (origin MCP server: serve / install) =====

/// `CostFooter` over the local metering database: the origin MCP server
/// appends this spend line to `complete` / `status` results so
/// in-session model arbitrage stays cost-visible to the calling agent.
struct LocalCostFooter {
    source: bitrouter::paths::ConfigSource,
}

#[async_trait::async_trait]
impl bitrouter_mcp::server::CostFooter for LocalCostFooter {
    async fn line(&self) -> Option<String> {
        use bitrouter::metering::store::TimeWindow;
        let store = bitrouter::metering::reader::open_readonly(&self.source).await?;
        let today = store.spend_summary(TimeWindow::Today).await.ok()?;
        (today.requests > 0).then(|| {
            format!(
                "bitrouter: spend today {} ({} requests)",
                bitrouter::metering::fmt_usd(today.spend_micro_usd),
                today.requests
            )
        })
    }
}

async fn mcp_cmd(action: McpAction, output: &Output) -> Result<()> {
    match action {
        McpAction::Serve {
            transport,
            backend,
            local_url,
            cloud_url,
            token,
            bind,
        } => {
            // The skills backend is the origin AgentSkills server over the
            // installed-skills root — the `bitrouter_skills` gateway server
            // harnesses launch as a subprocess. Stdio-only: it serves the
            // caller's own installed-skills tree, so it must inherit the
            // launching process's identity rather than ride an
            // unauthenticated HTTP listener.
            //
            // Two surfaces over the same root, deliberately: the
            // `skills_search` / `skills_get` *tools*, which any MCP client can
            // call today, and SEP-2640's `skills/list` / `skills/get`
            // *methods* plus `resources/*`, which is what SEP-aware hosts will
            // consume. Neither supersedes the other.
            if backend == Some(McpBackend::Skills) {
                if matches!(transport, McpTransport::Http) {
                    anyhow::bail!(
                        "the skills backend is stdio-only (harnesses launch it as a subprocess)"
                    );
                }
                let base_repo = std::env::current_dir().context("resolving current directory")?;
                let server = bitrouter_mcp::server::BitrouterMcp::builder()
                    .skills(std::sync::Arc::new(
                        bitrouter::skills_query::InstalledSkills::new(base_repo.clone()),
                    ))
                    .skill_catalog(std::sync::Arc::new(
                        bitrouter::skills_catalog::InstalledSkillCatalog::new(base_repo),
                    ))
                    .build();
                return bitrouter_mcp::server::serve_stdio(server, None).await;
            }
            let transport = bitrouter_mcp::Transport::from(transport);
            // Skills was handled and returned above. Local/Cloud map straight
            // across; an unset backend takes the transport default
            // (stdio→local, http→cloud).
            let backend = match backend {
                Some(McpBackend::Local) => bitrouter_mcp::BackendKind::Local,
                Some(McpBackend::Cloud) => bitrouter_mcp::BackendKind::Cloud,
                Some(McpBackend::Skills) | None => match transport {
                    bitrouter_mcp::Transport::Stdio => bitrouter_mcp::BackendKind::Local,
                    bitrouter_mcp::Transport::Http => bitrouter_mcp::BackendKind::Cloud,
                },
            };
            let cloud_token = token.or_else(|| std::env::var("BITROUTER_TOKEN").ok());
            if matches!(transport, bitrouter_mcp::Transport::Http) && cloud_token.is_some() {
                eprintln!(
                    "note: --token/BITROUTER_TOKEN is ignored for --transport http (multi-tenant; each client sends its own Authorization)"
                );
            }
            // The spend footer only makes sense where the local metering
            // database *is* the caller's spend: stdio → local daemon.
            let local_stdio = matches!(
                (transport, backend),
                (
                    bitrouter_mcp::Transport::Stdio,
                    bitrouter_mcp::BackendKind::Local
                )
            );
            let source = local_stdio
                .then(|| bitrouter::paths::resolve_config(None).ok())
                .flatten();
            let cost_footer: Option<std::sync::Arc<dyn bitrouter_mcp::server::CostFooter>> =
                source.clone().map(|source| {
                    std::sync::Arc::new(LocalCostFooter { source })
                        as std::sync::Arc<dyn bitrouter_mcp::server::CostFooter>
                });
            // `route_preview` reads this machine's routing table, so it rides
            // the same pairing as the spend footer: stdio → local daemon. It
            // prefers the live daemon's view (subscription providers, reloads)
            // and falls back to static config when the control socket is
            // unreachable — the same order `bitrouter route` uses. Best-effort
            // throughout: an unreadable config just means no `route_preview`
            // tool, never a failed `mcp serve`.
            let routing: Option<
                std::sync::Arc<dyn bitrouter_mcp::capabilities::routing::RoutingQuery>,
            > = match source {
                Some(source) => match bitrouter::paths::load_config(&source).await {
                    Ok(cfg) => {
                        let socket = resolve_client_socket_from(&source, None).await.ok();
                        Some(std::sync::Arc::new(
                            bitrouter::routing_preview::RoutingPreview::new(&cfg, socket),
                        ))
                    }
                    Err(_) => None,
                },
                None => None,
            };
            bitrouter_mcp::serve(bitrouter_mcp::ServeOptions {
                transport,
                backend,
                local_url,
                cloud_url,
                cloud_token,
                bind,
                cost_footer,
                routing,
            })
            .await
        }
        McpAction::Install { client, config } => {
            bitrouter_mcp::install(bitrouter_mcp::InstallOptions {
                client: client.into(),
                config_path: config,
            })
        }
        McpAction::Search { query, limit } => {
            let outcome = bitrouter::mcp_registry::RegistryClient::new()?
                .servers(Some(&query), limit)
                .await?;
            output.emit(&mcp_registry_report(outcome)?)?;
            Ok(())
        }
        McpAction::List { limit } => {
            let outcome = bitrouter::mcp_registry::RegistryClient::new()?
                .servers(None, limit)
                .await?;
            output.emit(&mcp_registry_report(outcome)?)?;
            Ok(())
        }
        McpAction::Add { name } => {
            let outcome = bitrouter::mcp_registry::RegistryClient::new()?
                .latest(&name)
                .await?;
            match bitrouter::mcp_registry::add_stub(&outcome.data) {
                Ok(stub) => {
                    output.emit(&McpAddReport {
                        name,
                        id: stub.id,
                        yaml: stub.yaml,
                    })?;
                    Ok(())
                }
                Err(e) => anyhow::bail!(e),
            }
        }
    }
}

/// Map fetched registry entries to the `mcp list` / `mcp search` report.
fn mcp_registry_report(
    outcome: bitrouter::mcp_registry::FetchOutcome<Vec<bitrouter::mcp_registry::ServerEntry>>,
) -> Result<McpRegistryReport> {
    let servers = bitrouter::mcp_registry::registry_rows(&outcome.data)
        .into_iter()
        .map(|row| McpRegistryRow {
            name: row.name,
            version: row.version,
            install: row.install.to_string(),
            description: row.description,
        })
        .collect();
    Ok(McpRegistryReport {
        servers,
        from_cache: outcome.from_cache,
    })
}

// ===== serve / daemon control =====

/// Resolve the control-socket path for a *daemon-control* subcommand
/// (`stop`, `reload`, `status`). An explicit `--socket` override wins;
/// otherwise we resolve the config path via the standard chain, try to
/// load the YAML to read `server.control_socket`, and join the value
/// onto the config file's directory.
///
/// Loading the YAML is **best-effort**: a broken or env-var-incomplete
/// config falls back to the default socket name in the same directory.
/// That keeps `bitrouter status` answerable in exactly the state where
/// the user most wants to ask (config can't load → daemon can't be
/// running → "stopped"). The "real" config error still surfaces the
/// next time the user runs `serve` / `start`.
async fn resolve_client_socket(config: Option<&Path>, socket: Option<&Path>) -> Result<PathBuf> {
    if let Some(s) = socket {
        return Ok(s.to_path_buf());
    }
    let source = bitrouter::paths::resolve_config(config)?;
    match &source {
        bitrouter::paths::ConfigSource::File(path) => {
            let socket_str = match config::load(path).await {
                Ok(cfg) => cfg.server.control_socket,
                Err(_) => daemon::DEFAULT_CONTROL_SOCKET.to_string(),
            };
            Ok(daemon::resolve_socket_path(path, &socket_str))
        }
        bitrouter::paths::ConfigSource::Default { home } => Ok(home.join("bitrouter.sock")),
    }
}

// ===== tracing subscriber init =====

/// The filter used when neither `RUST_LOG` nor `server.log_level` supplies a
/// usable one.
const DEFAULT_LOG_FILTER: &str = "info";

/// Resolve the tracing filter. Precedence, highest first:
///
/// 1. **`RUST_LOG`** — the Rust convention, and the escape hatch an operator
///    reaches for during an incident without editing (and reloading) config.
/// 2. **`server.log_level`** from `bitrouter.yaml`. Only `serve` has a config
///    loaded this early; every other command passes `None`, because its
///    subscriber is installed before any config is read.
/// 3. **`info`**.
///
/// Returns the filter plus an optional warning. Nothing can be logged before
/// the subscriber this feeds is installed, so an unparseable filter string is
/// handed back for the caller to emit *after* `init()` rather than silently
/// swallowed — the same deferred-diagnostic shape `serve` already uses for
/// OTel init errors.
fn resolve_env_filter(
    config_log_level: Option<&str>,
) -> (tracing_subscriber::EnvFilter, Option<String>) {
    // `EnvFilter::try_from_default_env` collapses "unset" and "set but
    // invalid" into one `Err`, which is exactly the distinction that decides
    // whether the config value gets a turn — so read the variable directly.
    let rust_log = std::env::var(tracing_subscriber::EnvFilter::DEFAULT_ENV).ok();
    resolve_env_filter_from(rust_log.as_deref(), config_log_level)
}

/// The precedence logic behind [`resolve_env_filter`], with the environment
/// passed in so it is testable without mutating process-global state.
fn resolve_env_filter_from(
    rust_log: Option<&str>,
    config_log_level: Option<&str>,
) -> (tracing_subscriber::EnvFilter, Option<String>) {
    let non_blank = |s: &&str| !s.trim().is_empty();
    let (source, raw) = match (
        rust_log.filter(non_blank),
        config_log_level.filter(non_blank),
    ) {
        (Some(raw), _) => (tracing_subscriber::EnvFilter::DEFAULT_ENV, raw),
        (None, Some(level)) => ("server.log_level", level),
        (None, None) => return (tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER), None),
    };
    match parse_log_filter(raw) {
        Ok(filter) => (filter, None),
        Err(reason) => (
            tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER),
            Some(format!(
                "invalid {source} value {raw:?}: {reason} — falling back to `{DEFAULT_LOG_FILTER}`"
            )),
        ),
    }
}

/// Parse one filter string, rejecting the failure mode `EnvFilter` itself
/// won't.
///
/// `EnvFilter::try_new("dbug")` **succeeds**: with no directive syntax present
/// it reads the word as a *target* named `dbug` at trace level. The resulting
/// filter matches nothing, so a one-character typo in `log_level` silently
/// mutes the daemon instead of erroring — the worst possible outcome for a
/// logging setting. So a bare word (no `=`, no `,`) must parse as a level;
/// anything carrying directive syntax is handed to `EnvFilter` as-is.
fn parse_log_filter(raw: &str) -> std::result::Result<tracing_subscriber::EnvFilter, String> {
    let raw = raw.trim();
    if !raw.contains('=')
        && !raw.contains(',')
        && raw
            .parse::<tracing_subscriber::filter::LevelFilter>()
            .is_err()
    {
        return Err(
            "expected a level (`trace`, `debug`, `info`, `warn`, `error`, `off`) or an \
             `EnvFilter` directive list such as `info,bitrouter=debug`"
                .to_string(),
        );
    }
    tracing_subscriber::EnvFilter::try_new(raw).map_err(|e| e.to_string())
}

/// Install a basic fmt-only tracing subscriber. Used for every command
/// except `serve` and the `acp` subcommands — see
/// [`init_serve_tracing_subscriber`] and [`init_stderr_tracing_subscriber`].
///
/// Runs before any config is read, so `RUST_LOG` is the only input.
fn init_basic_tracing_subscriber() {
    let (env_filter, warning) = resolve_env_filter(None);
    tracing_subscriber::fmt()
        // Diagnostics MUST go to stderr so stdout stays a pure JSON result
        // surface (`tracing_subscriber::fmt()` otherwise defaults to stdout).
        .with_writer(std::io::stderr)
        .with_env_filter(env_filter)
        .init();
    if let Some(warning) = warning {
        tracing::warn!("{warning}");
    }
}

/// Install a tracing subscriber that writes to **stderr**. Used for the `acp`
/// subcommands, which keep stdout exclusively for their machine-readable
/// protocol stream (JSON-RPC for `acp serve`, NDJSON for `acp prompt`) —
/// logging on stdout would corrupt the stream the caller parses.
///
/// Runs before any config is read, so `RUST_LOG` is the only input.
fn init_stderr_tracing_subscriber() {
    let (env_filter, warning) = resolve_env_filter(None);
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(env_filter)
        .init();
    if let Some(warning) = warning {
        tracing::warn!("{warning}");
    }
}

/// Where this process's session log lives: `~/.bitrouter/logs/`, one file per
/// session, named for the wall-clock start and the pid.
///
/// The pid is what makes it unique — two sessions started in the same second
/// must not share a file, and at this point in startup there is no session id
/// to name it after (the log has to exist before the agent that would produce
/// one is spawned).
fn session_log_path() -> anyhow::Result<PathBuf> {
    let dir = bitrouter::paths::runtime_home()?.join("logs");
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y%m%dT%H%M%S");
    Ok(dir.join(format!("session-{stamp}-{}.log", std::process::id())))
}

/// Install a tracing subscriber that writes to **stderr and a session log
/// file**. Used for the `acp` subcommands.
///
/// The fourth initializer, alongside [`init_basic_tracing_subscriber`],
/// [`init_stderr_tracing_subscriber`], and [`init_serve_tracing_subscriber`].
/// It exists because an ACP session has *two* diagnostic streams — the
/// substrate's own and the agent child's, which `spawn_agent_process` now
/// captures and re-emits through `tracing` — and reading them means reading
/// them interleaved, in one place, after the fact. stderr is kept as well as
/// the file: dropping it would silently change what an operator running the
/// command in a terminal sees.
///
/// Returns the log's path so the caller can name it in a failure. A file that
/// cannot be opened is **not** fatal — the session still runs, logging to
/// stderr alone — because losing the transcript is not a reason to refuse to
/// start.
fn init_session_log_tracing_subscriber(also_stderr: bool) -> Option<PathBuf> {
    use tracing_subscriber::fmt::writer::MakeWriterExt;

    let env_filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    };
    let opened = session_log_path().and_then(|path| {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok((path, file))
    });
    match opened {
        Ok((path, file)) => {
            let file = std::sync::Arc::new(file);
            // `chat` passes `false`: it draws on this terminal, and a log line
            // between two frames scrolls the screen underneath its renderer.
            // Everything is still in the file, and `chat` shows the tail of it
            // when a session ends badly.
            if also_stderr {
                tracing_subscriber::fmt()
                    .with_ansi(false)
                    .with_writer(std::io::stderr.and(file))
                    .with_env_filter(env_filter())
                    .init();
            } else {
                tracing_subscriber::fmt()
                    .with_ansi(false)
                    .with_writer(file)
                    .with_env_filter(env_filter())
                    .init();
            }
            Some(path)
        }
        Err(e) => {
            init_stderr_tracing_subscriber();
            tracing::warn!("no session log ({e}); logging to stderr only");
            None
        }
    }
}

/// Install the full tracing subscriber for the `serve` command: fmt plus
/// — when OTel is configured — the bridge layer that mirrors `tracing`
/// spans into OTel via the supplied exporter's SDK tracer.
///
/// `tracing-opentelemetry`'s bridge layer captures its tracer eagerly,
/// so this MUST be called after [`bitrouter_observe::otel::OtelExporter::new`]
/// has built the real exporter; passing `None` (OTel disabled in config)
/// installs the fmt-only registry.
///
/// This is the one path with a config in hand, so it is where
/// `server.log_level` takes effect. Resolution happens once here — a later
/// `bitrouter reload` re-reads the config but cannot re-install the
/// subscriber, so a changed `log_level` needs a restart.
fn init_serve_tracing_subscriber(
    exporter: Option<&bitrouter_observe::otel::OtelExporter>,
    config_log_level: &str,
) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let (env_filter, warning) = resolve_env_filter(Some(config_log_level));
    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer());
    match exporter {
        Some(exp) => registry
            .with(bitrouter_observe::otel::http_layer::tracing_subscriber_layer(exp))
            .init(),
        None => registry.init(),
    }
    if let Some(warning) = warning {
        tracing::warn!("{warning}");
    }
}

/// Resolve the `bitrouter.log` path for `start` / `restart`. An
/// explicit `--log` override wins; otherwise we place the log next to
/// the config file (e.g. `~/.bitrouter/bitrouter.log`) so the daemon's
/// runtime artefacts — config, socket, pid file, log — all live in one
/// directory. The legacy default of `./bitrouter.log` would land the
/// log file in whichever CWD the launcher happened to be in.
fn resolve_log_path(home: &Path, log: Option<&Path>) -> PathBuf {
    if let Some(l) = log {
        return l.to_path_buf();
    }
    home.join("bitrouter.log")
}

/// Variant of [`resolve_client_socket`] for subcommands (`restart`,
/// `route`) that load the config for other reasons anyway, so a config
/// failure is a real error worth surfacing.
async fn resolve_client_socket_from(
    source: &bitrouter::paths::ConfigSource,
    socket: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(s) = socket {
        return Ok(s.to_path_buf());
    }
    match source {
        bitrouter::paths::ConfigSource::File(path) => {
            let cfg = config::load(path)
                .await
                .with_context(|| format!("loading {}", path.display()))?;
            Ok(daemon::resolve_socket_path(
                path,
                &cfg.server.control_socket,
            ))
        }
        bitrouter::paths::ConfigSource::Default { home } => Ok(home.join("bitrouter.sock")),
    }
}

async fn serve(source: &bitrouter::paths::ConfigSource) -> Result<()> {
    // Ensure the bitrouter home directory exists (zero-config first-run
    // creates `~/.bitrouter` on demand) and chdir into it. Every
    // relative path in the config — `database.url`,
    // `server.control_socket`, policy / agent / mcp file references —
    // then interprets relative to one stable location instead of
    // whichever CWD the launcher happened to be in. The daemon's
    // runtime artefacts (db, socket, pid, log) all land in the home.
    let home = source.home();
    bitrouter::paths::ensure_home_directory(home)?;
    std::env::set_current_dir(home)
        .with_context(|| format!("chdir to bitrouter home {}", home.display()))?;

    let mut cfg = bitrouter::paths::load_config(source).await?;
    // Auto-enable the `claude-code` subscription provider when the user has
    // signed in (a `claude-code` credential is in the OAuth store). Runs before
    // the registry merge so the merge fills the inserted provider's
    // `api_base` / `api_protocol` / auth from the fetched registry entry.
    bitrouter::claude_code::enable_if_logged_in(&mut cfg);
    // Fetch + merge the public provider registry before assembly, so the daemon
    // routes every credentialed provider's registered models. Best-effort and
    // cache-backed; a no-op when disabled or unreachable with no cache.
    bitrouter::merge_registry_into(&mut cfg).await;
    announce_zero_config(source, &cfg);
    maybe_announce_telemetry(home);
    let listen = cfg.server.listen.clone();
    // For a `File` source the socket is resolved against the config file's
    // directory (preserves any user override); for `Default` it lives at
    // `<home>/bitrouter.sock`. Shared with `start`/`spawn` via `socket_path_for`.
    let socket_path = daemon::socket_path_for(source, &cfg);
    let pid_path = pid_path_for(&socket_path);

    let config_path_for_reload = match source {
        bitrouter::paths::ConfigSource::File(path) => Some(path.as_path()),
        bitrouter::paths::ConfigSource::Default { .. } => None,
    };
    let assembled = bitrouter::build_app_with_path(&cfg, config_path_for_reload).await?;
    // The OTel exporter was just constructed (inside `build_app_with_path`).
    // Hand its SDK tracer to the `tracing-opentelemetry` bridge layer now
    // — the bridge captures its tracer at construction, so this can only
    // happen after the exporter exists.
    init_serve_tracing_subscriber(assembled.otel_exporter.as_deref(), &cfg.server.log_level);
    // Surface any deferred OTel-init failure now that the subscriber is up.
    if let Some(msg) = &assembled.otel_init_error {
        tracing::error!("{msg}");
    }
    let workflow_trace_capture =
        bitrouter::workflow_state::real_trace::capture_from_env().map_err(anyhow::Error::from)?;
    if workflow_trace_capture.is_some() {
        tracing::info!(
            env = bitrouter::workflow_state::real_trace::WORKFLOW_TRACE_JSONL_ENV,
            "workflow trace capture enabled"
        );
    }
    let app = Arc::new(assembled.app);
    let eval_router = bitrouter::eval::api::router(
        assembled.eval_service.clone(),
        assembled.db.clone(),
        cfg.server.skip_auth,
    );
    let policy_store = assembled.policy_store;
    let trajectory_outbox_for_shutdown = assembled.trajectory_outbox_publisher.clone();
    // Clone before moving the original into `run_control_socket` — we
    // need a handle here too so the shutdown path below can drive the
    // exporter flush before the runtime tears down.
    let observe_provider = assembled.observe;
    let observe_for_shutdown = observe_provider.clone();
    let reload_source = match source {
        bitrouter::paths::ConfigSource::File(path) => {
            bitrouter::reload::ReloadSource::File(path.clone())
        }
        bitrouter::paths::ConfigSource::Default { .. } => bitrouter::reload::ReloadSource::Default,
    };
    // Cloned before the reloader takes it: the control socket needs the same
    // handle to install `providers/set` route overrides into the live
    // transform.
    let policy_router_for_control = assembled.policy_table_router.clone();
    let acp_runtime_for_control = assembled.acp_runtime.clone();
    let reloader: Arc<dyn daemon::DaemonReloader> = Arc::new(
        bitrouter::reload::AppReloader::new(
            policy_store.clone(),
            assembled.routing_table,
            assembled.upstream_executor,
            reload_source,
        )
        .with_policy_runtime(assembled.policy_runtime)
        .with_policy_table_router(assembled.policy_table_router),
    );

    daemon::write_pid_file(&pid_path).await?;
    println!(
        "bitrouter {} — serving on {listen} (control: {})",
        bitrouter::VERSION,
        socket_path.display()
    );

    let http_app = app.clone();
    let http_listen = listen.clone();
    let (http_shutdown_tx, http_shutdown_rx) = tokio::sync::oneshot::channel();
    let http = async move {
        // Wrap the SDK router in tower-http's TraceLayer (plus inbound W3C
        // trace-context propagation) so the inbound HTTP request becomes
        // the SERVER span parent of the bitrouter `chat` INTERNAL span.
        let otel_wrapper = bitrouter_observe::otel::http_layer::router_wrapper();
        let shutdown = async move {
            let _ = http_shutdown_rx.await;
        };
        match workflow_trace_capture {
            Some(capture) => {
                let workflow_wrapper = capture.router_wrapper();
                let eval_router = eval_router.clone();
                http_app
                    .serve_with_router_wrapper_and_shutdown(
                        &http_listen,
                        move |router| {
                            workflow_wrapper(otel_wrapper(router.merge(eval_router.clone())))
                        },
                        shutdown,
                    )
                    .await
            }
            None => {
                http_app
                    .serve_with_router_wrapper_and_shutdown(
                        &http_listen,
                        move |router| otel_wrapper(router.merge(eval_router.clone())),
                        shutdown,
                    )
                    .await
            }
        }
        .map_err(anyhow::Error::from)
    };
    let control = daemon::run_control_socket_with_acp_runtime(
        socket_path,
        app.clone(),
        listen,
        reloader.clone(),
        observe_provider,
        policy_router_for_control,
        acp_runtime_for_control,
    );

    // SIGHUP triggers a config reload — reload should be available via either
    // `bitrouter reload` (the control endpoint) *or* a HUP signal. Same fan-out
    // as the Reload command — every reloadable subsystem. SIGHUP is Unix-only;
    // on Windows there is no equivalent, so the HUP future stays pending and
    // reload is reached exclusively through `bitrouter reload`.
    let hup_reloader = reloader.clone();
    let hup = async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut hup = match signal(SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => return Err::<(), anyhow::Error>(anyhow::Error::from(e)),
            };
            loop {
                if hup.recv().await.is_none() {
                    return Ok(());
                }
                match hup_reloader.reload().await {
                    Ok(()) => tracing::info!("SIGHUP — reload succeeded"),
                    Err(e) => tracing::warn!(error = %e, "SIGHUP reload failed"),
                }
            }
        }
        #[cfg(not(unix))]
        {
            // No SIGHUP on this platform — keep the reloader handle alive and
            // park forever so the `select!` arm below never fires.
            let _keep = &hup_reloader;
            std::future::pending::<()>().await;
            Ok::<(), anyhow::Error>(())
        }
    };

    // Termination signals end the loop the same way `bitrouter stop` does — so
    // the shutdown path below (observe flush, pid-file cleanup) runs in every
    // graceful termination mode. On Unix that's SIGINT (ctrl-C) and SIGTERM
    // (systemd / `kill`); on Windows it's the console control events
    // (Ctrl-C / Ctrl-Break / window close / system shutdown).
    let term = async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigint = signal(SignalKind::interrupt()).map_err(anyhow::Error::from)?;
            let mut sigterm = signal(SignalKind::terminate()).map_err(anyhow::Error::from)?;
            tokio::select! {
                _ = sigint.recv() => tracing::info!("SIGINT — shutting down"),
                _ = sigterm.recv() => tracing::info!("SIGTERM — shutting down"),
            }
            Ok::<(), anyhow::Error>(())
        }
        #[cfg(windows)]
        {
            use tokio::signal::windows;
            let mut ctrl_c = windows::ctrl_c().map_err(anyhow::Error::from)?;
            let mut ctrl_break = windows::ctrl_break().map_err(anyhow::Error::from)?;
            let mut ctrl_close = windows::ctrl_close().map_err(anyhow::Error::from)?;
            let mut ctrl_shutdown = windows::ctrl_shutdown().map_err(anyhow::Error::from)?;
            tokio::select! {
                _ = ctrl_c.recv() => tracing::info!("Ctrl-C — shutting down"),
                _ = ctrl_break.recv() => tracing::info!("Ctrl-Break — shutting down"),
                _ = ctrl_close.recv() => tracing::info!("console close — shutting down"),
                _ = ctrl_shutdown.recv() => tracing::info!("system shutdown — shutting down"),
            }
            Ok::<(), anyhow::Error>(())
        }
    };

    // Control/termination requests signal the SDK server and then keep polling
    // that same future until axum and every required finalizer finish. An HTTP
    // failure still returns directly; a HUP listener failure only disables
    // reload signaling and leaves the server running.
    let result = supervise_http_shutdown(http, control, hup, term, http_shutdown_tx).await;

    if let Some(publisher) = trajectory_outbox_for_shutdown {
        match publisher.drain_after_active_worker().await {
            Ok(summary) if summary.failed > 0 => tracing::warn!(
                attempted = summary.attempted,
                delivered = summary.delivered,
                failed = summary.failed,
                "trajectory outbox drain completed with poison items still pending"
            ),
            Ok(_) => {}
            Err(_) => tracing::warn!(
                reason = "shutdown_drain_failed",
                "trajectory outbox shutdown drain failed"
            ),
        }
    }

    // Drive the OTel exporter's flush before anything else drops — its
    // `rt-tokio` background tasks need a live async runtime to drain,
    // and `spawn_blocking` (inside the provider's `shutdown`) parks on
    // a dedicated thread so the runtime is free to keep ticking. The
    // impl is idempotent: a follow-up Drop is a no-op.
    observe_for_shutdown.shutdown().await;

    daemon::remove_pid_file(&pid_path).await;
    result
}

async fn start(
    source: &bitrouter::paths::ConfigSource,
    log_path: &Path,
    action: &'static str,
) -> Result<DaemonActionReport> {
    // Make sure the bitrouter home exists *before* we open the log
    // file inside it. (Zero-config first-run lands here with the home
    // not yet created on disk.)
    bitrouter::paths::ensure_home_directory(source.home())?;

    // Refuse to start a second daemon on top of a live one — silent overlap
    // would race two `serve`s for the same socket and one would die into the
    // log file (the user wouldn't see it).
    let cfg_socket_path: Option<PathBuf> = match source {
        bitrouter::paths::ConfigSource::File(path) => match config::load(path).await {
            Ok(cfg) => Some(daemon::socket_path_for(source, &cfg)),
            // Best-effort: a broken/env-incomplete config can't locate the
            // socket, but `serve` would fail the same way → the child-death
            // check below still surfaces it.
            Err(_) => None,
        },
        bitrouter::paths::ConfigSource::Default { home } => Some(home.join("bitrouter.sock")),
    };
    if let Some(socket) = &cfg_socket_path {
        let pid_path = pid_path_for(socket);
        if let Some(pid) = daemon::read_pid_file(&pid_path).await {
            if process_is_alive(pid) {
                anyhow::bail!(
                    "bitrouter is already running (pid {pid}); use `restart` or `stop` first"
                );
            }
            // Stale PID file — clean up before proceeding.
            daemon::remove_pid_file(&pid_path).await;
        }
    }

    // Launch the detached `serve` and poll its control socket until it is
    // actually serving — so "started" only prints once the daemon can answer
    // (config load + DB migrations + registry fetch all complete first).
    let outcome = daemon::start_and_wait(
        source,
        log_path,
        cfg_socket_path.as_deref(),
        daemon::DAEMON_READY_TIMEOUT,
    )
    .await?;

    match outcome {
        daemon::DaemonStartOutcome::Ready(info) => Ok(DaemonActionReport::started(
            action,
            "started",
            info.pid,
            info.listen,
            info.models,
            log_path.display().to_string(),
        )),
        // The process is alive but slow to answer — don't kill it; the daemon
        // may still be migrating / fetching the registry. Report and exit 0.
        daemon::DaemonStartOutcome::NotReadyInTime { pid } => {
            let p = bitrouter::style::Palette::for_stderr();
            eprintln!(
                "{cyan}note:{reset} bitrouter daemon started (pid {pid}) but has not become \
                 ready after {secs}s — check logs at {log}",
                cyan = p.cyan,
                reset = p.reset,
                secs = daemon::DAEMON_READY_TIMEOUT.as_secs(),
                log = log_path.display(),
            );
            Ok(DaemonActionReport::not_ready(
                action,
                pid,
                log_path.display().to_string(),
            ))
        }
        daemon::DaemonStartOutcome::Exited { status, log_tail } => {
            daemon::eprint_failure_log(log_path, &log_tail);
            anyhow::bail!("daemon exited during startup ({status})")
        }
    }
}

/// Tell the operator they're running zero-config — and exactly which
/// providers auto-enabled from the environment, so the absence of a
/// model later doesn't read as a bug. No-op for a `File` source.
fn announce_zero_config(
    source: &bitrouter::paths::ConfigSource,
    cfg: &bitrouter_sdk::config::Config,
) {
    if !source.is_default() {
        return;
    }
    let enabled: Vec<&str> = cfg.providers.keys().map(String::as_str).collect();
    if enabled.is_empty() {
        print_onboarding_hint();
    } else {
        bitrouter::error_report::info(format_args!(
            "zero-config mode — auto-enabled providers: {}",
            enabled.join(", ")
        ));
    }
}

/// Multi-line guidance shown when zero-config detects no credential of any
/// kind. The recommendation chain is intentional:
///
///   1. `bitrouter cloud login` — one OAuth account, every supported model.
///   2. `BITROUTER_API_KEY` — long-lived `brk_…` key, same coverage.
///   3. Any upstream provider the user already pays for, locally.
///
/// Rendered directly (not through `error_report::info`) because that helper
/// is single-line by design.
/// First-run telemetry notice, shown exactly once per install (guarded by a
/// sentinel in the home). BitRouter ships telemetry **off by default**; this
/// notice exists so opting in is an informed, one-time choice. Failure to write
/// the sentinel is non-fatal — telemetry is never blocked on the notice.
fn maybe_announce_telemetry(home: &std::path::Path) {
    match bitrouter::paths::mark_telemetry_notice_shown(home) {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            tracing::debug!("telemetry notice sentinel: {e:#}");
            return;
        }
    }
    let p = bitrouter::style::Palette::for_stderr();
    eprintln!(
        "{cyan}{bold}info:{reset} optional usage telemetry is available — and OFF by default.",
        cyan = p.cyan,
        bold = p.bold,
        reset = p.reset,
    );
    eprintln!();
    eprintln!("  Nothing is sent unless you opt in. Two levels are offered:");
    eprintln!(
        "    • metadata — model, tokens, latency, finish reason, routing (no message content)"
    );
    eprintln!("    • full     — the above plus request + response message content");
    eprintln!();
    eprintln!("  Enable it under plugins.bitrouter-observe.telemetry in your config:");
    eprintln!();
    eprintln!("       plugins:");
    eprintln!("         bitrouter-observe:");
    eprintln!("           telemetry:");
    eprintln!("             enabled: true");
    eprintln!("             level: metadata   # or: full");
    eprintln!();
    eprintln!("  Remove the block (or set enabled: false) to turn it off again.");
    eprintln!();
}

fn print_onboarding_hint() {
    let p = bitrouter::style::Palette::for_stderr();
    eprintln!(
        "{cyan}{bold}info:{reset} no providers are configured yet. Choose one:",
        cyan = p.cyan,
        bold = p.bold,
        reset = p.reset,
    );
    eprintln!();
    eprintln!("  1. Sign in to BitRouter Cloud — one account covers every model:");
    eprintln!();
    eprintln!("       bitrouter cloud login");
    eprintln!("       bitrouter cloud --help        # manage keys, usage, policies, billing");
    eprintln!();
    eprintln!("  2. Or paste a BitRouter API key:");
    eprintln!();
    eprintln!("       export BITROUTER_API_KEY=brk_…");
    eprintln!();
    eprintln!("  3. Or use a provider you already pay for, locally:");
    eprintln!();
    eprintln!("       bitrouter providers login claude-code     # Claude Pro/Max subscription");
    eprintln!("       bitrouter providers login github-copilot  # GitHub Copilot subscription");
    eprintln!("       bitrouter providers login openai-codex    # ChatGPT subscription");
    eprintln!();
    eprintln!("     …or set an API-key env var:");
    eprintln!();
    let env_vars = other_provider_env_var_hints();
    for var in &env_vars {
        eprintln!("       export {var}=…");
    }
    eprintln!();
}

/// Deduplicated, sorted env-var names for every built-in provider except
/// `BITROUTER_API_KEY` (rendered separately as step 2). Used by the
/// onboarding hint.
fn other_provider_env_var_hints() -> Vec<String> {
    let mut vars: Vec<String> = bitrouter_providers::zero_config_env_var_providers()
        .into_iter()
        .map(|(_, env)| env)
        .filter(|v| v != "BITROUTER_API_KEY")
        .collect();
    vars.sort();
    vars.dedup();
    vars
}

async fn stop(socket: &Path) -> Result<DaemonActionReport> {
    match daemon::send_command(socket, &DaemonCommand::Stop).await? {
        DaemonResponse::Ok => Ok(DaemonActionReport::simple("stop", "stopped")),
        DaemonResponse::Error { message } => Err(anyhow::anyhow!(message)),
        other => Err(anyhow::anyhow!("unexpected response: {other:?}")),
    }
}

const RESTART_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(30);
const RESTART_CLEANUP_PERIOD: std::time::Duration = std::time::Duration::from_secs(2);
const RESTART_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

struct RestartRelease {
    ready: bool,
}

#[derive(Clone, Copy)]
struct RestartTarget {
    pid: u32,
}

async fn wait_for_restart_condition(
    timeout: std::time::Duration,
    mut ready: impl FnMut() -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if ready() {
            return true;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return ready();
        }
        tokio::time::sleep(RESTART_POLL_INTERVAL.min(remaining)).await;
    }
}

async fn await_restart_release<IsAlive, EndpointInUse>(
    socket: &Path,
    target: Option<RestartTarget>,
    mut is_alive: IsAlive,
    mut endpoint_in_use: EndpointInUse,
) -> RestartRelease
where
    IsAlive: FnMut(u32) -> bool,
    EndpointInUse: FnMut(&Path) -> bool,
{
    let Some(target) = target else {
        let ready =
            wait_for_restart_condition(RESTART_GRACE_PERIOD, || !endpoint_in_use(socket)).await;
        return RestartRelease { ready };
    };
    let pid = target.pid;

    let process_exited = wait_for_restart_condition(RESTART_GRACE_PERIOD, || !is_alive(pid)).await;
    if process_exited {
        let ready =
            wait_for_restart_condition(RESTART_CLEANUP_PERIOD, || !endpoint_in_use(socket)).await;
        return RestartRelease { ready };
    }
    RestartRelease { ready: false }
}

async fn restart_control_phase<SendCommand, SendCommandFuture, IsAlive, EndpointInUse>(
    socket: &Path,
    pidfile_pid: Option<u32>,
    endpoint_was_in_use: bool,
    mut send_command: SendCommand,
    is_alive: IsAlive,
    endpoint_in_use: EndpointInUse,
) -> Result<RestartRelease>
where
    SendCommand: FnMut(DaemonCommand) -> SendCommandFuture,
    SendCommandFuture: std::future::Future<Output = Result<DaemonResponse>>,
    IsAlive: FnMut(u32) -> bool,
    EndpointInUse: FnMut(&Path) -> bool,
{
    let target = if endpoint_was_in_use {
        let status = send_command(DaemonCommand::Status).await.map_err(|_| {
            anyhow::anyhow!("running daemon process identity could not be verified for restart")
        })?;
        let pid = match status {
            DaemonResponse::Status { pid, .. } if pid != 0 => pid,
            _ => anyhow::bail!("running daemon process identity could not be verified for restart"),
        };
        match send_command(DaemonCommand::Stop).await {
            Ok(DaemonResponse::Ok) => {}
            Ok(DaemonResponse::Error { message }) => return Err(anyhow::anyhow!(message)),
            Ok(other) => return Err(anyhow::anyhow!("unexpected response: {other:?}")),
            Err(e) => tracing::warn!(error = %e, "stop failed — proceeding to start"),
        }
        Some(RestartTarget { pid })
    } else {
        pidfile_pid.map(|pid| RestartTarget { pid })
    };

    let release = await_restart_release(socket, target, is_alive, endpoint_in_use).await;
    Ok(release)
}

async fn restart(
    source: &bitrouter::paths::ConfigSource,
    socket: &Path,
    log_path: &Path,
) -> Result<DaemonActionReport> {
    let pid_path = pid_path_for(socket);
    let pidfile_pid = daemon::read_pid_file(&pid_path)
        .await
        .filter(|pid| process_is_alive(*pid));
    // A reachable control endpoint must authenticate the daemon process through
    // Status before Stop. With no endpoint, a live pidfile can delay replacement
    // but never authorize a signal. Stop transport failure remains best-effort
    // after authentication; explicit or unexpected Stop responses remain fatal.
    let endpoint_was_in_use = daemon::endpoint_in_use(socket);
    let release = restart_control_phase(
        socket,
        pidfile_pid,
        endpoint_was_in_use,
        |command| async move { daemon::send_command(socket, &command).await },
        process_is_alive,
        daemon::endpoint_in_use,
    )
    .await?;
    if !release.ready {
        anyhow::bail!("old daemon did not release its process and control endpoint in time");
    }
    start(source, log_path, "restart").await
}

async fn reload(socket: &Path) -> Result<DaemonActionReport> {
    // Snapshot every env-var-credentialed built-in provider's key from
    // *this* (CLI) process and hand them to the daemon along with the
    // reload command, so `export OPENAI_API_KEY=…; bitrouter reload`
    // propagates the new value into the running daemon instead of
    // requiring a full stop+start. The daemon writes them into its
    // env-override map before re-parsing config / re-running
    // zero-config provider detection.
    let env: Vec<(String, String)> = bitrouter_providers::zero_config_env_var_providers()
        .into_iter()
        .filter_map(|(_, var)| {
            std::env::var(&var)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| (var, v))
        })
        .collect();
    match daemon::send_command(socket, &DaemonCommand::Reload { env }).await? {
        DaemonResponse::Ok => Ok(DaemonActionReport::simple("reload", "reloaded")),
        DaemonResponse::Error { message } => Err(anyhow::anyhow!(message)),
        other => Err(anyhow::anyhow!("unexpected response: {other:?}")),
    }
}

async fn status(socket: &Path) -> Result<StatusReport> {
    let report = match daemon::send_command(socket, &DaemonCommand::Status).await {
        Ok(DaemonResponse::Status {
            pid,
            listen,
            models,
        }) => StatusReport::running(pid, listen, models, socket.display().to_string()),
        Ok(DaemonResponse::Error { message }) => return Err(anyhow::anyhow!(message)),
        Ok(other) => return Err(anyhow::anyhow!("unexpected response: {other:?}")),
        // No daemon listening on the socket → report stopped, not error.
        // Anything else (permission denied, malformed response, …) is a
        // real failure and bubbles to the pretty reporter.
        Err(e) if daemon::is_not_reachable(&e) => {
            StatusReport::stopped(socket.display().to_string())
        }
        Err(e) => return Err(e),
    };
    // #607 self-update nudge — emitted to stderr so stdout stays a pure JSON
    // result (`status 2>/dev/null | jq` must not see the nudge).
    if let Ok(source) = bitrouter::paths::resolve_config(None) {
        bitrouter::update::maybe_nudge(source.home(), &bitrouter::style::Palette::for_stderr())
            .await;
    }
    Ok(report)
}

/// `bitrouter status --requests` — the settled-request table.
///
/// One snapshot, printed the same way whether stdout is a terminal or a pipe.
/// Nothing here takes the screen, so `| less`, `> file`, and an agent reading
/// the bytes all see exactly what a person does.
async fn request_table(config: Option<&Path>, socket: &Path) -> Result<()> {
    let source = bitrouter::paths::resolve_config(config)?;
    let window = bitrouter::metering::store::TimeWindow::Today;
    print!(
        "{}",
        bitrouter::tui::oneshot_text(&source, socket, window).await
    );
    Ok(())
}

async fn route(
    model: &str,
    source: &bitrouter::paths::ConfigSource,
    socket: &Path,
) -> Result<RouteReport> {
    // Try the running daemon first — its routing table reflects any `reload`s.
    if daemon::endpoint_in_use(socket) {
        match daemon::send_command(
            socket,
            &DaemonCommand::Route {
                model: model.into(),
            },
        )
        .await
        {
            Ok(DaemonResponse::Route { chain }) => {
                return Ok(route_report(model, "live daemon", chain));
            }
            Ok(DaemonResponse::Error { message }) => return Err(anyhow::anyhow!(message)),
            Ok(other) => return Err(anyhow::anyhow!("unexpected response: {other:?}")),
            Err(e) => {
                // Fall through to the standalone resolution. The daemon may
                // just not be reachable from this client invocation.
                tracing::debug!(error = %e, "daemon route failed — resolving from config");
            }
        }
    }
    let cfg = bitrouter::paths::load_config(source).await?;
    let chain = commands::resolve_route(&cfg, model).await?;
    let label = if source.is_default() {
        "zero-config"
    } else {
        "config"
    };
    Ok(route_report(model, label, chain))
}

/// Build a [`RouteReport`] from a resolved hop chain (wire-safe `RouteHop`s).
fn route_report(model: &str, resolved_via: &str, chain: Vec<RouteHop>) -> RouteReport {
    RouteReport {
        model: model.to_string(),
        resolved_via: resolved_via.to_string(),
        chain: chain
            .into_iter()
            .map(|h| RouteHopView {
                provider: h.provider,
                service_id: h.service_id,
                protocol: h.api_protocol,
            })
            .collect(),
    }
}

// ===== management commands =====

/// Read a provider API key from stdin (for `providers login --key-stdin`).
/// Consumes the whole stream and trims surrounding whitespace/newline so a
/// `printf %s "$KEY" | …` pipe and an `echo "$KEY" | …` pipe both work.
fn read_api_key_from_stdin() -> Result<String> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading API key from stdin")?;
    let key = buf.trim().to_string();
    if key.is_empty() {
        anyhow::bail!("no API key on stdin (--key-stdin)");
    }
    Ok(key)
}

async fn key(action: KeyAction) -> Result<KeySignReport> {
    match action {
        KeyAction::Sign { user, db, policy } => {
            let key = commands::key_sign(&db, &user, policy.as_deref()).await?;
            Ok(KeySignReport {
                id: key.id,
                user,
                secret: key.secret,
                policy,
                hash_stored: true,
            })
        }
    }
}

async fn models(
    source: &bitrouter::paths::ConfigSource,
    provider: Option<&str>,
) -> Result<ModelsReport> {
    let cfg = bitrouter::paths::load_config(source).await?;
    let models = commands::list_models(&cfg, provider).await?;
    Ok(ModelsReport {
        models: models
            .into_iter()
            .map(|(id, providers)| ModelRow { id, providers })
            .collect(),
    })
}

async fn policy(action: PolicyAction, output: &Output) -> Result<()> {
    match action {
        PolicyAction::Create { id, dir } => {
            let path = commands::create_policy(&dir, &id).await?;
            output.emit(&PolicyCreateReport {
                id,
                path: path.display().to_string(),
                created: true,
            })?;
        }
        PolicyAction::Init {
            name,
            preset,
            strong,
            strong_effort,
            economy,
            economy_effort,
            config,
        } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let config_path = require_policy_config_path(&source)?;
            let source_raw = tokio::fs::read_to_string(config_path).await?;
            let source_config = bitrouter_sdk::config::parse(&source_raw)?;
            if let Some(strong_model) = strong.as_deref() {
                bitrouter::policy_lock::validate_routable_model(&source_config, strong_model)
                    .await?;
                bitrouter::policy_lock::validate_routable_effort(
                    &source_config,
                    strong_model,
                    strong_effort,
                )
                .await?;
            }
            bitrouter::policy_lock::validate_routable_model(&source_config, &economy).await?;
            bitrouter::policy_lock::validate_routable_effort(
                &source_config,
                &economy,
                economy_effort,
            )
            .await?;
            let update = bitrouter::policy_lock::initialize_files_with_efforts(
                config_path,
                &name,
                &preset,
                strong.as_deref(),
                strong_effort,
                &economy,
                economy_effort,
            )
            .await?;
            output.emit(
                &routing_policy_report(config_path, "init", true, update.changes, None).await?,
            )?;
        }
        PolicyAction::Check { config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let config_path = require_policy_config_path(&source)?;
            output.emit(
                &routing_policy_report(config_path, "check", false, Vec::new(), None).await?,
            )?;
        }
        PolicyAction::Verify { evidence, config } => {
            if !evidence {
                anyhow::bail!("policy verify currently requires `--evidence`");
            }
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let config_path = require_policy_config_path(&source)?;
            let verification = bitrouter::policy_lock::verify_evidence_files(config_path).await?;
            output.emit(&PolicyReport {
                action: "verify-evidence".into(),
                path: Some(config_path.display().to_string()),
                candidate_path: None,
                digest: Some(verification.policy_digest.clone()),
                mode: "n/a".into(),
                policies: Vec::new(),
                bindings: Default::default(),
                changes: vec![format!(
                    "verified {} eval results under {}",
                    verification.eval_results, verification.evidence_root
                )],
                policy: Some(serde_json::to_value(verification)?),
                applied: false,
            })?;
        }
        PolicyAction::Status { config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let config_path = require_policy_config_path(&source)?;
            output.emit(
                &routing_policy_report(config_path, "status", false, Vec::new(), None).await?,
            )?;
        }
        PolicyAction::Show { name, config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let config_path = require_policy_config_path(&source)?;
            output.emit(
                &routing_policy_report(config_path, "show", false, Vec::new(), Some(&name)).await?,
            )?;
        }
        PolicyAction::Reload { config, socket } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let socket = resolve_client_socket_from(&source, socket.as_deref()).await?;
            output.emit(&reload(&socket).await?)?;
        }
        PolicyAction::Compile {
            output: candidate_path,
            snapshot_time,
            eval_snapshot,
            config,
        } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let config_path = require_policy_config_path(&source)?;
            let snapshot_time =
                snapshot_time.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
            let update = bitrouter::policy_lock::compile_files_with_eval(
                config_path,
                snapshot_time,
                eval_snapshot.as_deref(),
            )
            .await?;
            let digest = bitrouter::policy_lock::export_candidate_file(
                &update.path,
                &candidate_path,
                &update.document,
            )?;
            let mut changes = update.changes;
            changes.extend(
                update
                    .conflicts
                    .into_iter()
                    .map(|conflict| format!("conflict: {conflict}")),
            );
            let mut report =
                routing_policy_report(config_path, "compile", false, changes, None).await?;
            report.candidate_path = Some(candidate_path.display().to_string());
            report.digest = Some(digest);
            output.emit(&report)?;
        }
        PolicyAction::Diff { active, candidate } => {
            let active_lock = bitrouter::policy_lock::load(&active).await?;
            let candidate_lock = bitrouter::policy_lock::load(&candidate).await?;
            let changes = bitrouter::policy_lock::diff_explanations(
                &active_lock.document,
                &candidate_lock.document,
            );
            output.emit(&PolicyReport {
                action: "diff".into(),
                path: Some(active.display().to_string()),
                candidate_path: Some(candidate.display().to_string()),
                digest: Some(candidate_lock.digest),
                mode: "n/a".into(),
                policies: candidate_lock.document.policies.keys().cloned().collect(),
                bindings: Default::default(),
                changes,
                policy: None,
                applied: false,
            })?;
        }
        PolicyAction::Publish {
            candidate,
            config,
            socket,
        } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let config_path = require_policy_config_path(&source)?;
            let update =
                bitrouter::policy_lock::publish_candidate_file(config_path, &candidate).await?;
            reload_published_policy_or_restore(&source, &update, socket.as_deref()).await?;
            let changes = update.changes.clone();
            let mut report =
                routing_policy_report(config_path, "publish", true, changes, None).await?;
            report.candidate_path = Some(candidate.display().to_string());
            report.digest = Some(update.digest);
            output.emit(&report)?;
        }
        PolicyAction::Evolve {
            apply,
            output: candidate_output,
            config,
        } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let config_path = require_policy_config_path(&source)?;
            let update = bitrouter::policy_lock::evolve_files(config_path, apply).await?;
            let action = if candidate_output.is_some() {
                "evolve-export"
            } else if apply {
                "evolve"
            } else {
                "evolve-dry-run"
            };
            let published = apply;
            if published {
                reload_published_policy_or_restore(&source, &update, None).await?;
            }
            let mut changes = update.changes.clone();
            changes.extend(
                update
                    .conflicts
                    .iter()
                    .map(|conflict| format!("conflict: {conflict}")),
            );
            let mut report =
                routing_policy_report(config_path, action, published, changes, None).await?;
            if let Some(candidate_path) = candidate_output {
                report.digest = Some(bitrouter::policy_lock::export_candidate_file(
                    &update.path,
                    &candidate_path,
                    &update.document,
                )?);
                report.candidate_path = Some(candidate_path.display().to_string());
            } else {
                report.digest = Some(update.digest);
            }
            output.emit(&report)?;
        }
        PolicyAction::Rollback {
            digest,
            config,
            socket,
        } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let config_path = require_policy_config_path(&source)?;
            let cfg = config::load(config_path).await?;
            if cfg.policy.mode == config::PolicyRuntimeMode::Frozen {
                anyhow::bail!("policy runtime mode is frozen; rollback requires adaptive mode");
            }
            if cfg.policy.mode == config::PolicyRuntimeMode::Frozen {
                anyhow::bail!(
                    "policy runtime mode is frozen; use adaptive mode for policy publication"
                );
            }
            let loaded = bitrouter::policy_lock::load_for_config(&cfg, Some(config_path))
                .await?
                .ok_or_else(|| anyhow::anyhow!("no policy lock is configured"))?;
            let history_dir = bitrouter::policy_lock::default_history_dir(&loaded.path);
            let record = bitrouter::policy_lock::rollback_to_digest(
                &loaded.path,
                &loaded.digest,
                &digest,
                &history_dir,
            )?;
            if let Err(error) = reload_policy_if_reachable(&source, socket.as_deref()).await {
                bitrouter::policy_lock::rollback_to_digest(
                    &loaded.path,
                    &record.child_digest,
                    &record.parent_digest,
                    &history_dir,
                )?;
                let _ = reload_policy_if_reachable(&source, socket.as_deref()).await;
                return Err(error.context("daemon rejected rollback; restored previous lock"));
            }
            output.emit(
                &routing_policy_report(
                    config_path,
                    "rollback",
                    true,
                    vec![format!("restored {}", record.child_digest)],
                    None,
                )
                .await?,
            )?;
        }
    }
    Ok(())
}

async fn optimize(action: OptimizeAction, output: &Output) -> Result<()> {
    match action {
        OptimizeAction::Run {
            policy,
            candidate_tier,
            exploration_ppm,
            minimum_tasks,
            maximum_tasks,
            minimum_pass_rate_ppm,
            evaluator_config_digest,
            config,
            socket,
        } => {
            let source = bitrouter::paths::resolve_config(Some(&config))?;
            let config_path = require_policy_config_path(&source)?.to_path_buf();
            let options = bitrouter::optimization::controller::OptimizationOptions {
                policy: policy.clone(),
                candidate_tier,
                challenger_exposure_ppm: exploration_ppm,
                minimum_tasks_per_arm: minimum_tasks,
                maximum_challenger_tasks: maximum_tasks,
                minimum_pass_rate_ppm,
                evaluator_config_digest,
            };
            let prepared =
                bitrouter::optimization::controller::prepare_files(&config_path, options).await?;
            let mut report = optimization_run_report(&policy, &prepared);
            let publication =
                bitrouter::optimization::controller::publish_prepared(prepared).await?;
            report.active_policy_digest = publication.active_policy_digest.clone();
            report.published = publication.published;
            if publication.update.is_some() {
                let endpoint = resolve_client_socket_from(&source, socket.as_deref()).await?;
                report.reload_attempted = daemon::endpoint_in_use(&endpoint);
                reload_optimization_publication_or_restore(
                    &source,
                    &publication,
                    socket.as_deref(),
                )
                .await?;
            }
            output.emit(&report)?;
            Ok(())
        }
        OptimizeAction::Status { policy, config } => {
            let source = bitrouter::paths::resolve_config(Some(&config))?;
            let config_path = require_policy_config_path(&source)?;
            let status =
                bitrouter::optimization::controller::read_status(config_path, &policy).await?;
            output.emit(&optimization_status_report(status))?;
            Ok(())
        }
    }
}

fn optimization_run_report(
    policy: &str,
    prepared: &bitrouter::optimization::controller::PreparedOptimizationStep,
) -> OptimizationControllerReport {
    let cohort = prepared.step.evidence.cohort.as_ref();
    OptimizationControllerReport {
        action: "optimize.run",
        policy: policy.into(),
        decision: controller_action_name(prepared.step.action),
        parent_policy_digest: Some(prepared.parent_policy_digest.clone()),
        active_policy_digest: prepared.parent_policy_digest.clone(),
        eval_snapshot_root: Some(prepared.step.evidence.eval_snapshot_root.clone()),
        observed_subject_digest: Some(prepared.step.evidence.observed_subject_digest.clone()),
        treatment: prepared.treatment.as_ref().map(treatment_report),
        control: cohort.map(|assessment| arm_report(&assessment.control)),
        challenger: cohort.map(|assessment| arm_report(&assessment.challenger)),
        cost_delta_micro_usd: cohort.and_then(|assessment| assessment.cost_delta_micro_usd),
        evaluator_config_digest: cohort
            .and_then(|assessment| assessment.evaluator_config_digest.clone())
            .or_else(|| {
                prepared
                    .treatment
                    .as_ref()
                    .and_then(|treatment| treatment.gate.evaluator_config_digest.clone())
            }),
        published: false,
        reload_attempted: false,
    }
}

fn optimization_status_report(
    status: bitrouter::optimization::controller::OptimizationStatus,
) -> OptimizationControllerReport {
    let decision = if status.active_experiment.is_some() {
        "exploring"
    } else {
        "idle"
    };
    OptimizationControllerReport {
        action: "optimize.status",
        policy: status.policy,
        decision,
        parent_policy_digest: status.parent_policy_digest,
        active_policy_digest: status.active_policy_digest,
        eval_snapshot_root: status.eval_snapshot_root,
        observed_subject_digest: status.observed_subject_digest,
        treatment: status.active_experiment.as_ref().map(treatment_report),
        control: None,
        challenger: None,
        cost_delta_micro_usd: None,
        evaluator_config_digest: status
            .active_experiment
            .and_then(|experiment| experiment.gate.evaluator_config_digest),
        published: false,
        reload_attempted: false,
    }
}

fn treatment_report(
    exploration: &bitrouter::optimization::exploration::RouteExploration,
) -> TreatmentReport {
    TreatmentReport {
        target_request_key: exploration.target_request_key.clone(),
        champion_tier: exploration.champion_tier.clone(),
        challenger_tier: exploration.challenger_tier.clone(),
        challenger_exposure_ppm: exploration.challenger_exposure_ppm,
        minimum_tasks_per_arm: exploration.gate.minimum_tasks_per_arm,
        maximum_challenger_tasks: exploration.gate.maximum_challenger_tasks,
        minimum_pass_rate_ppm: exploration.gate.minimum_pass_rate_ppm,
    }
}

fn arm_report(assessment: &bitrouter::optimization::cohort::ArmAssessment) -> ArmReport {
    ArmReport {
        observed: assessment.observed,
        eligible: assessment.eligible,
        excluded: assessment.excluded,
        conclusive: assessment.conclusive,
        pass: assessment.pass,
        fail: assessment.fail,
        hard_violations: assessment.hard_violations,
        pass_rate_ppm: assessment.pass_rate_ppm,
        mean_cost_micro_usd: assessment.mean_cost_micro_usd,
    }
}

fn controller_action_name(
    action: bitrouter::optimization::controller::ControllerAction,
) -> &'static str {
    match action {
        bitrouter::optimization::controller::ControllerAction::Explore => "explore",
        bitrouter::optimization::controller::ControllerAction::Promote => "promote",
        bitrouter::optimization::controller::ControllerAction::Retreat => "retreat",
        bitrouter::optimization::controller::ControllerAction::Hold => "hold",
        bitrouter::optimization::controller::ControllerAction::Converged => "converged",
    }
}

async fn eval(action: EvalAction, output: &Output) -> Result<()> {
    use bitrouter::eval::admission::SubmissionPrincipal;
    use bitrouter::eval::types::{EvalSubject, EvaluationResult};

    let (action_name, data) = match action {
        EvalAction::Subject { action } => match action {
            EvalSubjectAction::Seal {
                draft,
                output: path,
            } => {
                let mut subject: EvalSubject = read_eval_file(&draft)?;
                subject
                    .evidence
                    .sort_by(|left, right| left.evidence_id.cmp(&right.evidence_id));
                subject.evidence_digest =
                    bitrouter::eval::types::evidence_digest(&subject.evidence)?;
                bitrouter::eval::types::validate_subject(&subject)?;
                let sealed = serde_json::to_string_pretty(&subject)? + "\n";
                std::fs::write(&path, sealed)
                    .with_context(|| format!("writing sealed eval subject {}", path.display()))?;
                (
                    "subject-seal",
                    serde_json::json!({
                        "eval_id": subject.eval_id,
                        "evidence_digest": subject.evidence_digest,
                        "output": path,
                    }),
                )
            }
            EvalSubjectAction::Put { file, config } => {
                let subject: EvalSubject = read_eval_file(&file)?;
                let service = local_eval_service(config.as_deref()).await?;
                let outcome = service.store().insert_subject(&subject).await?;
                (
                    "subject-put",
                    serde_json::json!({
                        "eval_id": subject.eval_id,
                        "outcome": format!("{outcome:?}").to_ascii_lowercase(),
                    }),
                )
            }
            EvalSubjectAction::Get { eval_id, config } => {
                let service = local_eval_service(config.as_deref()).await?;
                let subject = service
                    .store()
                    .subject_for_owner(&eval_id, "local")
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("eval subject '{eval_id}' not found"))?;
                ("subject-get", serde_json::to_value(subject)?)
            }
            EvalSubjectAction::List { config } => {
                let service = local_eval_service(config.as_deref()).await?;
                (
                    "subject-list",
                    serde_json::to_value(service.store().list_subjects_for_owner("local").await?)?,
                )
            }
        },
        EvalAction::Result { action } => match action {
            EvalResultAction::Submit { file, config } => {
                let result: EvaluationResult = read_eval_file(&file)?;
                let service = local_eval_service(config.as_deref()).await?;
                let outcome = service
                    .submit(result, SubmissionPrincipal::LocalOperator)
                    .await?;
                ("result-submit", serde_json::to_value(outcome)?)
            }
        },
        EvalAction::Snapshot { action } => match action {
            EvalSnapshotAction::Freeze { at, config } => {
                let service = local_eval_service(config.as_deref()).await?;
                let frozen_at = at.unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
                let snapshot = service
                    .store()
                    .freeze_snapshot_for_owner(&frozen_at, "local")
                    .await?;
                ("snapshot-freeze", serde_json::to_value(snapshot)?)
            }
            EvalSnapshotAction::Get {
                evidence_root,
                config,
            } => {
                let service = local_eval_service(config.as_deref()).await?;
                let snapshot = service
                    .store()
                    .snapshot_by_root_for_owner(&evidence_root, "local")
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("eval snapshot '{evidence_root}' not found"))?;
                ("snapshot-get", serde_json::to_value(snapshot)?)
            }
        },
        EvalAction::Status { config } => {
            let service = local_eval_service(config.as_deref()).await?;
            let subjects = service.store().list_subjects_for_owner("local").await?;
            let admissions = service.store().latest_admissions_for_owner("local").await?;
            let mut counts = std::collections::BTreeMap::<String, usize>::new();
            for event in admissions.values() {
                *counts
                    .entry(format!("{:?}", event.status).to_ascii_lowercase())
                    .or_default() += 1;
            }
            (
                "status",
                serde_json::json!({
                    "subjects": subjects.len(),
                    "results": admissions.len(),
                    "admission": counts,
                }),
            )
        }
    };
    output.emit(&EvalReport {
        action: action_name.into(),
        data,
    })?;
    Ok(())
}

async fn trajectory(
    config_path: Option<&Path>,
    action: TrajectoryAction,
    output: &Output,
) -> Result<()> {
    let source = bitrouter::paths::resolve_config(config_path)?;
    let (config, store) = local_trajectory_store(&source).await?;
    match action {
        TrajectoryAction::Inspect { episode_id } => {
            output.emit(&trajectory_inspect_report(&store, &episode_id).await?)?;
        }
        TrajectoryAction::Replay { episode_id } => {
            output.emit(&trajectory_replay_report(&store, &episode_id).await?)?;
        }
        TrajectoryAction::Prune { before, dry_run } => {
            let summary = store
                .prune_before(&before, dry_run, config.trajectory.outbox_batch_size)
                .await?;
            output.emit(&TrajectoryPruneReport::new(before, dry_run, summary))?;
        }
    }
    Ok(())
}

async fn local_trajectory_store(
    source: &bitrouter::paths::ConfigSource,
) -> Result<(
    bitrouter_sdk::config::Config,
    bitrouter::trajectory::store::TrajectoryStore,
)> {
    let config = bitrouter::paths::load_config(source).await?;
    let database_url = bitrouter::db::anchor_url(&config.database.url, source.home());
    let db = bitrouter::db::connect(&database_url).await?;
    bitrouter::db::run_migrations(&db).await?;
    Ok((
        config,
        bitrouter::trajectory::store::TrajectoryStore::new(db),
    ))
}

async fn local_eval_service(config_path: Option<&Path>) -> Result<bitrouter::eval::EvalService> {
    let source = bitrouter::paths::resolve_config(config_path)?;
    let config = bitrouter::paths::load_config(&source).await?;
    let db = bitrouter::db::connect(&config.database.url).await?;
    bitrouter::db::run_migrations(&db).await?;
    Ok(bitrouter::eval::EvalService::new(
        bitrouter::eval::store::EvalStore::new(db),
        config.eval,
    ))
}

fn read_eval_file<T>(path: &Path) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading eval exchange file {}", path.display()))?;
    match serde_json::from_str(&content) {
        Ok(value) => Ok(value),
        Err(json_error) => serde_saphyr::from_str(&content)
            .with_context(|| format!("parsing {} as JSON ({json_error}) or YAML", path.display())),
    }
}

async fn reload_policy_if_reachable(
    source: &bitrouter::paths::ConfigSource,
    socket_override: Option<&Path>,
) -> Result<()> {
    let socket = resolve_client_socket_from(source, socket_override).await?;
    if !daemon::endpoint_in_use(&socket) {
        return Ok(());
    }
    reload(&socket).await.map(|_| ())
}

#[derive(Debug, Eq, PartialEq)]
enum OptimizationRecoveryRevision {
    Parent,
    Child,
    Both,
    Other(String),
}

#[derive(Debug, Eq, PartialEq)]
struct OptimizationRecoveryPair {
    config: OptimizationRecoveryRevision,
    policy: OptimizationRecoveryRevision,
}

impl OptimizationRecoveryPair {
    fn description(&self) -> String {
        format!(
            "config={}, policy={}",
            self.config.description(),
            self.policy.description()
        )
    }
}

impl OptimizationRecoveryRevision {
    fn description(&self) -> &str {
        match self {
            Self::Parent => "parent",
            Self::Child => "child",
            Self::Both => "parent and child (identical bytes)",
            Self::Other(detail) => detail,
        }
    }

    fn is_child(&self) -> bool {
        matches!(self, Self::Child | Self::Both)
    }
}

async fn observe_optimization_recovery_pair(
    publication: &bitrouter::optimization::controller::OptimizationPublication,
    update: &bitrouter::policy_lock::PolicyFileUpdate,
) -> Result<OptimizationRecoveryPair> {
    let config = std::fs::read_to_string(&publication.config_path)
        .with_context(|| format!("reading {}", publication.config_path.display()))?;
    let config = if publication.config_before == publication.config_after
        && config == publication.config_before
    {
        OptimizationRecoveryRevision::Both
    } else if config == publication.config_before {
        OptimizationRecoveryRevision::Parent
    } else if config == publication.config_after {
        OptimizationRecoveryRevision::Child
    } else {
        OptimizationRecoveryRevision::Other("different bytes".into())
    };
    let active = bitrouter::policy_lock::load(&update.path).await?;
    let policy = if active.digest == publication.parent_policy_digest {
        OptimizationRecoveryRevision::Parent
    } else if active.digest == publication.active_policy_digest {
        OptimizationRecoveryRevision::Child
    } else {
        OptimizationRecoveryRevision::Other(active.digest)
    };
    Ok(OptimizationRecoveryPair { config, policy })
}

fn optimizer_recovery_failure(
    initial_error: &anyhow::Error,
    message: impl std::fmt::Display,
    recovery_errors: &[String],
) -> anyhow::Error {
    let recovery_detail = if recovery_errors.is_empty() {
        String::new()
    } else {
        format!(
            "; recovery operation errors: {}",
            recovery_errors.join("; ")
        )
    };
    anyhow::anyhow!("{initial_error:#}").context(format!("{message}{recovery_detail}"))
}

async fn reload_reconciled_optimization_pair<Reload, ReloadFuture>(
    reload: &Reload,
    initial_error: &anyhow::Error,
    pair: &str,
    recovery_errors: &[String],
) -> Result<()>
where
    Reload: Fn() -> ReloadFuture,
    ReloadFuture: std::future::Future<Output = Result<()>>,
{
    match reload().await {
        Ok(()) => Err(optimizer_recovery_failure(
            initial_error,
            format!(
                "daemon rejected optimization publication; reconciled and reloaded the {pair} config and policy"
            ),
            recovery_errors,
        )),
        Err(reload_error) => {
            let mut errors = recovery_errors.to_vec();
            errors.push(format!(
                "reloading reconciled {pair} pair: {reload_error:#}"
            ));
            Err(optimizer_recovery_failure(
                initial_error,
                format!(
                    "daemon rejected optimization publication; reconciled the {pair} config and policy but recovery reload failed"
                ),
                &errors,
            ))
        }
    }
}

async fn reload_published_policy_or_restore(
    source: &bitrouter::paths::ConfigSource,
    update: &bitrouter::policy_lock::PolicyFileUpdate,
    socket_override: Option<&Path>,
) -> Result<()> {
    if let Err(error) = reload_policy_if_reachable(source, socket_override).await {
        let parent_digest = update
            .document
            .artifact
            .as_ref()
            .and_then(|artifact| artifact.parent_digest.as_deref())
            .ok_or_else(|| anyhow::anyhow!("published policy has no parent digest"))?;
        let history_dir = bitrouter::policy_lock::default_history_dir(&update.path);
        let restore = bitrouter::policy_lock::rollback_to_digest(
            &update.path,
            &update.digest,
            parent_digest,
            &history_dir,
        );
        let restore_reload = if restore.is_ok() {
            reload_policy_if_reachable(source, socket_override).await
        } else {
            Ok(())
        };
        return match (restore, restore_reload) {
            (Ok(_), Ok(())) => {
                Err(error.context("daemon rejected candidate; restored previous lock"))
            }
            (restore, restore_reload) => Err(error.context(format!(
                "daemon rejected candidate and recovery failed (policy: {}; reload: {})",
                restore
                    .err()
                    .map(|value| format!("{value:#}"))
                    .unwrap_or_else(|| "ok".into()),
                restore_reload
                    .err()
                    .map(|value| format!("{value:#}"))
                    .unwrap_or_else(|| "ok".into()),
            ))),
        };
    }
    Ok(())
}

async fn reload_optimization_publication_or_restore(
    source: &bitrouter::paths::ConfigSource,
    publication: &bitrouter::optimization::controller::OptimizationPublication,
    socket_override: Option<&Path>,
) -> Result<()> {
    reload_optimization_publication_or_restore_with(
        publication,
        move || reload_policy_if_reachable(source, socket_override),
        || Ok(()),
    )
    .await
}

async fn reload_optimization_publication_or_restore_with<Reload, ReloadFuture, BeforeRecovery>(
    publication: &bitrouter::optimization::controller::OptimizationPublication,
    reload: Reload,
    before_recovery: BeforeRecovery,
) -> Result<()>
where
    Reload: Fn() -> ReloadFuture,
    ReloadFuture: std::future::Future<Output = Result<()>>,
    BeforeRecovery: FnOnce() -> Result<()>,
{
    reload_optimization_publication_or_restore_with_mutations(
        publication,
        reload,
        before_recovery,
        |path, expected_digest, target_digest, history_dir| {
            bitrouter::policy_lock::rollback_to_digest_unlocked(
                path,
                expected_digest,
                target_digest,
                history_dir,
            )
            .map(|_| ())
        },
        bitrouter::policy_lock::write_text_atomic_unlocked,
    )
    .await
}

async fn reload_optimization_publication_or_restore_with_mutations<
    Reload,
    ReloadFuture,
    BeforeRecovery,
    MutatePolicy,
    MutateConfig,
>(
    publication: &bitrouter::optimization::controller::OptimizationPublication,
    reload: Reload,
    before_recovery: BeforeRecovery,
    mutate_policy: MutatePolicy,
    mutate_config: MutateConfig,
) -> Result<()>
where
    Reload: Fn() -> ReloadFuture,
    ReloadFuture: std::future::Future<Output = Result<()>>,
    BeforeRecovery: FnOnce() -> Result<()>,
    MutatePolicy: Fn(&Path, &str, &str, &Path) -> Result<()>,
    MutateConfig: Fn(&Path, &str, &str) -> Result<()>,
{
    let initial_error = match reload().await {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    let initial_detail = format!("{initial_error:#}");
    if let Err(seam_error) = before_recovery() {
        return Err(initial_error.context(format!(
            "daemon rejected optimization publication and recovery setup failed: {seam_error:#}"
        )));
    }
    let Some(update) = publication.update.as_ref() else {
        return Err(anyhow::anyhow!(
            "daemon rejected optimization publication ({initial_detail}); recovery has no policy update"
        ));
    };
    let _config_guard =
        bitrouter::policy_lock::acquire_publication_lock(&publication.config_path).map_err(
            |recovery| {
                anyhow::anyhow!(
                    "daemon rejected optimization publication ({initial_detail}); acquiring config recovery lock failed: {recovery:#}"
                )
            },
        )?;
    let _policy_guard = bitrouter::policy_lock::acquire_publication_lock(&update.path).map_err(
        |recovery| {
            anyhow::anyhow!(
                "daemon rejected optimization publication ({initial_detail}); acquiring policy recovery lock failed: {recovery:#}"
            )
        },
    )?;
    if update.digest != publication.active_policy_digest {
        return Err(initial_error.context(format!(
            "daemon rejected optimization publication; recovery is stale because update {} does not match published policy {}",
            update.digest, publication.active_policy_digest
        )));
    }
    let initial_pair = observe_optimization_recovery_pair(publication, update)
        .await
        .map_err(|recovery| {
            anyhow::anyhow!(
                "daemon rejected optimization publication ({initial_detail}); reading recovery state failed: {recovery:#}"
            )
        })?;
    if !initial_pair.config.is_child() || initial_pair.policy != OptimizationRecoveryRevision::Child
    {
        return Err(initial_error.context(format!(
            "daemon rejected optimization publication; recovery is stale because the active pair is {}",
            initial_pair.description()
        )));
    }
    let history_dir = bitrouter::policy_lock::default_history_dir(&update.path);
    let mut recovery_errors = Vec::new();
    if let Err(policy_error) = mutate_policy(
        &update.path,
        &publication.active_policy_digest,
        &publication.parent_policy_digest,
        &history_dir,
    ) {
        recovery_errors.push(format!("restoring parent policy: {policy_error:#}"));
    }
    let after_policy = observe_optimization_recovery_pair(publication, update)
        .await
        .map_err(|recovery| {
            optimizer_recovery_failure(
                &initial_error,
                format!(
                    "daemon rejected optimization publication; reconciling disk state after policy recovery failed: {recovery:#}"
                ),
                &recovery_errors,
            )
        })?;
    match (&after_policy.config, &after_policy.policy) {
        (
            OptimizationRecoveryRevision::Child | OptimizationRecoveryRevision::Both,
            OptimizationRecoveryRevision::Child,
        ) => {
            return reload_reconciled_optimization_pair(
                &reload,
                &initial_error,
                "child",
                &recovery_errors,
            )
            .await;
        }
        (OptimizationRecoveryRevision::Both, OptimizationRecoveryRevision::Parent) => {
            return reload_reconciled_optimization_pair(
                &reload,
                &initial_error,
                "parent",
                &recovery_errors,
            )
            .await;
        }
        (OptimizationRecoveryRevision::Child, OptimizationRecoveryRevision::Parent) => {}
        _ => {
            return Err(optimizer_recovery_failure(
                &initial_error,
                format!(
                    "daemon rejected optimization publication; recovery is stale after policy recovery because the pair is {}",
                    after_policy.description()
                ),
                &recovery_errors,
            ));
        }
    }
    if let Err(config_error) = mutate_config(
        &publication.config_path,
        &publication.config_after,
        &publication.config_before,
    ) {
        recovery_errors.push(format!("restoring parent config: {config_error:#}"));
    }
    let after_config = observe_optimization_recovery_pair(publication, update)
        .await
        .map_err(|recovery| {
            optimizer_recovery_failure(
                &initial_error,
                format!(
                    "daemon rejected optimization publication; reconciling disk state after config recovery failed: {recovery:#}"
                ),
                &recovery_errors,
            )
        })?;
    match (&after_config.config, &after_config.policy) {
        (OptimizationRecoveryRevision::Parent, OptimizationRecoveryRevision::Parent) => {
            return reload_reconciled_optimization_pair(
                &reload,
                &initial_error,
                "parent",
                &recovery_errors,
            )
            .await;
        }
        (OptimizationRecoveryRevision::Child, OptimizationRecoveryRevision::Parent) => {}
        _ => {
            return Err(optimizer_recovery_failure(
                &initial_error,
                format!(
                    "daemon rejected optimization publication; recovery is stale after config recovery because the pair is {}",
                    after_config.description()
                ),
                &recovery_errors,
            ));
        }
    }
    if let Err(compensation_error) = mutate_policy(
        &update.path,
        &publication.parent_policy_digest,
        &publication.active_policy_digest,
        &history_dir,
    ) {
        recovery_errors.push(format!(
            "restoring child policy after config recovery failure: {compensation_error:#}"
        ));
    }
    let after_compensation = observe_optimization_recovery_pair(publication, update)
        .await
        .map_err(|recovery| {
            optimizer_recovery_failure(
                &initial_error,
                format!(
                    "daemon rejected optimization publication; reconciling disk state after policy compensation failed: {recovery:#}"
                ),
                &recovery_errors,
            )
        })?;
    if after_compensation.config.is_child()
        && after_compensation.policy == OptimizationRecoveryRevision::Child
    {
        return reload_reconciled_optimization_pair(
            &reload,
            &initial_error,
            "child",
            &recovery_errors,
        )
        .await;
    }
    Err(optimizer_recovery_failure(
        &initial_error,
        format!(
            "daemon rejected optimization publication; recovery is stale after policy compensation because the pair is {}",
            after_compensation.description()
        ),
        &recovery_errors,
    ))
}

fn require_policy_config_path(source: &bitrouter::paths::ConfigSource) -> Result<&Path> {
    match source {
        bitrouter::paths::ConfigSource::File(path) => Ok(path),
        bitrouter::paths::ConfigSource::Default { .. } => anyhow::bail!(
            "routing policies require a file-backed bitrouter.yaml; run `bitrouter init` first"
        ),
    }
}

async fn routing_policy_report(
    config_path: &Path,
    action: &str,
    applied: bool,
    changes: Vec<String>,
    show: Option<&str>,
) -> Result<PolicyReport> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let cfg = config::parse(&raw).context("parsing bitrouter.yaml")?;
    let loaded = bitrouter::policy_lock::load_for_config(&cfg, Some(config_path)).await?;
    let policy = match show {
        Some(name) => {
            let lock = loaded
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no policy lock is configured"))?;
            let definition = lock
                .document
                .policies
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("policy '{name}' does not exist"))?;
            Some(serde_json::to_value(definition).context("serializing policy")?)
        }
        None => None,
    };
    let bindings = cfg
        .presets
        .iter()
        .filter_map(|(name, preset)| {
            preset
                .policy
                .as_ref()
                .map(|policy| (name.clone(), policy.clone()))
        })
        .collect();
    let path = loaded
        .as_ref()
        .map(|lock| lock.path.clone())
        .or_else(|| bitrouter::policy_lock::resolve_path(&cfg, Some(config_path)));
    let policies = loaded
        .as_ref()
        .map(|lock| lock.document.policies.keys().cloned().collect())
        .unwrap_or_default();
    Ok(PolicyReport {
        action: action.to_string(),
        path: path.map(|path| path.display().to_string()),
        candidate_path: None,
        digest: loaded.as_ref().map(|lock| lock.digest.clone()),
        mode: match cfg.policy.mode {
            config::PolicyRuntimeMode::Frozen => "frozen",
            config::PolicyRuntimeMode::Adaptive => "adaptive",
        }
        .to_string(),
        policies,
        bindings,
        changes,
        policy,
        applied,
    })
}

async fn providers(action: ProviderAction, output: &Output) -> Result<()> {
    match action {
        ProviderAction::List { config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            let providers = commands::list_providers(&cfg)
                .into_iter()
                .map(|p| ProviderRow {
                    id: p.id,
                    models: p.model_count,
                    active: p.active,
                    api_base: p.api_base,
                })
                .collect();
            output.emit(&ProvidersReport { providers })?;
            Ok(())
        }
        ProviderAction::Login {
            provider,
            label,
            import_existing,
            no_browser,
            api_key,
            key_stdin,
        } => {
            // `--key-stdin` reads the key from stdin (one line); it funnels into
            // the same non-interactive API-key path as `--api-key`.
            let api_key = if key_stdin {
                Some(read_api_key_from_stdin()?)
            } else {
                api_key
            };
            // The built-in `bitrouter` provider authenticates with the cloud
            // OAuth credential, so logging into it IS the cloud sign-in
            // (`cloud login`); other providers use the per-provider store.
            if provider == "bitrouter" {
                if import_existing || no_browser {
                    anyhow::bail!(
                        "`bitrouter providers login bitrouter` uses BitRouter Cloud OAuth; \
                         --import-existing/--no-browser apply to upstream provider logins"
                    );
                }
                // A supplied key seeds the cloud credential the same way
                // `cloud login --api-key` does.
                bitrouter::cloud::cli::run(
                    bitrouter::cloud::cli::CloudAction::Login {
                        authorization_server: None,
                        client_id: None,
                        scope: None,
                        api_key,
                    },
                    output.format(),
                )
                .await
            } else {
                let outcome = bitrouter::commands::login_provider_with_options(
                    &provider,
                    &label,
                    bitrouter::commands::ProviderLoginOptions {
                        import_existing,
                        no_browser,
                        api_key,
                    },
                )
                .await?;
                output.emit(&ProviderLoginReport {
                    provider: outcome.provider,
                    label: outcome.label,
                    method: outcome.method,
                    credential: "saved",
                    path: outcome.path,
                })?;
                Ok(())
            }
        }
        ProviderAction::Logout { provider } => {
            if provider == "bitrouter" {
                bitrouter::cloud::cli::run(
                    bitrouter::cloud::cli::CloudAction::Logout {
                        authorization_server: None,
                        client_id: None,
                    },
                    output.format(),
                )
                .await
            } else {
                let removed = bitrouter::commands::logout_provider(&provider).await?;
                output.emit(&ProviderLogoutReport { provider, removed })?;
                Ok(())
            }
        }
    }
}

async fn tools(action: ToolsAction, output: &Output) -> Result<()> {
    use bitrouter::tools as tools_cmd;

    match action {
        ToolsAction::List { config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            let servers = tools_cmd::list(&cfg)
                .await
                .into_iter()
                .map(|row| match row.outcome {
                    Ok(tools) => ServerToolsView {
                        server: row.server,
                        tools: Some(
                            tools
                                .into_iter()
                                .map(|t| ToolInfo {
                                    name: t.name,
                                    description: t.description,
                                })
                                .collect(),
                        ),
                        error: None,
                    },
                    Err(e) => ServerToolsView {
                        server: row.server,
                        tools: None,
                        error: Some(e),
                    },
                })
                .collect();
            output.emit(&ToolsListReport { servers })?;
            Ok(())
        }
        ToolsAction::Status { config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            let servers = tools_cmd::status(&cfg)
                .await
                .into_iter()
                .map(|row| {
                    let (ok, latency_ms, error) = match row.outcome {
                        Ok(d) => (true, Some(d.as_millis()), None),
                        Err(e) => (false, None, Some(e)),
                    };
                    ServerStatusView {
                        server: row.server,
                        ok,
                        latency_ms,
                        transport: row.transport,
                        error,
                    }
                })
                .collect();
            output.emit(&ToolsStatusReport { servers })?;
            Ok(())
        }
        ToolsAction::Discover { server, config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            match tools_cmd::discover(&cfg, &server).await {
                Ok(yaml) => {
                    output.emit(&ToolsDiscoverReport { server, yaml })?;
                    Ok(())
                }
                Err(e) => anyhow::bail!("discover '{server}': {e}"),
            }
        }
    }
}

// ===== observe =====

async fn observe(action: ObserveAction, output: &Output) -> Result<()> {
    match action {
        ObserveAction::Status { config, socket } => {
            let socket = resolve_client_socket(config.as_deref(), socket.as_deref()).await?;
            output.emit(&observe_status(&socket).await?)?;
            Ok(())
        }
    }
}

/// `bitrouter observe status` — ask the running daemon for the OTel
/// exporter snapshot, pretty-print (or JSON-dump) the result. When no
/// daemon is reachable, fall back to a "stopped" report that still
/// carries the compile-time `OTEL_ENABLED` flag so the user can tell
/// "feature off" from "daemon down."
async fn observe_status(socket: &Path) -> Result<ObserveStatusReport> {
    use bitrouter_observe::OTEL_ENABLED;

    let (snapshot, daemon_reachable) =
        match daemon::send_command(socket, &DaemonCommand::ObserveStatus).await {
            Ok(DaemonResponse::ObserveStatus { payload }) => (payload, true),
            Ok(DaemonResponse::Error { message }) => return Err(anyhow::anyhow!(message)),
            Ok(other) => return Err(anyhow::anyhow!("unexpected response: {other:?}")),
            Err(e) if daemon::is_not_reachable(&e) => {
                (daemon::ObserveStatusPayload::unwired(OTEL_ENABLED), false)
            }
            Err(e) => return Err(e),
        };

    Ok(ObserveStatusReport {
        daemon_reachable,
        snapshot,
        socket: socket.display().to_string(),
    })
}

async fn agents_cmd(action: AgentsAction, output: &Output) -> Result<()> {
    use bitrouter::agents as agents_cmd;

    match action {
        AgentsAction::List { remote, config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            let agents = agents_cmd::list(&cfg)
                .into_iter()
                .map(|row| AgentRow {
                    id: row.id,
                    configured: row.configured,
                    in_catalog: row.in_catalog,
                    description: row.description,
                })
                .collect();
            // `--remote`: append the ACP registry as an optional section of the
            // same report (one JSON document either way).
            let registry = if remote {
                let fetched =
                    bitrouter::agent_registry::fetch(bitrouter::agent_registry::REGISTRY_URL)
                        .await?;
                Some(
                    agents_cmd::registry_rows(&fetched)
                        .into_iter()
                        .map(|row| AgentRegistryRow {
                            id: row.id,
                            version: row.version,
                            install: row.install.to_string(),
                            description: row.description,
                        })
                        .collect(),
                )
            } else {
                None
            };
            output.emit(&AgentsListReport { agents, registry })?;
            Ok(())
        }
        AgentsAction::Check { config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            let agents = agents_cmd::check(&cfg)
                .await
                .into_iter()
                .map(|row| {
                    let (ok, latency_ms, error) = match row.outcome {
                        Ok(d) => (true, Some(d.as_millis()), None),
                        Err(e) => (false, None, Some(e)),
                    };
                    AgentCheckRow {
                        id: row.id,
                        ok,
                        latency_ms,
                        error,
                    }
                })
                .collect();
            output.emit(&AgentsCheckReport { agents })?;
            Ok(())
        }
        AgentsAction::Install { id } => match agents_cmd::install(&id) {
            Ok(yaml) => {
                output.emit(&AgentInstallReport { id, yaml })?;
                Ok(())
            }
            // Not in the compiled catalog: fall back to the ACP registry
            // (npx/uvx distributions only).
            Err(catalog_miss) => {
                let registry = match bitrouter::agent_registry::fetch(
                    bitrouter::agent_registry::REGISTRY_URL,
                )
                .await
                {
                    Ok(registry) => registry,
                    Err(fetch_err) => {
                        anyhow::bail!(
                            "{catalog_miss}\n(also failed to consult the ACP registry: {fetch_err})"
                        )
                    }
                };
                match agents_cmd::install_from_registry(&registry, &id) {
                    Ok(yaml) => {
                        output.emit(&AgentInstallReport { id, yaml })?;
                        Ok(())
                    }
                    Err(e) => anyhow::bail!(e),
                }
            }
        },
    }
}

/// Shared body for `bitrouter launch` and the deprecated `spawn --agent`
/// alias: resolve config, then either preflight (`--check`) or exec the
/// interactive harness with its traffic routed through the daemon.
async fn run_launch(
    config: Option<&std::path::Path>,
    opts: bitrouter::spawn::SpawnOptions,
    output: &bitrouter::output::Output,
) -> Result<()> {
    let source = bitrouter::paths::resolve_config(config)?;
    let cfg = bitrouter::paths::load_config(&source).await?;
    if opts.check {
        let report = bitrouter::spawn::check(&cfg, &opts).await?;
        output.emit(&report)?;
        if report.exit_code() == 0 {
            Ok(())
        } else {
            std::process::exit(report.exit_code());
        }
    } else {
        bitrouter::spawn::run(&source, &cfg, opts).await
    }
}

// ===== `bitrouter acp …` (per-session ACP substrate) =====

async fn acp_cmd(cmd: AcpCmd) -> Result<()> {
    match cmd {
        AcpCmd::Serve {
            agent,
            turn_timeout,
            routing,
            config,
        } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            let options = bitrouter::acp_cli::launch_options(turn_timeout);
            let ctx = bitrouter::acp_cli::SpawnContext {
                source: &source,
                config: cfg,
                agent_id: &agent,
                options,
                routing,
            };
            bitrouter::acp_cli::serve(ctx).await
        }
        AcpCmd::Prompt {
            agent,
            turn_timeout,
            no_wait,
            routing,
            config,
            text,
        } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            let options = bitrouter::acp_cli::launch_options(turn_timeout);
            let mut stdout = tokio::io::stdout();
            let ctx = bitrouter::acp_cli::SpawnContext {
                source: &source,
                config: cfg,
                agent_id: &agent,
                options,
                routing,
            };
            bitrouter::acp_cli::prompt(ctx, &text, no_wait, None, &mut stdout).await
        }
    }
}

// ===== helpers =====

/// Derive the pid file path that matches a control-socket path: same
/// directory, same stem, `.pid` extension. (Both default to `./bitrouter.*`.)
fn pid_path_for(socket: &Path) -> PathBuf {
    let mut p = socket.to_path_buf();
    p.set_extension("pid");
    p
}

/// Liveness check: on Unix `kill -0 <pid>` returns success iff the pid is
/// reachable (i.e. exists and we have permission to signal it). No actual
/// signal is sent. We shell out to keep `apps/bitrouter` `#![forbid(unsafe_code)]`.
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Liveness check on Windows: `tasklist` filtered to the pid. `tasklist` ships
/// on every Windows install, so we shell out (rather than calling the Win32
/// API) to keep `apps/bitrouter` free of `unsafe`. When no process matches,
/// `tasklist` prints an informational line instead of a CSV row — so we look
/// for the quoted pid the CSV format emits (`"<pid>"`).
#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();
    match output {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.contains(&format!("\"{pid}\""))
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== tracing filter resolution =====

    #[test]
    fn config_log_level_is_used_when_rust_log_is_unset() {
        let (filter, warning) = resolve_env_filter_from(None, Some("debug"));
        assert_eq!(filter.to_string(), "debug");
        assert!(warning.is_none());
    }

    #[test]
    fn config_log_level_accepts_per_target_filter_syntax() {
        let (filter, warning) = resolve_env_filter_from(None, Some("warn,bitrouter=trace"));
        // `EnvFilter`'s `Display` does not preserve directive order.
        let rendered = filter.to_string();
        assert!(rendered.contains("warn"), "{rendered}");
        assert!(rendered.contains("bitrouter=trace"), "{rendered}");
        assert!(warning.is_none());
    }

    #[test]
    fn rust_log_wins_over_config_log_level() {
        let (filter, warning) = resolve_env_filter_from(Some("trace"), Some("error"));
        assert_eq!(filter.to_string(), "trace");
        assert!(warning.is_none());
    }

    #[test]
    fn defaults_to_info_when_neither_source_is_set() {
        let (filter, warning) = resolve_env_filter_from(None, None);
        assert_eq!(filter.to_string(), DEFAULT_LOG_FILTER);
        assert!(warning.is_none());
    }

    #[test]
    fn blank_sources_fall_through_rather_than_erroring() {
        // An empty `RUST_LOG=` must not shadow a real config value, and a
        // blank `log_level: ""` must not be treated as a filter.
        let (filter, warning) = resolve_env_filter_from(Some("  "), Some("debug"));
        assert_eq!(filter.to_string(), "debug");
        assert!(warning.is_none());

        let (filter, warning) = resolve_env_filter_from(None, Some(""));
        assert_eq!(filter.to_string(), DEFAULT_LOG_FILTER);
        assert!(warning.is_none());
    }

    #[test]
    fn invalid_config_log_level_warns_and_falls_back() {
        let (filter, warning) = resolve_env_filter_from(None, Some("not-a-level"));
        assert_eq!(filter.to_string(), DEFAULT_LOG_FILTER);
        let warning = warning.expect("an unparseable log_level must be reported");
        assert!(warning.contains("server.log_level"), "{warning}");
        assert!(warning.contains("not-a-level"), "{warning}");
    }

    #[test]
    fn typo_level_does_not_silently_mute_the_daemon() {
        // Regression guard: `EnvFilter::try_new("dbug")` succeeds, reading the
        // word as a *target* at trace level — a filter that matches nothing.
        // Left unchecked, `log_level: dbug` would silence the daemon with no
        // diagnostic at all.
        assert_eq!(
            tracing_subscriber::EnvFilter::try_new("dbug")
                .expect("EnvFilter accepts a bare word as a target")
                .to_string(),
            "dbug=trace",
        );

        let (filter, warning) = resolve_env_filter_from(None, Some("dbug"));
        assert_eq!(filter.to_string(), DEFAULT_LOG_FILTER);
        assert!(warning.is_some(), "a typo'd level must warn, not mute");
    }

    #[test]
    fn every_level_name_is_accepted() {
        for level in ["trace", "debug", "info", "warn", "error", "off"] {
            let (filter, warning) = resolve_env_filter_from(None, Some(level));
            assert!(warning.is_none(), "{level} should be valid: {warning:?}");
            assert_eq!(filter.to_string(), level);
        }
    }

    #[test]
    fn invalid_rust_log_warns_and_does_not_fall_back_to_config() {
        // `RUST_LOG` is an explicit operator override; a typo in it should be
        // reported rather than silently resolved from config behind the
        // operator's back.
        let (filter, warning) = resolve_env_filter_from(Some("not-a-level"), Some("debug"));
        assert_eq!(filter.to_string(), DEFAULT_LOG_FILTER);
        let warning = warning.expect("an unparseable RUST_LOG must be reported");
        assert!(warning.contains("RUST_LOG"), "{warning}");
    }

    #[test]
    fn server_config_default_log_level_is_a_valid_filter() {
        // The default that ships in `ServerConfig` (and the JSON Schema) must
        // survive the same parse path a user-supplied value takes.
        let default_level = config::ServerConfig::default().log_level;
        let (filter, warning) = resolve_env_filter_from(None, Some(&default_level));
        assert_eq!(filter.to_string(), DEFAULT_LOG_FILTER);
        assert!(warning.is_none());
    }

    enum RestartCommandKind {
        Status,
        Stop,
    }

    impl RestartCommandKind {
        fn matches(&self, command: &DaemonCommand) -> bool {
            matches!(
                (self, command),
                (Self::Status, DaemonCommand::Status) | (Self::Stop, DaemonCommand::Stop)
            )
        }
    }

    #[derive(Clone)]
    struct RestartGateHarness {
        old_pid: u32,
        alive: Arc<std::sync::atomic::AtomicBool>,
        endpoint_in_use: Arc<std::sync::atomic::AtomicBool>,
        sent_commands: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl RestartGateHarness {
        fn new(old_pid: u32, alive: bool, endpoint_in_use: bool) -> Self {
            Self {
                old_pid,
                alive: Arc::new(std::sync::atomic::AtomicBool::new(alive)),
                endpoint_in_use: Arc::new(std::sync::atomic::AtomicBool::new(endpoint_in_use)),
                sent_commands: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        fn spawn(&self, target: Option<RestartTarget>) -> tokio::task::JoinHandle<RestartRelease> {
            let expected_pid = self.old_pid;
            let alive = self.alive.clone();
            let endpoint_in_use = self.endpoint_in_use.clone();
            tokio::spawn(async move {
                await_restart_release(
                    Path::new("restart.sock"),
                    target,
                    move |pid| {
                        pid == expected_pid && alive.load(std::sync::atomic::Ordering::SeqCst)
                    },
                    move |_| endpoint_in_use.load(std::sync::atomic::Ordering::SeqCst),
                )
                .await
            })
        }

        fn spawn_control(
            &self,
            pidfile_pid: Option<u32>,
            endpoint_was_in_use: bool,
            responses: Vec<(RestartCommandKind, anyhow::Result<DaemonResponse>)>,
        ) -> tokio::task::JoinHandle<anyhow::Result<RestartRelease>> {
            let expected_pid = self.old_pid;
            let alive = self.alive.clone();
            let endpoint_in_use = self.endpoint_in_use.clone();
            let sent_commands = self.sent_commands.clone();
            tokio::spawn(async move {
                let mut responses = std::collections::VecDeque::from(responses);
                restart_control_phase(
                    Path::new("restart.sock"),
                    pidfile_pid,
                    endpoint_was_in_use,
                    move |command| {
                        sent_commands.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let response = match responses.pop_front() {
                            Some((expected, response)) if expected.matches(&command) => response,
                            Some(_) => Err(anyhow::anyhow!("unexpected daemon command order")),
                            None => Err(anyhow::anyhow!("unexpected extra daemon command")),
                        };
                        std::future::ready(response)
                    },
                    move |pid| {
                        pid == expected_pid && alive.load(std::sync::atomic::Ordering::SeqCst)
                    },
                    move |_| endpoint_in_use.load(std::sync::atomic::Ordering::SeqCst),
                )
                .await
            })
        }

        fn set_alive(&self, alive: bool) {
            self.alive.store(alive, std::sync::atomic::Ordering::SeqCst);
        }

        fn set_endpoint_in_use(&self, endpoint_in_use: bool) {
            self.endpoint_in_use
                .store(endpoint_in_use, std::sync::atomic::Ordering::SeqCst);
        }

        fn sent_commands(&self) -> usize {
            self.sent_commands.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn restart_waits_for_exact_old_pid_after_endpoint_release() -> anyhow::Result<()> {
        let harness = RestartGateHarness::new(4_242, true, false);
        let gate = harness.spawn(Some(RestartTarget { pid: 4_242 }));

        tokio::task::yield_now().await;
        assert!(
            !gate.is_finished(),
            "endpoint release incorrectly proved exact process exit"
        );
        harness.set_alive(false);
        tokio::time::advance(RESTART_POLL_INTERVAL).await;
        tokio::task::yield_now().await;
        let release = gate.await?;
        assert!(release.ready);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn restart_pid_reuse_timeout_never_signals_the_reused_process() -> anyhow::Result<()> {
        let harness = RestartGateHarness::new(4_252, true, false);
        let gate = harness.spawn(Some(RestartTarget { pid: 4_252 }));

        tokio::task::yield_now().await;
        // The old daemon exits and another process immediately reuses the same
        // numeric pid between conservative liveness polls. Numeric pid evidence
        // cannot distinguish the replacement and therefore cannot authorize a
        // signal after the grace period.
        harness.set_alive(false);
        harness.set_alive(true);
        tokio::time::advance(RESTART_GRACE_PERIOD).await;
        tokio::task::yield_now().await;

        let release = gate.await?;
        assert!(
            !release.ready,
            "reused live pid was treated as released instead of returning a bounded error"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn restart_pidfile_only_target_times_out_without_force_kill() -> anyhow::Result<()> {
        let harness = RestartGateHarness::new(4_248, true, false);
        let gate = harness.spawn_control(Some(4_248), false, vec![]);

        tokio::task::yield_now().await;
        tokio::time::advance(RESTART_GRACE_PERIOD).await;
        tokio::task::yield_now().await;
        let release = gate.await??;
        assert!(!release.ready, "stale live pid evidence became ready");
        assert_eq!(harness.sent_commands(), 0);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn restart_control_status_pid_survives_missing_pidfile_and_endpoint_release()
    -> anyhow::Result<()> {
        let harness = RestartGateHarness::new(4_249, true, false);
        let control = harness.spawn_control(
            None,
            true,
            vec![
                (
                    RestartCommandKind::Status,
                    Ok(DaemonResponse::Status {
                        pid: 4_249,
                        listen: "127.0.0.1:4356".to_string(),
                        models: 0,
                    }),
                ),
                (RestartCommandKind::Stop, Ok(DaemonResponse::Ok)),
            ],
        );

        tokio::task::yield_now().await;
        assert_eq!(harness.sent_commands(), 2);
        assert!(
            !control.is_finished(),
            "endpoint release bypassed Status pid"
        );
        harness.set_alive(false);
        tokio::time::advance(RESTART_POLL_INTERVAL).await;
        tokio::task::yield_now().await;
        let release = control.await??;
        assert!(release.ready);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn restart_control_status_pid_times_out_even_with_different_pidfile() -> anyhow::Result<()>
    {
        let harness = RestartGateHarness::new(4_250, true, false);
        let control = harness.spawn_control(
            Some(9_999),
            true,
            vec![
                (
                    RestartCommandKind::Status,
                    Ok(DaemonResponse::Status {
                        pid: 4_250,
                        listen: "127.0.0.1:4356".to_string(),
                        models: 0,
                    }),
                ),
                (RestartCommandKind::Stop, Ok(DaemonResponse::Ok)),
            ],
        );

        tokio::task::yield_now().await;
        tokio::time::advance(RESTART_GRACE_PERIOD).await;
        tokio::task::yield_now().await;
        assert_eq!(harness.sent_commands(), 2);
        let release = control.await??;
        assert!(!release.ready);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn restart_status_failures_never_promote_pidfile_to_kill_authority() -> anyhow::Result<()>
    {
        let cases = [
            (
                "zero pid",
                Ok(DaemonResponse::Status {
                    pid: 0,
                    listen: "127.0.0.1:4356".to_string(),
                    models: 0,
                }),
            ),
            (
                "daemon error",
                Ok(DaemonResponse::Error {
                    message: "status unavailable".to_string(),
                }),
            ),
            ("unexpected response", Ok(DaemonResponse::Ok)),
            (
                "transport error",
                Err(anyhow::anyhow!("control transport unavailable")),
            ),
        ];

        for (name, response) in cases {
            let harness = RestartGateHarness::new(4_251, true, false);
            let control = harness.spawn_control(
                Some(4_251),
                true,
                vec![(RestartCommandKind::Status, response)],
            );
            let outcome = control.await?;
            assert!(outcome.is_err(), "{name} did not fail closed");
            assert_eq!(
                harness.sent_commands(),
                1,
                "{name} sent Stop without authenticated process identity"
            );
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn restart_waits_for_endpoint_after_exact_old_pid_exits() -> anyhow::Result<()> {
        let harness = RestartGateHarness::new(4_243, false, true);
        let gate = harness.spawn(Some(RestartTarget { pid: 4_243 }));

        tokio::task::yield_now().await;
        assert!(
            !gate.is_finished(),
            "exact process exit bypassed endpoint cleanup"
        );
        harness.set_endpoint_in_use(false);
        tokio::time::advance(RESTART_POLL_INTERVAL).await;
        tokio::task::yield_now().await;
        let release = gate.await?;
        assert!(release.ready);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn restart_without_valid_pid_uses_endpoint_fallback_without_force_kill()
    -> anyhow::Result<()> {
        let harness = RestartGateHarness::new(4_246, false, true);
        let gate = harness.spawn(None);

        tokio::task::yield_now().await;
        tokio::time::advance(RESTART_GRACE_PERIOD - RESTART_POLL_INTERVAL).await;
        tokio::task::yield_now().await;
        assert!(!gate.is_finished());
        harness.set_endpoint_in_use(false);
        tokio::time::advance(RESTART_POLL_INTERVAL).await;
        tokio::task::yield_now().await;
        let release = gate.await?;
        assert!(release.ready);

        let timed_out_harness = RestartGateHarness::new(4_247, false, true);
        let timed_out = timed_out_harness.spawn(None);
        tokio::task::yield_now().await;
        tokio::time::advance(RESTART_GRACE_PERIOD).await;
        tokio::task::yield_now().await;
        let release = timed_out.await?;
        assert!(!release.ready);
        Ok(())
    }

    #[tokio::test]
    async fn outer_shutdown_keeps_http_future_until_required_drain_recovers() -> anyhow::Result<()>
    {
        for control_trigger in [true, false] {
            let accepting = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let drain_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let recovery = Arc::new(tokio::sync::Notify::new());
            let inflight_release = Arc::new(tokio::sync::Notify::new());
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
            let (control_tx, control_rx) = tokio::sync::oneshot::channel();
            let (term_tx, term_rx) = tokio::sync::oneshot::channel();

            let http_accepting = accepting.clone();
            let http_attempts = drain_attempts.clone();
            let http_recovery = recovery.clone();
            let http_inflight_release = inflight_release.clone();
            let http = async move {
                shutdown_rx
                    .await
                    .map_err(|_| anyhow::anyhow!("outer shutdown sender disappeared"))?;
                http_accepting.store(false, std::sync::atomic::Ordering::SeqCst);
                http_inflight_release.notified().await;
                http_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                http_recovery.notified().await;
                http_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            };
            let control = async move {
                control_rx
                    .await
                    .map_err(|_| anyhow::anyhow!("test control sender disappeared"))?;
                Ok(())
            };
            let term = async move {
                term_rx
                    .await
                    .map_err(|_| anyhow::anyhow!("test term sender disappeared"))?;
                Ok(())
            };
            let hup = async move {
                if control_trigger {
                    return Err(anyhow::anyhow!("test HUP setup unavailable"));
                }
                std::future::pending::<anyhow::Result<()>>().await
            };
            let supervision = tokio::spawn(supervise_http_shutdown(
                http,
                control,
                hup,
                term,
                shutdown_tx,
            ));

            tokio::task::yield_now().await;
            assert!(
                !supervision.is_finished(),
                "a HUP setup error must not drop the HTTP server"
            );
            assert!(accepting.load(std::sync::atomic::Ordering::SeqCst));
            if control_trigger {
                control_tx
                    .send(())
                    .map_err(|_| anyhow::anyhow!("test control receiver disappeared"))?;
            } else {
                term_tx
                    .send(())
                    .map_err(|_| anyhow::anyhow!("test term receiver disappeared"))?;
            }
            tokio::task::yield_now().await;
            assert!(!accepting.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(drain_attempts.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(
                !supervision.is_finished(),
                "in-flight HTTP work must finish before required drain"
            );

            inflight_release.notify_one();
            tokio::task::yield_now().await;
            assert_eq!(drain_attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(
                !supervision.is_finished(),
                "the outer supervisor dropped a server waiting on required drain"
            );
            recovery.notify_one();
            tokio::task::yield_now().await;
            assert_eq!(drain_attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
            supervision.await??;
        }
        Ok(())
    }

    #[tokio::test]
    async fn outer_shutdown_returns_http_errors_without_waiting_for_a_trigger() -> anyhow::Result<()>
    {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let error = supervise_http_shutdown(
            async { Err(anyhow::anyhow!("test HTTP failure")) },
            std::future::pending::<anyhow::Result<()>>(),
            std::future::pending::<anyhow::Result<()>>(),
            std::future::pending::<anyhow::Result<()>>(),
            shutdown_tx,
        )
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("HTTP failure unexpectedly succeeded"))?;
        assert_eq!(error.to_string(), "test HTTP failure");
        assert!(shutdown_rx.await.is_err());
        Ok(())
    }

    #[test]
    fn trajectory_inspect_replay_and_prune_flags_parse() {
        use clap::Parser;

        let inspect = Cli::try_parse_from([
            "bitrouter",
            "trajectory",
            "--config",
            "/tmp/custom-bitrouter.yaml",
            "inspect",
            "episode-1",
            "--json",
        ])
        .expect("parse trajectory inspect");
        assert!(inspect.json);
        assert!(matches!(
            inspect.command,
            Some(Command::Trajectory {
                config: Some(config),
                action: TrajectoryAction::Inspect { episode_id },
            }) if episode_id == "episode-1" && config == Path::new("/tmp/custom-bitrouter.yaml")
        ));

        let replay = Cli::try_parse_from([
            "bitrouter",
            "trajectory",
            "replay",
            "episode-2",
            "--config",
            "/tmp/custom-bitrouter.yaml",
            "--human",
        ])
        .expect("parse trajectory replay");
        assert!(replay.human);
        assert!(matches!(
            replay.command,
            Some(Command::Trajectory {
                config: Some(config),
                action: TrajectoryAction::Replay { episode_id },
            }) if episode_id == "episode-2" && config == Path::new("/tmp/custom-bitrouter.yaml")
        ));

        let prune = Cli::try_parse_from([
            "bitrouter",
            "trajectory",
            "prune",
            "--before",
            "2026-08-01T00:00:00Z",
            "--dry-run",
        ])
        .expect("parse trajectory prune");
        assert!(matches!(
            prune.command,
            Some(Command::Trajectory {
                config: None,
                action: TrajectoryAction::Prune { before, dry_run: true },
            }) if before == "2026-08-01T00:00:00Z"
        ));
        assert!(
            Cli::try_parse_from([
                "bitrouter",
                "trajectory",
                "prune",
                "--before",
                "not-a-timestamp",
            ])
            .is_err()
        );
    }

    #[tokio::test]
    async fn trajectory_store_anchors_file_and_zero_config_databases_to_daemon_home()
    -> anyhow::Result<()> {
        use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

        async fn seed_episode(database_url: &str, episode_id: &str) -> anyhow::Result<()> {
            let db = bitrouter::db::connect(database_url).await?;
            bitrouter::db::run_migrations(&db).await?;
            db.execute(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "INSERT INTO trajectory_episodes \
                 (episode_id, owner_user_id, correlation_source, correlation_key_id, \
                  correlation_digest, history_completeness, next_sequence, first_captured_at, \
                  last_captured_at, closed_at, latest_request_id) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                [
                    episode_id.into(),
                    "owner-home".into(),
                    "explicit_root".into(),
                    "key-home".into(),
                    format!("hmac-sha256:key-home:{}", "0".repeat(64)).into(),
                    "complete".into(),
                    1_i64.into(),
                    "2026-08-01T00:00:00Z".into(),
                    "2026-08-01T00:00:00Z".into(),
                    Option::<String>::None.into(),
                    Option::<String>::None.into(),
                ],
            ))
            .await?;
            Ok(())
        }

        let file_home = tempfile::tempdir()?;
        let config_path = file_home.path().join("bitrouter.yaml");
        std::fs::write(
            &config_path,
            "inherit_defaults: false\ndatabase:\n  url: sqlite://./daemon.db\n",
        )?;
        let file_db = file_home.path().join("daemon.db");
        seed_episode(
            &format!("sqlite://{}?mode=rwc", file_db.display()),
            "episode-file-home",
        )
        .await?;
        assert_ne!(std::env::current_dir()?, file_home.path());
        let file_source = bitrouter::paths::ConfigSource::File(config_path);
        let (_, file_store) = local_trajectory_store(&file_source).await?;
        assert_eq!(
            file_store
                .resolve_episode_owner("episode-file-home")
                .await?
                .as_deref(),
            Some("owner-home")
        );

        let zero_home = tempfile::tempdir()?;
        let zero_db = zero_home.path().join("bitrouter.db");
        seed_episode(
            &format!("sqlite://{}?mode=rwc", zero_db.display()),
            "episode-zero-home",
        )
        .await?;
        assert_ne!(std::env::current_dir()?, zero_home.path());
        let zero_source = bitrouter::paths::ConfigSource::Default {
            home: zero_home.path().to_path_buf(),
        };
        let (_, zero_store) = local_trajectory_store(&zero_source).await?;
        assert_eq!(
            zero_store
                .resolve_episode_owner("episode-zero-home")
                .await?
                .as_deref(),
            Some("owner-home")
        );
        Ok(())
    }

    #[test]
    fn reconcile_metering_accepts_explicit_cloud_credentials_file() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "bitrouter",
            "workflow-state",
            "reconcile-metering",
            "--database-url",
            "sqlite:///tmp/usage.db",
            "--credentials-file",
            "/tmp/account-credentials.json",
            "--request-id",
            "req-1",
        ])
        .expect("parse credentials file");
        match cli.command {
            Some(Command::WorkflowState {
                action:
                    WorkflowStateAction::ReconcileMetering {
                        credentials_file, ..
                    },
            }) => assert_eq!(
                credentials_file.as_deref(),
                Some(Path::new("/tmp/account-credentials.json"))
            ),
            _ => panic!("expected reconcile-metering"),
        }
    }

    #[tokio::test]
    async fn settlement_explicit_key_bypasses_malformed_credentials_file() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("account-credentials.json");
        std::fs::write(&path, b"malformed")?;
        let key = settlement_api_key(
            Some("brk_explicit.secret"),
            Some(&path),
            "https://api.bitrouter.ai/v1",
        )
        .await?;
        assert_eq!(key, "brk_explicit.secret");
        Ok(())
    }

    #[tokio::test]
    async fn settlement_credentials_file_rejects_oauth() -> anyhow::Result<()> {
        use bitrouter_providers::hosted::account::credentials::{Credentials, StoredCredential};
        use bitrouter_providers::hosted::account::manager::CredentialManager;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("account-credentials.json");
        let manager = CredentialManager::with_client(&path, reqwest::Client::new());
        manager
            .save(StoredCredential::from(Credentials {
                access_token: "oauth-access".to_owned(),
                refresh_token: Some("oauth-refresh".to_owned()),
                expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
                refresh_token_expires_at: None,
                token_type: "Bearer".to_owned(),
                scope: "inference:invoke".to_owned(),
                client_id: "bitrouter-cli".to_owned(),
                authorization_server: "https://api.bitrouter.ai".to_owned(),
                namespace_id: Some("ns-test".to_owned()),
                subject: None,
            }))
            .await?;
        let error = match settlement_api_key(None, Some(&path), "https://api.bitrouter.ai/v1").await
        {
            Ok(_) => anyhow::bail!("OAuth unexpectedly resolved for settlement"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("requires a static BitRouter API key")
        );
        Ok(())
    }

    #[tokio::test]
    async fn settlement_credentials_file_rejects_wrong_origin() -> anyhow::Result<()> {
        use bitrouter_providers::hosted::account::credentials::StoredCredential;
        use bitrouter_providers::hosted::account::manager::CredentialManager;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("account-credentials.json");
        let manager = CredentialManager::with_client(&path, reqwest::Client::new());
        manager
            .save(StoredCredential::api_key(
                "brk_stored.secret".to_owned(),
                "https://other.example".to_owned(),
            ))
            .await?;
        let error = match settlement_api_key(None, Some(&path), "https://api.bitrouter.ai/v1").await
        {
            Ok(_) => anyhow::bail!("credential unexpectedly crossed origins"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("does not match"));
        Ok(())
    }

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        // Panics if clap detects a conflict (e.g. `--tag` vs global `--version`).
        Cli::command().debug_assert();
    }

    #[test]
    fn generic_eval_commands_parse_as_nested_exchange_operations() {
        use clap::Parser;
        let submit = Cli::try_parse_from(["bitrouter", "eval", "result", "submit", "result.json"])
            .expect("parse eval result submit");
        assert!(matches!(
            submit.command,
            Some(Command::Eval {
                action: EvalAction::Result {
                    action: EvalResultAction::Submit { .. }
                }
            })
        ));
        let freeze = Cli::try_parse_from([
            "bitrouter",
            "eval",
            "snapshot",
            "freeze",
            "--at",
            "2026-07-30T00:00:00Z",
        ])
        .expect("parse eval snapshot freeze");
        assert!(matches!(
            freeze.command,
            Some(Command::Eval {
                action: EvalAction::Snapshot {
                    action: EvalSnapshotAction::Freeze { .. }
                }
            })
        ));
    }

    #[test]
    fn workflow_bundle_allows_eval_exchange_to_own_task_outcomes() {
        use clap::Parser;

        let cli = Cli::try_parse_from([
            "bitrouter",
            "workflow-state",
            "bundle",
            "--run-label",
            "generic-eval-run",
            "--traces",
            "traces.jsonl",
            "--cloud-usage",
            "usage.jsonl",
            "--output-dir",
            "artifact",
        ])
        .expect("parse workflow bundle without request-scoped outcomes");

        assert!(matches!(
            cli.command,
            Some(Command::WorkflowState {
                action: WorkflowStateAction::Bundle { outcomes: None, .. }
            })
        ));
    }

    #[test]
    fn policy_oracle_accepts_human_readable_cost_assumptions() {
        use clap::Parser;

        let cli = Cli::try_parse_from([
            "bitrouter",
            "workflow-state",
            "policy-oracle",
            "--traces",
            "traces.jsonl",
            "--cloud-usage",
            "usage.jsonl",
            "--policy-lock",
            "policy-lock.yaml",
            "--policy",
            "auto",
            "--effective-cost-factor",
            "0.24",
            "--target-savings",
            "0.30",
            "--target-savings",
            "0.40",
            "--output",
            "oracle.json",
        ])
        .expect("parse policy counterfactual oracle");

        assert!(matches!(
            cli.command,
            Some(Command::WorkflowState {
                action: WorkflowStateAction::PolicyOracle {
                    effective_cost_factor_ppm: 240_000,
                    target_savings_ppm,
                    ..
                }
            }) if target_savings_ppm == vec![300_000, 400_000]
        ));
    }

    #[tokio::test]
    async fn eval_subject_seal_derives_a_digest_without_the_ledger() -> Result<()> {
        use clap::Parser;

        let directory = tempfile::tempdir()?;
        let draft = directory.path().join("subject-draft.json");
        let reversed_draft = directory.path().join("subject-draft-reversed.json");
        let sealed = directory.path().join("subject-sealed.json");
        let sealed_again = directory.path().join("subject-sealed-again.json");
        std::fs::write(
            &draft,
            r#"{
  "schema_version": 1,
  "eval_id": "eval-generic-1",
  "scope": "task",
  "subject_id": "task-generic-1",
  "policy_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "preset": null,
  "cohort": null,
  "holdout": false,
  "decisions": [
    {
      "decision_id": "decision-1",
      "policy": "auto",
      "route_projection": "agent_route/v1|code:generation|implement|normal",
      "request_key": "agent_route/v1|unknown|implement|normal",
      "selected_tier": "economy",
      "baseline_tier": "strong",
      "policy_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }
  ],
  "requested_dimensions": ["quality.pass"],
  "evidence": [
    {
      "evidence_id": "verifier-result",
      "kind": "task.verifier",
      "digest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
      "redacted": true,
      "attributes": {}
    },
    {
      "evidence_id": "audit-result",
      "kind": "task.audit",
      "digest": "sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
      "redacted": true,
      "attributes": {"status": "clean"}
    }
  ],
  "evidence_digest": "",
  "observed_at": "2026-07-31T00:00:00Z"
}
"#,
        )?;
        let mut reversed =
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&draft)?)?;
        let evidence = reversed
            .get_mut("evidence")
            .and_then(serde_json::Value::as_array_mut)
            .ok_or_else(|| anyhow::anyhow!("draft evidence must be an array"))?;
        evidence.reverse();
        std::fs::write(&reversed_draft, serde_json::to_string_pretty(&reversed)?)?;

        let cli = Cli::try_parse_from([
            "bitrouter",
            "eval",
            "subject",
            "seal",
            draft
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("draft path is not UTF-8"))?,
            "--output",
            sealed
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("sealed path is not UTF-8"))?,
        ])?;
        let Some(Command::Eval { action }) = cli.command else {
            anyhow::bail!("expected eval subject seal command")
        };

        eval(action, &Output::new(bitrouter::output::Format::Json)).await?;

        let sealed_text = std::fs::read_to_string(&sealed)?;
        let subject: bitrouter::eval::types::EvalSubject = serde_json::from_str(&sealed_text)?;
        bitrouter::eval::types::validate_subject(&subject)?;

        let repeat = Cli::try_parse_from([
            "bitrouter",
            "eval",
            "subject",
            "seal",
            reversed_draft
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("reversed draft path is not UTF-8"))?,
            "--output",
            sealed_again
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("repeat output path is not UTF-8"))?,
        ])?;
        let Some(Command::Eval { action }) = repeat.command else {
            anyhow::bail!("expected repeated eval subject seal command")
        };
        eval(action, &Output::new(bitrouter::output::Format::Json)).await?;
        let repeated_text = std::fs::read_to_string(&sealed_again)?;
        let repeated: bitrouter::eval::types::EvalSubject = serde_json::from_str(&repeated_text)?;
        bitrouter::eval::types::validate_subject(&repeated)?;
        assert_eq!(subject.evidence_digest, repeated.evidence_digest);
        assert_eq!(
            sealed_text, repeated_text,
            "equivalent evidence order must produce deterministic pretty JSON"
        );
        assert!(
            !directory.path().join("bitrouter.db").exists(),
            "sealing must not initialize a ledger"
        );
        Ok(())
    }

    #[test]
    fn mcp_serve_backends_are_local_cloud_and_skills() {
        use clap::Parser;
        for (flag, expected) in [
            ("local", McpBackend::Local),
            ("cloud", McpBackend::Cloud),
            ("skills", McpBackend::Skills),
        ] {
            let cli = Cli::try_parse_from(["bitrouter", "mcp", "serve", "--backend", flag])
                .expect("parse");
            match cli.command {
                Some(Command::Mcp {
                    action: McpAction::Serve { backend, .. },
                }) => assert_eq!(backend, Some(expected)),
                _ => panic!("expected `mcp serve --backend {flag}` to parse"),
            }
        }
        // The orchestrator's fleet bridge is gone; its backend name no longer
        // parses.
        assert!(
            Cli::try_parse_from(["bitrouter", "mcp", "serve", "--backend", "fleet"]).is_err(),
            "the fleet backend must no longer be accepted"
        );
    }

    #[test]
    fn update_flags_parse() {
        use clap::Parser;
        let cli =
            Cli::try_parse_from(["bitrouter", "update", "--check", "--tag", "1.0.0-alpha.18"])
                .expect("parse");
        match cli.command {
            Some(Command::Update {
                check,
                tag,
                stable,
                restart,
                yes,
            }) => {
                assert!(check);
                assert_eq!(tag.as_deref(), Some("1.0.0-alpha.18"));
                assert!(!stable && !restart && !yes);
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn bare_invocation_parses_to_no_subcommand() {
        use clap::Parser;
        // Bare `bitrouter` → no subcommand → onboarding entry dispatch.
        let cli = Cli::try_parse_from(["bitrouter"]).expect("parse");
        assert!(cli.command.is_none());
        // Global flags still parse without a subcommand.
        let human = Cli::try_parse_from(["bitrouter", "--human"]).expect("parse");
        assert!(human.command.is_none() && human.human);
    }

    #[test]
    fn init_wizard_flags_parse() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "bitrouter",
            "init",
            "--yes",
            "--force",
            "--reset",
            "--api-key",
            "brk_abc.secret",
            "--provider",
            "openai",
            "--provider-api-key",
            "sk-openai",
            "--harness",
            "claude",
            "--harness",
            "codex",
            "--after",
            "exit",
            "--model",
            "openai/gpt-5",
            "--write-config",
        ])
        .expect("parse");
        match cli.command {
            Some(Command::Init {
                yes,
                force,
                reset,
                api_key,
                providers,
                provider_api_keys,
                harnesses,
                after,
                model,
                write_config,
                ..
            }) => {
                assert!(yes && force && reset && write_config);
                assert_eq!(api_key.as_deref(), Some("brk_abc.secret"));
                assert_eq!(providers, vec!["openai"]);
                assert_eq!(provider_api_keys, vec!["sk-openai"]);
                assert_eq!(
                    harnesses,
                    vec![
                        bitrouter::spawn::SpawnAgent::Claude,
                        bitrouter::spawn::SpawnAgent::Codex
                    ]
                );
                assert_eq!(after, Some(bitrouter::onboarding::AfterAction::Exit));
                assert_eq!(model.as_deref(), Some("openai/gpt-5"));
            }
            _ => panic!("expected Init"),
        }
    }

    #[test]
    fn init_onboarding_rejects_removed_optimization_flags() {
        use clap::Parser;

        for removed_args in [
            &["--optimize"][..],
            &["--optimize-workflow-command", "./run-eval"][..],
            &["--optimize-workflow-arg", "--smoke"][..],
            &["--optimize-workflow-input", ".venv"][..],
            &["--optimize-success", "tests pass"][..],
            &["--optimize-strong", "openai:gpt-5"][..],
            &["--optimize-strong-effort", "high"][..],
            &["--optimize-economy", "openai:gpt-5-mini"][..],
            &["--optimize-economy-effort", "low"][..],
            &["--optimize-normalized-price", "openai:gpt-5=100"][..],
            &["--optimize-preference", "balanced"][..],
        ] {
            let mut command = vec!["bitrouter", "init"];
            command.extend_from_slice(removed_args);
            assert!(
                Cli::try_parse_from(command).is_err(),
                "removed init arguments {removed_args:?} still parse"
            );
        }
    }

    #[test]
    fn providers_login_api_key_flag_parses() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "bitrouter",
            "providers",
            "login",
            "openai",
            "--api-key",
            "sk-abc",
        ])
        .expect("parse");
        match cli.command {
            Some(Command::Providers {
                action:
                    ProviderAction::Login {
                        provider,
                        api_key,
                        key_stdin,
                        ..
                    },
            }) => {
                assert_eq!(provider, "openai");
                assert_eq!(api_key.as_deref(), Some("sk-abc"));
                assert!(!key_stdin);
            }
            _ => panic!("expected provider login"),
        }
    }

    #[test]
    fn providers_login_api_key_conflicts_with_oauth_flags() {
        use clap::Parser;
        let parsed = Cli::try_parse_from([
            "bitrouter",
            "providers",
            "login",
            "openai",
            "--api-key",
            "sk-abc",
            "--no-browser",
        ]);
        let err = match parsed {
            Ok(_) => panic!("api-key + oauth flags must conflict"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn provider_login_import_existing_flags_parse() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "bitrouter",
            "providers",
            "login",
            "openai-codex",
            "--import-existing",
            "--no-browser",
            "--label",
            "work",
        ])
        .expect("parse");
        match cli.command {
            Some(Command::Providers {
                action:
                    ProviderAction::Login {
                        provider,
                        label,
                        import_existing,
                        no_browser,
                        ..
                    },
            }) => {
                assert_eq!(provider, "openai-codex");
                assert_eq!(label, "work");
                assert!(import_existing);
                assert!(no_browser);
            }
            _ => panic!("expected provider login"),
        }
    }

    #[test]
    fn routing_policy_init_and_evolve_flags_parse() {
        use clap::Parser;
        let init = Cli::try_parse_from([
            "bitrouter",
            "policy",
            "init",
            "terminal-bench",
            "--preset",
            "coding",
            "--strong",
            "openai-codex:gpt-5.6-sol",
            "--strong-effort",
            "high",
            "--economy",
            "openai-codex:gpt-5.6-sol",
            "--economy-effort",
            "low",
            "--config",
            "team/bitrouter.yaml",
        ])
        .expect("parse init");
        match init.command {
            Some(Command::Policy {
                action:
                    PolicyAction::Init {
                        name,
                        preset,
                        strong,
                        strong_effort,
                        economy,
                        economy_effort,
                        config,
                    },
            }) => {
                assert_eq!(name, "terminal-bench");
                assert_eq!(preset, "coding");
                assert_eq!(strong.as_deref(), Some("openai-codex:gpt-5.6-sol"));
                assert_eq!(
                    strong_effort,
                    Some(bitrouter_sdk::language_model::types::ReasoningEffort::High)
                );
                assert_eq!(economy, "openai-codex:gpt-5.6-sol");
                assert_eq!(
                    economy_effort,
                    Some(bitrouter_sdk::language_model::types::ReasoningEffort::Low)
                );
                assert_eq!(config, Some(PathBuf::from("team/bitrouter.yaml")));
            }
            _ => panic!("expected policy init"),
        }

        let evolve = Cli::try_parse_from(["bitrouter", "policy", "evolve", "--apply"])
            .expect("parse evolve");
        assert!(matches!(
            evolve.command,
            Some(Command::Policy {
                action: PolicyAction::Evolve { apply: true, .. }
            })
        ));

        let export = Cli::try_parse_from([
            "bitrouter",
            "policy",
            "evolve",
            "--output",
            "candidate.yaml",
        ])
        .expect("parse candidate export");
        assert!(matches!(
            export.command,
            Some(Command::Policy {
                action: PolicyAction::Evolve {
                    apply: false,
                    output: Some(path),
                    ..
                }
            }) if path == Path::new("candidate.yaml")
        ));

        assert!(Cli::try_parse_from(["bitrouter", "policy", "evolve", "--freeze"]).is_err());
        assert!(Cli::try_parse_from(["bitrouter", "policy", "lock"]).is_err());
        assert!(Cli::try_parse_from(["bitrouter", "policy", "unlock"]).is_err());
        assert!(
            Cli::try_parse_from([
                "bitrouter",
                "policy",
                "evolve",
                "--apply",
                "--output",
                "candidate.yaml",
            ])
            .is_err()
        );
    }

    #[test]
    fn policy_compile_diff_and_rollback_commands_parse() {
        use clap::Parser;

        assert!(
            Cli::try_parse_from([
                "bitrouter",
                "policy",
                "compile",
                "--output",
                "candidate.yaml"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["bitrouter", "policy", "diff", "active.yaml", "next.yaml"])
                .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "bitrouter",
                "policy",
                "publish",
                "candidate.yaml",
                "--config",
                "bitrouter.yaml",
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["bitrouter", "policy", "rollback", "sha256:abc"]).is_ok());
    }

    #[test]
    fn optimize_accepts_only_history_controller_commands() -> anyhow::Result<()> {
        use clap::Parser;

        assert!(
            Cli::try_parse_from([
                "bitrouter",
                "optimize",
                "run",
                "--policy",
                "auto",
                "--candidate-tier",
                "economy",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["bitrouter", "optimize", "status", "--policy", "auto",]).is_ok()
        );

        for removed in ["setup", "resolve", "review", "publish", "rollback"] {
            assert!(
                Cli::try_parse_from(["bitrouter", "optimize", removed]).is_err(),
                "removed optimize command {removed} still parses"
            );
        }
        for removed_args in [
            &["run", "--workflow-command", "./run-eval"][..],
            &["run", "--workflow-arg", "--smoke"][..],
            &["run", "--workflow-input", ".venv"][..],
            &["run", "--evaluator-agent", "codex-acp"][..],
            &["run", "--evaluator-model", "gpt-5.6"][..],
            &["run", "--evaluator-via-cloud"][..],
            &["run", "--enable-adaptive"][..],
        ] {
            let mut command = vec!["bitrouter", "optimize"];
            command.extend_from_slice(removed_args);
            assert!(
                Cli::try_parse_from(command).is_err(),
                "removed optimize arguments {removed_args:?} still parse"
            );
        }

        let run = Cli::try_parse_from(["bitrouter", "optimize", "run"])?;
        assert!(matches!(
            run.command,
            Some(Command::Optimize {
                action: OptimizeAction::Run {
                    policy,
                    candidate_tier: None,
                    exploration_ppm: 100_000,
                    minimum_tasks: 3,
                    maximum_tasks: 20,
                    minimum_pass_rate_ppm: 900_000,
                    evaluator_config_digest: None,
                    config,
                    socket: None,
                }
            }) if policy == "auto"
                && config == Path::new("bitrouter.yaml")
        ));
        Ok(())
    }

    #[test]
    fn optimize_status_without_active_exploration_is_idle() {
        let report = optimization_status_report(
            bitrouter::optimization::controller::OptimizationStatus {
                policy: "auto".into(),
                policy_mode: bitrouter_sdk::config::PolicyRuntimeMode::Adaptive,
                active_policy_digest:
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                parent_policy_digest: None,
                eval_snapshot_root: None,
                observed_subject_digest: None,
                active_experiment: None,
            },
        );

        assert_eq!(report.action, "optimize.status");
        assert_eq!(report.decision, "idle");
    }

    #[test]
    fn optimize_status_with_active_exploration_is_exploring() {
        let report = optimization_status_report(
            bitrouter::optimization::controller::OptimizationStatus {
                policy: "auto".into(),
                policy_mode: bitrouter_sdk::config::PolicyRuntimeMode::Adaptive,
                active_policy_digest:
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                parent_policy_digest: None,
                eval_snapshot_root: None,
                observed_subject_digest: None,
                active_experiment: Some(bitrouter::optimization::exploration::RouteExploration {
                    experiment_id: "experiment-1".into(),
                    target_request_key: "agent_route/v1|unknown|implement|normal".into(),
                    champion_tier: "strong".into(),
                    challenger_tier: "economy".into(),
                    challenger_exposure_ppm: 100_000,
                    gate: bitrouter::optimization::exploration::OptimizationGate {
                        minimum_tasks_per_arm: 3,
                        maximum_challenger_tasks: 20,
                        minimum_pass_rate_ppm: 900_000,
                        evaluator_config_digest: None,
                    },
                }),
            },
        );

        assert_eq!(report.action, "optimize.status");
        assert_eq!(report.decision, "exploring");
    }

    #[tokio::test]
    async fn optimize_explore_json_reports_the_explicit_evaluator_pin() -> anyhow::Result<()> {
        use std::collections::{BTreeMap, BTreeSet};

        use bitrouter::eval::EvalService;
        use bitrouter::eval::admission::SubmissionPrincipal;
        use bitrouter::eval::store::EvalStore;
        use bitrouter::eval::types::{
            EVAL_SCHEMA_VERSION, EvalDecisionRef, EvalScope, EvalSubject, EvalVerdict,
            EvaluationResult, EvaluatorIdentity, EvaluatorKind, EvidenceItem, evidence_digest,
        };

        const PIN: &str = "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let directory = tempfile::tempdir()?;
        let config_path = directory.path().join("bitrouter.yaml");
        let database_path = directory.path().join("eval.db");
        let database_url = format!("sqlite://{}?mode=rwc", database_path.display());
        std::fs::write(
            &config_path,
            "database:\n  url: sqlite://./eval.db\npolicy:\n  mode: frozen\n  path: policy-lock.yaml\npresets:\n  auto:\n    model: strong-model\n",
        )?;
        let database = bitrouter::db::connect(&database_url).await?;
        bitrouter::db::run_migrations(&database).await?;
        drop(database);
        let initialized = bitrouter::policy_lock::initialize_files(
            &config_path,
            "auto",
            "auto",
            Some("strong-model"),
            "economy-model",
        )
        .await?;
        let store = EvalStore::new(bitrouter::db::connect(&database_url).await?);
        let service = EvalService::new(store.clone(), bitrouter_sdk::config::EvalConfig::default());
        for (scope, subject_id, cost) in [
            (EvalScope::Request, "request-report-pin", Some("900")),
            (EvalScope::Task, "task-report-pin", None),
        ] {
            let evidence = cost.map_or_else(Vec::new, |cost| {
                vec![EvidenceItem {
                    evidence_id: format!("evidence-{subject_id}"),
                    kind: "request.outcome".into(),
                    digest: PIN.into(),
                    redacted: true,
                    attributes: BTreeMap::from([("cost_micro_usd".into(), cost.into())]),
                }]
            });
            let digest = evidence_digest(&evidence)?;
            let eval_id = format!("eval-{subject_id}");
            let subject = EvalSubject {
                schema_version: EVAL_SCHEMA_VERSION,
                eval_id: eval_id.clone(),
                scope,
                subject_id: subject_id.into(),
                policy_digest: initialized.digest.clone(),
                preset: Some("auto".into()),
                cohort: None,
                holdout: false,
                decisions: vec![EvalDecisionRef {
                    decision_id: format!("decision-{subject_id}"),
                    policy: "auto".into(),
                    route_projection: "agent_route/v1|unknown|implement|normal".into(),
                    request_key: "agent_route/v1|unknown|implement|normal".into(),
                    selected_tier: "strong".into(),
                    selected_effort: None,
                    baseline_tier: None,
                    baseline_effort: None,
                    policy_digest: initialized.digest.clone(),
                    experiment: None,
                }],
                requested_dimensions: BTreeSet::new(),
                evidence,
                evidence_digest: digest.clone(),
                observed_at: "2026-08-17T00:00:00Z".into(),
            };
            store.insert_subject(&subject).await?;
            service
                .submit(
                    EvaluationResult {
                        schema_version: EVAL_SCHEMA_VERSION,
                        eval_id,
                        evidence_digest: digest,
                        evaluator: EvaluatorIdentity {
                            authority_id: "local".into(),
                            evaluator_id: "history".into(),
                            kind: EvaluatorKind::TaskNative,
                            version: "1".into(),
                            config_digest: PIN.into(),
                        },
                        verdict: if scope == EvalScope::Request {
                            EvalVerdict::Inconclusive
                        } else {
                            EvalVerdict::Pass
                        },
                        metrics: BTreeMap::new(),
                        hard_violations: Vec::new(),
                        confidence_ppm: None,
                        evidence_refs: Vec::new(),
                        decision_credit: BTreeMap::new(),
                        idempotency_key: format!("result-{subject_id}"),
                        submitted_at: "2026-08-17T00:00:01Z".into(),
                    },
                    SubmissionPrincipal::LocalOperator,
                )
                .await?;
        }
        let prepared = bitrouter::optimization::controller::prepare_files(
            &config_path,
            bitrouter::optimization::controller::OptimizationOptions {
                evaluator_config_digest: Some(PIN.into()),
                ..Default::default()
            },
        )
        .await?;
        let report = optimization_run_report("auto", &prepared);
        let json = serde_json::to_value(report)?;

        assert_eq!(json["decision"], "explore");
        assert_eq!(json["evaluator_config_digest"], PIN);
        Ok(())
    }

    struct RecoveryFixture {
        _directory: tempfile::TempDir,
        config_path: PathBuf,
        policy_path: PathBuf,
        config_before: String,
        config_after: String,
        parent_digest: String,
        child_document: bitrouter::policy_lock::PolicyLock,
        publication: bitrouter::optimization::controller::OptimizationPublication,
    }

    fn recovery_fixture() -> anyhow::Result<RecoveryFixture> {
        let directory = tempfile::tempdir()?;
        let config_path = directory.path().join("bitrouter.yaml");
        let policy_path = directory.path().join("policy-lock.yaml");
        let config_before =
            "# keep operator bytes\npolicy:\n  mode: frozen\n  path: policy-lock.yaml\n"
                .to_string();
        let config_after = bitrouter::policy_lock::edit_config_mode(
            &config_before,
            config::PolicyRuntimeMode::Adaptive,
        )?;
        std::fs::write(&config_path, &config_after)?;
        let parent = bitrouter::policy_lock::PolicyLock::default();
        let parent_digest =
            bitrouter::policy_lock::write_atomic_unlocked(&policy_path, None, &parent)?;
        let mut child_document = parent;
        let artifact = child_document
            .artifact
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("default compiled policy has no artifact"))?;
        artifact.parent_digest = Some(parent_digest.clone());
        artifact.source_snapshot_time_unix_ms = 1;
        let history = bitrouter::policy_lock::default_history_dir(&policy_path);
        let record = bitrouter::policy_lock::publish_candidate_unlocked(
            &policy_path,
            &parent_digest,
            &child_document,
            &history,
        )?;
        let update = bitrouter::policy_lock::PolicyFileUpdate {
            path: policy_path.clone(),
            digest: record.child_digest.clone(),
            document: child_document.clone(),
            changes: Vec::new(),
            conflicts: Vec::new(),
        };
        let publication = bitrouter::optimization::controller::OptimizationPublication {
            action: bitrouter::optimization::controller::ControllerAction::Explore,
            parent_policy_digest: parent_digest.clone(),
            active_policy_digest: record.child_digest,
            eval_snapshot_root:
                "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            published: true,
            config_activated: true,
            config_path: config_path.clone(),
            config_before: config_before.clone(),
            config_after: config_after.clone(),
            update: Some(update),
        };
        Ok(RecoveryFixture {
            _directory: directory,
            config_path,
            policy_path,
            config_before,
            config_after,
            parent_digest,
            child_document,
            publication,
        })
    }

    #[tokio::test]
    async fn optimizer_reload_recovery_restores_an_adaptive_publication_with_identical_config()
    -> anyhow::Result<()> {
        use std::collections::{BTreeMap, BTreeSet};

        use bitrouter::eval::EvalService;
        use bitrouter::eval::admission::SubmissionPrincipal;
        use bitrouter::eval::store::EvalStore;
        use bitrouter::eval::types::{
            EVAL_SCHEMA_VERSION, EvalDecisionRef, EvalScope, EvalSubject, EvalVerdict,
            EvaluationResult, EvaluatorIdentity, EvaluatorKind, EvidenceItem, evidence_digest,
        };

        const EVIDENCE_DIGEST: &str =
            "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let directory = tempfile::tempdir()?;
        let config_path = directory.path().join("bitrouter.yaml");
        let database_path = directory.path().join("eval.db");
        let database_url = format!("sqlite://{}?mode=rwc", database_path.display());
        std::fs::write(
            &config_path,
            "database:\n  url: sqlite://./eval.db\npolicy:\n  mode: frozen\n  path: policy-lock.yaml\npresets:\n  auto:\n    model: strong-model\n",
        )?;
        let database = bitrouter::db::connect(&database_url).await?;
        bitrouter::db::run_migrations(&database).await?;
        drop(database);
        let initialized = bitrouter::policy_lock::initialize_files(
            &config_path,
            "auto",
            "auto",
            Some("strong-model"),
            "economy-model",
        )
        .await?;
        let frozen_config = std::fs::read_to_string(&config_path)?;
        let adaptive_config = bitrouter::policy_lock::edit_config_mode(
            &frozen_config,
            config::PolicyRuntimeMode::Adaptive,
        )?;
        std::fs::write(&config_path, &adaptive_config)?;
        let store = EvalStore::new(bitrouter::db::connect(&database_url).await?);
        let service = EvalService::new(store.clone(), bitrouter_sdk::config::EvalConfig::default());
        for (scope, subject_id, cost) in [
            (EvalScope::Request, "request-adaptive-recovery", Some("900")),
            (EvalScope::Task, "task-adaptive-recovery", None),
        ] {
            let evidence = cost.map_or_else(Vec::new, |cost| {
                vec![EvidenceItem {
                    evidence_id: format!("evidence-{subject_id}"),
                    kind: "request.outcome".into(),
                    digest: EVIDENCE_DIGEST.into(),
                    redacted: true,
                    attributes: BTreeMap::from([("cost_micro_usd".into(), cost.into())]),
                }]
            });
            let digest = evidence_digest(&evidence)?;
            let eval_id = format!("eval-{subject_id}");
            let subject = EvalSubject {
                schema_version: EVAL_SCHEMA_VERSION,
                eval_id: eval_id.clone(),
                scope,
                subject_id: subject_id.into(),
                policy_digest: initialized.digest.clone(),
                preset: Some("auto".into()),
                cohort: None,
                holdout: false,
                decisions: vec![EvalDecisionRef {
                    decision_id: format!("decision-{subject_id}"),
                    policy: "auto".into(),
                    route_projection: "agent_route/v1|unknown|implement|normal".into(),
                    request_key: "agent_route/v1|unknown|implement|normal".into(),
                    selected_tier: "strong".into(),
                    selected_effort: None,
                    baseline_tier: None,
                    baseline_effort: None,
                    policy_digest: initialized.digest.clone(),
                    experiment: None,
                }],
                requested_dimensions: BTreeSet::new(),
                evidence,
                evidence_digest: digest.clone(),
                observed_at: "2026-08-17T00:00:00Z".into(),
            };
            store.insert_subject(&subject).await?;
            service
                .submit(
                    EvaluationResult {
                        schema_version: EVAL_SCHEMA_VERSION,
                        eval_id,
                        evidence_digest: digest,
                        evaluator: EvaluatorIdentity {
                            authority_id: "local".into(),
                            evaluator_id: "history".into(),
                            kind: EvaluatorKind::TaskNative,
                            version: "1".into(),
                            config_digest: EVIDENCE_DIGEST.into(),
                        },
                        verdict: if scope == EvalScope::Request {
                            EvalVerdict::Inconclusive
                        } else {
                            EvalVerdict::Pass
                        },
                        metrics: BTreeMap::new(),
                        hard_violations: Vec::new(),
                        confidence_ppm: None,
                        evidence_refs: Vec::new(),
                        decision_credit: BTreeMap::new(),
                        idempotency_key: format!("result-{subject_id}"),
                        submitted_at: "2026-08-17T00:00:01Z".into(),
                    },
                    SubmissionPrincipal::LocalOperator,
                )
                .await?;
        }
        let prepared = bitrouter::optimization::controller::prepare_files(
            &config_path,
            bitrouter::optimization::controller::OptimizationOptions::default(),
        )
        .await?;
        let publication = bitrouter::optimization::controller::publish_prepared(prepared).await?;
        let policy_path = publication
            .update
            .as_ref()
            .map(|update| update.path.clone())
            .context("adaptive optimizer publication has no policy update")?;
        assert_eq!(publication.config_before, adaptive_config);
        assert_eq!(publication.config_after, adaptive_config);
        assert!(!publication.config_activated);
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let live = Arc::new(std::sync::Mutex::new((
            config::PolicyRuntimeMode::Adaptive,
            initialized.digest.clone(),
        )));
        let reload = {
            let attempts = attempts.clone();
            let live = live.clone();
            let config_path = config_path.clone();
            let policy_path = policy_path.clone();
            move || {
                let attempts = attempts.clone();
                let live = live.clone();
                let config_path = config_path.clone();
                let policy_path = policy_path.clone();
                async move {
                    if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        anyhow::bail!("injected adaptive daemon rejection of child policy")
                    }
                    let raw = std::fs::read_to_string(config_path)?;
                    let mode = bitrouter_sdk::config::parse(&raw)?.policy.mode;
                    let digest = bitrouter::policy_lock::load(&policy_path).await?.digest;
                    *live
                        .lock()
                        .map_err(|_| anyhow::anyhow!("modeled daemon state lock poisoned"))? =
                        (mode, digest);
                    Ok(())
                }
            }
        };

        let error =
            reload_optimization_publication_or_restore_with(&publication, reload, || Ok(()))
                .await
                .err()
                .context("injected adaptive daemon rejection unexpectedly succeeded")?;
        let detail = format!("{error:#}");

        assert!(detail.contains("injected adaptive daemon rejection of child policy"));
        assert!(detail.contains("reconciled and reloaded the parent"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(std::fs::read_to_string(&config_path)?, adaptive_config);
        assert_eq!(
            bitrouter_sdk::config::parse(&std::fs::read_to_string(&config_path)?)?
                .policy
                .mode,
            config::PolicyRuntimeMode::Adaptive
        );
        assert_eq!(
            bitrouter::policy_lock::load(&policy_path).await?.digest,
            initialized.digest
        );
        assert_eq!(
            *live
                .lock()
                .map_err(|_| anyhow::anyhow!("modeled daemon state lock poisoned"))?,
            (config::PolicyRuntimeMode::Adaptive, initialized.digest)
        );
        Ok(())
    }

    #[tokio::test]
    async fn optimizer_reload_recovery_restores_one_disk_and_live_parent_pair() -> anyhow::Result<()>
    {
        let fixture = recovery_fixture()?;
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let live = Arc::new(std::sync::Mutex::new((
            config::PolicyRuntimeMode::Frozen,
            fixture.parent_digest.clone(),
        )));
        let reload = {
            let attempts = attempts.clone();
            let live = live.clone();
            let config_path = fixture.config_path.clone();
            let policy_path = fixture.policy_path.clone();
            move || {
                let attempts = attempts.clone();
                let live = live.clone();
                let config_path = config_path.clone();
                let policy_path = policy_path.clone();
                async move {
                    if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        anyhow::bail!("injected daemon rejection of child policy")
                    }
                    let raw = std::fs::read_to_string(config_path)?;
                    let mode = bitrouter_sdk::config::parse(&raw)?.policy.mode;
                    let digest = bitrouter::policy_lock::load(&policy_path).await?.digest;
                    *live
                        .lock()
                        .map_err(|_| anyhow::anyhow!("modeled daemon state lock poisoned"))? =
                        (mode, digest);
                    Ok(())
                }
            }
        };

        let error = reload_optimization_publication_or_restore_with(
            &fixture.publication,
            reload,
            || Ok(()),
        )
        .await
        .err()
        .context("injected daemon rejection unexpectedly succeeded")?;

        assert!(format!("{error:#}").contains("injected daemon rejection"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            std::fs::read_to_string(&fixture.config_path)?,
            fixture.config_before
        );
        assert_eq!(
            bitrouter::policy_lock::load(&fixture.policy_path)
                .await?
                .digest,
            fixture.parent_digest
        );
        assert_eq!(
            *live
                .lock()
                .map_err(|_| anyhow::anyhow!("modeled daemon state lock poisoned"))?,
            (config::PolicyRuntimeMode::Frozen, fixture.parent_digest)
        );
        Ok(())
    }

    #[tokio::test]
    async fn optimizer_reload_recovery_reconciles_a_post_commit_policy_error() -> anyhow::Result<()>
    {
        let fixture = recovery_fixture()?;
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let live = Arc::new(std::sync::Mutex::new((
            config::PolicyRuntimeMode::Frozen,
            fixture.parent_digest.clone(),
        )));
        let reload = {
            let attempts = attempts.clone();
            let events = events.clone();
            let live = live.clone();
            let config_path = fixture.config_path.clone();
            let policy_path = fixture.policy_path.clone();
            move || {
                let attempts = attempts.clone();
                let events = events.clone();
                let live = live.clone();
                let config_path = config_path.clone();
                let policy_path = policy_path.clone();
                async move {
                    if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        events
                            .lock()
                            .map_err(|_| anyhow::anyhow!("recovery event lock poisoned"))?
                            .push("reload-child-rejected".into());
                        anyhow::bail!("injected daemon rejection of child policy")
                    }
                    let raw = std::fs::read_to_string(config_path)?;
                    let mode = bitrouter_sdk::config::parse(&raw)?.policy.mode;
                    let digest = bitrouter::policy_lock::load(&policy_path).await?.digest;
                    events
                        .lock()
                        .map_err(|_| anyhow::anyhow!("recovery event lock poisoned"))?
                        .push("reload-parent".into());
                    *live
                        .lock()
                        .map_err(|_| anyhow::anyhow!("modeled daemon state lock poisoned"))? =
                        (mode, digest);
                    Ok(())
                }
            }
        };
        let policy_events = events.clone();
        let config_events = events.clone();

        let error = reload_optimization_publication_or_restore_with_mutations(
            &fixture.publication,
            reload,
            || Ok(()),
            move |path, expected, target, history| {
                bitrouter::policy_lock::rollback_to_digest_unlocked(
                    path, expected, target, history,
                )?;
                policy_events
                    .lock()
                    .map_err(|_| anyhow::anyhow!("recovery event lock poisoned"))?
                    .push("policy-parent-post-commit-error".into());
                anyhow::bail!("injected post-commit policy failure")
            },
            move |path, expected, updated| {
                bitrouter::policy_lock::write_text_atomic_unlocked(path, expected, updated)?;
                config_events
                    .lock()
                    .map_err(|_| anyhow::anyhow!("recovery event lock poisoned"))?
                    .push("config-parent".into());
                Ok(())
            },
        )
        .await
        .err()
        .context("post-commit policy failure unexpectedly succeeded")?;
        let detail = format!("{error:#}");

        assert!(detail.contains("injected daemon rejection of child policy"));
        assert!(detail.contains("injected post-commit policy failure"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            *events
                .lock()
                .map_err(|_| anyhow::anyhow!("recovery event lock poisoned"))?,
            [
                "reload-child-rejected",
                "policy-parent-post-commit-error",
                "config-parent",
                "reload-parent",
            ]
        );
        assert_eq!(
            std::fs::read_to_string(&fixture.config_path)?,
            fixture.config_before
        );
        assert_eq!(
            bitrouter::policy_lock::load(&fixture.policy_path)
                .await?
                .digest,
            fixture.parent_digest
        );
        assert_eq!(
            *live
                .lock()
                .map_err(|_| anyhow::anyhow!("modeled daemon state lock poisoned"))?,
            (config::PolicyRuntimeMode::Frozen, fixture.parent_digest)
        );
        Ok(())
    }

    #[tokio::test]
    async fn optimizer_reload_recovery_reconciles_a_post_rename_config_error() -> anyhow::Result<()>
    {
        let fixture = recovery_fixture()?;
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let live = Arc::new(std::sync::Mutex::new((
            config::PolicyRuntimeMode::Frozen,
            fixture.parent_digest.clone(),
        )));
        let reload = {
            let attempts = attempts.clone();
            let events = events.clone();
            let live = live.clone();
            let config_path = fixture.config_path.clone();
            let policy_path = fixture.policy_path.clone();
            move || {
                let attempts = attempts.clone();
                let events = events.clone();
                let live = live.clone();
                let config_path = config_path.clone();
                let policy_path = policy_path.clone();
                async move {
                    if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        events
                            .lock()
                            .map_err(|_| anyhow::anyhow!("recovery event lock poisoned"))?
                            .push("reload-child-rejected".into());
                        anyhow::bail!("injected daemon rejection of child policy")
                    }
                    let raw = std::fs::read_to_string(config_path)?;
                    let mode = bitrouter_sdk::config::parse(&raw)?.policy.mode;
                    let digest = bitrouter::policy_lock::load(&policy_path).await?.digest;
                    events
                        .lock()
                        .map_err(|_| anyhow::anyhow!("recovery event lock poisoned"))?
                        .push("reload-parent".into());
                    *live
                        .lock()
                        .map_err(|_| anyhow::anyhow!("modeled daemon state lock poisoned"))? =
                        (mode, digest);
                    Ok(())
                }
            }
        };
        let policy_events = events.clone();
        let config_events = events.clone();

        let error = reload_optimization_publication_or_restore_with_mutations(
            &fixture.publication,
            reload,
            || Ok(()),
            move |path, expected, target, history| {
                bitrouter::policy_lock::rollback_to_digest_unlocked(
                    path, expected, target, history,
                )?;
                policy_events
                    .lock()
                    .map_err(|_| anyhow::anyhow!("recovery event lock poisoned"))?
                    .push("policy-parent".into());
                Ok(())
            },
            move |path, expected, updated| {
                bitrouter::policy_lock::write_text_atomic_unlocked(path, expected, updated)?;
                config_events
                    .lock()
                    .map_err(|_| anyhow::anyhow!("recovery event lock poisoned"))?
                    .push("config-parent-post-rename-error".into());
                anyhow::bail!("injected post-rename config failure")
            },
        )
        .await
        .err()
        .context("post-rename config failure unexpectedly succeeded")?;
        let detail = format!("{error:#}");

        assert!(detail.contains("injected daemon rejection of child policy"));
        assert!(detail.contains("injected post-rename config failure"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            *events
                .lock()
                .map_err(|_| anyhow::anyhow!("recovery event lock poisoned"))?,
            [
                "reload-child-rejected",
                "policy-parent",
                "config-parent-post-rename-error",
                "reload-parent",
            ]
        );
        assert_eq!(
            std::fs::read_to_string(&fixture.config_path)?,
            fixture.config_before
        );
        assert_eq!(
            bitrouter::policy_lock::load(&fixture.policy_path)
                .await?
                .digest,
            fixture.parent_digest
        );
        assert_eq!(
            *live
                .lock()
                .map_err(|_| anyhow::anyhow!("modeled daemon state lock poisoned"))?,
            (config::PolicyRuntimeMode::Frozen, fixture.parent_digest)
        );
        Ok(())
    }

    #[tokio::test]
    async fn optimizer_reload_recovery_never_overwrites_a_concurrent_policy_winner()
    -> anyhow::Result<()> {
        let fixture = recovery_fixture()?;
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let winner_digest = Arc::new(std::sync::Mutex::new(None::<String>));
        let live = Arc::new(std::sync::Mutex::new((
            config::PolicyRuntimeMode::Frozen,
            fixture.parent_digest.clone(),
        )));
        let reload = {
            let attempts = attempts.clone();
            move || {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    anyhow::bail!("injected daemon rejection before concurrent publication")
                }
            }
        };
        let before_recovery = {
            let policy_path = fixture.policy_path.clone();
            let child_digest = fixture.publication.active_policy_digest.clone();
            let mut winner = fixture.child_document.clone();
            let winner_digest = winner_digest.clone();
            let live = live.clone();
            move || {
                let artifact = winner
                    .artifact
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("winner artifact is missing"))?;
                artifact.parent_digest = Some(child_digest.clone());
                artifact.source_snapshot_time_unix_ms = 2;
                let record = bitrouter::policy_lock::publish_candidate(
                    &policy_path,
                    &child_digest,
                    &winner,
                    &bitrouter::policy_lock::default_history_dir(&policy_path),
                )?;
                *winner_digest
                    .lock()
                    .map_err(|_| anyhow::anyhow!("winner digest lock poisoned"))? =
                    Some(record.child_digest.clone());
                *live
                    .lock()
                    .map_err(|_| anyhow::anyhow!("modeled daemon state lock poisoned"))? =
                    (config::PolicyRuntimeMode::Adaptive, record.child_digest);
                Ok(())
            }
        };

        let error = reload_optimization_publication_or_restore_with(
            &fixture.publication,
            reload,
            before_recovery,
        )
        .await
        .err()
        .context("stale recovery unexpectedly succeeded")?;
        let winner_digest = winner_digest
            .lock()
            .map_err(|_| anyhow::anyhow!("winner digest lock poisoned"))?
            .clone()
            .context("concurrent winner was not published")?;

        assert!(error.to_string().contains("recovery is stale"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read_to_string(&fixture.config_path)?,
            fixture.config_after
        );
        assert_eq!(
            bitrouter::policy_lock::load(&fixture.policy_path)
                .await?
                .digest,
            winner_digest
        );
        assert_eq!(
            *live
                .lock()
                .map_err(|_| anyhow::anyhow!("modeled daemon state lock poisoned"))?,
            (config::PolicyRuntimeMode::Adaptive, winner_digest)
        );
        Ok(())
    }

    #[tokio::test]
    async fn optimizer_reload_recovery_reports_rejection_and_recovery_failures()
    -> anyhow::Result<()> {
        let fixture = recovery_fixture()?;
        let config_path = fixture.config_path.clone();

        let error = reload_optimization_publication_or_restore_with(
            &fixture.publication,
            || async { anyhow::bail!("injected daemon rejection for combined error") },
            move || {
                std::fs::remove_file(&config_path)?;
                Ok(())
            },
        )
        .await
        .err()
        .context("recovery with a missing config unexpectedly succeeded")?;
        let detail = format!("{error:#}");

        assert!(detail.contains("injected daemon rejection for combined error"));
        assert!(detail.contains("reading"));
        assert!(detail.contains("bitrouter.yaml"));
        Ok(())
    }

    #[test]
    fn workflow_state_reliability_report_flags_parse() {
        use clap::Parser;

        let cli = Cli::try_parse_from([
            "bitrouter",
            "workflow-state",
            "reliability-report",
            "--database-url",
            "sqlite:///tmp/bitrouter.db",
            "--config",
            "/tmp/bitrouter.yaml",
            "--output",
            "/tmp/reliability.json",
        ])
        .expect("parse reliability report");

        match cli.command {
            Some(Command::WorkflowState {
                action:
                    WorkflowStateAction::ReliabilityReport {
                        database_url,
                        config,
                        output,
                    },
            }) => {
                assert_eq!(database_url, "sqlite:///tmp/bitrouter.db");
                assert_eq!(config, PathBuf::from("/tmp/bitrouter.yaml"));
                assert_eq!(output, PathBuf::from("/tmp/reliability.json"));
            }
            _ => panic!("expected reliability report"),
        }
    }

    #[test]
    fn cloud_api_owns_header_short_flag_in_full_command_tree() {
        let cli = Cli::try_parse_from([
            "bitrouter",
            "cloud",
            "api",
            "/v1/models",
            "-H",
            "X-Test: value",
        ])
        .unwrap();

        match cli.command {
            Some(Command::Cloud {
                action: bitrouter::cloud::cli::CloudAction::Api(args),
            }) => assert_eq!(args.headers, ["X-Test: value"]),
            _ => panic!("expected cloud API command"),
        }

        let human = Cli::try_parse_from(["bitrouter", "-H", "cloud", "whoami"]).unwrap();
        assert!(human.human_short);
    }

    #[tokio::test]
    async fn config_validate_rejects_a_missing_bound_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bitrouter.yaml");
        tokio::fs::write(
            &path,
            r#"presets:
  coding:
    model: anthropic/claude-opus-4.8
    policy: missing
"#,
        )
        .await
        .unwrap();
        let source = bitrouter::paths::ConfigSource::File(path);

        let report = validate_config(&source).await.unwrap();

        assert!(!report.valid);
        assert!(report.errors[0].contains("policy lock"));
    }
}
