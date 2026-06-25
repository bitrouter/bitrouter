//! `bitrouter` CLI entry point — a thin shell over the `bitrouter` lib.
//!
//! Subcommand surface: `serve` / `start` / `stop` / `restart` /
//! `reload` / `status` / `route` / `init` / `key sign` / `models` / `tools` /
//! `policy create` / `providers (list|login|logout)` / `agents` /
//! `agent-proxy` / `spawn` / `cloud` / `skills`. Cloud-account sign-in lives under
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
use bitrouter::output::Output;
use bitrouter::output::reports::admin::{
    KeySignReport, PolicyCreateReport, ProviderLoginReport, ProviderLogoutReport,
};
use bitrouter::output::reports::agents::{
    AgentCheckRow, AgentInstallReport, AgentRow, AgentsCheckReport, AgentsListReport,
};
use bitrouter::output::reports::config::{InitReport, UnsetVar, ValidateReport};
use bitrouter::output::reports::daemon::{
    DaemonActionReport, RouteHopView, RouteReport, StatusReport,
};
use bitrouter::output::reports::observe::ObserveStatusReport;
use bitrouter::output::reports::routing::{ModelRow, ModelsReport, ProviderRow, ProvidersReport};
use bitrouter::output::reports::tools::{
    ServerStatusView, ServerToolsView, ToolInfo, ToolsDiscoverReport, ToolsListReport,
    ToolsStatusReport,
};
use bitrouter::output::reports::verify::{Check, VerifyReport};
use bitrouter_sdk::config;

