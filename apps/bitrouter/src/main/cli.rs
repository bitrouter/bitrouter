//! BitRouter gateway and operator command-line interface.

use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Arc;

#[path = "shared_operator.rs"]
mod shared_operator;
#[cfg(test)]
#[path = "shared_tests.rs"]
mod shared_tests;
#[path = "../commands/tenancy.rs"]
mod tenancy;
use crate::tracing_filter::resolve_env_filter;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

use crate::administration_target::{InspectionTarget, ReloadSubmission};
use crate::commands;

use crate::config;
use crate::daemon::{self, DaemonCommand, DaemonResponse, process_is_alive};
use crate::output::reports::admin::{
    KeySignReport, PolicyCreateReport, ProviderLoginReport, ProviderLogoutReport,
};

use crate::output::reports::config::{UnsetVar, ValidateReport};
use crate::output::reports::daemon::DaemonActionReport;
use crate::output::reports::eval::EvalReport;
#[cfg(feature = "mcp")]
use crate::output::reports::mcp::{McpCheckReport, McpCheckRow};
use crate::output::reports::mcp::{McpRegistryReport, McpRegistryRow};
use crate::output::reports::observe::ObserveStatusReport;
use crate::output::reports::policy::PolicyReport;
use crate::output::reports::routing::{ProviderRow, ProvidersReport};
#[cfg(feature = "mcp")]
use crate::output::reports::tools::{
    ServerStatusView, ServerToolsView, ToolInfo, ToolsDiscoverReport, ToolsListReport,
    ToolsStatusReport,
};
use crate::output::reports::trajectory::{
    TrajectoryPruneReport, inspect_report as trajectory_inspect_report,
    replay_report as trajectory_replay_report,
};
use crate::output::{CliReport, Output};
use bitrouter_ai::control::reload::{OperationReport, OperationStatus};
use bitrouter_ai::control::requests::RequestFilters;
use bitrouter_ai::control::route::RouteInput;
use {
    bitrouter_ai::control::administration::PolicyInput,
    bitrouter_ai::control::administration::PolicyView,
};

/// BitRouter — an LLM API router.
#[derive(Parser)]
#[command(
    name = crate::commands::invocation::DEFAULT,
    version,
    about,
    after_help = "Global compatibility options (place before COMMAND): --json, --human, --context <NAME>"
)]
struct Cli {
    /// Force JSON output (the default; agent-native). Conflicts with `--human`.
    #[arg(
        short = 'j',
        long,
        global = true,
        hide = true,
        conflicts_with = "human"
    )]
    json: bool,
    /// Render the human-readable view to stdout instead of JSON.
    #[arg(long, global = true, hide = true)]
    human: bool,
    /// Compatibility spelling for `--human` when placed before the subcommand.
    #[arg(short = 'H', hide = true, conflicts_with = "json")]
    human_short: bool,
    /// Run a supported administration action against a named remote context. The
    /// built-in `local` context keeps the normal local behavior.
    #[arg(long, global = true, hide = true, value_name = "NAME")]
    context: Option<String>,
    /// No subcommand dispatches to the onboarding entry (`onboarding::entry`):
    /// first-run onboarding, otherwise ensure a ready background daemon.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum AfterSetup {
    Serve,
    Exit,
}

impl From<AfterSetup> for crate::onboarding::AfterAction {
    fn from(action: AfterSetup) -> Self {
        match action {
            AfterSetup::Serve => Self::Serve,
            AfterSetup::Exit => Self::Exit,
        }
    }
}

#[derive(Subcommand)]
enum SharedAction {
    #[command(flatten)]
    Operator(shared_operator::Action),
    /// Atomically seed a new shared namespace; requires multi-instance.
    Init {
        /// Explicit shared process configuration; no discovery fallback.
        #[arg(long)]
        config: PathBuf,
        /// Bounded JSON seed plan, or '-' to read stdin.
        #[arg(long)]
        plan: PathBuf,
        /// Stable audit identity for this explicitly privileged local operation.
        #[arg(long)]
        operator_id: String,
        /// Caller-owned ID; retain this and the identical plan after uncertainty.
        #[arg(long)]
        operation_id: String,
        /// Permit creating database-wide virtual principals from the seed plan.
        #[arg(long)]
        global_principals: bool,
    },
}

