# [SUPERSEDED] CLI/TUI shared agents ops + headless-CLI consistency pass

> **Superseded.** This spec is preserved for historical context. It treats the
> CLI and TUI as co-equal product surfaces; the workspace has since pivoted
> to **headless-first** with the CLI as the canonical operator surface and
> the TUI as an opt-in interactive mode. The accompanying workspace reorg
> (bitrouter ↔ bitrouter-cli split, bitrouter-tui merge) is documented in
> `CHANGELOG.md`.

## Use case

The bitrouter binary exposes ~18 subcommands and the TUI exposes ~15
slash commands. The two surfaces are nominally the same product
(`bitrouter agents list` ≈ `/agents list`), but in practice:

1. **CLI output is not pipeable.** Every read command (`agents list`,
   `providers list`, `route list`, `models list`, …) emits human-formatted
   prose with box-drawing rules and ✓/✗ glyphs. Scripting against bitrouter
   today means parsing decorated text. `policy eval` is the only command
   that exchanges structured data with stdin.
2. **CLI banners and informational text are interleaved with data on
   stdout**, so `bitrouter providers list | grep openai` returns the
   "Providers / ─────" header alongside the matching row.
3. **Destructive verbs are inconsistent** (`route rm`, `wallet delete`,
   `policy delete`, `key revoke`, `agents uninstall`). `revoke` and
   `uninstall` are semantically distinct and should stay; `rm` vs
   `delete` is pure inconsistency.
4. **TUI re-implements ~250 LOC of agents/registry/install logic
   inline** (`bitrouter-tui/src/app/slash.rs::slash_agents_*`
   duplicates the registry fetch, install progress forwarder, and
   list formatting from `bitrouter/src/cli/agents.rs`). Formats
   drift: CLI prints `\u{2713} installed (...)` aligned to 20 chars;
   TUI prints `[{method}] {:<22} v{version}`. Same data, two
   formatters, two bug surfaces.

Three personas this hurts:

- **Operator on a server** wants `bitrouter agents list --output json |
  jq` to feed a dashboard. Can't today.
- **CI / scripts** want `bitrouter route add foo openai:gpt-4o` to fail
  cleanly with a non-decorated error on stderr and a structured success
  on stdout. Today success messages and headers all go to stdout.
- **TUI user** types `/agents install codex-acp` and gets a different
  progress UX than the CLI user running `bitrouter agents install
  codex-acp`, because the same registry/install pipeline is re-coded in
  each layer.

## Why is the current behavior insufficient?

Concretely:

- `bitrouter/src/cli/agents.rs::run_list` (line 17) and
  `bitrouter-tui/src/app/slash.rs::slash_agents_list` (line 243) both
  fetch the ACP registry, merge config + state, and render a list — but
  inline, with different field widths, different status strings, and
  different "fallback when registry unavailable" handling.
- `bitrouter-tui/src/app/slash.rs::spawn_progress_forwarder` (line 857)
  duplicates the install-progress fan-out already present in
  `cli/agents.rs::run_install`'s reporter task (line 118).
- `bitrouter/src/cli/route.rs` uses `reqwest::blocking::Client` (lines
  96, 117) inside `#[tokio::main]`. Fine for CLI but unreachable from
  the TUI loop. (TUI doesn't expose `/route` today; this only matters
  if/when it does.)
- CLI list commands use `println!` for the data rows *and* for the
  decorative banners (e.g. `cli/providers.rs:19–23`,
  `cli/agents.rs:69–73`). There is no single point where output
  format is decided.
- No `Serialize` impl exists on any "list result" type today; adding
  `--output json` requires inventing those types ad hoc across 8
  modules.

Net effect: bitrouter is operable but not *integratable*, and the two
front-ends drift further apart with every feature.

## Proposed behavior

Two pieces, designed to land together but reviewable in isolation. **No
new crate.** All changes happen inside existing crates, sized to the
actual duplication rather than a hypothetical full ops surface.

### A. Consolidate agents ops in `bitrouter-providers`

The only meaningful cross-binary duplication is ACP agents
(list/install/uninstall/update). The `bitrouter-providers` crate
already owns every primitive these compose — `acp::registry`,
`acp::eager`, `acp::state`, `acp::discovery`, `acp::shim` — and **both
binaries already depend on it**:

