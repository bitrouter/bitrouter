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
  `routable` today. This decides the dispatcher's shape.
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
- **The dispatcher gains one arm** — `Resolution::Expand(prompt)`.

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
the `/commands` *report* row is a discriminator on a merged **view**, not on the
registry — the registries stay separate.

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
