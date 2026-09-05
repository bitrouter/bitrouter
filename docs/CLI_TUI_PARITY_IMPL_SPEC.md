# Implementation spec: CLI ↔ TUI parity, track 1

Status: **proposed — nothing built** · Date: 2026-09-05 · Branch: `claude/cli-tui-parity-spec`
· Rationale of record: [`CLI_TUI_PARITY_SPEC.md`](CLI_TUI_PARITY_SPEC.md) (the
*research spec*). This document does not repeat its argument; every "why" below
is a pointer into it by section.
· Target tree: **`origin/claude/mcp-drop-complete` at `07a2096d`** — that is
`main` (`4fcd8018`) plus the still-open stack #869 → #870 → #875. Every
`file:line` below was read at that commit. See [§1.1](#11-which-tree-this-is-written-against)
for why not `main`.

> **What this is.** The thing an engineer builds from: the Rust for the type
> changes, the shape of the dispatcher, phase boundaries with the files each
> touches, the guard tests as tests, and what each open decision blocks. If a
> line here would not be consulted while writing code, it belongs in the
> research spec and was cut.
>
> **What this is not.** Track 2 — the ~25 commands that are hostile in a
> session — is not designed here. Each needs a bespoke affordance and
> [research §3.1](CLI_TUI_PARITY_SPEC.md#31-the-goal-the-maintainer-actually-stated)
> / [D5](CLI_TUI_PARITY_SPEC.md#d5--settle-the-scope-empirically-before-phase-2)
> has not ordered them. Every phase below is **track 1**, plus one phase for the
> prompt-expansion registry that D2 decided.

## Contents

- [1. Ground truth](#1-ground-truth)
- [2. The amendments, and what they changed here](#2-the-amendments-and-what-they-changed-here)
- [3. Type changes](#3-type-changes)
- [4. The dispatcher](#4-the-dispatcher)
- [5. Rendering](#5-rendering)

---

## 1. Ground truth

### 1.1 Which tree this is written against

The research spec was measured at `43ae57d8` on `claude/actions-table-phase04`.
That commit is **not on `main`**: `main` (`4fcd8018`) has no
`crates/bitrouter-mcp/src/actions/` at all. The `ACTIONS` table this whole design
extends exists only on the open stack:

| PR | Branch | Base | State (2026-09-05) | What it adds |
|---|---|---|---|---|
| #869 | `claude/actions-table-phases-1-3` | `main` | open | `ACTIONS` with `status`, `list_models`, `route` |
| #870 | `claude/actions-table-phase04` | #869 | open | `skills_search`, `skills_get` rows; one skills root |
| #875 | `claude/mcp-drop-complete` | #870 | open | removes the `complete` tool and its row |

**This spec is buildable only once that stack merges.** It is written against the
stack's tip so that nothing here has to be re-derived when it does. #866
(`878178a4`, the headless permission policy and `chat/effects.rs`) is on `main`
and therefore also on the tip.

### 1.2 Anchors an implementer touches

Read at `07a2096d`. Where the research spec gives a different number, the
research spec is stale; the numbers below are the ones to use.

| What | Where |
|---|---|
| The table | `crates/bitrouter-mcp/src/actions/mod.rs` — `pub struct ActionSpec { id, cli_leaf, mcp_tool, output_schema }`, `pub const ACTIONS: &[ActionSpec]` with **five** rows: `status`, `list_models`, `route` (tool `route_preview`), `skills_search`, `skills_get` |
| The four port traits | `crates/bitrouter-mcp/src/actions/{status,models,route,skills}.rs` — `StatusQuery::status(&CallerAuth)`, `ModelsQuery::list_models(&CallerAuth)`, `RouteQuery::route(RouteInput)`, `SkillsQuery::{list, get}` |
| Their app-side implementations | `apps/bitrouter/src/actions/{status,models,route,skills}.rs` — `DaemonStatus::new(socket, Option<ConfigSource>)`, `RoutableModels::new(ConfigSource, Option<PathBuf>)`, `RouteAction::new(ConfigSource, Option<PathBuf>)`, `InstalledSkills::new(Vec<SkillsRoot>)`; each has an inherent `report(..)` the CLI leaf calls and a trait impl the MCP tool calls |
| The CLI leaves | `apps/bitrouter/src/main.rs:3363` `async fn status`, `:3462` `async fn route`, `:3510` `async fn models` — each constructs the app-side action and calls `.report()` |
| The existing guards | `apps/bitrouter/src/main.rs:5893` `every_mcp_tool_has_an_actions_row`, `:5912` `every_actions_row_resolves_to_a_cli_leaf`, `:5949` `every_actions_row_matches_its_tools_output_schema`; the fully-stubbed builder `every_tool()` at `:5827` |
| The two-surface acceptance test | `apps/bitrouter/src/actions/route.rs:361` `both_surfaces_produce_the_same_report` (and `both_surfaces_apply_the_policy_table` below it) |
| HTTP profile guards | `crates/bitrouter-mcp/src/server.rs:1258` `http_profile_never_carries_host_bound_tools` (asserts `["list_models", "status"]`, and `["list_models"]` for a status-less backend); `crates/bitrouter-mcp/src/lib.rs:298` `wiring_skills_into_stdio_does_not_widen_the_http_profile` |
| The two profiles | `crates/bitrouter-mcp/src/lib.rs:158` `fn stdio_profile`; `crates/bitrouter-mcp/src/server.rs:766` `fn http_profile` (crate-private; `:762` `http_profile_for_test`); the host-bound invariant in prose at `server.rs:714`–`:731` |
| Multi-tenant regression test | `crates/bitrouter-mcp/tests/multitenant_http.rs:58` `two_callers_forward_distinct_bearers` |
| The reducer's dispatch | `crates/bitrouter-tui/src/machine.rs:387` `if line.trim() == "/commands"`, `:394` `if line.trim() == "/route"` gated on `state.routable`; `State::new(routable: bool)` at `:115`; `routable` re-asked at `:582` (`Picker::open`) |
| The reducer's effects | `machine.rs:204` `Effect::ListRoutes`, `:206` `Effect::SetRoute(String)`; no `ResetRoute`. `Notice::{Say, Commands}` |
| The reducer's phases | `machine.rs:77` `enum Phase { Idle, Turn, Answering(Prompt), Routing(Picker) }` |
| `NOT_ROUTABLE` | `machine.rs:72` |
| The interactive driver | `apps/bitrouter/src/chat/session.rs:250`–`:405` `run` → `drive`; the effect `match` at `:352`–`:400`; `Notice::Commands` arm at `:368` renders `bitrouter_tui::render::session::commands(view.commands())`; `State::new(can_reroute(client))` at `:267` |
| The piped driver | `chat/session.rs:444` `chat_plain`; `:476` `if line.trim() == "/route"` → `"/route needs a terminal"` |
| The wire half | `apps/bitrouter/src/chat/effects.rs` (140 lines) — `Wire::apply` runs `Resolve`, `Cancel`, `ListRoutes`, `SetRoute`; returns every other effect to the driver |
| The chat guard | `apps/bitrouter/src/chat/mod.rs` `the_chat_module_reaches_nothing_daemon_wide` — scans `effects.rs`, `input.rs`, `session.rs` for `crate::daemon`, `crate::metering`, `crate::policy`, `MeteringStore`, `bitrouter_sdk::config`, `control_socket`, `DaemonRouteControl`, `DaemonSessionCost`, `LocalControllerBinding` |
| The launch half | `apps/bitrouter/src/acp_cli.rs:1167` `struct LocalControllerBinding { socket_path, api_principal, controller_instance_id }`; `:1175` `open(source, config, routed, explicit_base_url) -> Option<Self>` (returns `None` under `--direct` or an explicit `--base-url`); `:1202` `route_control()`; `:1212` `session_cost()`; the `chat` launch builds the binding at `:1295` and calls `chat::session::run(..)` at `:1501` with `source`, `config`, `routed` and the binding all in scope |
| `ControlledSession` | `acp_cli.rs:1982` — `pub(crate) client: AcpClient`, `binding: Option<LocalControllerBinding>` |
| The in-process controller | `acp_cli.rs:2087` `agent_client_protocol::Channel::duplex()` |
| Headless presentation | `acp_cli.rs:104` `enum PromptFormat { Json, Text, Quiet }`; `:2325` `enum Presenter` — `Json` writes one NDJSON line per update (no function named `emit_update` exists) |
| Routing flags | `acp_cli.rs:83` `struct RoutingOptions { direct, base_url, model, no_start }` |
| The daemon's lease commands | `apps/bitrouter/src/daemon.rs:61`–`:99` `DaemonCommand::{AcpControllerCleanup, AcpRouteList, AcpRouteSet, AcpSessionSpend, AcpRouteReset}`, each keyed by `(api_principal, controller_instance_id, session_id)`; handler for `AcpRouteSet` at `:644`. **There is no variant that lists controllers or sessions** |
| The SDK client's route surface | `crates/bitrouter-sdk/src/acp/client.rs:364` `enum RouteMethod { List, Set, Reset }`; `:393` `RouteControlCapability`, `:400` `from_init`, `:430` `allows`; `:460` `enum RouteError { Unavailable, InvalidRoute, Other }`; `:721` `route_list`, `:734` `route_set`, `:743` `route_reset` |
| Agent commands on the wire | `crates/bitrouter-sdk/src/acp/translate.rs:59` `struct AgentCommand { name, description }` (drops ACP's `input`); `:223` `SessionUpdate::AvailableCommandsUpdate` → `SessionUpdateKind::AvailableCommands` |
| The journal's copy | `crates/bitrouter-tui/src/journal.rs:179` `self.commands = update.available_commands` |
| The TUI's command renderer | `crates/bitrouter-tui/src/render/session.rs:69` `pub fn commands(&[AvailableCommand]) -> Vec<Line<'static>>`; empty case `:72` `"this agent advertises no commands"` |
| The view's notice API | `crates/bitrouter-tui/src/view.rs:106` `notice(text)`, `:111` `notice_lines(Vec<Line<'static>>)` (replaces, does not append), `:120` `clear_notice` |
| Plain-text seam | `apps/bitrouter/src/output/mod.rs:83` `Output::render_to_vec(&dyn CliReport) -> Vec<u8>` renders with `Theme::none()`; `output/human.rs:39` `Theme::none`, `:51` `Theme::for_stdout` |
| `CliReport` impls for the shared reports | `apps/bitrouter/src/output/reports/daemon.rs:111` `StatusReport`, `:192` `RouteReport`; `output/reports/routing.rs:17` `ModelsReport` |
| Crate walls | `crates/bitrouter-tui/Cargo.toml` — depends on `agent-client-protocol-schema`, `ratatui`, `serde_json`, `crossterm`, `similar`, `unicode-width` and **nothing of BitRouter's**; `apps/bitrouter/Cargo.toml:60`–`:63` — no `ratatui`, depends on `bitrouter-tui` and `bitrouter-mcp` |
| Docs to keep in lockstep | `docs/CLI.md:535`–`:537` (the in-session command table); `skills/bitrouter/references/cli.md:207` |

### 1.3 Facts the design rests on, checked

- **`bitrouter-tui` cannot see `ACTIONS`.** It does not depend on `bitrouter-mcp`
  (or `bitrouter-sdk`), and [research §12](CLI_TUI_PARITY_SPEC.md#12-rendering)
  rejects adding that edge. So the reducer can never hold an `ActionSpec`. It has
  to be *handed* plain data derived from the table, the way it is handed
  `routable` today. This decides the dispatcher's shape ([§4](#4-the-dispatcher)).
- **`chat/` can name `crate::actions`.** The guard's forbidden list does not
  include it, and `apps/bitrouter/src/actions/` already exists and already names
  `crate::daemon` internally. A type in `crate::actions` that is *handed* to
  `chat/` is the same move as `route_control`.
- **The three app-side read actions take `(ConfigSource, Option<PathBuf>)` or
  `(PathBuf, Option<ConfigSource>)`.** Both are in scope at `acp_cli.rs:1501`
  (`source`, `config`; the socket path via `crate::daemon::socket_path_for`).
  Nothing new has to be threaded to construct them there.
- **`ModelsQuery::list_models` and `StatusQuery::status` take `&CallerAuth`; the
  local implementations document that they ignore it.** The session surface is
  local and single-tenant, so it passes `CallerAuth::default()`, exactly as the
  stdio MCP profile does.
- **`RouteReport`, `StatusReport` and `ModelsReport` all implement `CliReport`
  app-side**, so `Output::render_to_vec` already renders each to plain bytes.
- **The daemon cannot enumerate controllers or sessions, and cannot notify a
  controller that a lease changed.** `DaemonCommand` has no listing variant; the
  controller reads spend through a cache it refreshes itself and reads route
  state only when the TUI asks (`AcpRouteList`). This is what makes D7(b)
  unbuildable as recorded — [§2.2](#22-d7-decided-b--the-cli-leaf-is-not-buildable-in-this-stack)
  below.

---

## 2. The amendments, and what they changed here

Three amendments landed in the research spec at `2afddf2c` after most of it was
written. Each is checked against the sections written before it. Only the
resolution is recorded here; the argument is in the research spec.

### 2.1 D2 decided: two registries

[Research D2](CLI_TUI_PARITY_SPEC.md#d2--the-user-configured-escape-hatch) and
[D12](CLI_TUI_PARITY_SPEC.md#d12--take-693s-inversion) now say: one dispatcher,
resolving against a closed guarded registry (`ACTIONS`) **and** an open
prompt-expansion registry with no `run:` key. [§9](CLI_TUI_PARITY_SPEC.md#9-the-mechanism),
[§10](CLI_TUI_PARITY_SPEC.md#10-source-of-truth), [§12](CLI_TUI_PARITY_SPEC.md#12-rendering)
and [§13](CLI_TUI_PARITY_SPEC.md#13-the-guards) were written for one.

**What still holds.** §9's mechanism (local dispatch through the action ports)
is untouched — prompt expansion never reaches a port; it produces a prompt.
§10's "`ACTIONS` extends, no sibling" holds *for router actions*; the second
registry is not a sibling table of the same kind, it is config. §12's rendering
holds — a prompt-expansion command renders nothing of its own; the turn does.

**What does not hold, and how it is resolved.**

- **§13's guards are exhaustive over `ACTIONS` and silent about the open
  registry, and that is correct rather than a gap** — but only if the two sets
  cannot overlap. So the one new rule is a **load-time collision check**: a
  prompt-expansion command whose name equals any `tui_command` (or its alias)
  is a configuration error, reported when `bitrouter.yaml` is loaded. That is
  the "cannot collide in any way that matters" clause of D2 made mechanical,
  and it is what lets G1–G3 stay exhaustive.
- **The resolver has three sources, not two.** [D14](CLI_TUI_PARITY_SPEC.md#d14--how-the-two-command-sources-are-distinguished)'s
  `source: "bitrouter" | "agent"` gains `"config"`. The precedence rule stays
  D14's — local wins, shadowed entries are listed as shadowed — with `config`
  ordered between `bitrouter` and `agent`. It cannot collide with `bitrouter`
  (rejected at load), so the only shadowing that can occur is `config` over
  `agent`, and `bitrouter` over `agent`.
- **The dispatcher gains one arm** — `Resolution::Expand(prompt)`
  ([§4](#4-the-dispatcher)).

**Is the two-registry model coherent?** Yes, on one condition that this spec
makes structural: **the two are never held in one collection.** The reducer's
`State` holds `commands: Vec<Command>` (derived from `ACTIONS`) and
`prompt_commands: Vec<PromptCommand>` (derived from config) as two fields, and
the resolver consults them in order. The only thing connecting them is that
order, stated once. That is why it does not collapse into OpenCode's one-map
with-a-`source`-tag ([research §6.9.3](CLI_TUI_PARITY_SPEC.md#693-opencode--the-one-that-got-closest-and-the-inversion-that-did-it)):
OpenCode needed a documented last-writer-wins rule because two kinds of entry
shared a map and could collide at runtime; here the closed set is checked at
compile-and-test time, the open set is checked against it at config load, and
no runtime precedence rule between them is ever needed. The `source` field on
the `/commands` *report* row ([§3.4](#34-the-commands-report--cratesbitrouter-mcpsrcactionscommandsrs-new-phase-3))
is a discriminator on a merged **view**, not on the registry — the registries
stay separate.

### 2.2 D7 decided (b) — the CLI leaf is not buildable in this stack

[Research D7](CLI_TUI_PARITY_SPEC.md#d7--does-the-route-picker-need-a-cli-leaf)
records the maintainer choosing **(b)**: `route/set` gains a CLI leaf, so it
leaves set C. Two consequences for sections written earlier, and one finding.

**P2's worked example is gone.** [Research §3](CLI_TUI_PARITY_SPEC.md#3-the-goal-restated)
P2 says a row with a `tui_command` and no `cli_leaf` is legal only when its
subject is a live session, and used `route/set` as the example. With D7(b) the
example leaves set C. **P2 still says something**, because two rows in this spec
are set-C members that D7(b) does not touch:

- `commands` — the agent's advertised list is an artefact of one live ACP
  connection. Its CLI leaf (`acp commands`, a later phase) opens a *fresh*
  session to ask; it cannot ask about the one the TUI is in.
- `route_reset` — D7(b) names a *setter*; nothing decided a reset leaf. Until
  one exists, reset is TUI-only.

So P2's clause is kept, with the example changed, and G2 encodes it as
`cli_leaf.is_none() ⇒ reach == Reach::SessionBound`.

**D7(b) as recorded cannot be built in this stack, and this spec does not
schedule it.** Stated as a finding rather than a disagreement: the decision is
an intent; the tree lacks three things the intent needs.

1. **Nothing can name a running session from outside its process.**
   `DaemonCommand` has no variant that lists controllers or sessions
   ([§1.2](#12-anchors-an-implementer-touches)); `AcpCmd` has only `Serve` and
   `Prompt`. A second terminal has no `--session <id>` to type. The research
   spec's own prerequisite (1) — session discovery — is unbuilt.
2. **Leases are keyed by the controller's own identity.** `AcpRouteSet` takes
   `(api_principal, controller_instance_id, session_id)`, and
   `controller_instance_id` is minted per `chat` launch
   (`LocalControllerBinding::open`). A CLI process would have to learn and
   present another process's triple. The daemon does not authenticate the
   triple — the socket is trusted — so this is discoverability, not security;
   but the namespace exists so that one controller's leases are its own, and a
   leaf that writes into another controller's namespace is new semantics the
   daemon has no rule for.
3. **The TUI cannot learn that its route changed underneath it.** There is no
   daemon→controller notification. `Effect::RouteInForce` is emitted only when
   the TUI's own `SetRoute` returns. A route set from a second terminal would
   leave the footer showing the old route — a direct violation of
   [research invariant 5](CLI_TUI_PARITY_SPEC.md#15-invariants) (*a write
   reports what is in force*). Closing this needs either a push channel on the
   duplex or a poll, neither of which exists.

The research spec's paragraph under D7(b) already notes item 1 and half of item
2. It does not note item 3, which is the one that makes the leaf **unsafe to
ship without a daemon-side design** — that design is #863's territory (the
daemon control plane), not this spec's. The maintainer's stated need — *a user
or an agent can pick a model and its provider from the headless CLI* — is met
today at launch by `RoutingOptions::model` (`acp_cli.rs:83`), which accepts
`provider:model`. What D7(b) adds is mid-session change from a second process,
and that is what is blocked.

**Default so work proceeds:** `route_set` and `route_reset` ship as set-C rows
(`cli_leaf: None`, `reach: SessionBound`) under P2 as written. When #863 (or a
successor) supplies session discovery and a lease-change notification, the leaf
is a row edit plus a clap subcommand, and G2 will stop needing the set-C clause
for `route_set`. Recorded as blocker **B-D7** in the open-decisions section.

### 2.3 §3.1's two tracks replaced §2.4's 4–6 target

[Research §14](CLI_TUI_PARITY_SPEC.md#14-phases) was written against the 4–6
target and then annotated *"every phase below is track 1"*. Checked: that
annotation is correct — every phase there is a read, or `/route reset`, or the
agent's command list, none of which is hostile in-session. Nothing in §14
belongs to track 2, so nothing had to be removed. What changed for this spec is
only that the phases are **stated as track 1 and stop there**; the research
spec's phase 4 ("argument parsing, only if phase 3 needs it") is folded into
the open-decisions section under D9 rather than kept as a phase, because it has
no deliverable of its own.

**One phasing correction beyond the amendments.** Research §14's phase 0
(discoverability) and phase 1 (`/route reset` and the write model) are merged
here into one phase. Phase 0 alone would add a `route_reset` row reachable on
**no** surface — no leaf, no tool, no TUI command — which contradicts the
table's own charter (*"only actions with more than one surface belong here"*)
and CLAUDE.md rule 4. The reset effect is ~15 lines. They ship together.

---

## 3. Type changes

Every type below has a named reader in some phase. Three things the research
spec proposed are **cut** for having none, each with the reason at the point
it would have appeared.

### 3.1 `ActionSpec` — `crates/bitrouter-mcp/src/actions/mod.rs`

```rust
/// One action, and the surfaces that answer it.
pub struct ActionSpec {
    /// Stable action id, e.g. `"status"`.
    pub id: &'static str,
    /// The CLI leaf that answers it, space-separated (`"skills list"`), or
    /// `None` when no CLI command does.
    pub cli_leaf: Option<&'static str>,
    /// The MCP tool that answers it, or `None` when no tool does.
    pub mcp_tool: Option<&'static str>,
    /// The TUI slash command that invokes it, without the leading `/`,
    /// space-separated when the second word is fixed (`"route reset"`).
    /// `None` is a declaration — this action is not offered in a session, by
    /// rule R1 of the research spec — not a backlog. Read by the app when it
    /// builds the reducer's command list, and by guards G1 and G2.
    pub tui_command: Option<&'static str>,
    /// Whether the action observes or changes state. Read by G2 and G3 only;
    /// nothing branches on it at run time.
    pub effect: Effect,
    /// What a session must hold before the command is *offered*. Resolved by
    /// the driver once, at launch, from what the controller advertised; the
    /// action itself never asks. Read by the app when it fills
    /// `Command::unavailable`.
    pub requires: Requires,
    /// How far the answer travels. Read by G2 (a row with a `tui_command` and
    /// no `cli_leaf` must be `SessionBound`) and by the HTTP-profile guard
    /// (only `Portable` tools may appear on `/mcp-control`).
    pub reach: Reach,
    /// The shared report's JSON Schema, or `None` while the action has no
    /// shared report type yet. Unchanged.
    pub output_schema: Option<fn() -> rmcp::model::JsonObject>,
}

/// Observes or changes state.
///
/// Distinct from `bitrouter_tui::machine::Effect`, which the chat driver
/// imports. The driver never needs this one — only guards read it — so the
/// two names never meet in a `use` list; refer to this one by path
/// (`actions::Effect`) where both are in scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Safe to run at any time, including twice.
    Read,
    /// Changes state. `inverse` is the `id` of the row that undoes it — rule
    /// R5. Not `Option`: a write with no inverse is not admitted to the table
    /// at all, and no CLI-only write has a row.
    Write { inverse: &'static str },
}

/// What a session must hold for the command to be offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requires {
    /// Offered in every session. The answer may still degrade — `status`
    /// reports `running: false`, `list_models` and `route` fall back to
    /// config — but the report says so itself (`resolved_via`), so the driver
    /// has nothing to decide.
    Nothing,
    /// Needs the controller to have advertised `_bitrouter/route/*` for the
    /// method the action uses. Absent under `--direct` or an explicit
    /// `--base-url`: listed in `/commands` with the reason, not run.
    Binding,
}

/// How far an answer travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Answerable for any caller from any host. May appear on the HTTP
    /// profile — a backend that cannot answer omits the tool rather than
    /// fabricating (`server.rs:1258`).
    Portable,
    /// Resolves against the serving machine's own config, control socket, or
    /// installed-skills root. Local transports only (`server.rs:714`).
    HostBound,
    /// Its subject is a live session on the serving process. Local only, and
    /// the only `Reach` a row may have when it has a `tui_command` and no
    /// `cli_leaf` (P2's set-C clause).
    SessionBound,
}
```

**Cut from the research spec's proposal, and why.**

- `Requires::Daemon` ([research §10](CLI_TUI_PARITY_SPEC.md#10-source-of-truth)).
  No reader: the driver would do the same thing for `Daemon` as for `Nothing`
  (offer the command), because degradation already happens inside the port and
  is reported in `resolved_via`. A variant nothing branches on is
  documentation in the type system.
- `Reach::Degraded` ([research D13](CLI_TUI_PARITY_SPEC.md#d13--the-remote-transport-requirement)).
  No reader distinguishes it from `Portable`: the HTTP guard's question is
  "may this tool appear on `/mcp-control`", and both answer yes. The fact that
  `status` may be *omitted* by a status-less backend is already asserted
  literally by `http_profile_never_carries_host_bound_tools` and is a property
  of the backend, not the row.
- `Effect::Write { inverse: Option<..> }` → non-optional, above.

**`Reach` is kept** — the research spec's D13 says "build nothing until a
remote caller asks", which would have cut it too, but it has two readers
today: G2's set-C clause needs *some* marker for "subject is a live session"
(the old marker, `Effect::Write`, is wrong for `commands`, which is a read),
and the existing HTTP guard becomes table-driven by reading it (G6, in the
guards section). Both land in phase 0.

**The rows, at the end of phase 0**, with later phases' edits marked:

| `id` | `cli_leaf` | `mcp_tool` | `tui_command` | `effect` | `requires` | `reach` | `output_schema` |
|---|---|---|---|---|---|---|---|
| `status` | `status` | `status` | `None` → `Some("status")` in phase 1 | `Read` | `Nothing` | `Portable` | `Some` |
| `list_models` | `models` | `list_models` | `None` → `Some("models")` in phase 2 | `Read` | `Nothing` | `Portable` | `Some` |
| `route` | `route` | `route_preview` | `None` → `Some("preview")` in phase 2 | `Read` | `Nothing` | `HostBound` | `Some` |
| `skills_search` | `skills list` | `skills_search` | `None` | `Read` | `Nothing` | `HostBound` | `Some` |
| `skills_get` | `None` | `skills_get` | `None` | `Read` | `Nothing` | `HostBound` | `Some` |
| **`commands`** (new) | `None` → `Some("acp commands")` in phase 3 | `None` | `Some("commands")` | `Read` | `Nothing` | `SessionBound` | `None` → `Some` in phase 3 |
| **`route_set`** (new) | `None` | `None` | `Some("route")` | `Write { inverse: "route_reset" }` | `Binding` | `SessionBound` | `None` |
| **`route_reset`** (new) | `None` | `None` | `Some("route reset")` | `Write { inverse: "route_set" }` | `Binding` | `SessionBound` | `None` |

`route_set` and `route_reset` carry `output_schema: None` permanently: each has
one surface, so there is no second shape to hold it to, and the wire response
(`String` / `()`) is the SDK client's, not a report. The module doc's sentence
*"the backlog is empty today, so every row below carries a real schema"* must
be rewritten in phase 0 to say which rows have no schema and why.

### 3.2 The reducer's data — `crates/bitrouter-tui/src/machine.rs`

The reducer cannot see `ActionSpec` ([§1.3](#13-facts-the-design-rests-on-checked)).
It is handed this instead, built by the app from the rows that carry a
`tui_command`:

```rust
/// One of BitRouter's own slash commands, as the reducer needs to know it.
///
/// Built by the app from the `ACTIONS` rows that carry a `tui_command`; the
/// reducer never sees the table, so it cannot offer a command the table does
/// not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The words after the slash, space-separated: `"status"`, `"route reset"`.
    pub name: &'static str,
    /// The `ACTIONS` row id. Handed back unchanged in `Effect::Action`, and
    /// matched by the reducer only for the ids in `REDUCER_OWNED`.
    pub action: &'static str,
    /// One line for `/commands`.
    pub summary: &'static str,
    /// `Some(reason)` when the row's `requires` is unmet in this session. The
    /// command is still listed — with the reason — and typing it answers with
    /// the reason instead of running. (Invariant: absent, not dead; VS Code's
    /// visibility/executability split, research §6.5.)
    pub unavailable: Option<&'static str>,
}

/// A user-authored prompt-expansion command (research D2). Plain data: the
/// reducer substitutes and sends; it never runs anything. Phase 4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCommand {
    pub name: String,
    pub description: String,
    /// `$ARGUMENTS` is replaced by everything typed after the name.
    pub template: String,
}

/// The rows the reducer dispatches itself rather than as `Effect::Action`,
/// because each needs reducer-owned state — the journal's command list, or
/// the picker's phase. G1 asserts every entry is a row id with a
/// `tui_command`. Adding a name here without a row fails the build's tests.
pub const REDUCER_OWNED: &[&str] = &["commands", "route_set", "route_reset"];

/// Names that mean another command. G1 asserts no alias is itself a
/// `tui_command`.
pub const ALIASES: &[(&str, &str)] = &[("help", "commands")];
```

`State` changes (phase 0 unless marked):

```rust
pub struct State {
    pub phase: Phase,
    pub editor: Editor,
    pub prompts: usize,
    /// BitRouter's own commands, in `/commands` order. **Replaces `routable`.**
    pub commands: Vec<Command>,
    /// The user's prompt-expansion commands. Empty until phase 4.
    pub prompt_commands: Vec<PromptCommand>,
    pub queued: VecDeque<Prompt>,
}

impl State {
    /// `State::new(routable: bool)` becomes this. Every call site in
    /// `machine.rs`'s tests (`State::new(true)` / `State::new(false)`)
    /// changes to pass a command list; a helper `fn routable(bool) -> Vec<Command>`
    /// in the test module keeps them one line each.
    pub fn new(commands: Vec<Command>) -> Self { /* .. */ }

    /// Is this row offered *and* runnable here? Replaces the two reads of
    /// `routable` (`machine.rs:395`, `:582`).
    pub fn available(&self, action: &str) -> bool {
        self.commands
            .iter()
            .any(|c| c.action == action && c.unavailable.is_none())
    }
}
```

`Effect` and `Action` changes:

```rust
pub enum Effect {
    // .. existing variants unchanged ..
    /// Run this `ACTIONS` row through the app's ports and show the report as
    /// a notice. Emitted for every BitRouter command not in `REDUCER_OWNED`.
    /// `args` is the rest of the line, whitespace-split — never one string
    /// (research §6.9.6, finding 14). Phase 1.
    Action {
        action: &'static str,
        args: Vec<String>,
    },
    /// Drop this session's route lease: `_bitrouter/route/reset`. Phase 0.
    ResetRoute,
}

pub enum Action {
    // .. existing variants unchanged, except:
    /// `_bitrouter/route/set` or `/reset` came back. `Ok(Some(route))` is the
    /// route now in force; `Ok(None)` means the lease is gone and the daemon's
    /// default applies. Was `Result<String, String>`.
    Routed(Result<Option<String>, String>),
}
```

Widening `Routed` is what lets reset reuse today's `routed()` →
`Effect::RouteInForce(Option<String>)` path unchanged; `RouteInForce` already
takes an `Option`.

### 3.3 `SessionPorts` — `apps/bitrouter/src/actions/session.rs` (new, phase 1)

**This deviates from the research spec, which proposed a new trait
`SessionActions`.** No new trait is needed: the four port traits already exist,
and the stdio MCP profile is already built by bundling `Arc<dyn StatusQuery>`,
`Arc<dyn ModelsQuery>`, `Arc<dyn RouteQuery>` (`lib.rs:158` `stdio_profile`).
The session surface is the same bundle with a third consumer. That is
[research D12](CLI_TUI_PARITY_SPEC.md#d12--take-693s-inversion)'s inversion
in its cheapest form, and it closes most of §9's "conventional, not structural"
worry: the driver can reach an action only through the same `dyn` port the
MCP server uses, so a second implementation would have to be a second
`impl StatusQuery`.

```rust
//! The session surface of the shared actions — the same ports the stdio MCP
//! profile is built from, handed to the chat driver as one value.
//!
//! Lives in `crate::actions`, not `crate::chat`: constructing the ports names
//! the daemon and the config source, which the chat guard forbids in `chat/`.
//! The driver receives a `&SessionPorts` and names nothing else.

use std::path::PathBuf;
use std::sync::Arc;

use bitrouter_mcp::actions::models::ModelsQuery;
use bitrouter_mcp::actions::route::{RouteInput, RouteQuery};
use bitrouter_mcp::actions::status::StatusQuery;
use bitrouter_mcp::backend::CallerAuth;
use bitrouter_mcp::error::ToolError;

use crate::output::CliReport;
use crate::paths::ConfigSource;

pub struct SessionPorts {
    status: Arc<dyn StatusQuery>,
    models: Arc<dyn ModelsQuery>,
    route: Arc<dyn RouteQuery>,
}

impl SessionPorts {
    /// The same three constructors `bitrouter status`, `bitrouter models` and
    /// `bitrouter route` call (`main.rs:3363`, `:3510`, `:3462`), with the
    /// same arguments. A1 holds this function to those three.
    pub fn open(source: ConfigSource, socket: PathBuf) -> Self {
        Self {
            status: Arc::new(crate::actions::status::DaemonStatus::new(
                socket.clone(),
                Some(source.clone()),
            )),
            models: Arc::new(crate::actions::models::RoutableModels::new(
                source.clone(),
                Some(socket.clone()),
            )),
            route: Arc::new(crate::actions::route::RouteAction::new(source, Some(socket))),
        }
    }

    /// Resolve one row id and its arguments to a report.
    ///
    /// The only place a TUI command becomes a port call. The reducer emits
    /// only ids the app gave it, so an unknown id here is a bug in the list
    /// the app built, and is reported as an error rather than a panic.
    pub async fn run(
        &self,
        action: &str,
        args: &[String],
    ) -> Result<Box<dyn CliReport>, ToolError> {
        // Local and single-tenant, exactly as the stdio MCP profile: the local
        // implementations document that they ignore the caller.
        let caller = CallerAuth::default();
        match action {
            "status" => Ok(Box::new(self.status.status(&caller).await?)),
            "list_models" => Ok(Box::new(
                self.models
                    .list_models(&caller)
                    .await?
                    .filtered(args.first().map(String::as_str)),
            )),
            "route" => {
                let Some(model) = args.first() else {
                    return Err(ToolError::new("usage: /preview <model>"));
                };
                Ok(Box::new(
                    self.route
                        .route(RouteInput {
                            model: model.clone(),
                            prompt: None,
                        })
                        .await?,
                ))
            }
            other => Err(ToolError::new(format!("no session action `{other}`"))),
        }
    }
}
```

`Box<dyn CliReport>` is what makes the driver's rendering arm one line:
`StatusReport`, `ModelsReport` and `RouteReport` all implement `CliReport`
app-side already (`output/reports/daemon.rs:111`, `:192`;
`output/reports/routing.rs:17`). The driver never names a report type — which is
what G4 asserts.

**Construction site:** `acp_cli.rs`, beside the binding at `:1295`, before the
call to `chat::session::run` at `:1501`:

```rust
let ports = crate::actions::session::SessionPorts::open(
    source.clone(),
    crate::daemon::socket_path_for(source, &config),
);
```

Both arguments are in scope there today. `run` and `chat_plain` each gain a
`ports: &SessionPorts` parameter.

**The offered command list, built at the same site:**

```rust
/// `ACTIONS` rows with a `tui_command`, in table order, with each row's
/// `requires` resolved against what the controller advertised. Lives in
/// `apps/bitrouter/src/actions/session.rs` beside `SessionPorts`.
pub fn offered_commands(client: &AcpClient) -> Vec<bitrouter_tui::machine::Command> {
    use bitrouter_mcp::actions::{Requires, ACTIONS};
    use bitrouter_sdk::acp::client::RouteMethod;
    let capability = client.route_control();
    ACTIONS
        .iter()
        .filter_map(|row| {
            let name = row.tui_command?;
            let unavailable = match (row.requires, row.id) {
                (Requires::Nothing, _) => None,
                (Requires::Binding, "route_reset")
                    if !capability.allows(RouteMethod::Reset) =>
                {
                    Some(NOT_RESETTABLE)
                }
                (Requires::Binding, _)
                    if !(capability.allows(RouteMethod::List)
                        && capability.allows(RouteMethod::Set)) =>
                {
                    Some(NOT_ROUTABLE)
                }
                (Requires::Binding, _) => None,
            };
            Some(bitrouter_tui::machine::Command {
                name,
                action: row.id,
                summary: summary_for(row.id),
                unavailable,
            })
        })
        .collect()
}
```

`summary_for` is a `match` over row ids returning `&'static str` — the one-line
help strings live app-side, not in the table, because no other surface reads
them (the MCP tool has its own description; the CLI leaf has clap's). G1 covers
it: a row with a `tui_command` and no arm fails. `NOT_ROUTABLE` moves here from
`machine.rs:72`, since the reducer no longer decides availability;
`can_reroute` (`chat/session.rs:88`) is subsumed by this function and is
deleted.

### 3.4 The commands report — `crates/bitrouter-mcp/src/actions/commands.rs` (new, phase 3)

```rust
/// Where a command comes from. `bitrouter` beats `config` beats `agent`
/// (research D14; `config` added by D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommandSource {
    Bitrouter,
    Config,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct CommandRow {
    /// Without the slash. Never rewritten — no sigil, no prefix (D14).
    pub name: String,
    pub description: String,
    /// ACP's `input.hint`. Agent rows only; `None` when the agent gave none.
    pub hint: Option<String>,
    pub source: CommandSource,
    /// A same-named command from a higher-precedence source exists, so typing
    /// this name reaches that one. Listed, not dropped.
    pub shadowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct CommandsReport {
    /// Whether an `available_commands_update` arrived at all. `false` with no
    /// agent rows is "the agent said nothing before the deadline"; `true`
    /// with no agent rows is "the agent said: none". The protocol makes both
    /// possible (research §1, post-research table).
    pub received: bool,
    pub commands: Vec<CommandRow>,
}
```

Built by one function, app-side, from the same inputs on both surfaces:

```rust
// apps/bitrouter/src/actions/commands.rs
pub fn commands_report(
    bitrouter: &[bitrouter_tui::machine::Command],
    config: &[bitrouter_tui::machine::PromptCommand],
    agent: &[agent_client_protocol_schema::v1::AvailableCommand],
    received: bool,
) -> CommandsReport
```

It takes ACP's own `AvailableCommand` — which carries `input` — rather than the
SDK's `AgentCommand`, so the `hint` is available without touching
`translate.rs`. The journal already holds `Vec<AvailableCommand>`
(`journal.rs:179`), and the headless leaf reads the raw update stream
(`client.subscribe_raw_updates()`, as `chat_plain` does). The `translate.rs:59`
change the research spec asks for — carry `input.hint` into `AgentCommand` — is
still made in phase 3, but for the NDJSON `--format json` consumer's benefit,
not for this report.

`impl CliReport for CommandsReport` lives in
`apps/bitrouter/src/output/reports/commands.rs` (new): three headed groups in
source order, `shadowed` rows marked, and the `received: false` case rendered as
a distinct line from the empty-agent-list case.

### 3.5 Prompt-expansion config — `crates/bitrouter-sdk/src/config/mod.rs` (phase 4)

```rust
// Added to `pub struct Config` (mod.rs:46):
/// The interactive session's own configuration.
#[serde(default)]
pub chat: ChatConfig,

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChatConfig {
    /// Prompt-expansion commands. `/name args` in `bitrouter chat`, or
    /// `bitrouter acp prompt "/name args"`, sends `prompt` with `$ARGUMENTS`
    /// replaced by `args`. There is deliberately no key that runs anything.
    #[serde(default)]
    pub commands: Vec<PromptCommandConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PromptCommandConfig {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub prompt: String,
}
```

The collision check cannot live in the SDK (it needs `ACTIONS`, and
`bitrouter-mcp` depends on `bitrouter-sdk`, not the reverse). It lives beside
`offered_commands`:

```rust
// apps/bitrouter/src/actions/session.rs
/// The user's prompt-expansion commands, or an error naming the first one
/// whose name is a BitRouter command or alias. Checked here, at launch, so the
/// guards over `ACTIONS` stay exhaustive: the open registry can never hold a
/// name the closed one has.
pub fn prompt_commands(
    config: &bitrouter_sdk::config::ChatConfig,
) -> anyhow::Result<Vec<bitrouter_tui::machine::PromptCommand>>
```

---

## 4. The dispatcher

`machine.rs:387`'s `if line.trim() == "/commands"` and `:394`'s
`if line.trim() == "/route"` become one resolver over the lists in `State`.
The resolver is a free function over slices so that the piped loop and the
headless `acp prompt` path run the same one.

```rust
/// What a submitted line turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolution {
    /// A BitRouter command the reducer runs itself (`REDUCER_OWNED`).
    Owned { action: &'static str, args: Vec<String> },
    /// A BitRouter command the app runs through its ports.
    Action { action: &'static str, args: Vec<String> },
    /// A listed BitRouter command this session cannot run; the reason.
    Unavailable(&'static str),
    /// A prompt-expansion command; the expanded prompt. Phase 4.
    Expand(String),
    /// Not ours — a prompt, including any command the agent advertises.
    Prompt(String),
}

/// Resolve one line. Precedence is fixed here and nowhere else: a two-word
/// BitRouter name, then a one-word one, then an alias, then a prompt-expansion
/// command, then the agent. That order *is* D14's rule — local wins.
pub fn resolve(
    commands: &[Command],
    prompt_commands: &[PromptCommand],
    line: &str,
) -> Resolution {
    let line = line.trim();
    let Some(rest) = line.strip_prefix('/') else {
        return Resolution::Prompt(line.to_string());
    };
    let words: Vec<&str> = rest.split_whitespace().collect();
    // Longest BitRouter name first, so `/route reset` is not `/route` + "reset".
    for take in [2, 1] {
        let Some(head) = words.get(..take) else { continue };
        let typed = head.join(" ");
        let name = ALIASES
            .iter()
            .find(|(alias, _)| *alias == typed)
            .map_or(typed.as_str(), |(_, target)| target);
        if let Some(command) = commands.iter().find(|c| c.name == name) {
            let args = words[take..].iter().map(|w| w.to_string()).collect();
            return match command.unavailable {
                Some(reason) => Resolution::Unavailable(reason),
                None if REDUCER_OWNED.contains(&command.action) => Resolution::Owned {
                    action: command.action,
                    args,
                },
                None => Resolution::Action {
                    action: command.action,
                    args,
                },
            };
        }
    }
    if let Some(expansion) = words
        .first()
        .and_then(|name| prompt_commands.iter().find(|p| p.name == *name))
    {
        return Resolution::Expand(
            expansion
                .template
                .replace("$ARGUMENTS", &words[1..].join(" ")),
        );
    }
    Resolution::Prompt(line.to_string())
}
```

`submit()` becomes a `match` over it:

```rust
fn submit(state: &mut State) -> Vec<Effect> {
    if state.editor.line().trim().is_empty() {
        state.editor.clear();
        return vec![Effect::Echo, Effect::Paint(Trigger::Key)];
    }
    let line = state.editor.take();
    let mut effects = vec![Effect::Echo, Effect::ClearNotice];
    match resolve(&state.commands, &state.prompt_commands, &line) {
        Resolution::Owned { action: "commands", .. } => {
            effects.push(Effect::Notice(Notice::Commands));
        }
        // Today's `/route` path (`machine.rs:395`–`:396`), unchanged: the
        // picker opens on `Action::Routes`, which `Effect::ListRoutes` fetches.
        Resolution::Owned { action: "route_set", .. } => {
            effects.push(Effect::ListRoutes);
            return effects;
        }
        Resolution::Owned { action: "route_reset", .. } => {
            effects.push(Effect::ResetRoute);
            return effects;
        }
        // `REDUCER_OWNED` is exhaustive and G1 pins it; an id here that is
        // not matched above is a list the app built wrongly, said as such.
        Resolution::Owned { action, .. } => {
            effects.push(Effect::Notice(Notice::Say(format!(
                "`{action}` is marked reducer-owned but has no reducer arm"
            ))));
        }
        Resolution::Action { action, args } => {
            effects.push(Effect::Action { action, args });
            return effects;
        }
        Resolution::Unavailable(reason) => {
            effects.push(Effect::Notice(Notice::Say(reason.to_string())));
        }
        Resolution::Expand(prompt) | Resolution::Prompt(prompt) => {
            state.prompts = state.prompts.saturating_add(1);
            state.phase = Phase::Turn;
            effects.push(Effect::Prompt {
                line: prompt,
                nth: state.prompts,
            });
        }
    }
    effects.push(Effect::Paint(Trigger::Key));
    effects
}
```

Three properties of this shape, each of which a guard or an invariant depends
on:

1. **The reducer owns three verbs and knows nothing else by name.** Everything
   not in `REDUCER_OWNED` is `Effect::Action` with an opaque id. Adding a fourth
   read command is a row edit plus a `summary_for` arm plus a `run` arm — no
   reducer change.
2. **The set of names is data the app supplied**, so there is nothing for G1 to
   walk on the reducer side except `REDUCER_OWNED` and `ALIASES`. That is why G1
   is small.
3. **An agent's `/status` is shadowed, not eaten silently.** `resolve` finds
   the BitRouter row first; `/commands` lists the agent's `status` as
   `shadowed: true`. The research spec's D14 accepts this and names the escape
   (`//status` forwards verbatim) as a later one-character addition. Not built.

**The three callers of `resolve`:**

| Caller | `commands` | `prompt_commands` | What it does with each `Resolution` |
|---|---|---|---|
| `machine::submit` (interactive) | `state.commands` | `state.prompt_commands` | as above |
| `chat/session.rs` `chat_plain` (piped) | the app's `offered_commands` | from config | `Owned` → prints `"/<name> needs a terminal"` (the picker and the journal need keys and a screen); `Action` → `ports.run` → `render_to_vec` → stdout; `Unavailable` → prints the reason; `Expand`/`Prompt` → the turn |
| `acp_cli.rs` `acp prompt` (headless one-shot) | **`&[]`** | from config | `Expand` → the turn with the expanded text; `Prompt` → the turn. Passing no BitRouter commands is deliberate: `acp prompt "/status"` is not a scheduled surface (research D12 — nothing asks for it), so the text goes to the agent as it does today |

The `acp prompt` row is what makes `claude -p /project:cmd`'s shape hold here
([research §6.9.1](CLI_TUI_PARITY_SPEC.md#691-claude-code--a-documented-terminal-only-class-and-a-shared-class-that-is-not-commands)):
a prompt-expansion command is expanded identically by the TUI and by the
headless CLI, through the same function.

**`Wire::apply` gains one arm** (`chat/effects.rs`, beside `SetRoute`):

```rust
Effect::ResetRoute => self.replies.push_back(Action::Routed(
    match self.client.route_reset(self.session_id).await {
        Ok(()) => Ok(None),
        Err(RouteError::Unavailable(message)) => Err(format!(
            "route unchanged: route control is unavailable ({message})"
        )),
        Err(RouteError::InvalidRoute(message)) => Err(format!("route unchanged: {message}")),
        Err(RouteError::Other(error)) => Err(format!("route unchanged: {error:#}")),
    },
)),
```

and the `SetRoute` arm's `Ok(in_force) => Ok(in_force)` becomes
`Ok(in_force) => Ok(Some(in_force))`. `machine::routed()` then reports
`Ok(None)` as `"route reset to the daemon's default"` and `RouteInForce(None)`.

---

## 5. Rendering

One renderer, three outputs — the research spec's §12, kept whole:

```
report ──serde──────────────────────────────> --json          (exists)
       ──CliReport::render(Theme::for_stdout)> CLI human view (exists)
       ──Output::render_to_vec (Theme::none)─> bytes ─lines─> TUI notice
```

The driver's arm, `chat/session.rs`, beside `Notice::Commands` at `:368`:

```rust
Effect::Action { action, args } => {
    let lines = match ports.run(action, &args).await {
        Ok(report) => plain_lines(
            &crate::output::Output::new(crate::output::Format::Human)
                .render_to_vec(report.as_ref()),
        ),
        Err(error) => vec![Line::from(format!("{error}"))],
    };
    view.notice_lines(lines);
}
```

```rust
/// The CLI's plain rendering, one `Line` per line. Private to `session.rs`.
fn plain_lines(bytes: &[u8]) -> Vec<Line<'static>> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| Line::from(line.to_string()))
        .collect()
}
```

Rules the code above must keep, each with its enforcement:

- **`Theme::none()` only.** `render_to_vec` hard-codes it. `Theme::for_stdout`
  must not appear anywhere under `chat/` — G5 adds the string to the chat
  guard's forbidden list. A themed render would write raw ANSI into the
  differential writer's screen.
- **The driver names no report type.** It deals in `Box<dyn CliReport>`; the
  words `StatusReport`, `ModelsReport`, `RouteReport`, `SkillsReport`,
  `CommandsReport` must not appear under `chat/` — G4 adds them to the same
  list. That is also how research invariant 7 (*the TUI holds no daemon-wide
  state*) is enforced: a type the driver cannot name cannot be retained.
- **Notices replace, they do not accumulate.** `view.notice_lines` replaces the
  previous notice (`view.rs:111`), and `submit` emits `Effect::ClearNotice`
  before every command, so a report is on screen until the next line is typed
  and never longer — R4's *transient* clause, by construction.
- **The action is awaited inline, on the loop.** `ports.run` runs while no turn
  is streaming (commands resolve only at an idle prompt, as `/route` does
  today), so blocking the `select!` for a control-socket round trip or a
  read-only metering open is the same cost `Effect::ListRoutes` already pays.
- **Width.** The tables `human::Table` lays out for an 80-column stdout wrap
  through `wrap.rs` in a narrow terminal. The research spec's §12 names this
  the real risk; `status` is a block and `route` a chain, so `models` on a wide
  catalog is the one to check by hand at 60 columns during phase 2. A report
  that does not fit is a reason to drop its `tui_command`, not to add a
  renderer.

`/commands` renders through the same path from phase 3 on: the
`Notice::Commands` arm builds `commands_report(&state.commands,
&state.prompt_commands, journal.commands(), true)` and hands it to
`render_to_vec`. Until then (phases 0–2) it renders TUI-natively through an
extended `render::session::commands(bitrouter: &[Command], agent: &[AvailableCommand])`,
which adds the "BitRouter" group above the agent's and marks shadowed agent
rows. Phase 3 deletes that renderer.