- `bitrouter/Cargo.toml:55` —
  `bitrouter-providers = { workspace = true, features = ["agentskills"] }`
- `bitrouter-tui/Cargo.toml:13` —
  `bitrouter-providers = { workspace = true, features = ["acp"] }`
- The CLI's agents commands already import `acp::{eager, registry,
  state, types, discovery, shim}` directly (`cli/agents.rs:6–9, 64,
  227, 232–243`); the TUI's slash commands import the same modules
  (`slash.rs:250–298`).

Adding `bitrouter-providers/src/acp/ops.rs` is *finishing the
abstraction the crate already implies* — its existing modules are the
building blocks; `ops` is the assembly. No new dependency, no new
feature flag (it lives behind the existing `acp` feature).

**Module shape**

```
bitrouter-providers/src/acp/
  ops.rs               // new
    pub mod types;     // AgentList, AgentInfo, InstalledAgent, OpError
    pub fn list_agents(config: &BitrouterConfig, paths: &AcpPaths,
                       refresh: bool) -> Result<AgentList, OpError>;
    pub fn install_agent(id: &str, config: &BitrouterConfig,
                         paths: &AcpPaths) -> OpHandle<InstalledAgent>;
    pub fn uninstall_agent(id: &str, paths: &AcpPaths)
                       -> impl Future<Output = Result<(), OpError>>;
    pub fn update_agents(target: Option<&str>, config: &BitrouterConfig,
                         paths: &AcpPaths) -> OpHandle<Vec<InstalledAgent>>;
    pub fn check_routing(config: &BitrouterConfig) -> RoutingCheck;
```

`AcpPaths` is a small struct (`cache_dir`, `agents_dir`,
`agent_state_file`) carved out of the binary's `RuntimePaths` —
defined in `bitrouter-providers` so it doesn't require importing
the binary's path module. Today both binaries already pass these
three paths separately into the underlying acp::* primitives, so the
struct is purely a grouping convenience.

**Return-type discipline**

Each operation returns a `Result<T, OpError>` where `T` is a
plain-old-data struct deriving `Serialize`. No `println!`, no
`eprintln!`. Example:

```rust
#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    pub id: String,
    pub version: Option<String>,
    pub installed: Option<InstalledRecord>,
    pub on_path: bool,
    pub from_registry: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentList {
    pub registry_version: Option<String>,
    pub registry_url: String,
    pub agents: Vec<AgentInfo>,
    pub warnings: Vec<String>, // e.g. "registry unavailable: ..."
}
```

CLI's renderer turns `AgentList` into the existing pretty-print; JSON
mode does `serde_json::to_writer(stdout(), &list)`. TUI iterates
`list.agents` and emits scrollback rows in its own style. Same data,
two formatters, one fetch path.

**Long-running operations (install / update)**

Progress streaming is preserved via channels. The op returns an
`OpHandle`; the caller chooses how to render each event.

```rust
pub enum ProgressEvent {
    Downloading { bytes: u64, total: Option<u64> },
    Extracting,
    Done { path: PathBuf },
    Failed { message: String },
}