#[derive(Subcommand)]
enum Command {
    /// Report this executable's compiled features, profile and source identity.
    BuildInfo,
    /// Explicit shared-gateway initialization and administration.
    Shared {
        #[command(subcommand)]
        action: SharedAction,
    },
    /// Manage tenant configuration over explicit HTTP or inspect local DB evidence.
    Tenancy {
        #[command(subcommand)]
        action: tenancy::Action,
    },
    /// Load a config, run migrations, and serve HTTP + control socket
    /// **in the foreground**.
    Serve {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Set only by the `start` launcher for an owned detached daemon.
        #[arg(long, hide = true)]
        managed_child: bool,
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
    /// Inspect a retained remote administration operation.
    Operations {
        #[command(subcommand)]
        action: OperationsAction,
    },
    /// Report a running daemon's status (pid, listen address, model count).
    /// Prints `running: no` when no daemon is reachable.
    ///
    /// With `--requests`, reports what the router has actually done instead:
    /// settled requests (time, model, the provider that actually served,
    /// tokens, cost, latency, status) plus daemon state and a spend rollup
    /// scoped to every caller. Read straight from the metering store, so it
    /// works with no daemon running. JSON by default like every other command;
    /// `--human` renders the table.
    Status {
        /// Path to `bitrouter.yaml` (used to locate the control socket).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit control socket path. Overrides the config-derived path.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Report settled requests and the spend rollup instead of one
        /// status line.
        #[arg(short, long, hide = true)]
        requests: bool,
    },
    /// Show recent settled requests and aggregate spend.
    Requests {
        /// Maximum number of recent requests to return.
        #[arg(long, default_value_t = bitrouter_ai::control::requests::MAX_REQUEST_ROWS)]
        limit: u64,
        /// Inclusive RFC3339 lower bound. Requires `--until`.
        #[arg(long)]
        since: Option<String>,
        /// Exclusive RFC3339 upper bound. Requires `--since`.
        #[arg(long)]
        until: Option<String>,
        /// Keep only requests resolved to this model id.
        #[arg(long)]
        model: Option<String>,
        /// Keep only requests served by this provider id.
        #[arg(long)]
        provider: Option<String>,
        /// Path to `bitrouter.yaml` (used to locate the local control socket).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit local control socket path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Resolve a model name through the routing table. Uses the running
    /// daemon if reachable, otherwise loads the config — policy table included
    /// — and resolves locally. Read-only: nothing is sent upstream.
    Route {
        /// The model name to resolve.
        model: String,
        /// Prompt text to resolve *for*. The policy table keys on the
        /// agent-loop step a request represents, so the model it selects can
        /// differ with the prompt; omit for a bare model resolution.
        #[arg(long)]
        prompt: Option<String>,
        /// Path to `bitrouter.yaml` (used as the standalone fallback and
        /// to locate the control socket).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit control socket path. Overrides the config-derived path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Configure gateway credentials and write a starter configuration.
    Init {
        /// Configuration to create/update; defaults to the resolved config or BitRouter home.
        #[arg(short, long)]
        config: Option<PathBuf>,
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
        #[command(flatten)]
        resources: crate::onboarding::resources::Options,
        /// Finish by starting the gateway or returning to the caller.
        #[arg(long, value_enum)]
        after: Option<AfterSetup>,
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
    /// Manage named remote-control targets. Contexts store a token environment
    /// variable name, never the token value.
    Context {
        #[command(subcommand)]
        action: ContextAction,
    },
    /// MCP server introspection — list/status/discover against the upstreams
    /// declared under `mcp_servers` in `bitrouter.yaml`. v1.0 does not maintain
    /// a global tool registry; these are one-shot queries.
    #[command(hide = true)]
    Tools {
        #[command(subcommand)]
        action: ToolsAction,
    },
    /// Observability inspection (OTel exporter state, cardinality usage).
    Observe {
        #[command(subcommand)]
        action: ObserveAction,
    },
    /// Access-control policy management for virtual keys.
    AccessPolicy {
        #[command(subcommand)]
        action: AccessPolicyAction,
    },
    /// Model routing policy management.
    Policy {
        #[command(subcommand)]
        action: PolicyAction,
    },
    /// Evaluator-neutral evidence exchange.
    Eval {
        #[command(subcommand)]
        action: EvalAction,
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
    /// Show connected agent instances; subcommands inspect catalog and run history.
    Agents {
        #[command(subcommand)]
        action: AgentsAction,
    },
    /// Manage your BitRouter Cloud account — sign in/out, namespaces, keys,
    /// usage, requests, billing, policies, budgets, presets, and BYOK. Start
    /// with `cloud login`.
    Cloud {
        #[command(subcommand)]
        action: crate::cloud::cli::CloudAction,
    },
    /// Manage upstream MCP configuration and explicitly check connections.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// Update the installed `bitrouter` binary in place to the latest release.
    /// Includes prereleases by default. Package-manager installs report an
    /// upgrade command for the owning package.
    Update {
        /// Report whether a newer version exists, then exit without changing
        /// anything.
        #[arg(long)]
        check: bool,
        /// Select an exact split-release tag, including for rollback.
        #[arg(long)]
        tag: Option<String>,
        /// Only consider stable (non-prerelease) releases.
        #[arg(long)]
        stable: bool,
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ContextAction {
    /// Add a named remote target.
    Add {
        /// Context name used by `--context`.
        name: String,
        /// HTTPS origin or endpoint ending in `/control/v1`. Plain HTTP is
        /// accepted only for loopback/SSH-forwarded endpoints.
        #[arg(long)]
        endpoint: String,
        /// Environment variable containing this context's bearer token.
        #[arg(long, value_name = "NAME")]
        token_env: String,
    },
    /// List configured remote targets.
    List,
    /// Show one remote target without reading its token.
    Show { name: String },
    /// Remove one remote target.
    Remove { name: String },
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
    /// Connect, negotiate MCP capabilities, and list advertised tools.
    Check {
        /// Check only this configured server; omit to check all of them.
        server: Option<String>,
        /// Path to `bitrouter.yaml`.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Search the official MCP registry (registry.modelcontextprotocol.io)
    /// for upstream MCP servers. Rows carry an install-support column:
    /// `remote` (zero-install), `npx`/`uvx` (stub-able), `manual`
    /// (other package types).
    #[command(hide = true)]
    Search {
        /// Search text (matched server-side against registry names).
        query: String,
        /// Maximum rows to print.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    #[command(flatten)]
    Config(crate::mcp_config::Command),
}

#[derive(Subcommand)]
enum AgentsAction {
    /// Inspect the agent catalog advertised through the service control API.
    List {
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(long)]
        socket: Option<PathBuf>,
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
enum OperationsAction {
    /// Show one retained reload operation owned by this credential.
    Show {
        /// Reload request UUID returned by `bitrouter reload`.
        request_id: String,
        /// Daemon boot instance UUID returned with the operation.
        #[arg(long)]
        instance: String,
    },
}

#[derive(Subcommand)]
enum AccessPolicyAction {
    /// Write a starter access-control policy for virtual-key authorization.
    Create {
        /// Policy id used as its file stem and `id` field.
        id: String,
        #[arg(long, default_value = "./policies")]
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum PolicyAction {
    /// Create model-only policy source and bind it to a named router.
    Init {
        name: String,
        /// Router addressed as `bitrouter/<id>`; defaults to `coding`.
        #[arg(long, default_value = "coding")]
        router: String,
        /// Strong base model; inferred from an existing router when omitted.
        #[arg(long)]
        strong: Option<String>,
        #[arg(long, requires = "strong")]
        strong_effort: Option<bitrouter_ai::types::ReasoningEffort>,
        #[arg(long)]
        economy: String,
        #[arg(long)]
        economy_effort: Option<bitrouter_ai::types::ReasoningEffort>,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Validate configuration and its model-only policy source on disk.
    Check {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Inspect the selected instance's active policy; use --disk offline.
    Status {
        #[arg(long, value_enum, conflicts_with = "disk")]
        view: Option<PolicyViewArg>,
        #[arg(long)]
        disk: bool,
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Export complete resubmittable source after checking an optional name.
    Show {
        name: Option<String>,
        #[arg(long, value_enum, conflicts_with = "disk")]
        view: Option<PolicyViewArg>,
        #[arg(long)]
        disk: bool,
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Atomically reload the selected daemon's saved configuration and source.
    Reload {
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Compare two current policy source documents.
    Diff { active: PathBuf, candidate: PathBuf },
    /// Publish model-only source using an explicit compare-and-swap digest.
    Publish {
        candidate: PathBuf,
        #[arg(long, value_name = "SHA256")]
        expected_digest: String,
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Reactivate a current-format history source using compare-and-swap.
    Rollback {
        digest: String,
        #[arg(long, value_name = "SHA256")]
        expected_digest: String,
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

/// CLI spelling for the typed policy source.  The action contract itself stays
/// free of clap concerns in `actions::administration`.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum PolicyViewArg {
    Active,
    Disk,
}

impl From<PolicyViewArg> for PolicyView {
    fn from(view: PolicyViewArg) -> Self {
        match view {
            PolicyViewArg::Active => Self::Active,
            PolicyViewArg::Disk => Self::Disk,
        }
    }
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
        /// Explicit local control socket for the daemon's accepted provider catalog.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Check one configured provider's endpoint and model list.
    ///
    /// Requests the provider's model list with its protocol, configured API key
    /// or static headers, and timeouts. The check reads configuration and
    /// reports the model IDs it sees.
    Check {
        /// Provider id from `providers:` in `bitrouter.yaml`.
        provider: String,
        /// Account label when the provider has more than one credential.
        /// Omit it to check every account.
        #[arg(long)]
        account: Option<String>,
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults.
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

const CLI_MAIN_STACK_SIZE: usize = 8 * 1024 * 1024;

/// Render runnable command examples with the executable name used by the operator.
fn command_for_invocation(invoked: &'static str) -> clap::Command {
    Cli::command().name(invoked).bin_name(invoked)
}

/// Parse the process arguments after applying the invocation-derived help.
fn parse_cli(invoked: &'static str) -> Cli {
    use clap::FromArgMatches;

    let matches = command_for_invocation(invoked).get_matches();
    match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    }
}

pub(crate) fn entry() {
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
    let invoked = crate::commands::invocation::record_argv0();
    let cli = parse_cli(invoked);
    let raw_cloud_api = matches!(
        &cli.command,
        Some(Command::Cloud {
            action: crate::cloud::cli::CloudAction::Api(_)
        })
    );
    let output = crate::output::Output::from_flags(cli.json, cli.human || cli.human_short);
    // Keep the large dispatch future on the heap, reserving the dedicated
    // stack for synchronous call chains such as TLS on the cloud path.
    match Box::pin(run(cli, &output)).await {
        Ok(()) => {}
        Err(e) => {
            // The raw cloud API reserves stdout for the response body. Its
            // errors go to stderr; other commands emit the selected envelope.
            if raw_cloud_api {
                eprintln!("error: {e:#}");
            } else {
                emit_command_error(&e, &output);
            }
            std::process::exit(1);
        }
    }
}

fn emit_command_error(error: &anyhow::Error, output: &Output) {
    if let Some(incomplete) = error.downcast_ref::<crate::onboarding::resources::Incomplete>() {
        let _ = output.emit(&incomplete.report);
        eprintln!("error: {incomplete}");
        return;
    }

    let _ = output.emit(&crate::output::error::envelope_from_anyhow(error));
}

async fn run(cli: Cli, output: &crate::output::Output) -> Result<()> {
    #[cfg(not(feature = "multi-instance"))]
    if matches!(&cli.command, Some(Command::Shared { .. })) {
        anyhow::bail!("shared administration requires the multi-instance feature");
    }
    if matches!(&cli.command, Some(Command::Shared { .. })) {
        anyhow::ensure!(
            cli.context.is_none(),
            "shared administration requires explicit targets and does not use named contexts"
        );
    }
    #[cfg(not(feature = "mcp"))]
    if matches!(
        &cli.command,
        Some(
            Command::Tools { .. }
                | Command::Mcp {
                    action: McpAction::Check { .. }
                }
        )
    ) {
        anyhow::bail!("MCP diagnostics require the mcp feature");
    }
    if !defers_tracing_to_host(cli.command.as_ref()) {
        init_basic_tracing_subscriber();
    }

    let Some(command) = cli.command else {
        if cli.context.as_deref().is_some_and(|name| name != "local") {
            return Err(crate::gateway::error::BitrouterError::bad_request(
                "a remote context requires an explicit administration action",
            )
            .into());
        }
        // Bare `bitrouter` — the onboarding front door (wizard when
        // unconfigured; background ensure-start when configured).
        return crate::onboarding::entry(output).await;
    };

    if matches!(&command, Command::BuildInfo)
        && cli.context.as_deref().is_some_and(|name| name != "local")
    {
        return Err(crate::gateway::error::BitrouterError::bad_request(
            "build-info describes the local executable; use remote status for daemon identity",
        )
        .into());
    }
    let remote_context = if matches!(&command, Command::Shared { .. } | Command::Tenancy { .. }) {
        None
    } else if matches!(&command, Command::Context { .. }) {
        if cli.context.as_deref().is_some_and(|name| name != "local") {
            return Err(crate::gateway::error::BitrouterError::bad_request(format!(
                "`{} context` manages targets and does not run against a remote context",
                crate::commands::invocation::name()
            ))
            .into());
        }
        None
    } else {
        cli.context
            .as_deref()
            .map(crate::contexts::resolve)
            .transpose()?
            .flatten()
    };
    validate_remote_invocation(&command, remote_context.is_some())?;

    match command {
        Command::Shared { action } => run_shared(action, output).await,
        Command::Tenancy { action } => tenancy::run(&action, cli.context.as_deref(), output).await,
        Command::BuildInfo => {
            output.emit(&crate::output::reports::artifact::BuildInfo {
                product: env!("CARGO_PKG_NAME"),
                artifact: crate::artifact::current(),
            })?;
            Ok(())
        }
        Command::Serve {
            config,
            managed_child,
        } => {
            let source = crate::paths::resolve_config(config.as_deref())?;
            crate::host::serve_cli(&source, managed_child).await
        }
        Command::Start { config, log } => {
            let source = crate::paths::resolve_config(config.as_deref())?;
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
            let source = crate::paths::resolve_config(config.as_deref())?;
            crate::host::validate_local_daemon_source(&source).await?;
            let socket = resolve_client_socket_from(&source, socket.as_deref()).await?;
            let log_path = resolve_log_path(source.home(), log.as_deref());
            output.emit(&restart(&source, &socket, &log_path).await?)?;
            Ok(())
        }
        Command::Reload { config, socket } => {
            if remote_context.is_some() {
                let target = inspection_target(
                    remote_context.as_ref(),
                    cli.context.as_deref(),
                    config.as_deref(),
                    socket.as_deref(),
                )
                .await?;
                let ReloadSubmission::Remote(report) = target.reload().await? else {
                    anyhow::bail!("remote reload resolved to a local control target")
                };
                return emit_operation_result(output, &report);
            }
            let socket = resolve_client_socket(config.as_deref(), socket.as_deref()).await?;
            output.emit(&reload(&socket).await?)?;
            Ok(())
        }
        Command::Operations { action } => {
            let remote = remote_context.as_ref().ok_or_else(|| {
                crate::gateway::error::BitrouterError::bad_request(
                    "`operations show` requires a named remote context",
                )
            })?;
            let target =
                inspection_target(Some(remote), cli.context.as_deref(), None, None).await?;
            match action {
                OperationsAction::Show {
                    request_id,
                    instance,
                } => {
                    let report = target.operation(&request_id, &instance).await?;
                    emit_operation_result(output, &report)
                }
            }
        }
        Command::Status {
            config,
            socket,
            requests,
        } => {
            if requests {
                eprintln!(
                    "note: `{cli} status --requests` is a compatibility form; use \
                     `{cli} requests`.",
                    cli = crate::commands::invocation::name()
                );
            }
            let target = inspection_target(
                remote_context.as_ref(),
                cli.context.as_deref(),
                config.as_deref(),
                socket.as_deref(),
            )
            .await?;
            if requests {
                output.emit(&target.requests(RequestFilters::default()).await?)?;
            } else {
                output.emit(&target.status().await?)?;
            }
            Ok(())
        }
        Command::Requests {
            limit,
            since,
            until,
            model,
            provider,
            config,
            socket,
        } => {
            let target = inspection_target(
                remote_context.as_ref(),
                cli.context.as_deref(),
                config.as_deref(),
                socket.as_deref(),
            )
            .await?;
            output.emit(
                &target
                    .requests(RequestFilters {
                        limit,
                        since,
                        until,
                        model,
                        provider,
                    })
                    .await?,
            )?;
            Ok(())
        }
        Command::Route {
            model,
            prompt,
            config,
            socket,
        } => {
            let target = inspection_target(
                remote_context.as_ref(),
                cli.context.as_deref(),
                config.as_deref(),
                socket.as_deref(),
            )
            .await?;
            output.emit(&target.route(RouteInput { model, prompt }).await?)?;
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
            resources,
            after,
        } => {
            let flags = crate::onboarding::OnboardingFlags {
                config,
                yes,
                force,
                reset,
                cloud_login,
                api_key,
                providers,
                provider_api_keys,
                use_detected,
                resources,
                after: after.map(Into::into),
            };
            crate::onboarding::run(flags, output).await
        }
        Command::Config { action } => config_cmd(action, output).await,
        Command::Key { action } => {
            output.emit(&key(action).await?)?;
            Ok(())
        }
        Command::Models { config, provider } => {
            let target = inspection_target(
                remote_context.as_ref(),
                cli.context.as_deref(),
                config.as_deref(),
                None,
            )
            .await?;
            output.emit(&target.models(provider.as_deref()).await?)?;
            Ok(())
        }
        Command::Context { action } => {
            match action {
                ContextAction::Add {
                    name,
                    endpoint,
                    token_env,
                } => output.emit(&crate::contexts::add(&name, &endpoint, &token_env)?)?,
                ContextAction::List => output.emit(&crate::contexts::list()?)?,
                ContextAction::Show { name } => output.emit(&crate::contexts::show(&name)?)?,
                ContextAction::Remove { name } => output.emit(&crate::contexts::remove(&name)?)?,
            }
            Ok(())
        }
        Command::Tools { action } => tools(action, output).await,
        Command::Observe { action } => {
            administration_observe(
                action,
                output,
                remote_context.as_ref(),
                cli.context.as_deref(),
            )
            .await
        }
        Command::AccessPolicy { action } => {
            let AccessPolicyAction::Create { id, dir } = action;
            let path = commands::create_policy(&dir, &id).await?;
            output.emit(&PolicyCreateReport {
                id,
                path: path.display().to_string(),
                created: true,
            })?;
            Ok(())
        }
        Command::Policy { action } => {
            administration_policy(
                action,
                output,
                remote_context.as_ref(),
                cli.context.as_deref(),
            )
            .await
        }
        Command::Eval { action } => eval(action, output).await,
        Command::Trajectory { config, action } => {
            trajectory(config.as_deref(), action, output).await
        }
        Command::Providers { action } => {
            administration_providers(
                action,
                output,
                remote_context.as_ref(),
                cli.context.as_deref(),
            )
            .await
        }
        Command::Agents {
            action: AgentsAction::List { config, socket },
        } => {
            let target = inspection_target(
                remote_context.as_ref(),
                cli.context.as_deref(),
                config.as_deref(),
                socket.as_deref(),
            )
            .await?;
            output.emit(&target.agents().await?)?;
            Ok(())
        }
        Command::Cloud { action } => crate::cloud::cli::run(action, output.format()).await,
        Command::Mcp { action } => mcp_cmd(action, output).await,
        Command::Update {
            check,
            tag,
            stable,
            yes,
        } => {
            let source = crate::paths::resolve_config(None)?;
            let socket = resolve_client_socket_from(&source, None).await?;
            let opts = crate::update::UpdateOptions {
                check,
                tag,
                stable,
                yes,
            };
            // Preserve the installation pathname before its inode is replaced.
            // Linux current_exe then refers to the old, deleted executable:
            // https://man7.org/linux/man-pages/man5/proc_pid_exe.5.html
            let candidate = std::env::current_exe()?.canonicalize()?;
            let mut outcome = crate::update::run(opts, &socket).await?;
            if outcome.status == "updated" {
                let verification = read_candidate_artifact(&candidate)
                    .await
                    .map_err(|error| format!("{error:#}"));
                outcome.record_target_verification(verification);
            }
            output.emit(&outcome)?;
            if outcome.exit_code() != 0 {
                std::process::exit(outcome.exit_code());
            }
            Ok(())
        }
    }
}

async fn read_candidate_artifact(path: &Path) -> Result<bitrouter_ai::artifact::ArtifactIdentity> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(path)
            .args(["--json", "build-info"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("replacement build-info timed out")??;
    anyhow::ensure!(output.status.success(), "replacement build-info failed");
    let info: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("decoding replacement build-info")?;
    anyhow::ensure!(
        info["product"] == env!("CARGO_PKG_NAME"),
        "replacement product mismatch"
    );
    serde_json::from_value(info).context("decoding replacement artifact identity")
}

// ===== `bitrouter config …` (config tooling) =====

async fn config_cmd(action: ConfigAction, output: &Output) -> Result<()> {
    match action {
        ConfigAction::Validate { config } => {
            let source = crate::paths::resolve_config(config.as_deref())?;
            let report = validate_config(&source).await?;
            output.emit(&report)?;
            if report.valid {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
    }
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
async fn validate_config(source: &crate::paths::ConfigSource) -> Result<ValidateReport> {
    use crate::paths::ConfigSource;
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
    let parsed = parsed.map_err(anyhow::Error::new).and_then(|config| {
        crate::assemble::validate_host_configuration(&config)?;
        Ok(config)
    });
    let missing = missing.into_inner();

    match parsed {
        Ok(cfg) => match crate::policy_source::load_for_config(&cfg, Some(path)).await {
            Ok(_) => {
                let report = ValidateReport::valid(
                    path.display().to_string(),
                    cfg.providers.len(),
                    cfg.models.len(),
                    missing
                        .into_iter()
                        .map(|name| UnsetVar { unset_env: name })
                        .collect(),
                )
                .with_routers(cfg.routers.len())
                // Reported, never fatal: an unread `plugins.<id>` block is a
                // misconfiguration rather than a malformed config, and this
                // command is CI-gating. The daemon warns about the same set on
                // every start (`assemble::ignored_config_warnings`) — that is the path
                // that reaches an operator who never runs `validate`.
                .with_ignored_config(crate::assemble::ignored_config_file_warnings(&cfg));
                Ok(report)
            }
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

// ===== `bitrouter mcp …` (upstream gateway inspection) =====

async fn mcp_cmd(action: McpAction, output: &Output) -> Result<()> {
    match action {
        #[cfg(not(feature = "mcp"))]
        McpAction::Check { .. } => anyhow::bail!("MCP diagnostics require the mcp feature"),
        #[cfg(feature = "mcp")]
        McpAction::Check { server, config } => {
            let source = crate::paths::resolve_config(config.as_deref())?;
            let config = crate::paths::load_config(&source).await?;
            let servers = crate::tools::check(&config, server.as_deref())
                .await
                .map_err(crate::gateway::error::BitrouterError::bad_request)?
                .into_iter()
                .map(|row| match row.outcome {
                    Ok(tools) => McpCheckRow {
                        server: row.server,
                        transport: row.transport,
                        ok: true,
                        latency_ms: row.latency.as_millis(),
                        capabilities: vec!["tools".to_string()],
                        tools: tools.into_iter().map(|tool| tool.name).collect(),
                        error: None,
                    },
                    Err(error) => McpCheckRow {
                        server: row.server,
                        transport: row.transport,
                        ok: false,
                        latency_ms: row.latency.as_millis(),
                        capabilities: Vec::new(),
                        tools: Vec::new(),
                        error: Some(error),
                    },
                })
                .collect();
            let report = McpCheckReport { servers };
            let ok = report.servers.iter().all(|server| server.ok);
            output.emit(&report)?;
            if ok { Ok(()) } else { exit_with(1) }
        }
        McpAction::Search { query, limit } => {
            eprintln!(
                "note: `{cli} mcp search` is deprecated and hidden from help.",
                cli = crate::commands::invocation::name()
            );
            let outcome = crate::mcp_registry::RegistryClient::new()?
                .servers(Some(&query), limit)
                .await?;
            output.emit(&mcp_registry_report(outcome)?)?;
            Ok(())
        }
        McpAction::Config(command) => crate::mcp_config::execute(command, output).await,
    }
}

/// Map fetched registry entries to the `mcp list` / `mcp search` report.
fn mcp_registry_report(
    outcome: crate::mcp_registry::FetchOutcome<Vec<crate::mcp_registry::ServerEntry>>,
) -> Result<McpRegistryReport> {
    let servers = crate::mcp_registry::registry_rows(&outcome.data)
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
/// otherwise a verified runtime locator wins over the currently saved
/// `server.control_socket`. This keeps a daemon reachable after its saved
/// socket changes or its source becomes invalid or missing.
///
/// With no verified running locator, loading YAML remains best-effort: an
/// unreadable source falls back to the default socket beside its logical
/// config path so passive status can still report `running: false`. Commands
/// that need to start a daemon continue to load and validate the source.
async fn resolve_client_socket(config: Option<&Path>, socket: Option<&Path>) -> Result<PathBuf> {
    if let Some(s) = socket {
        return Ok(s.to_path_buf());
    }
    if let Some(located) = crate::daemon_locator::locate(config).await? {
        return Ok(located.socket().to_path_buf());
    }
    let source = crate::daemon_locator::selected_source(config)?;
    match &source {
        crate::paths::ConfigSource::File(path) => {
            let socket_str = match config::load(path).await {
                Ok(cfg) => cfg.server.control_socket,
                Err(_) => daemon::DEFAULT_CONTROL_SOCKET.to_string(),
            };
            Ok(daemon::resolve_socket_path(path, &socket_str))
        }
        crate::paths::ConfigSource::Default { home } => Ok(home.join("bitrouter.sock")),
    }
}

// ===== tracing subscriber init =====

fn defers_tracing_to_host(command: Option<&Command>) -> bool {
    matches!(command, Some(Command::Serve { .. }))
}

/// Install a basic fmt-only tracing subscriber. Used for every command
/// except `serve`, which installs its exporter through `crate::host`.
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

/// Resolve the `bitrouter.log` path for `start` / `restart`. An
/// explicit `--log` override wins; otherwise we place the log next to
/// the config file (e.g. `~/.bitrouter/bitrouter.log`) so the daemon's
/// runtime artefacts — config, socket, pid file, log — all live in one
/// directory, independently of the launcher’s working directory.
fn resolve_log_path(home: &Path, log: Option<&Path>) -> PathBuf {
    if let Some(l) = log {
        return l.to_path_buf();
    }
    home.join("bitrouter.log")
}

/// Variant of [`resolve_client_socket`] for subcommands that already resolved
/// a source. A verified runtime locator still wins; otherwise these commands
/// load saved config and surface errors because they need its contents.
async fn resolve_client_socket_from(
    source: &crate::paths::ConfigSource,
    socket: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(s) = socket {
        return Ok(s.to_path_buf());
    }
    if let Some(located) = crate::daemon_locator::locate_source(source).await? {
        return Ok(located.socket().to_path_buf());
    }
    match source {
        crate::paths::ConfigSource::File(path) => {
            let cfg = config::load(path)
                .await
                .with_context(|| format!("loading {}", path.display()))?;
            Ok(daemon::resolve_socket_path(
                path,
                &cfg.server.control_socket,
            ))
        }
        crate::paths::ConfigSource::Default { home } => Ok(home.join("bitrouter.sock")),
    }
}

fn reject_remote_local_target_flags(config: Option<&Path>, socket: Option<&Path>) -> Result<()> {
    crate::administration_target::reject_remote_local_flags(config, socket)
}

async fn run_shared(action: SharedAction, output: &Output) -> Result<()> {
    #[cfg(not(feature = "multi-instance"))]
    {
        let _ = (action, output);
        anyhow::bail!("shared administration requires the multi-instance feature");
    }
    #[cfg(feature = "multi-instance")]
    match action {
        SharedAction::Operator(action) => shared_operator::run(action, output).await,
        SharedAction::Init {
            config,
            plan,
            operator_id,
            operation_id,
            global_principals,
        } => {
            let receipt = crate::host::initialize_shared(
                &config,
                &plan,
                &operator_id,
                &operation_id,
                global_principals,
            )
            .await?;
            output.emit(&receipt)?;
            Ok(())
        }
    }
}

fn emit_operation_result(output: &Output, report: &OperationReport) -> Result<()> {
    output.emit(report)?;
    if report.succeeded() {
        return Ok(());
    }
    let status = match report.status {
        OperationStatus::Running => "is still running",
        OperationStatus::Failed => "failed",
        OperationStatus::PartiallyApplied => "partially applied",
        OperationStatus::Unknown => "has an unknown outcome",
        OperationStatus::Succeeded => "succeeded",
    };
    anyhow::bail!(
        "reload {status}; inspect `{cli} --context <name> operations show {} --instance {}` before retrying",
        report.request_id,
        report.server_instance_id,
        cli = crate::commands::invocation::name()
    )
}

/// Validate remote eligibility and target flags before any command branch can
/// load a local config, provider environment, or database.  The named context
/// is the complete target; administration reads are passive and explicitly
/// represented by the control inventory rather than an independent CLI list.
fn validate_remote_invocation(command: &Command, remote: bool) -> Result<()> {
    if !remote {
        return Ok(());
    }
    if let Some(leaf) = remote_cli_leaf(command) {
        crate::remote_control::inventory::by_cli(leaf).ok_or_else(|| {
            anyhow::anyhow!("control inventory has no action for CLI leaf `{leaf}`")
        })?;
        let (config, socket) = remote_target_flags(command).ok_or_else(|| {
            anyhow::anyhow!("remote CLI leaf `{leaf}` has no target-flag contract")
        })?;
        return reject_remote_local_target_flags(config, socket);
    }
    if let Some(leaf) = remote_resource_leaf(command) {
        crate::remote_control::inventory::RESOURCES
            .iter()
            .find(|resource| resource.cli_leaf == Some(leaf))
            .ok_or_else(|| {
                anyhow::anyhow!("control inventory has no resource for CLI leaf `{leaf}`")
            })?;
        return Ok(());
    }
    Err(crate::gateway::error::BitrouterError::bad_request(
        "this command is local-only for a remote context",
    )
    .into())
}

fn remote_cli_leaf(command: &Command) -> Option<&'static str> {
    match command {
        Command::Reload { .. } => Some("reload"),
        Command::Status { requests: true, .. } | Command::Requests { .. } => Some("requests"),
        Command::Status {
            requests: false, ..
        } => Some("status"),
        Command::Models { .. } => Some("models"),
        Command::Route { .. } => Some("route"),
        Command::Providers {
            action: ProviderAction::List { .. },
        } => Some("providers list"),
        Command::Observe {
            action: ObserveAction::Status { .. },
        } => Some("observe status"),
        Command::Policy {
            action: PolicyAction::Status { .. },
        } => Some("policy status"),
        Command::Policy {
            action: PolicyAction::Show { .. },
        } => Some("policy show"),
        Command::Agents {
            action: AgentsAction::List { .. },
            ..
        } => Some("agents list"),
        _ => None,
    }
}

fn remote_resource_leaf(command: &Command) -> Option<&'static str> {
    match command {
        Command::Operations {
            action: OperationsAction::Show { .. },
        } => Some("operations show"),
        _ => None,
    }
}

fn remote_target_flags(command: &Command) -> Option<(Option<&Path>, Option<&Path>)> {
    match command {
        Command::Reload { config, socket }
        | Command::Status { config, socket, .. }
        | Command::Requests { config, socket, .. }
        | Command::Route { config, socket, .. }
        | Command::Providers {
            action: ProviderAction::List { config, socket },
        }
        | Command::Observe {
            action: ObserveAction::Status { config, socket },
        }
        | Command::Policy {
            action:
                PolicyAction::Status { config, socket, .. } | PolicyAction::Show { config, socket, .. },
        }
        | Command::Agents {
            action: AgentsAction::List { config, socket },
            ..
        } => Some((config.as_deref(), socket.as_deref())),
        Command::Models { config, .. } => Some((config.as_deref(), None)),
        _ => None,
    }
}

async fn start(
    source: &crate::paths::ConfigSource,
    log_path: &Path,
    action: &'static str,
) -> Result<DaemonActionReport> {
    crate::host::validate_local_daemon_source(source).await?;
    crate::host::validate_startup_features()?;
    // Make sure the bitrouter home exists *before* we open the log
    // file inside it. (Zero-config first-run lands here with the home
    // not yet created on disk.)
    crate::paths::ensure_home_directory(source.home())?;

    if let Some(located) = crate::daemon_locator::locate_source(source).await? {
        anyhow::bail!(
            "bitrouter is already running (pid {}); use `restart` or `stop` first",
            located.pid()
        );
    }

    // Refuse to start a second daemon on top of a live one — silent overlap
    // would race two `serve`s for the same socket and one would die into the
    // log file (the user wouldn't see it).
    let cfg_socket_path: Option<PathBuf> = match source {
        crate::paths::ConfigSource::File(path) => match config::load(path).await {
            Ok(cfg) => Some(daemon::socket_path_for(source, &cfg)),
            // Best-effort: a broken/env-incomplete config can't locate the
            // socket, but `serve` would fail the same way → the child-death
            // check below still surfaces it.
            Err(_) => None,
        },
        crate::paths::ConfigSource::Default { home } => Some(home.join("bitrouter.sock")),
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
            let p = crate::style::Palette::for_stderr();
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
    source: &crate::paths::ConfigSource,
    socket: &Path,
    log_path: &Path,
) -> Result<DaemonActionReport> {
    crate::host::validate_startup_features()?;
    crate::paths::ensure_home_directory(source.home())?;
    let _lock = crate::daemon_locator::lock_restart(source).await?;
    crate::host::validate_local_daemon_source(source).await?;
    let config = crate::paths::load_config(source).await?;
    let replacement = daemon::socket_path_for(source, &config);
    anyhow::ensure!(
        replacement == socket || !daemon::endpoint_in_use(&replacement),
        "replacement control endpoint is already in use"
    );
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
    let env: Vec<(String, String)> = crate::gateway::catalog::cache::credential_env_var_providers()
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

async fn inspection_target(
    remote_context: Option<&crate::contexts::RemoteContext>,
    context_name: Option<&str>,
    config: Option<&Path>,
    socket: Option<&Path>,
) -> Result<InspectionTarget> {
    let remote = match remote_context {
        Some(context) => {
            let name = context_name
                .ok_or_else(|| anyhow::anyhow!("remote context name is unavailable"))?;
            Some((name.to_string(), context.clone()))
        }
        None => None,
    };
    InspectionTarget::resolve(remote, config, socket).await
}

fn selected_policy_view(view: Option<PolicyViewArg>, disk: bool) -> PolicyView {
    if disk {
        PolicyView::Disk
    } else {
        view.map(PolicyView::from).unwrap_or(PolicyView::Active)
    }
}

async fn administration_providers(
    action: ProviderAction,
    output: &Output,
    remote_context: Option<&crate::contexts::RemoteContext>,
    context_name: Option<&str>,
) -> Result<()> {
    match action {
        ProviderAction::List { config, socket } => {
            let target = inspection_target(
                remote_context,
                context_name,
                config.as_deref(),
                socket.as_deref(),
            )
            .await?;
            if let Some(source) = target.local_source() {
                output.emit(&providers_report(source).await?)?;
            } else {
                output.emit(&target.providers().await?)?;
            }
            Ok(())
        }
        ProviderAction::Check { .. } => {
            if remote_context.is_some() {
                return Err(crate::gateway::error::BitrouterError::bad_request(
                    "`providers check` reads the local configuration and is unavailable for a remote context",
                )
                .into());
            }
            providers(action, output).await
        }
        action => {
            if remote_context.is_some() {
                return Err(crate::gateway::error::BitrouterError::bad_request(
                    "provider credential administration is local-only for a remote context",
                )
                .into());
            }
            providers(action, output).await
        }
    }
}

async fn administration_observe(
    action: ObserveAction,
    output: &Output,
    remote_context: Option<&crate::contexts::RemoteContext>,
    context_name: Option<&str>,
) -> Result<()> {
    match action {
        ObserveAction::Status { config, socket } => {
            let target = inspection_target(
                remote_context,
                context_name,
                config.as_deref(),
                socket.as_deref(),
            )
            .await?;
            if let Some(socket) = target.local_socket() {
                output.emit(&observe_status(socket).await?)?;
            } else {
                output.emit(&target.observe().await?)?;
            }
            Ok(())
        }
    }
}

/// Preserve the established local observe report, including its local socket
/// and exporter endpoint fields. The redacted administration report is used
/// for every remote target and by the dashboard.
async fn observe_status(socket: &Path) -> Result<ObserveStatusReport> {
    use crate::telemetry::OTEL_ENABLED;

    let (snapshot, daemon_reachable) =
        match daemon::send_command(socket, &DaemonCommand::ObserveStatus).await {
            Ok(DaemonResponse::ObserveStatus { payload }) => (payload, true),
            Ok(DaemonResponse::Error { message }) => return Err(anyhow::anyhow!(message)),
            Ok(other) => return Err(anyhow::anyhow!("unexpected response: {other:?}")),
            Err(error) if daemon::is_not_reachable(&error) => {
                (daemon::ObserveStatusPayload::unwired(OTEL_ENABLED), false)
            }
            Err(error) => return Err(error),
        };

    Ok(ObserveStatusReport {
        daemon_reachable,
        snapshot,
        socket: socket.display().to_string(),
    })
}

async fn administration_policy(
    action: PolicyAction,
    output: &Output,
    remote_context: Option<&crate::contexts::RemoteContext>,
    context_name: Option<&str>,
) -> Result<()> {
    if remote_context.is_some()
        && !matches!(
            &action,
            PolicyAction::Status { .. } | PolicyAction::Show { .. }
        )
    {
        return Err(crate::gateway::error::BitrouterError::bad_request(
            "this policy action is local-only for a remote context",
        )
        .into());
    }
    match action {
        PolicyAction::Init {
            name,
            router,
            strong,
            strong_effort,
            economy,
            economy_effort,
            config,
        } => {
            let source = crate::paths::resolve_config(config.as_deref())?;
            let config_path = require_policy_config_path(&source)?;
            let cfg = config::load(config_path).await?;
            if let Some(model) = strong.as_deref() {
                crate::policy_source::validate_routable_model(&cfg, model).await?;
                crate::policy_source::validate_routable_effort(&cfg, model, strong_effort).await?;
            }
            crate::policy_source::validate_routable_model(&cfg, &economy).await?;
            crate::policy_source::validate_routable_effort(&cfg, &economy, economy_effort).await?;
            let update = crate::policy_source::initialize_router_files_with_efforts(
                config_path,
                &name,
                &router,
                strong.as_deref(),
                strong_effort,
                &economy,
                economy_effort,
            )
            .await?;
            let mut report =
                routing_policy_report(config_path, "init", true, update.changes).await?;
            report.config_activation =
                Some(crate::output::reports::config::ConfigActivation::SavedOnly);
            output.emit(&report)?;
        }
        PolicyAction::Check { config } => {
            let source = crate::paths::resolve_config(config.as_deref())?;
            let config_path = require_policy_config_path(&source)?;
            output.emit(&routing_policy_report(config_path, "check", false, Vec::new()).await?)?;
        }
        PolicyAction::Status {
            view,
            disk,
            config,
            socket,
        } => {
            let target = inspection_target(
                remote_context,
                context_name,
                config.as_deref(),
                socket.as_deref(),
            )
            .await?;
            output.emit(
                &target
                    .policy(PolicyInput {
                        view: selected_policy_view(view, disk),
                        name: None,
                    })
                    .await?,
            )?;
        }
        PolicyAction::Show {
            name,
            view,
            disk,
            config,
            socket,
        } => {
            let target = inspection_target(
                remote_context,
                context_name,
                config.as_deref(),
                socket.as_deref(),
            )
            .await?;
            output.emit(
                &target
                    .policy(PolicyInput {
                        view: selected_policy_view(view, disk),
                        name,
                    })
                    .await?,
            )?;
        }
        PolicyAction::Reload { config, socket } => {
            let source = crate::paths::resolve_config(config.as_deref())?;
            let socket = resolve_client_socket_from(&source, socket.as_deref()).await?;
            output.emit(&reload(&socket).await?)?;
        }
        PolicyAction::Diff { active, candidate } => {
            let active_source = crate::policy_source::load(&active).await?;
            let candidate_source = crate::policy_source::load(&candidate).await?;
            let changes = crate::policy_source::diff_explanations(
                &active_source.document,
                &candidate_source.document,
            );
            output.emit(&PolicyReport {
                action: "diff".into(),
                path: Some(active.display().to_string()),
                candidate_path: Some(candidate.display().to_string()),
                digest: Some(candidate_source.digest),
                source: Some("disk".into()),
                policies: candidate_source.document.policies.keys().cloned().collect(),
                bindings: Default::default(),
                changes,
                policy: Some(candidate_source.document),
                applied: false,
                config_activation: None,
            })?;
        }
        PolicyAction::Publish {
            candidate,
            expected_digest,
            config,
            socket,
        } => {
            let source = crate::paths::resolve_config(config.as_deref())?;
            let config_path = require_policy_config_path(&source)?;
            let update = crate::policy_source::publish_candidate_file(
                config_path,
                &candidate,
                &expected_digest,
            )
            .await?;
            reload_published_policy_or_restore(&source, &update, socket.as_deref()).await?;
            let mut report =
                routing_policy_report(config_path, "publish", true, update.changes).await?;
            report.candidate_path = Some(candidate.display().to_string());
            output.emit(&report)?;
        }
        PolicyAction::Rollback {
            digest,
            expected_digest,
            config,
            socket,
        } => {
            let source = crate::paths::resolve_config(config.as_deref())?;
            let config_path = require_policy_config_path(&source)?;
            let update =
                crate::policy_source::rollback_files(config_path, &digest, &expected_digest)
                    .await?;
            reload_published_policy_or_restore(&source, &update, socket.as_deref()).await?;
            output.emit(
                &routing_policy_report(config_path, "rollback", true, update.changes).await?,
            )?;
        }
    }
    Ok(())
}

async fn eval(action: EvalAction, output: &Output) -> Result<()> {
    use crate::eval::admission::SubmissionPrincipal;
    use {bitrouter_ai::eval::EvalSubject, bitrouter_ai::eval::EvaluationResult};

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
                    crate::storage::evaluation::evidence_digest(&subject.evidence)?;
                crate::storage::evaluation::validate_subject(&subject)?;
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
    let source = crate::paths::resolve_config(config_path)?;
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
    source: &crate::paths::ConfigSource,
) -> Result<(
    crate::config::Config,
    crate::trajectory::store::TrajectoryStore,
)> {
    let config = crate::paths::load_config(source).await?;
    let database_url = crate::db::anchor_url(&config.database.url, source.home());
    let db = crate::db::connect(&database_url).await?;
    crate::db::run_migrations(&db).await?;
    Ok((config, crate::trajectory::store::TrajectoryStore::new(db)))
}

async fn local_eval_service(config_path: Option<&Path>) -> Result<crate::eval::EvalService> {
    let source = crate::paths::resolve_config(config_path)?;
    let config = crate::paths::load_config(&source).await?;
    let db = crate::db::connect(&config.database.url).await?;
    crate::db::run_migrations(&db).await?;
    Ok(crate::eval::EvalService::new(
        crate::eval::store::EvalStore::new(db),
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
    source: &crate::paths::ConfigSource,
    socket_override: Option<&Path>,
) -> Result<()> {
    let socket = resolve_client_socket_from(source, socket_override).await?;
    if socket_override.is_none() && !daemon::endpoint_in_use(&socket) {
        return Ok(());
    }
    reload(&socket).await.map(|_| ())
}

async fn reload_published_policy_or_restore(
    source: &crate::paths::ConfigSource,
    update: &crate::policy_source::PolicyFileUpdate,
    socket_override: Option<&Path>,
) -> Result<()> {
    reload_published_policy_or_restore_with(update, || {
        reload_policy_if_reachable(source, socket_override)
    })
    .await
}

async fn reload_published_policy_or_restore_with<Reload, ReloadFuture>(
    update: &crate::policy_source::PolicyFileUpdate,
    reload: Reload,
) -> Result<()>
where
    Reload: Fn() -> ReloadFuture,
    ReloadFuture: std::future::Future<Output = Result<()>>,
{
    let initial_error = match reload().await {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    let parent_digest = update.parent_digest.as_deref().ok_or_else(|| {
        anyhow::anyhow!("daemon rejected publication; publication has no previous source digest: {initial_error:#}")
    })?;
    let history = crate::policy_source::default_history_dir(&update.path);
    match crate::policy_source::rollback_to_digest(
        &update.path,
        &update.digest,
        parent_digest,
        &history,
    ) {
        Ok(_) => match reload().await {
            Ok(()) => Err(initial_error.context(
                "daemon rejected publication; restored and reloaded previous policy source",
            )),
            Err(recovery_error) => Err(initial_error.context(format!(
                "daemon rejected publication; restored previous policy source but recovery reload failed: {recovery_error:#}",
            ))),
        },
        Err(restore_error) => Err(initial_error.context(format!(
            "daemon rejected publication; recovery refused or failed without overwriting a concurrent source: {restore_error:#}",
        ))),
    }
}

fn require_policy_config_path(source: &crate::paths::ConfigSource) -> Result<&Path> {
    match source {
        crate::paths::ConfigSource::File(path) => Ok(path),
        crate::paths::ConfigSource::Default { .. } => anyhow::bail!(
            "routing policies require a file-backed bitrouter.yaml; run `{} init` first",
            crate::commands::invocation::name()
        ),
    }
}

async fn routing_policy_report(
    config_path: &Path,
    action: &str,
    applied: bool,
    changes: Vec<String>,
) -> Result<PolicyReport> {
    let mut cfg = config::load(config_path).await?;
    crate::policy_source::apply_defaults(&mut cfg, Some(config_path));
    let loaded = crate::policy_source::load_for_config(&cfg, Some(config_path)).await?;
    let bindings = cfg
        .router_policy_bindings()
        .map(|(name, policy, _)| (name.to_owned(), policy.to_owned()))
        .collect();
    Ok(PolicyReport {
        action: action.to_string(),
        path: loaded
            .as_ref()
            .and_then(|source| source.path.as_ref())
            .map(|path| path.display().to_string()),
        candidate_path: None,
        digest: loaded.as_ref().map(|source| source.digest.clone()),
        source: Some(
            if loaded.as_ref().is_some_and(|source| source.path.is_some()) {
                "disk"
            } else {
                "bundled"
            }
            .to_string(),
        ),
        policies: loaded
            .as_ref()
            .map(|source| source.document.policies.keys().cloned().collect())
            .unwrap_or_default(),
        bindings,
        changes,
        policy: loaded.map(|source| source.document),
        applied,
        config_activation: None,
    })
}

async fn providers(action: ProviderAction, output: &Output) -> Result<()> {
    match action {
        ProviderAction::List { config, .. } => {
            let source = crate::paths::resolve_config(config.as_deref())?;
            output.emit(&providers_report(&source).await?)?;
            Ok(())
        }
        ProviderAction::Check {
            provider,
            account,
            config,
        } => {
            let source = crate::paths::resolve_config(config.as_deref())?;
            let report = match &source {
                crate::paths::ConfigSource::File(path) => {
                    let yaml = tokio::fs::read_to_string(path)
                        .await
                        .with_context(|| format!("reading {}", path.display()))?;
                    crate::providers::check::check_document(
                        &yaml,
                        &provider,
                        account.as_deref(),
                        crate::config::env_lookup,
                    )
                    .await?
                }
                crate::paths::ConfigSource::Default { .. } => {
                    let loaded = crate::paths::load_config(&source).await?;
                    crate::providers::check::check_loaded(&loaded, &provider, account.as_deref())
                        .await?
                }
            };
            output.emit(&report)?;
            exit_with(report.exit_code())
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
            if let Some(message) = crate::gateway::providers::retired::provider_message(&provider) {
                anyhow::bail!("{message}");
            }
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
                        "`{cli} providers login bitrouter` uses BitRouter Cloud OAuth; \
                         --import-existing/--no-browser apply to upstream provider logins",
                        cli = crate::commands::invocation::name()
                    );
                }
                // A supplied key seeds the cloud credential the same way
                // `cloud login --api-key` does.
                crate::cloud::cli::run(
                    crate::cloud::cli::CloudAction::Login {
                        authorization_server: None,
                        client_id: None,
                        scope: None,
                        api_key,
                    },
                    output.format(),
                )
                .await
            } else {
                let outcome = crate::commands::login_provider_with_options(
                    &provider,
                    &label,
                    crate::commands::ProviderLoginOptions {
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
                crate::cloud::cli::run(
                    crate::cloud::cli::CloudAction::Logout {
                        authorization_server: None,
                        client_id: None,
                    },
                    output.format(),
                )
                .await
            } else {
                let removed = crate::commands::logout_provider(&provider).await?;
                output.emit(&ProviderLogoutReport { provider, removed })?;
                Ok(())
            }
        }
    }
}

/// Preserve the local `providers list` report shape. Remote administration
/// never uses this adapter because provider API bases are not a remote read.
async fn providers_report(source: &crate::paths::ConfigSource) -> Result<ProvidersReport> {
    let cfg = crate::paths::load_config(source).await?;
    let providers = commands::list_providers(&cfg)
        .into_iter()
        .map(|provider| ProviderRow {
            id: provider.id,
            models: provider.model_count,
            active: provider.active,
            api_base: provider.api_base,
        })
        .collect();
    Ok(ProvidersReport { providers })
}

#[cfg(not(feature = "mcp"))]
async fn tools(_action: ToolsAction, _output: &Output) -> Result<()> {
    anyhow::bail!("MCP diagnostics require the mcp feature")
}

#[cfg(feature = "mcp")]
async fn tools(action: ToolsAction, output: &Output) -> Result<()> {
    use crate::tools as tools_cmd;

    match action {
        ToolsAction::List { config } => {
            eprintln!(
                "note: `{cli} tools list` is a compatibility form; use `{cli} mcp check`.",
                cli = crate::commands::invocation::name()
            );
            let source = crate::paths::resolve_config(config.as_deref())?;
            let cfg = crate::paths::load_config(&source).await?;
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
            eprintln!(
                "note: `{cli} tools status` is a compatibility form; use `{cli} mcp check`.",
                cli = crate::commands::invocation::name()
            );
            let source = crate::paths::resolve_config(config.as_deref())?;
            let cfg = crate::paths::load_config(&source).await?;
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
            eprintln!(
                "note: `{cli} tools discover` is deprecated; use `{cli} mcp check <server>`.",
                cli = crate::commands::invocation::name()
            );
            let source = crate::paths::resolve_config(config.as_deref())?;
            let cfg = crate::paths::load_config(&source).await?;
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

/// Leave with a status a successful command chose for itself — `providers
/// check`, `spawn --check`, and a headless prompt's permission tally — or
/// return normally for zero.
fn exit_with(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        std::process::exit(code);
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn gateway_exposes_service_entries_and_rejects_local_execution() -> anyhow::Result<()> {
        for private in [
            "harness-host",
            "task",
            "run",
            "acp",
            "threads",
            "spawn",
            "workflow-state",
        ] {
            assert!(
                Cli::try_parse_from(["bitrouter", private]).is_err(),
                "{private}"
            );
            assert!(Cli::command().find_subcommand(private).is_none());
        }
        Cli::try_parse_from(["bitrouter", "serve", "--config", "service.yaml"])?;
        Cli::try_parse_from(["bitrouter", "start", "--config", "service.yaml"])?;
        Ok(())
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
                        daemon_version: None,
                        artifact: None,
                        cli_owned: None,
                        listen: "127.0.0.1:4356".to_string(),
                        models: 0,
                        providers: Vec::new(),
                        saved_routers: None,
                        running_routers: None,
                        router_restart_required: None,
                        config_state: None,
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
                        daemon_version: None,
                        artifact: None,
                        cli_owned: None,
                        listen: "127.0.0.1:4356".to_string(),
                        models: 0,
                        providers: Vec::new(),
                        saved_routers: None,
                        running_routers: None,
                        router_restart_required: None,
                        config_state: None,
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
                    daemon_version: None,
                    artifact: None,
                    cli_owned: None,
                    listen: "127.0.0.1:4356".to_string(),
                    models: 0,
                    providers: Vec::new(),
                    saved_routers: None,
                    running_routers: None,
                    router_restart_required: None,
                    config_state: None,
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
            let db = crate::db::connect(database_url).await?;
            crate::db::run_migrations(&db).await?;
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
        let file_source = crate::paths::ConfigSource::File(config_path);
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
        let zero_source = crate::paths::ConfigSource::Default {
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

    fn control_action_arguments(leaf: &str) -> Option<Vec<&'static str>> {
        let args = match leaf {
            "reload" => vec!["bitrouter", "--context", "workstation", "reload"],
            "status" => vec!["bitrouter", "--context", "workstation", "status"],
            "models" => vec![
                "bitrouter",
                "--context",
                "workstation",
                "models",
                "--provider",
                "openai",
            ],
            "route" => vec![
                "bitrouter",
                "--context",
                "workstation",
                "route",
                "openai/gpt-5",
            ],
            "requests" => vec![
                "bitrouter",
                "--context",
                "workstation",
                "requests",
                "--limit",
                "100",
                "--since",
                "2026-01-01T00:00:00Z",
                "--until",
                "2026-01-01T01:00:00Z",
                "--model",
                "openai/gpt-5",
                "--provider",
                "openai",
            ],
            "providers list" => vec!["bitrouter", "--context", "workstation", "providers", "list"],
            "observe status" => vec!["bitrouter", "--context", "workstation", "observe", "status"],
            "policy status" => vec![
                "bitrouter",
                "--context",
                "workstation",
                "policy",
                "status",
                "--view",
                "active",
            ],
            "policy show" => vec![
                "bitrouter",
                "--context",
                "workstation",
                "policy",
                "show",
                "default",
                "--view",
                "disk",
            ],
            "agents list" => vec!["bitrouter", "--context", "workstation", "agents", "list"],
            _ => return None,
        };
        Some(args)
    }

    #[test]
    fn every_control_action_parses_and_is_remote_eligible() -> anyhow::Result<()> {
        for action in crate::remote_control::inventory::ACTIONS {
            let args = control_action_arguments(action.cli_leaf).ok_or_else(|| {
                anyhow::anyhow!(
                    "control action `{}` has no Clap argument fixture for `{}`",
                    action.id,
                    action.cli_leaf
                )
            })?;
            let cli = Cli::try_parse_from(args)
                .with_context(|| format!("parse control action `{}`", action.id))?;
            let command = cli.command.ok_or_else(|| {
                anyhow::anyhow!("control action `{}` parsed without a command", action.id)
            })?;
            assert_eq!(remote_cli_leaf(&command), Some(action.cli_leaf));
            validate_remote_invocation(&command, true)
                .with_context(|| format!("validate control action `{}`", action.id))?;
        }

        for resource in crate::remote_control::inventory::RESOURCES {
            let Some(leaf) = resource.cli_leaf else {
                continue;
            };
            let cli = Cli::try_parse_from([
                "bitrouter",
                "--context",
                "workstation",
                "operations",
                "show",
                "7ab707f8-50d1-4ba0-9aeb-8f4cb196712b",
                "--instance",
                "a68fed5e-18e0-4ed8-9023-9ad286a8351d",
            ])?;
            let command = cli.command.ok_or_else(|| {
                anyhow::anyhow!(
                    "control resource `{}` parsed without a command",
                    resource.id
                )
            })?;
            assert_eq!(remote_resource_leaf(&command), Some(leaf));
            validate_remote_invocation(&command, true)
                .with_context(|| format!("validate control resource `{}`", resource.id))?;
        }
        Ok(())
    }

    #[test]
    fn remote_control_actions_reject_local_target_flags_before_execution() -> anyhow::Result<()> {
        for action in crate::remote_control::inventory::ACTIONS {
            let mut args = control_action_arguments(action.cli_leaf).ok_or_else(|| {
                anyhow::anyhow!("missing control fixture for `{}`", action.cli_leaf)
            })?;
            args.extend(["--config", "/must-not-be-read/bitrouter.yaml"]);
            if action.cli_leaf != "models" {
                args.extend(["--socket", "/must-not-be-read/bitrouter.sock"]);
            }
            let cli = Cli::try_parse_from(args)
                .with_context(|| format!("parse target flags for `{}`", action.id))?;
            let command = cli.command.ok_or_else(|| {
                anyhow::anyhow!("control action `{}` parsed without a command", action.id)
            })?;
            let error = validate_remote_invocation(&command, true)
                .err()
                .ok_or_else(|| anyhow::anyhow!("{} accepted a local target flag", action.id))?;
            assert!(
                error
                    .to_string()
                    .contains("does not accept --config or --socket"),
                "{}: {error:#}",
                action.id
            );
        }
        Ok(())
    }

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        // Panics if clap detects a conflict (e.g. `--tag` vs global `--version`).
        Cli::command().debug_assert();
    }

    #[test]
    fn mcp_cli_exposes_check_but_not_the_removed_origin_commands() {
        use clap::Parser;

        assert!(matches!(
            Cli::try_parse_from(["bro", "mcp", "check"]),
            Ok(Cli {
                command: Some(Command::Mcp {
                    action: McpAction::Check { .. }
                }),
                ..
            })
        ));
        assert!(Cli::try_parse_from(["bro", "mcp", "serve"]).is_err());
        assert!(Cli::try_parse_from(["bro", "mcp", "install"]).is_err());
    }

    #[test]
    fn first_party_plugins_are_skill_only() {
        for (name, manifest) in [
            (
                "claude",
                include_str!("../../../../.claude-plugin/plugin.json"),
            ),
            (
                "codex",
                include_str!("../../../../.codex-plugin/plugin.json"),
            ),
        ] {
            let parsed = serde_json::from_str::<serde_json::Value>(manifest);
            assert!(parsed.is_ok(), "{name} plugin manifest is not valid JSON");
            let Ok(parsed) = parsed else {
                continue;
            };
            assert!(
                parsed.get("mcpServers").is_none(),
                "{name} plugin must not register an OSS origin MCP server"
            );
            assert!(
                !manifest.contains("mcp serve"),
                "{name} plugin references the removed origin command"
            );
        }
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
  "spec_version": "eval/1.0.0-beta.1",
  "identity": {"profile": "gateway_generic"},
  "eval_id": "eval-generic-1",
  "scope": "task",
  "subject_id": "task-generic-1",
  "cohort": null,
  "holdout": false,
  "decisions": [
    {
      "decision_id": "decision-1",
      "policy": "auto",
      "route_projection": "code:generation|implement|normal",
      "request_key": "unknown|implement|normal",
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

        eval(action, &Output::new(crate::output::Format::Json)).await?;

        let sealed_text = std::fs::read_to_string(&sealed)?;
        let subject: bitrouter_ai::eval::EvalSubject = serde_json::from_str(&sealed_text)?;
        crate::storage::evaluation::validate_subject(&subject)?;

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
        eval(action, &Output::new(crate::output::Format::Json)).await?;
        let repeated_text = std::fs::read_to_string(&sealed_again)?;
        let repeated: bitrouter_ai::eval::EvalSubject = serde_json::from_str(&repeated_text)?;
        crate::storage::evaluation::validate_subject(&repeated)?;
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
                yes,
            }) => {
                assert!(check);
                assert_eq!(tag.as_deref(), Some("1.0.0-alpha.18"));
                assert!(!stable && !yes);
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
            "--after",
            "exit",
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
                after,
                ..
            }) => {
                assert!(yes && force && reset);
                assert_eq!(api_key.as_deref(), Some("brk_abc.secret"));
                assert_eq!(providers, vec!["openai"]);
                assert_eq!(provider_api_keys, vec!["sk-openai"]);
                assert_eq!(after, Some(AfterSetup::Exit));
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
    fn cloud_api_owns_header_short_flag_in_full_command_tree() -> Result<()> {
        let cli = Cli::try_parse_from([
            "bitrouter",
            "cloud",
            "api",
            "/v1/models",
            "-H",
            "X-Test: value",
        ])?;

        match cli.command {
            Some(Command::Cloud {
                action: crate::cloud::cli::CloudAction::Api(args),
            }) => assert_eq!(args.headers, ["X-Test: value"]),
            _ => anyhow::bail!("expected cloud API command"),
        }

        let human = Cli::try_parse_from(["bitrouter", "-H", "cloud", "whoami"])?;
        assert!(human.human_short);
        Ok(())
    }

    #[test]
    fn offline_commands_expose_current_backup_operations() -> Result<()> {
        Cli::try_parse_from([
            "bitrouter",
            "tenancy",
            "offline",
            "snapshot",
            "--database",
            "/tmp/current.sqlite",
            "--bundle",
            "/tmp/current-bundle",
            "--scratch-directory",
            "/tmp/private-scratch",
        ])?;
        Cli::try_parse_from([
            "bitrouter",
            "tenancy",
            "offline",
            "verify",
            "--bundle",
            "/tmp/current-bundle",
            "--evidence-digest",
            "digest",
            "--scratch-directory",
            "/tmp/private-scratch",
        ])?;
        Cli::try_parse_from([
            "bro",
            "tenancy",
            "offline",
            "restore",
            "--bundle",
            "/tmp/current-bundle",
            "--evidence-digest",
            "digest",
            "--database",
            "/tmp/restored.sqlite",
            "--expected-installation-id",
            "install-current",
            "--wrapping-key-env",
            "WRAPPING_KEY",
            "--scratch-directory",
            "/tmp/private-scratch",
        ])?;
        Ok(())
    }

    #[test]
    fn policy_reads_default_to_active_and_require_explicit_disk() -> Result<()> {
        anyhow::ensure!(selected_policy_view(None, false) == PolicyView::Active);
        anyhow::ensure!(selected_policy_view(None, true) == PolicyView::Disk);
        anyhow::ensure!(selected_policy_view(Some(PolicyViewArg::Disk), false) == PolicyView::Disk);
        let cli = Cli::try_parse_from(["bro", "policy", "show", "--disk"])?;
        anyhow::ensure!(matches!(
            cli.command,
            Some(Command::Policy {
                action: PolicyAction::Show {
                    name: None,
                    disk: true,
                    ..
                }
            })
        ));
        anyhow::ensure!(
            Cli::try_parse_from(["bro", "policy", "show", "--disk", "--view", "active"]).is_err()
        );
        Ok(())
    }

    #[test]
    fn policy_publication_requires_cas_and_rejects_unknown_commands() -> Result<()> {
        for args in [
            vec!["bro", "policy", "publish", "candidate.yaml"],
            vec!["bro", "policy", "rollback", "sha256:old"],
            vec!["bro", "policy", "create", "acl"],
            vec!["bro", "policy", "compile", "--output", "candidate.yaml"],
            vec!["bro", "policy", "evolve", "--apply"],
            vec!["bro", "policy", "verify", "--evidence"],
            vec![
                "bro",
                "policy",
                "init",
                "auto",
                "--preset",
                "auto",
                "--economy",
                "fixture:cheap",
            ],
            vec!["bro", "optimize", "run"],
            vec!["bro", "optimize", "status"],
            vec!["bro", "config", "migrate-routers"],
        ] {
            anyhow::ensure!(
                Cli::try_parse_from(&args).is_err(),
                "unsupported or unsafe command parsed: {args:?}"
            );
        }
        for operation in ["publish", "rollback"] {
            Cli::try_parse_from([
                "bro",
                "policy",
                operation,
                "candidate",
                "--expected-digest",
                "sha256:current",
            ])?;
        }
        Cli::try_parse_from(["bro", "access-policy", "create", "acl"])?;
        let initialized = Cli::try_parse_from([
            "bro",
            "policy",
            "init",
            "coding",
            "--strong",
            "fixture:strong",
            "--economy",
            "fixture:cheap",
        ])?;
        anyhow::ensure!(matches!(initialized.command, Some(Command::Policy {
            action: PolicyAction::Init { router, .. }
        }) if router == "coding"));
        Ok(())
    }

    fn recovery_source(model: &str) -> Result<bitrouter_ai::policy::Document> {
        serde_saphyr::from_str(&format!(
            "spec_version: policy/1.0.0-beta.1\npolicies:\n  auto:\n    tiers:\n      strong: {{model: '{model}'}}\n    default_tier: strong\n"
        )).context("constructing current policy source")
    }

    fn source_recovery_fixture()
    -> Result<(tempfile::TempDir, crate::policy_source::PolicyFileUpdate)> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("policy.yaml");
        let parent = recovery_source("fixture:strong")?;
        let parent_digest = crate::policy_source::write_atomic(&path, None, &parent)?;
        let document = recovery_source("fixture:economy")?;
        let history = crate::policy_source::default_history_dir(&path);
        let publication =
            crate::policy_source::publish_candidate(&path, &parent_digest, &document, &history)?;
        Ok((
            directory,
            crate::policy_source::PolicyFileUpdate {
                path,
                digest: publication.child_digest,
                parent_digest: Some(parent_digest),
                changes: Vec::new(),
            },
        ))
    }

    #[tokio::test]
    async fn rejected_source_publication_restores_disk_and_reloads_parent() -> Result<()> {
        let (_directory, update) = source_recovery_fixture()?;
        let attempts = std::cell::Cell::new(0);
        let live = std::cell::RefCell::new(update.digest.clone());
        let error = reload_published_policy_or_restore_with(&update, || async {
            let attempt = attempts.get();
            attempts.set(attempt + 1);
            if attempt == 0 {
                anyhow::bail!("injected candidate rejection");
            }
            *live.borrow_mut() = crate::policy_source::load(&update.path).await?.digest;
            Ok(())
        })
        .await
        .err()
        .context("rejected publication unexpectedly succeeded")?;
        anyhow::ensure!(format!("{error:#}").contains("injected candidate rejection"));
        anyhow::ensure!(attempts.get() == 2, "recovery did not reload the parent");
        let disk = crate::policy_source::load(&update.path).await?;
        anyhow::ensure!(Some(disk.digest.as_str()) == update.parent_digest.as_deref());
        anyhow::ensure!(
            *live.borrow() == disk.digest,
            "disk and modeled active source differ"
        );
        anyhow::ensure!(disk.document == recovery_source("fixture:strong")?);
        Ok(())
    }

    #[tokio::test]
    async fn source_recovery_preserves_a_concurrent_publisher() -> Result<()> {
        let (_directory, update) = source_recovery_fixture()?;
        let winner = recovery_source("fixture:winner")?;
        let winner_digest = crate::policy_source::semantic_digest(&winner)?;
        let attempts = std::cell::Cell::new(0);
        let error = reload_published_policy_or_restore_with(&update, || async {
            attempts.set(attempts.get() + 1);
            crate::policy_source::publish_candidate(
                &update.path,
                &update.digest,
                &winner,
                &crate::policy_source::default_history_dir(&update.path),
            )?;
            anyhow::bail!("injected rejection after concurrent winner");
        })
        .await
        .err()
        .context("stale recovery unexpectedly succeeded")?;
        anyhow::ensure!(format!("{error:#}").contains("recovery refused or failed"));
        anyhow::ensure!(
            attempts.get() == 1,
            "stale recovery reloaded its rejected source"
        );
        let disk = crate::policy_source::load(&update.path).await?;
        anyhow::ensure!(disk.digest == winner_digest && disk.document == winner);
        Ok(())
    }

    #[tokio::test]
    async fn source_recovery_reports_both_reload_failures() -> Result<()> {
        let (_directory, update) = source_recovery_fixture()?;
        let attempts = std::cell::Cell::new(0);
        let error = reload_published_policy_or_restore_with(&update, || async {
            attempts.set(attempts.get() + 1);
            anyhow::bail!("injected reload failure {}", attempts.get());
        })
        .await
        .err()
        .context("rejected recovery reload unexpectedly succeeded")?;
        let detail = format!("{error:#}");
        anyhow::ensure!(
            detail.contains("injected reload failure 1")
                && detail.contains("injected reload failure 2")
        );
        anyhow::ensure!(attempts.get() == 2);
        anyhow::ensure!(
            Some(
                crate::policy_source::load(&update.path)
                    .await?
                    .digest
                    .as_str()
            ) == update.parent_digest.as_deref()
        );
        Ok(())
    }

    #[tokio::test]
    async fn successful_source_reload_keeps_the_published_document() -> Result<()> {
        let (_directory, update) = source_recovery_fixture()?;
        reload_published_policy_or_restore_with(&update, || async { Ok(()) }).await?;
        let disk = crate::policy_source::load(&update.path).await?;
        anyhow::ensure!(
            disk.digest == update.digest && disk.document == recovery_source("fixture:economy")?
        );
        Ok(())
    }

    #[tokio::test]
    async fn config_validate_rejects_a_missing_bound_policy() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bitrouter.yaml");
        tokio::fs::write(
            &path,
            "routers:\n  coding:\n    selection:\n      kind: policy\n      base_model: fixture/strong\n      policy: missing\n",
        ).await?;
        let source = crate::paths::ConfigSource::File(path);
        let report = validate_config(&source).await?;
        anyhow::ensure!(!report.valid, "missing policy unexpectedly validated");
        anyhow::ensure!(report.errors.iter().any(|error| error.contains("missing")));
        Ok(())
    }
}