/// BitRouter — an LLM API router.
#[derive(Parser)]
#[command(name = "bitrouter", version, about)]
struct Cli {
    /// Force JSON output (the default; agent-native). Conflicts with `--human`.
    #[arg(short = 'j', long, global = true, conflicts_with = "human")]
    json: bool,
    /// Render the human-readable view to stdout instead of JSON.
    #[arg(short = 'H', long, global = true)]
    human: bool,
    #[command(subcommand)]
    command: Command,
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
    Status {
        /// Path to `bitrouter.yaml` (used to locate the control socket).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit control socket path. Overrides the config-derived path.
        #[arg(long)]
        socket: Option<PathBuf>,
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
    /// Write a starter `bitrouter.yaml` (with `skip_auth: true`).
    Init {
        /// Path to write.
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
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
    /// Verify a confidential model's TEE attestation (L1): prove it runs on
    /// genuine Intel TDX + NVIDIA GPU hardware running the legitimate,
    /// policy-pinned model. Reads the DCAP policy pins + NVIDIA EAT key from the
    /// environment (see `bitrouter verify --help` notes / the skill).
    Verify {
        /// The confidential model id, e.g. `zai-org/GLM-5.1-FP8`.
        model: String,
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
    /// print install stubs. `bitrouter agent-proxy <id>` is the separate
    /// stdio bridge an editor spawns.
    Agents {
        #[command(subcommand)]
        action: AgentsAction,
    },
    /// Stdio bridge between an ACP-aware editor and a configured upstream
    /// agent. Routes inbound JSON-RPC requests through the `acp` pipeline,
    /// relays upstream notifications back to the editor.
    #[command(name = "agent-proxy")]
    AgentProxy {
        /// Agent id (must exist under `agents:` in the config).
        agent: String,
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Launch a coding-agent harness (Claude Code) as a child process with
    /// its API base URL pointed at the local BitRouter daemon — no agent
    /// config files are touched. Follows `cargo run`'s separator convention:
    /// bitrouter options come before `--`, everything after `--` is forwarded
    /// to the agent verbatim, e.g.
    /// `bitrouter spawn -a claude -- -p "summarize" --dangerously-skip-permissions`.
    ///
    /// The agent authenticates to BitRouter with `BITROUTER_API_KEY` when it is
    /// set; otherwise a local placeholder is used (fine under the `skip_auth`
    /// default written by `bitrouter init`). A missing agent binary is offered
    /// for install via its official native installer.
    Spawn {
        /// Which agent harness to launch.
        #[arg(short, long, value_enum)]
        agent: bitrouter::spawn::SpawnAgent,
        /// Path to `bitrouter.yaml` (used to derive the daemon base URL).
        /// When omitted, the binary resolves in this order: `./bitrouter.yaml`
        /// → `$BITROUTER_HOME/bitrouter.yaml` → `~/.bitrouter/bitrouter.yaml`
        /// → zero-config in-memory defaults.
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Override the agent's API base URL instead of deriving it from
        /// `server.listen` (e.g. when the daemon listens on a non-default
        /// address or a remote BitRouter).
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
        /// Arguments forwarded verbatim to the agent binary. Everything after
        /// `--` lands here.
        #[arg(last = true, allow_hyphen_values = true)]
        agent_args: Vec<String>,
    },
    /// Manage your BitRouter Cloud account — sign in/out, API keys, usage,
    /// billing, policies, BYOK, OAuth clients. Start with `cloud login`.
    Cloud {
        #[command(subcommand)]
        action: bitrouter::cloud::cli::CloudAction,
    },
    /// Install and manage Claude Code skills from GitHub, a git URL, or a
    /// BitRouter registry.
    Skills {
        #[command(subcommand)]
        action: bitrouter::skills::cli::SkillsAction,
    },
    /// Run or install BitRouter's origin MCP server.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
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

#[derive(Subcommand)]
enum McpAction {
    /// Serve the MCP server (stdio by default).
    Serve {
        /// `stdio` (local daemon) or `http` (cloud).
        #[arg(long, value_enum, default_value_t = McpTransport::Stdio)]
        transport: McpTransport,
        /// `local` or `cloud`. Defaults: stdio→local, http→cloud.
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
#[derive(Debug, Clone, Copy, ValueEnum)]
enum McpBackend {
    /// The local BYOK daemon at `127.0.0.1:4356`.
    Local,
    /// BitRouter Cloud at `api.bitrouter.ai`.
    Cloud,
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

impl From<McpBackend> for bitrouter_mcp::BackendKind {
    fn from(b: McpBackend) -> Self {
        match b {
            McpBackend::Local => bitrouter_mcp::BackendKind::Local,
            McpBackend::Cloud => bitrouter_mcp::BackendKind::Cloud,
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
    /// Show the bundled v1.0 catalog of well-known agents and which of
    /// them are present under `agents:` in the loaded config.
    List {
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
    /// Print a YAML stub for an agent in the catalog (paste under
    /// `agents:` in `bitrouter.yaml`).
    Install {
        /// Agent id from the catalog (see `bitrouter agents list`).
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
    /// Write a starter policy file to the policy dir.
    Create {
        /// Policy id (becomes the file stem and the `id:` field).
        id: String,
        /// Policy directory. Default matches the assembly default.
        #[arg(long, default_value = "./policies")]
        dir: PathBuf,
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
    /// Per-provider methods are auto-derived from the catalog: `anthropic`
    /// prompts for subscription (browser PKCE) or API-key paste;
    /// `openai-codex` runs the ChatGPT PKCE flow; `github-copilot` the GitHub
    /// device flow; everything else accepts a pasted API key. Logging in to
    /// the built-in `bitrouter` provider runs the same cloud sign-in as
    /// `bitrouter cloud login`.
    Login {
        /// Provider id (e.g. `anthropic`, `openai-codex`, `bitrouter`).
        provider: String,
        /// Account label this credential is stored under (default `default`).
        /// Ignored for the `bitrouter` provider (it uses the cloud credential).
        #[arg(short, long, default_value = "default")]
        label: String,
    },
    /// Log out of an upstream provider — clears every stored credential for
    /// it. For the built-in `bitrouter` provider this is `cloud logout`.
    Logout {
        /// Provider id whose stored credentials should be removed.
        provider: String,
    },
}

#[tokio::main]
async fn main() {
    // Parse once here so the global `--json` / `--human` flags are available to
    // render the *result* — a success report or the error envelope — through the
    // single `Output` driver. Diagnostics during execution go to stderr; the
    // result (this match) goes to stdout in the selected format, so
    // `bitrouter <cmd> 2>/dev/null | jq` always sees one clean JSON value.
    let cli = Cli::parse();
    let output = bitrouter::output::Output::from_flags(cli.json, cli.human);
    match run(cli, &output).await {
        Ok(()) => {}
        Err(e) => {
            let _ = output.emit(&bitrouter::output::error::envelope_from_anyhow(&e));
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
    if !matches!(cli.command, Command::Serve { .. }) {
        init_basic_tracing_subscriber();
    }

    match cli.command {
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
        Command::Status { config, socket } => {
            let socket = resolve_client_socket(config.as_deref(), socket.as_deref()).await?;
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
        Command::Init { config } => {
            output.emit(&init(&config).await?)?;
            Ok(())
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
        Command::Verify { model } => {
            output.emit(&verify_attestation(&model).await?)?;
            Ok(())
        }
        Command::Tools { action } => tools(action, output).await,
        Command::Observe { action } => observe(action, output).await,
        Command::Policy { action } => {
            output.emit(&policy(action).await?)?;
            Ok(())
        }
        Command::Providers { action } => providers(action, output).await,
        Command::Agents { action } => agents_cmd(action, output).await,
        Command::AgentProxy { agent, config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            agent_proxy_cmd(&agent, &source).await
        }
        Command::Spawn {
            agent,
            config,
            base_url,
            no_install,
            no_start,
            agent_args,
        } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            bitrouter::spawn::run(
                &source,
                &cfg,
                bitrouter::spawn::SpawnOptions {
                    agent,
                    agent_args,
                    base_url,
                    no_install,
                    no_start,
                },
            )
            .await
        }
        Command::Cloud { action } => bitrouter::cloud::cli::run(action, output.format()).await,
        Command::Skills { action } => bitrouter::skills::cli::run(action, output).await,
        Command::Mcp { action } => mcp_cmd(action).await,
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
                let log_path = resolve_log_path(source.home(), None);
                restart(&source, &socket, &log_path).await.map(|_| ())
            } else {
                Ok(())
            }
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

/// Validate a config file and print a short summary. Returns `Err` (→ exit 1)
/// on a malformed or unsafe config, so the command is CI-safe.
///
/// Validation runs the real parse path — deserialization, `${VAR}`
/// substitution, `derives` resolution, and the upstream-URL (SSRF) gate. It
/// does **not** load the JSON Schema (that artifact is for IDE autocomplete and
/// the `xtask` drift check); structural validation here is what `serde` +
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
        Ok(cfg) => Ok(ValidateReport::valid(
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
        Err(e) => Ok(ValidateReport::invalid(
            path.display().to_string(),
            e.to_string(),
        )),
    }
}

// ===== `bitrouter mcp …` (origin MCP server: serve / install) =====

async fn mcp_cmd(action: McpAction) -> Result<()> {
    match action {
        McpAction::Serve {
            transport,
            backend,
            local_url,
            cloud_url,
            token,
            bind,
        } => {
            let transport = bitrouter_mcp::Transport::from(transport);
            let backend = backend.map(Into::into).unwrap_or(match transport {
                bitrouter_mcp::Transport::Stdio => bitrouter_mcp::BackendKind::Local,
                bitrouter_mcp::Transport::Http => bitrouter_mcp::BackendKind::Cloud,
            });
            let cloud_token = token.or_else(|| std::env::var("BITROUTER_TOKEN").ok());
            if matches!(transport, bitrouter_mcp::Transport::Http) && cloud_token.is_some() {
                eprintln!(
                    "note: --token/BITROUTER_TOKEN is ignored for --transport http (multi-tenant; each client sends its own Authorization)"
                );
            }
            bitrouter_mcp::serve(bitrouter_mcp::ServeOptions {
                transport,
                backend,
                local_url,
                cloud_url,
                cloud_token,
                bind,
            })
            .await
        }
        McpAction::Install { client, config } => {
            bitrouter_mcp::install(bitrouter_mcp::InstallOptions {
                client: client.into(),
                config_path: config,
            })
        }
    }
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

/// Install a basic fmt-only tracing subscriber. Used for every command
/// except `serve` — see [`init_serve_tracing_subscriber`].
fn init_basic_tracing_subscriber() {
    tracing_subscriber::fmt()
        // Diagnostics MUST go to stderr so stdout stays a pure JSON result
        // surface (`tracing_subscriber::fmt()` otherwise defaults to stdout).
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

/// Install the full tracing subscriber for the `serve` command: fmt plus
/// — when OTel is configured — the bridge layer that mirrors `tracing`
/// spans into OTel via the supplied exporter's SDK tracer.
///
/// `tracing-opentelemetry`'s bridge layer captures its tracer eagerly,
/// so this MUST be called after [`bitrouter_observe::otel::OtelExporter::new`]
/// has built the real exporter; passing `None` (OTel disabled in config)
/// installs the fmt-only registry.
fn init_serve_tracing_subscriber(exporter: Option<&bitrouter_observe::otel::OtelExporter>) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer());
    match exporter {
        Some(exp) => registry
            .with(bitrouter_observe::otel::http_layer::tracing_subscriber_layer(exp))
            .init(),
        None => registry.init(),
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
    // Fetch + merge the provider registry (BYOK providers + the canonical model
    // catalog) before assembly, so the daemon routes every credentialed
    // provider's canonical models. Best-effort and cache-backed; a no-op when
    // disabled or unreachable with no cache.
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
    init_serve_tracing_subscriber(assembled.otel_exporter.as_deref());
    // Surface any deferred OTel-init failure now that the subscriber is up.
    if let Some(msg) = &assembled.otel_init_error {
        tracing::error!("{msg}");
    }
    let app = Arc::new(assembled.app);
    let policy_store = assembled.policy_store;
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
    let reloader: Arc<dyn daemon::DaemonReloader> = Arc::new(bitrouter::reload::AppReloader::new(
        policy_store.clone(),
        assembled.routing_table,
        reload_source,
    ));

    daemon::write_pid_file(&pid_path).await?;
    println!(
        "bitrouter {} — serving on {listen} (control: {})",
        bitrouter::VERSION,
        socket_path.display()
    );

    let http_app = app.clone();
    let http_listen = listen.clone();
    let http = async move {
        // Wrap the SDK router in tower-http's TraceLayer (plus inbound W3C
        // trace-context propagation) so the inbound HTTP request becomes
        // the SERVER span parent of the bitrouter `chat` INTERNAL span.
        http_app
            .serve_with_router_wrapper(
                &http_listen,
                bitrouter_observe::otel::http_layer::router_wrapper(),
            )
            .await
            .map_err(anyhow::Error::from)
    };
    let control = daemon::run_control_socket(
        socket_path,
        app.clone(),
        listen,
        reloader.clone(),
        observe_provider,
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

    let result = tokio::select! {
        r = http => r,
        r = control => r,
        // HUP loop never returns Ok by design (Unix); an error from signal
        // setup is logged and we keep serving. On Windows this arm is pending.
        r = hup => match r {
            Ok(()) => Ok(()),
            Err(e) => { tracing::warn!(error = %e, "SIGHUP listener unavailable"); Ok(()) }
        },
        r = term => match r {
            Ok(()) => Ok(()),
            Err(e) => { tracing::warn!(error = %e, "termination-signal listener unavailable"); Ok(()) }
        },
    };

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
    eprintln!("       bitrouter providers login anthropic       # Claude Pro/Max subscription");
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

async fn restart(
    source: &bitrouter::paths::ConfigSource,
    socket: &Path,
    log_path: &Path,
) -> Result<DaemonActionReport> {
    // Stop is best-effort — a missing daemon is fine, we just go straight to
    // start. Any other error from the running daemon is fatal. `endpoint_in_use`
    // abstracts "is a daemon bound here?" across the Unix socket file and the
    // Windows named pipe.
    if daemon::endpoint_in_use(socket) {
        match daemon::send_command(socket, &DaemonCommand::Stop).await {
            Ok(DaemonResponse::Ok) => {}
            Ok(DaemonResponse::Error { message }) => return Err(anyhow::anyhow!(message)),
            Ok(other) => return Err(anyhow::anyhow!("unexpected response: {other:?}")),
            Err(e) => tracing::warn!(error = %e, "stop failed — proceeding to start"),
        }
        //.2 allows in-flight requests up to 30s to drain. Wait that
        // long for the endpoint to be released. If it still isn't, escalate to
        // a forced kill of the recorded pid — otherwise `start` would race the
        // old process for the same endpoint and one of them would die silently.
        let pid_path = pid_path_for(socket);
        if !wait_for_socket_release(socket, std::time::Duration::from_secs(30)).await {
            tracing::warn!("endpoint still held after 30s — escalating to force-kill on pid file");
            if let Some(pid) = daemon::read_pid_file(&pid_path).await {
                force_kill(pid).await;
            }
            // One more brief wait so the OS cleans up the endpoint.
            wait_for_socket_release(socket, std::time::Duration::from_secs(2)).await;
            // The killed daemon never removed its pid file; do it now.
            daemon::remove_pid_file(&pid_path).await;
        }
    }
    start(source, log_path, "restart").await
}

/// Poll until the control endpoint is released (the old daemon drops the Unix
/// socket file / closes the last named-pipe instance on exit), up to
/// `timeout`. Returns true on success, false on timeout.
async fn wait_for_socket_release(socket: &Path, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !daemon::endpoint_in_use(socket) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    !daemon::endpoint_in_use(socket)
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

/// `bitrouter verify <model>` — run an L1 TEE-attestation check for a
/// confidential model and print the per-check breakdown.
///
/// Configuration comes from the environment (the DCAP policy MUST be pinned —
/// the verifier refuses to run unpinned, per the load-bearing trust anchor):
/// - `NEAR_BASE` (default `https://cloud-api.near.ai/v1`)
/// - `NEAR_KMS_ROOTS` — accepted dstack KMS root pubkey(s), comma-separated
/// - `NEAR_IMAGE_DIGESTS` and/or `NEAR_WORKLOAD_IDS` — at least one required
/// - `NEAR_BASE_MEASUREMENTS` — accepted base bundle(s), comma-separated; each
///   the hex of `MRTD ‖ RTMR0 ‖ RTMR1 ‖ RTMR2` (192 bytes). Required (issue #567)
/// - `NVIDIA_EAT_KEY_PEM` — path to NVIDIA's NRAS EAT key (EC public PEM)
async fn verify_attestation(model: &str) -> Result<VerifyReport> {
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use bitrouter_attestation::{
        AciDcapVerifierPolicy, ConfidentialVerifier, DcapQuoteVerifier, NVIDIA_NRAS_JWKS_URL,
        NearVerifier, NvidiaEatKey, ReportTransport, ReqwestTransport,
    };

    fn env_list(key: &str) -> Vec<String> {
        std::env::var(key)
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    let base =
        std::env::var("NEAR_BASE").unwrap_or_else(|_| "https://cloud-api.near.ai/v1".to_string());

    let policy = AciDcapVerifierPolicy::new(
        env_list("NEAR_WORKLOAD_IDS"),
        env_list("NEAR_IMAGE_DIGESTS"),
        env_list("NEAR_KMS_ROOTS"),
        env_list("NEAR_BASE_MEASUREMENTS"),
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "attestation policy not pinned ({e}); set NEAR_KMS_ROOTS, \
             NEAR_IMAGE_DIGESTS/NEAR_WORKLOAD_IDS and NEAR_BASE_MEASUREMENTS"
        )
    })?
    // TCB floor: require an up-to-date platform by default. Operators may
    // accept specific Intel advisories (out-of-date microcode) via this env.
    .with_allowed_tcb_advisory_ids(env_list("NEAR_TCB_ALLOWED_ADVISORIES"));

    // GPU EAT key: an explicit PEM override wins; otherwise fetch NVIDIA's JWKS
    // (its signing keys rotate, so we resolve per-request by the EAT `kid`).
    let nvidia_key = match std::env::var("NVIDIA_EAT_KEY_PEM") {
        Ok(path) => NvidiaEatKey::from_ec_pem(&std::fs::read(&path)?)
            .map_err(|e| anyhow::anyhow!("invalid NVIDIA_EAT_KEY_PEM ({path}): {e}"))?,
        Err(_) => {
            let url = std::env::var("NVIDIA_JWKS_URL")
                .unwrap_or_else(|_| NVIDIA_NRAS_JWKS_URL.to_string());
            match NvidiaEatKey::fetch_jwks(&url).await {
                Ok(key) => key,
                Err(e) => {
                    bitrouter::error_report::info(format!(
                        "could not fetch NVIDIA JWKS ({e}); the GPU check will fail closed"
                    ));
                    NvidiaEatKey::unconfigured()
                }
            }
        }
    };

    let verifier = NearVerifier::new(
        Arc::new(ReqwestTransport::new(&base)) as Arc<dyn ReportTransport>,
        Arc::new(DcapQuoteVerifier::default()),
        Arc::new(policy),
        Arc::new(nvidia_key),
    );

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let verdict = verifier.attestation_cached(model, now).await?;

    let c = &verdict.checks;
    let st = |b: bool| if b { "pass" } else { "fail" };
    let opt_st = |v: Option<bool>| match v {
        Some(true) => "pass",
        Some(false) => "fail",
        None => "skip",
    };
    let checks = vec![
        Check {
            name: "GPU NRAS attestation".into(),
            status: st(c.gpu_nras_pass),
        },
        Check {
            name: "Intel TDX DCAP quote".into(),
            status: st(c.dcap_quote_valid),
        },
        Check {
            name: "report_data binds key + nonce".into(),
            status: st(c.report_data_binds_key_and_nonce),
        },
        Check {
            name: "compose matches measured config".into(),
            status: st(c.compose_matches_mr_config),
        },
        Check {
            name: "event-log RTMR3 anchors policy fields".into(),
            status: opt_st(c.event_log_rtmr_ok),
        },
        Check {
            name: "base measurements match pin (MRTD/RTMR0-2)".into(),
            status: st(c.base_measurements_match),
        },
        Check {
            name: "policy accepts (KMS root + image/workload pin)".into(),
            status: st(c.policy_accepts),
        },
        Check {
            name: "TD debug disabled".into(),
            status: st(c.debug_disabled),
        },
        Check {
            name: match &c.tcb_status {
                Some(s) => format!("TCB level acceptable ({s})"),
                None => "TCB level acceptable".into(),
            },
            status: st(c.tcb_level_acceptable),
        },
    ];
    Ok(VerifyReport {
        model: model.to_string(),
        trust_boundary: verdict.trust_boundary,
        verified: verdict.verified,
        checks,
        signers: verdict.attested_addresses,
    })
}

// ===== management commands =====

async fn init(config_path: &Path) -> Result<InitReport> {
    commands::init(config_path).await?;
    Ok(InitReport {
        action: "init",
        path: config_path.display().to_string(),
        skip_auth: true,
    })
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

async fn policy(action: PolicyAction) -> Result<PolicyCreateReport> {
    match action {
        PolicyAction::Create { id, dir } => {
            let path = commands::create_policy(&dir, &id).await?;
            Ok(PolicyCreateReport {
                id,
                path: path.display().to_string(),
                created: true,
            })
        }
    }
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
        ProviderAction::Login { provider, label } => {
            // The built-in `bitrouter` provider authenticates with the cloud
            // OAuth credential, so logging into it IS the cloud sign-in
            // (`cloud login`); other providers use the per-provider store.
            if provider == "bitrouter" {
                bitrouter::cloud::cli::run(
                    bitrouter::cloud::cli::CloudAction::Login {
                        authorization_server: None,
                        client_id: None,
                        scope: None,
                    },
                    output.format(),
                )
                .await
            } else {
                let outcome = bitrouter::commands::login_provider(&provider, &label).await?;
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

async fn agent_proxy_cmd(agent: &str, source: &bitrouter::paths::ConfigSource) -> Result<()> {
    let cfg = bitrouter::paths::load_config(source).await?;
    bitrouter::agent_proxy::run(cfg, agent).await
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
        AgentsAction::List { config } => {
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
            output.emit(&AgentsListReport { agents })?;
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
            Err(e) => anyhow::bail!(e),
        },
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

/// Forcibly terminate `pid`. Best-effort — if the process is already gone we
/// silently move on. On Unix this is SIGKILL (the kernel returns ESRCH for a
/// dead pid); on Windows it's `taskkill /F`.
#[cfg(unix)]
async fn force_kill(pid: u32) {
    let _ = tokio::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
}

/// Forcibly terminate `pid` on Windows via `taskkill /F`. Best-effort.
#[cfg(windows)]
async fn force_kill(pid: u32) {
    let _ = tokio::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        // Panics if clap detects a conflict (e.g. `--tag` vs global `--version`).
        Cli::command().debug_assert();
    }

    #[test]
    fn update_flags_parse() {
        use clap::Parser;
        let cli =
            Cli::try_parse_from(["bitrouter", "update", "--check", "--tag", "1.0.0-alpha.18"])
                .expect("parse");
        match cli.command {
            Command::Update {
                check,
                tag,
                stable,
                restart,
                yes,
            } => {
                assert!(check);
                assert_eq!(tag.as_deref(), Some("1.0.0-alpha.18"));
                assert!(!stable && !restart && !yes);
            }
            _ => panic!("expected Update"),
        }
    }
}