pub struct OpHandle<R> {
    pub progress: mpsc::Receiver<ProgressEvent>,
    pub result:   tokio::task::JoinHandle<Result<R, OpError>>,
}
```

`cli/agents.rs::run_install` becomes ~20 lines of "forward progress
to println; await result; render". `slash::slash_agents_install`
becomes ~20 lines of "forward progress to `AppEvent::InstallProgress`;
await result; push scrollback row". The existing
`spawn_progress_forwarder` in `slash.rs:857` and the reporter task in
`cli/agents.rs:118` collapse into the ops layer.

**Error type**

```rust
// bitrouter-providers/src/acp/ops.rs
#[derive(Debug, thiserror::Error)]
pub enum OpError {
    #[error("not found: {kind} '{id}'")]
    NotFound { kind: &'static str, id: String },

    #[error("registry: {0}")]
    Registry(String),

    #[error("install: {0}")]
    Install(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl OpError {
    /// CLI exit code mapping. Stable across releases.
    pub fn exit_code(&self) -> i32 {
        match self {
            OpError::NotFound { .. } => 4,
            OpError::Registry(_)     => 8,
            OpError::Install(_)      => 9,
            OpError::Io(_)           => 1,
        }
    }
}
```

### B. CLI compute/render convention (in-binary)

The remaining CLI commands (`route`, `tools`, `models`, `key`,
`policy`, `providers`, `status`) have **no current TUI consumer**.
For these, the right deduplication is not a shared library — it's a
**module-internal split between compute and render** so we get
`--output json` uniformly without leaving the binary.

**Convention**

Each `bitrouter/src/cli/foo.rs` exposes three things:

```rust
// 1. A Serialize struct for the data the command returns.
#[derive(Serialize)]
pub struct FooListData { ... }

// 2. A pure compute function — talks to admin API / reads config /
//    inspects local state. Returns the struct, no formatting.
pub async fn query_list(config: &BitrouterConfig, addr: SocketAddr)
    -> Result<FooListData, CliError>;

// 3. A text renderer — takes the struct, writes to a Writer.
pub fn render_list_text(data: &FooListData, w: &mut impl Write)
    -> io::Result<()>;
```

`run_list()` (called from `main.rs`) becomes:

```rust
pub async fn run_list(config: &BitrouterConfig, addr: SocketAddr,
                     output: OutputFormat) -> Result<(), CliError> {
    let data = query_list(config, addr).await?;
    match output {
        OutputFormat::Text => render_list_text(&data, &mut io::stdout())?,
        OutputFormat::Json => serde_json::to_writer(io::stdout(), &data)?,
    }
    Ok(())
}
```

This is just code organization — no new module, no new crate. It
makes the data/presentation seam explicit *per command*, which is
what `--output json` needs and what the stdout/stderr audit needs.

**Shared types in `bitrouter-config`**

A handful of view structs are useful in both binaries even though no
function is shared. Add to `bitrouter-config`:

```rust
// bitrouter-config/src/view.rs (new, ~50 LOC)
#[derive(Serialize)]
pub struct ProviderSummary {
    pub name: String,
    pub api_base: Option<String>,
    pub auth_kind: AuthKind, // None | ApiKey | OAuth
}

#[derive(Serialize)]
pub struct ModelSummary {
    pub name: String,
    pub providers: Vec<String>,
    pub strategy: RoutingStrategy,
}

impl ProviderSummary {
    pub fn from_config(config: &BitrouterConfig) -> Vec<Self> { ... }
}
```

`bitrouter-config` is already a workspace dep of both binaries; this
adds 50 LOC of view structs with `Serialize`, no new dependency.

**Admin-API client**

`cli/admin_auth.rs` converts from `reqwest::blocking::Client` to
`reqwest::Client` (async). Stays in the binary — the TUI doesn't call
admin endpoints today, so there's nothing to share yet. If the TUI
later grows `/route` etc., we promote `admin_auth` into
`bitrouter-providers` (or carve out `bitrouter-ops`) *then*, when it
pays for itself.

### C. Headless-CLI consistency pass

Three changes, enabled by the compute/render split in §B:

#### C1. `--output <text|json>` everywhere (priority 1)

- New global flag on `Cli` in `bitrouter/src/main.rs`:
  ```rust
  /// Output format for read commands. Defaults to `text` on a TTY,
  /// `json` when stdout is not a TTY (CI / pipes).
  #[arg(long, short = 'o', global = true, value_enum)]
  output: Option<OutputFormat>,
  ```
- Commands in scope: `agents list`, `providers list`, `route list`,
  `models list`, `tools list`, `tools status`, `key list`, `policy
  list`, `policy show`, `status`, `whoami`. Each gets a `Serialize`
  return per §B.
- Mutating commands (`route add`, `agents install`) emit a one-line
  result row in text mode; in JSON mode they emit a single object
  (`{"result":"ok","id":"foo"}` or `{"result":"error","message":"..."}`).
- Default behavior auto-detects via `stdout().is_terminal()`: TTY →
  text, pipe → json. Override with `--output text` or
  `--output json`. (Matches `kubectl`'s recent direction.)

#### C2. stdout/stderr discipline (priority 2)

Audit and split:

| Today (stdout) | Should be | Examples |
|---|---|---|
| Decorative banners | stderr | "BitRouter v0.31.2 / ─────", `run_help_status`, `Providers ─────` |
| Section headers | stderr | "Agents", "Providers", "Recent events" |
| Counts / summaries (`(no agents installed)`) | stderr | `cli/route.rs:65`, `cli/providers.rs:12` |
| Data rows | **stdout** | The actual list contents |
| Mutating success line (`route 'X' added`) | **stdout** | One machine-parseable line |
| Warnings (`falling back to default config`) | stderr | `main.rs:867` (already stderr — good) |
| Progress (`[X] downloading: 42%`) | stderr | All install progress |
| Interactive prompts | stderr (dialoguer default) | wallet passphrase, reset confirm |

Concretely: every `println!` in `bitrouter/src/cli/*.rs` and the
`run_help_status` / `print_first_run_guidance` paths in `main.rs` gets
re-classified. Rough audit: ~40 sites, ~30 become `eprintln!`.

Drop decorative Unicode (✓ ✗ ─) when `!stdout().is_terminal()` —
swap for `[ok]` / `[!]` / plain hyphens. Honor `NO_COLOR`.

#### C3. Destructive-verb consistency (priority 3)

**Standardize on `delete`** for commands that remove a resource from
local state. Clearer to new users than `rm`, matches the verb used by
`wallet`, `policy`, and most cloud CLIs (`aws`, `gcloud`, `doctl`).

Renames:

| Today | Becomes | Note |
|---|---|---|
| `route rm` | `route delete` | Keep `rm` as a hidden alias for one minor release. |
| `wallet delete` | `wallet delete` | No change. |
| `policy delete` | `policy delete` | No change. |
| `key revoke` | `key revoke` | Keep — semantically distinct (invalidate, don't remove). |
| `agents uninstall` | `agents uninstall` | Keep — semantically distinct (remove disk artifacts). |

## Migration plan

Three phases, each independently releasable.

**Phase 1 — `acp::ops` extraction**
- Add `bitrouter-providers/src/acp/ops.rs` with `list_agents`,
  `install_agent`, `uninstall_agent`, `update_agents`,
  `check_routing`.
- Add `AcpPaths`, `OpError`, `OpHandle<R>`, `ProgressEvent`.
- Re-point `cli/agents.rs::run_*` at the new functions — they
  become thin renderers.
- Re-point `slash.rs::slash_agents_*` at the new functions —
  delete `spawn_progress_forwarder` (collapsed into `OpHandle`).
- Verify formatting parity on both surfaces.

**Phase 2 — CLI compute/render split**
- Apply the §B convention to each CLI module: `query_*` returns a
  `Serialize` struct, `render_*_text` formats it.
- Add `bitrouter-config/src/view.rs` with `ProviderSummary`,
  `ModelSummary`.
- Convert `cli/admin_auth.rs` to async `reqwest::Client`.
- Touches: `route`, `tools`, `models`, `key`, `policy`,
  `providers`, `status`, `agents` (text renderer half only —
  data half lands in Phase 1).

**Phase 3 — output flag + stdout/stderr audit + verb rename**
- Add global `--output text|json` with TTY-based default.
- Wire renderer dispatch in every `run_*` site.
- Walk every `println!` in `bitrouter/src/cli/*` and `main.rs`;
  reclassify per the §C2 table.
- Drop decorative Unicode when stdout isn't a TTY.
- Add `route delete` (alias `route rm` for one release).
- Update help text + CHANGELOG.

Phases 1–2 are pure refactors (behavior parity). Phase 3 is the only
one with surface changes — it warrants a CHANGELOG entry and a
minor-version bump.

## Out of scope

- Wallet management ops (`wallet create/import/info/...`). Stay in CLI
  for now — they need interactive passphrase entry that doesn't
  compose well with non-interactive callers.
- The TUI's session-management slash commands (`/session *`, `/obs`).
  These are TUI-native and have no headless equivalent.
- The interactive `bitrouter init` wizard. Keep CLI-only.
- A "TUI shells out to `bitrouter`" approach. Rejected — would force
  the TUI to parse its own pretty-printer, can't carry interactive
  flows, and forks a process per slash command.
- A new `bitrouter-ops` crate. Considered, rejected: ~75% of its
  modules would have a single caller (the CLI), making it a
  crate-shaped solution to a module-shaped problem. Revisit if the
  TUI later grows a real admin surface (`/route`, `/tools`, `/key`).
