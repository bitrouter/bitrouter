# Spec: one actions table — stopping CLI, MCP, and TUI from drifting apart

Status: **phases 0–4 implemented; 5 proposed** · Author: Claude (with Spikel)
· Date: 2026-09-04
· Issue: [#868](https://github.com/bitrouter/bitrouter/issues/868)
· Refs: [#863](https://github.com/bitrouter/bitrouter/issues/863) (open),
[#866](https://github.com/bitrouter/bitrouter/pull/866) (open)

**Implementation note (2026-09-04).** Phases 0 and 1 landed on
`claude/actions-table-phase01` (`023e219d`, `b72787a5`). Building them corrected
four things this spec had wrong and surfaced two regressions it had not
anticipated. All six are folded in below, marked
**[corrected post-implementation]**, with the original reasoning left readable
above each. The corrections: the guard test cannot live in the crate (§5),
`output_schema` must be optional and every tool needs a row from day one (§3,
§6 phase 1), `providers[]` needed a producer (§6 phase 1), and phase 0's interim
`currency` fix does not survive phase 1 (§6 phase 0).

**Follow-up (2026-09-04), still phase 1.** [D2](#d2--the-cloud-profile) was
implemented on its recommendation, reviewed, and **decided differently**:
`credits: Option<Credits>` is replaced by a universal `spend` position that
every deployment fills. That resolves the cost-footer regression as a side
effect — the footer's content returns as typed data rather than a prose line —
and closes the surface disagreement where only the cloud profile ever populated
the field. D2 and §6 phase 1 are rewritten below.

**Implementation note, phases 2 and 3 (2026-09-04).** The two remaining unified
actions were built **in parallel from the same phase-1 base**, on
`claude/actions-table-phase02` (`list_models`) and
`claude/actions-table-phase03` (`route`), and merged together on
`claude/actions-table-phases-1-3`. Neither branch could see the other while it
was being built, so each fixed the spec independently; both sets of corrections
are folded in below and marked **[corrected post-implementation]**, and the
`ACTIONS` table now carries a real `output_schema` for every row it claims.

*Phase 2 (`list_models`)* corrected three things: §6 phase 2's config-only
implementation is wrong for a server whose `complete` goes to the daemon (the
phase is daemon-first with a config fallback, and the fallback is what delivers
the standalone win); §10's "no `reqwest` call for … `list_models`" is
unachievable while the HTTP profile is assembled from an `Arc<dyn Backend>`; and
the report needed a `resolved_via` position the spec did not name, because two
sources of truth with different meanings cannot share an unlabelled shape. It
also confirmed the §3 rule-2 deviation phase 1 recorded, now covering
`Backend::models_port` as well.

*Phase 3 (`route`)* unified `route`, and settled
[D1](#d1--does-bitrouter-route---json-get-to-change-shape): **decided by the
human as option (a)** — break the shape, changelog it — so it is no longer an
open question. Building it corrected §6 phase 3 in three places: the `daemon`
label was not adopted, the CLI needed a `--prompt` flag to be a real peer of the
tool, and "`--config` becomes a constructor parameter" meant the config
*source*, not a new `mcp serve --config` flag. Two things the phase deleted are
recorded there too.

**Implementation note, phase 4 (2026-09-04).** Skills landed on
`claude/actions-table-phase04`. It corrected §6 phase 4 in four places, settled
[D3](#d3--invalid-skills-marked-or-hidden) on its recommendation, and carried a
piece of work the phase text could not have known about: **SEP-2640 was accepted
by the MCP Core Maintainers on 2026-09-03**, and this repository implemented an
earlier draft. The spec predates that, so §6 phase 4 says nothing about it; the
wire alignment is recorded in the phase below and in `CHANGELOG.md`. The lesson
for later phases is narrow but real: an action whose report is *also* somebody
else's wire format has a second source of truth, and unifying our surfaces over
a stale one would have made both of them wrong together.

Both phases independently reached for a provenance field on their report —
`ModelsReport::resolved_via: ModelsSource` and `RouteReport::resolved_via:
ResolvedVia`. They stay distinct types: the two report different things (which
*catalog view* answered, versus which *routing rule* fired), and collapsing them
into one enum would be a shape the surfaces do not share.

BitRouter answers the same five questions — *what models can I route? how would
this route? is it up? what skills do I have? run this completion* — through two
independent code paths, and nothing asserts they agree. This spec makes the
agreement **structural**: one report type per action, shared by the CLI leaf and
the MCP tool, with a guard test that fails when a surface appears without one.

## Contents

- [1. Verified starting state](#1-verified-starting-state)
- [2. Root cause — three of them, not one](#2-root-cause--three-of-them-not-one)
- [3. The contract](#3-the-contract)
- [4. Where the shared types live](#4-where-the-shared-types-live)
- [5. The guard test, and its direction](#5-the-guard-test-and-its-direction)
- [6. Phases](#6-phases)
- [7. Invariants the refactor must not break](#7-invariants-the-refactor-must-not-break)
- [8. Deliberate deviations from the issue](#8-deliberate-deviations-from-the-issue)
- [9. Open decisions](#9-open-decisions)
- [10. Acceptance](#10-acceptance)
- [11. Explicitly out of scope](#11-explicitly-out-of-scope)

---

## 1. Verified starting state

Measured in this worktree at `f4da66f3`, not inferred from the issue.

| Claim | Evidence |
|---|---|
| MCP `list_models` keeps one provider per model | [`local.rs:53`](../crates/bitrouter-mcp/src/backend/local.rs:53) — `m.providers.first()` |
| …while the wire already carries all of them | [`server.rs:665`](../crates/bitrouter-sdk/src/server.rs:665) emits `providers`; [`routing.rs:63`](../crates/bitrouter-sdk/src/language_model/routing.rs:63) `ModelInfo { id, providers: Vec<String> }` |
| The MCP crate re-declares that type, lossily | [`backend/mod.rs`](../crates/bitrouter-mcp/src/backend/mod.rs) `ModelInfo { id, provider: String }` |
| CLI `models` needs no daemon and runs `/models` discovery | [`commands.rs:189`](../apps/bitrouter/src/commands.rs:189) — `discover_models`, then `ConfigRoutingTable::list_models` |
| MCP `status` is a `GET /v1/models` in disguise | [`local.rs:131`](../crates/bitrouter-mcp/src/backend/local.rs:131) |
| CLI `status` is a control-socket probe, and a stopped daemon is exit 0 | [`main.rs:3240`](../apps/bitrouter/src/main.rs:3240) → [`StatusReport::stopped`](../apps/bitrouter/src/output/reports/daemon.rs) |
| The two status payloads share exactly one field (`listen`), and it means different things | `{listen, models, providers}` vs `{running, pid, listen, models, socket}`; MCP's `listen` is the *client's* `--local-url`, not what the daemon reports |
| `route_preview` config fallback runs the policy table; `bitrouter route` does not | [`routing_preview.rs:131`](../apps/bitrouter/src/routing_preview.rs:131) vs [`commands.rs:215`](../apps/bitrouter/src/commands.rs:215) |
| …and their report keys differ | `requested_model` / `provider_chain[].api_protocol` vs `model` / `chain[].protocol` |
| `route_preview` snapshots config **once, at `mcp serve` start** | [`RoutingPreview::new`](../apps/bitrouter/src/routing_preview.rs:47) is called during `McpAction::Serve` wiring ([`main.rs:2260`](../apps/bitrouter/src/main.rs:2260)); a `bitrouter.yaml` edit is invisible to a long-lived server that the CLI would pick up |
| Three skills discovery rules over two roots | `discover_all_skills` walks `root`, `root/skills`, `root/.claude/skills` ([`format.rs:137`](../apps/bitrouter/src/skills/format.rs:137)); `list_installed` reads only `<root>/.claude/skills` ([`root.rs:39`](../apps/bitrouter/src/skills/root.rs:39)); `skills/list` adds dir-name == frontmatter-name + format-bounds validation ([`skills_catalog.rs:63`](../apps/bitrouter/src/skills_catalog.rs:63)) |
| Only the CLI can reach user-global skills | `-g` → `SkillsRoot::Global`; both MCP surfaces are constructed over `current_dir()` ([`main.rs:2200`](../apps/bitrouter/src/main.rs:2200)) |
| `path` means two different things | CLI: the skill **directory**; MCP: the **`SKILL.md` file** |
| A fourth frontmatter reader exists | [`skill_body`](../apps/bitrouter/src/skills_query.rs:86), hand-rolled beside `extract_frontmatter_block` ([`format.rs:53`](../apps/bitrouter/src/skills/format.rs:53)) |
| The two `mcp serve` profiles are disjoint | `--backend skills` returns before completion/routing wiring ([`main.rs:2194`](../apps/bitrouter/src/main.rs:2194)); `mcp install` writes `["mcp","serve"]` ([`install.rs:15`](../crates/bitrouter-mcp/src/install.rs:15)), so an installed client never sees a skill |
| The TUI picker offers routes `set` can refuse | `route_suggestions` = model ids ([`daemon.rs:744`](../apps/bitrouter/src/daemon.rs:744)); `AcpRouteSet` validates with `route_chain` ([`daemon.rs:666`](../apps/bitrouter/src/daemon.rs:666)) |
| `BillingBalanceResponse` re-declares `BalanceResponse` minus `currency` | [`cloud.rs:13`](../crates/bitrouter-mcp/src/backend/cloud.rs:13) vs [`billing.rs:14`](../apps/bitrouter/src/cloud/management/billing.rs:14) |
| The MCP README's "full reference" link is dead twice over | [`README.md:95`](../crates/bitrouter-mcp/README.md:95) — `../skills/…` resolves to `crates/skills/…`, and `references/mcp-server.md` does not exist |
| `docs/CLI.md` documents `mcp search/list/add` but never `mcp serve`'s tools | `docs/CLI.md:298`–`324` |
| rmcp 3.1 can carry a typed report | `Tool::with_output_schema::<T: JsonSchema>` (`rmcp-3.1.0/src/model/tool.rs:242`); the `#[tool]` macro derives `output_schema` from a `Json<T>` return (`rmcp-macros-3.1.0/src/tool.rs:250`) |
| A router's tools are enumerable at test time | `ToolRouter::list_all()` (`rmcp-3.1.0/src/handler/server/router/tool.rs:581`) |
| The CLI already has a typed-report layer to extend | [`output/mod.rs`](../apps/bitrouter/src/output/mod.rs) — `CliReport: erased_serde::Serialize` + `render`, JSON by default |
| `apps/bitrouter` → `bitrouter-mcp` is the only edge between them | `apps/bitrouter/Cargo.toml:27`; the MCP crate depends on `bitrouter-sdk`, never on the app |

Two of the issue's premises need adjusting:

- **`CONTROL_PLANE_SPEC.md` is not in this tree.** It arrives with #863, still
  open. The const-table-plus-guard-test pattern the issue cites as precedent is
  therefore *proposed*, not established. This spec stands alone and does not
  block on #863.
- **#866 is open**, so the "session verbs already unified on `machine::Effect`"
  half is in flight, not landed. Nothing here depends on it.

## 2. Root cause — three of them, not one

The issue names one (raw HTTP clients where ports should be). There are three,
and only fixing all three stops the drift returning.

**(a) Two implementations.** `complete` / `list_models` / `status` are reqwest
clients *inside* `bitrouter-mcp` ([`backend/`](../crates/bitrouter-mcp/src/backend/)),
so they cannot share a function with the CLI. Correct as a layering instinct —
the crate must not depend on the app — but it bought layering with duplication.

**(b) No shared type, even where a port exists.** `route_preview` and
`skills_search` *do* go through ports, and still drift: the ports return
`serde_json::Value`, so the report is assembled twice, from two hand-written
`json!` literals. A port that returns `Value` types nothing.

**(c) No inventory.** Nothing enumerates "the things BitRouter can be asked to
do". A tool can be added to `CAPABILITIES` ([`server.rs:131`](../crates/bitrouter-mcp/src/server.rs:131))
with no CLI leaf and no test noticing; a CLI leaf can be added with no tool.

The TUI is a third consumer of (c) only. Its charter (`ACP_TUI_SPEC.md` §8.3:
session-scoped data only) means it should *not* share management reports — but
`_bitrouter/route/list` is exactly the case where it does share one and
disagrees ([§6, phase 5](#phase-5--the-tui-picker-and-the-docs)).

## 3. The contract

An **action** is one typed question with one typed answer, expressible on more
than one surface.

```
┌───────────── bitrouter-mcp (types + wiring, no business logic) ─────────────┐
│  actions::status::{StatusInput, StatusReport}   ← Serialize + JsonSchema    │
│  actions::status::StatusQuery                   ← the port (trait)          │
│  actions::ACTIONS: &[ActionSpec]                ← the inventory             │
│  server.rs  #[tool] status(..) -> Json<StatusReport>                        │
└───────────────────────────────┬─────────────────────────────────────────────┘
                                │ implements
┌───────────────────────────────▼─────────────────────────────────────────────┐
│  apps/bitrouter/src/actions/status.rs   ← the one implementation            │
│  apps/bitrouter/src/main.rs  `bitrouter status` → output.emit(&report)      │
│  impl CliReport for StatusReport         ← human rendering, app-side        │
└─────────────────────────────────────────────────────────────────────────────┘
```

Three rules:

1. **One report type per action**, deriving `Serialize + Deserialize +
   JsonSchema`. The MCP tool returns `Json<Report>`, so rmcp advertises
   `output_schema` from that same type. The CLI `emit`s it; `--json` is
   byte-identical to the tool's structured content.
2. **One implementation per action**, app-side, behind a port trait declared
   beside the types. The MCP crate keeps no business logic and no HTTP client.
3. **Human rendering stays app-side.** `impl CliReport for <foreign report>` is
   legal (local trait) and keeps `Human`/`Table` out of the crate.

`ActionSpec` is the inventory:

```rust
pub struct ActionSpec {
    /// Stable action id, e.g. "status".
    pub id: &'static str,
    /// The CLI leaf that answers it, space-separated ("skills list"), or None.
    pub cli_leaf: Option<&'static str>,
    /// The MCP tool that answers it, or None.
    pub mcp_tool: Option<&'static str>,
    /// The shared report's JSON Schema — the thing that must not drift — or
    /// `None` while the action has not been migrated onto a shared type yet.
    pub output_schema: Option<fn() -> rmcp::model::JsonObject>,
}
```

Storing the schema *as a function pointer* is what makes the table load-bearing
rather than documentary: the guard test compares the MCP tool's advertised
`output_schema` against `(row.output_schema)()`, and a CLI golden test validates
emitted JSON against the same schema. A row cannot claim agreement it does not
have.

**[corrected post-implementation]** `output_schema` is `Option`, not a bare
function pointer. The original shape assumed a row appears only as its action is
unified — but §5 requires *every tool* to have a row, so on day one the table
must already list `complete`, `list_models`, `route_preview`, `skills_search`
and `skills_get`, none of which has a shared type yet. `None` is the migration
backlog, not an exemption: the row still has to exist, which is what stops a
remotable action going uninventoried, but until a shared report replaces the two
hand-written shapes there is no schema to hold the surfaces to. Each `None` is
one of the phases below.

## 4. Where the shared types live

`apps/bitrouter` depends on `bitrouter-mcp`; the reverse edge would be a cycle.
The `#[tool]` macro expands inside `bitrouter-mcp`, so the report type must be
nameable there. **Types go down, logic stays up.**

| Candidate home | Verdict |
|---|---|
| `crates/bitrouter-mcp/src/actions/` | **Chosen.** The ports already live one directory over; the crate already carries `schemars`. |
| `crates/bitrouter-sdk` | For types that also ride a *wire* — `ModelInfo` is already there and phase 2 reuses it rather than re-declaring it. Not the default home: the SDK should not grow a management vocabulary its consumers don't speak. |
| A new `bitrouter-actions` crate | Rejected for now. Six rows do not earn a crate, and it would sit between two crates that already have a clean edge. Revisit if the cloud binary needs the types without rmcp. |

Consequence worth stating plainly: `bitrouter-mcp` stops being "the MCP server
crate" and becomes "the action contract plus its MCP binding". The crate name
stays; its module doc changes. `capabilities/` (ports) and `actions/` (types +
inventory) merge under `actions/`.

**Only actions with more than one surface migrate.** The CLI's ~100 other leaves
keep their `output/reports/*` types unchanged. This is not a project to move
every report into a crate.

## 5. The guard test, and its direction

Direction matters, and the issue's phrasing ("any tool **or leaf** missing from
the table") would make the table a second copy of clap — 103 rows to maintain
for the six that mean anything.

The asserted containments are:

- **Every MCP tool has a row.** `ToolRouter::list_all()` over a fully-wired
  builder; a tool without a row fails. This is the direction that matters: a
  remotable action must be inventoried.
- **Every row's `cli_leaf` resolves in clap.** Walk `Cli::command()`
  subcommands; a renamed leaf fails the test.
- **Every row's `mcp_tool`, where present, advertises the row's schema.**
  `tool.output_schema == Some(Arc::new((row.output_schema)()))`.
- **Not asserted:** that every CLI leaf has a row. `bitrouter policy verify`
  needs no MCP tool and should not need a table entry to exist.

The wired-builder requirement is real work: `CAPABILITIES` registers routers
only for wired capabilities, so the test needs a builder with every port filled
by a stub.

**[corrected post-implementation]** The guard **cannot** live in
`crates/bitrouter-mcp/tests/`, as this section originally proposed. The
clap-resolution assertion needs `Cli`, which is defined in
`apps/bitrouter/src/main.rs` and is unreachable from another crate — or even
from `apps/bitrouter/tests/`, since an integration test sees only the library.
The guard therefore lives in `main.rs`'s own test module, and the crate exposes
`BitrouterMcp::tools()` (public, documented as existing for this guard) so the
app can enumerate the tool surface. The containments are unchanged; only their
home moved. The HTTP-profile assertion (invariant 2) stays crate-side in
`server.rs`, where it needs no clap.

## 6. Phases

Each phase is independently shippable, ends green, and fixes user-visible
behavior. The issue's "first PR (fixes, no framework)" is honored where it is
actually framework-free — but *"`route_preview` and `bitrouter route` share one
report type"* **is** the framework, so it moves to phase 3.

### Phase 0 — free fixes, no types moved

- Delete `BillingBalanceResponse`; the cloud backend deserializes
  `bitrouter_sdk`-shaped fields or the app's `BalanceResponse` once phase 4
  makes it reachable. Interim: drop the duplicate struct's divergence by adding
  `currency`.
  - **[corrected post-implementation]** That interim fix survives only as far as
    phase 1, which deletes the struct outright. Both were done in one PR: phase 0
    stays independently shippable, and adding `currency` made the field live (it
    surfaced on `StatusInfo::Cloud`) rather than dead code.
- Fix [`README.md:95`](../crates/bitrouter-mcp/README.md:95): point at
  `docs/CLI.md`'s new origin-server section, not a file that never existed.
- Add a `bitrouter mcp serve` section to `docs/CLI.md`: transports, backends,
  the tool table, and which profile carries which tools.
- Update `skills/bitrouter/SKILL.md` / `references/` in the same change (the
  CLAUDE.md lockstep rule), since the tool table is agent-facing.

### Phase 1 — `status`, and the contract it introduces

The row with *zero* overlapping fields is the cheapest place to prove the
mechanism.

- `actions/status.rs` in the crate: `StatusReport { running, pid?, listen?,
  models?, providers[], socket?, spend? }` + `StatusQuery` port.
- App-side `actions::status` implements it over the control socket — the CLI's
  existing logic, moved, not rewritten.
- MCP `status` returns `Json<StatusReport>`; a stopped daemon is
  `running: false`, **not a tool error** (an agent polling for health must be
  able to tell "down" from "broken").
- `ACTIONS` + the guard test land here with one row.
- Deletes: `Backend::status`, `StatusInfo`, `ProviderStatus`, and the
  `/v1/models`-as-health-check.

`spend` is where the money question goes; see [D2](#d2--the-cloud-profile).

**[corrected post-implementation]** Three things this phase got wrong, and two
consequences it did not predict:

- **Not "one row" — a row per tool, one *with a schema*.** "`ACTIONS` … with one
  row" contradicts §5's "every MCP tool has a row": with a single row the guard
  fails immediately against the five other registered tools. All six rows land
  here; only `status` carries a schema (see §3).
- **`providers[]` had no producer.** Deleting the `/v1/models` health check —
  this phase's own instruction — removes the only thing that ever built a
  provider list, so the field would have been permanently empty: exactly the dead
  surface invariant 5 forbids. `DaemonResponse::Status` was extended with
  `providers` (`#[serde(default)]`, so the control-socket wire stays compatible),
  derived from the `ModelInfo { id, providers }` list the daemon already walks to
  count models. `bitrouter status` gains a real provider list as a side effect.
- **The HTTP profile needed a way to keep `status`.**
  `serve_http_on(Arc<dyn Backend>, …)` is pinned by `multitenant_http.rs`, so
  nothing outside the backend can reach the builder there.
  `Backend::status_port(self: Arc<Self>) -> Option<Arc<dyn StatusQuery>>` bridges
  it — wiring, not logic: `CloudBackend` hands itself over (the remaining
  credit, read with the caller's
  bearer), `LocalBackend` returns `None`. This leaves `impl StatusQuery for
  CloudBackend` crate-side, a real deviation from rule 2 of §3. It is consistent
  with `complete`/`list_models` still being reqwest there, and it closes when
  `complete` is port-ified.
- **~~Regression: `status` loses the cost footer.~~ Resolved by the spend
  reshape.** `Json<T>` produces structured content with no room for a second
  free-text block, so the prose spend line stopped riding along (`complete`
  keeps its). The follow-up makes that a non-issue rather than something to
  recover: the footer's *content* is now `StatusReport::spend`, typed instead of
  prose, and strictly richer — `unpriced` and a remaining cap have no room in a
  one-sentence footer. No `#[tool(output_schema = …)]` with a hand-built
  `CallToolResult` is needed. `docs/CLI.md`, the skill, and the code comment all
  say this now.
- **Regression: HTTP + local loses `status` entirely.** That path *was* the fake
  `/v1/models` check; nothing can replace it from a `--local-url` client, which
  has no control socket. Documented in the same three places.

### Phase 2 — `list_models`

- Reuse `bitrouter_sdk::…::routing::ModelInfo` (`providers: Vec<String>`).
  Delete the crate's truncating re-declaration. The fallback chain stops
  vanishing.
- `ModelsQuery` port; app-side implementation is today's `commands::list_models`
  (built-in defaults → `discover_models` → `list_models`), so **MCP gains
  standalone operation**: `list_models` stops requiring a running daemon, which
  is the difference an agent notices first.
- Shared `ModelsReport { models: Vec<ModelInfo> }`; `bitrouter models --provider`
  becomes a filter on the same report, and the MCP tool grows the same optional
  filter argument.

**[corrected post-implementation]** Three things, one of them the phase's central
decision:

- **Daemon-first, not config-only.** "app-side implementation is today's
  `commands::list_models`" gets the *source* wrong. Config-only is simpler and
  does deliver standalone operation, but it makes `list_models` disagree with the
  `complete` sitting next to it on the same server: `complete` goes to the
  daemon, and a static config projection is neither a subset nor a superset of
  what that daemon will accept. It is missing every provider whose credential is
  resolved at daemon start-up — `assemble.rs` runs
  `activate_stored_credential_providers` and `list_models_for` skips inactive
  providers, so the `claude-code` and `google-ai` subscription models are simply
  absent — and it lists models from a config edited since the daemon started,
  which that daemon would refuse. Measured on a two-provider config with a
  `claude-code` credential in the store: config-only listed 2 models, the live
  table 10. So the implementation is daemon-first with a config fallback, the
  order `route_preview` and `bitrouter route` already use, and the fallback is
  what delivers the standalone win the phase was named for. The daemon path is a
  new control-socket verb (`DaemonCommand::Models` →
  `DaemonResponse::Models { models: Vec<ModelInfo> }`), returning whole the list
  `Status` already walks to produce its count.
  - Consequence the phase text did not anticipate: **`bitrouter models` becomes
    daemon-aware too.** One implementation means one behaviour, and the CLI leaf
    gains the subscription models it used to drop. The daemon is the one serving
    that leaf's config (the socket resolves from the same `--config`), so
    `bitrouter models -c other.yaml` still answers about `other.yaml`.
  - Config resolution happens **per call**, not at `mcp serve` start, so the
    stale-snapshot bug phase 3 has to fix for `route_preview` is not reproduced
    here.
  - **[corrected at review, 2026-09-04]** The fallback now runs the
    stored-credential activation too (`commands::resolve_static`: built-in
    defaults, then `activate_stored_credential_providers`, the same two steps
    `assemble.rs` runs). As merged it applied only the defaults, so with no
    daemon running a subscription-backed provider was invisible — the "2
    models versus 10" measurement above was partly the fallback's own
    omission, not only the daemon's advantage. The same helper feeds
    `route`'s config fallback and `spawn`'s Codex preflight, so all three
    standalone answers resolve the config the way the daemon does. What the
    live table still has over the projection is `reload`s and the start-up
    state of the daemon that is actually serving `complete`.
- **The report needed `resolved_via`.** `ModelsReport { models }` alone cannot
  say which of two materially different views answered, and "what a running
  router will accept" versus "what this config would accept" is exactly the
  distinction an agent deciding whether it may route needs. `resolved_via:
  ModelsSource::{Live, Config}` carries it, mirroring `route_preview`'s field of
  the same name. **This changes `bitrouter models --json`'s shape** — additively:
  `models[]` and its entries are untouched, so a consumer reading models keeps
  working, but the object gains a key. Worth stating because it is an
  agent-readable surface.
- **`ModelsQuery` is a second `Backend` port, not the end of `Backend`.** Both
  backends implement `ModelsQuery` and hand it over via
  `Backend::models_port(self: Arc<Self>)`, the same wiring `status_port`
  introduced and the same deviation from §3 rule 2. It is not optional here: the
  HTTP profile is assembled from `serve_http_on(Arc<dyn Backend>, …)`, which
  `multitenant_http.rs` pins, so a backend that cannot hand over a port cannot
  keep its tool — and unlike `status`, a `--local-url` or cloud client's
  `GET /v1/models` *is* the right answer for its deployment, so dropping it would
  be a regression rather than an honest absence. `Backend` is therefore left with
  `complete` plus two port accessors, not `complete` alone; it collapses when
  `complete` is port-ified.
- Also deleted: `Builder::completion_local`, whose only caller was the stdio test
  fixture and which wired a `LocalBackend` as *just* the completion backend —
  misleading now that the same backend also answers `list_models`.

### Phase 3 — `route` / `route_preview`

One action, `route`, with input `{ model, prompt: Option<String> }` and one
`RouteReport`. This phase resolves three real disagreements, so it needs
decisions, not just plumbing:

- **Policy.** Both surfaces run the policy table in the config fallback. Today
  only `route_preview` does, which means `bitrouter route` can name a model the
  daemon would never pick. The report carries `requested_model`,
  `effective_model`, `effective_effort`, and `policy_decision`, and `resolved_via`
  distinguishes `daemon` (policy applied upstream, no static decision to show)
  from `config` / `zero-config`.
- **Key names.** The action keeps `route_preview`'s richer vocabulary
  (`provider_chain[].api_protocol`, `estimated_cost`), because it is the superset
  and the one an agent reads. `bitrouter route --json` changes shape; that is a
  breaking CLI change and belongs in the changelog.
- **Config freshness.** The adapter resolves config **per call** instead of
  snapshotting at `mcp serve` start, closing the stale-preview bug. `--config`
  stops being a CLI-only capability by becoming a constructor parameter both
  surfaces set.

**[corrected post-implementation]** Three things this phase got slightly wrong,
and two deletions it did not mention:

- **`resolved_via` keeps `live daemon`, not `daemon`.** The prose above names
  the live path `daemon`; both surfaces already emitted `live daemon` and
  nothing required breaking a third key on top of the two D1 sanctions. The
  value is now a typed enum (`ResolvedVia`) rather than a free string, so the
  three cases are in the schema instead of in a comment.
  - **[corrected at review, 2026-09-04]** The values are `live` / `config` /
    `zero_config` after all. Phase 2 had independently given `ModelsReport` a
    `resolved_via` of `live` / `config`, so the merged tree answered the same
    fact with `live` on one report and `live daemon` on the other — and
    `zero-config` hyphenated where its sibling did not. Almost-aligned is
    worse than plainly different; `ResolvedVia` now uses
    `rename_all = "snake_case"` to match `ModelsSource`. It rides the D1 break
    (the shape is already changing in this release) rather than being a third.
- **[corrected at review, 2026-09-04] The live path does not apply policy —
  on either surface, before or after this phase.** The bullet above ("the
  daemon applied policy upstream, no static decision to show") repeats a claim
  the old `route_preview` comment made and which is false:
  `DaemonCommand::Route` calls `routing_table().route_chain(model)` directly,
  while the daemon's `PolicyTableRouter` is a `PromptTransform` on the
  *request* pipeline (`assemble.rs`) that the control-socket verb never runs.
  So a `live` answer reports `effective_model == requested_model`, no
  `policy_decision`, and ignores `prompt`. The two surfaces agree (they share
  the code), which is what this phase set out to guarantee — but "neither
  surface can name a model the daemon would never pick" holds only on the
  config paths. The docs now say so instead of the opposite. The fix that
  closes it is daemon-side and deferred: extend the `route` verb to take the
  prompt and run the daemon's own live policy router, which is the only place
  pins, locks and trials are known — a static replay from config in the
  action would report a decision the daemon might not make.
- **The CLI needed `--prompt`.** The shared input is `{ model, prompt }`, but
  §6 said nothing about how the CLI expresses the second half — and without it
  the two surfaces still answer different questions, because the policy table
  keys on the agent-loop step the *prompt* implies. `bitrouter route` gains
  `--prompt <text>`; it is additive, so it is not part of the D1 break.
- **`--config` was never about a new flag.** "`--config` stops being a CLI-only
  capability" reads like `mcp serve --config <path>`; what it means, and what
  was built, is that the resolved [`ConfigSource`] is the constructor parameter
  — the CLI's from `-c`, `mcp serve`'s from the default resolution. A
  `mcp serve --config` flag would be new surface nothing asked for (CLAUDE.md
  4). Per-call freshness is what the bug needed, and that is independent of
  where the source came from.
- **`RoutingQuery` had to move, not just change.** §6 says the report type is
  shared; it does not say the *port* relocates. It does: the port and the type
  it returns belong in one file, so `capabilities::routing` is deleted and
  `actions::route` replaces it. `capabilities/` is now only the ports whose
  action has not been unified — which is the migration backlog made visible in
  the module tree, matching `output_schema: None` in the table.
- **`route_preview` also gained a rate card on the daemon path.** The old
  daemon branch built its report from the same `report()` helper, so it already
  priced the top hop from a config snapshot. Resolving config per call meant
  the daemon path had to read config *for pricing alone*; it does, best-effort,
  and an unreadable config costs `estimated_cost` rather than the report. The
  daemon answered the routing question; a preview with no rate card is still
  the right answer to it.

One thing the phase deliberately did **not** unify: `commands::resolve_route`
survives, because `bitrouter spawn`'s Codex preflight
(`apps/bitrouter/src/spawn.rs`) still calls it. That check asks a different
question — "does this model resolve at all" — and pulling it into the `route`
action would make a preflight depend on the policy table. Noted rather than
left silent: it is the one remaining place that resolves a chain without the
policy table.

### Phase 4 — skills: one discovery, one root, one shape

The largest user-visible win in the issue: a skill with broken YAML is listed by
the CLI and invisible to the agent, and the reverse for `./skills/foo`.

- **One discovery function.** `discover_all_skills` is the survivor (it already
  handles all three conventional layouts and symlink containment). `list_installed`
  becomes a filter over it, not a second `read_dir`.
- **One root resolution**, shared, including `-g`: MCP gains user-global skills
  by construction rather than by a second constructor.
- **One validation policy.** SEP-2640's dir-name == frontmatter-name rule is the
  strict one. Rather than silently applying it to one surface, the shared
  `SkillRow` carries `valid: bool` + `problem: Option<String>`, `skills/list`
  filters to valid entries (the SEP requires it), and both `bitrouter skills list`
  and `skills_search` show the invalid ones *marked*. An agent then learns why a
  skill it can see on disk is unusable — today it just isn't there.
- **`path` is disambiguated** into `dir` and `skill_md`. Both surfaces get both.
- **Delete `skill_body`**; `format.rs` owns frontmatter splitting.
- **The disjoint profiles close**: stdio `mcp serve` wires the skills and
  skill-catalog capabilities too. The identity argument that makes
  `--backend skills` stdio-only holds identically for stdio `mcp serve`, so an
  `mcp install`-ed client finally sees skills. `--backend skills` survives as the
  narrow gateway subprocess profile.
- `skills_get` keeps `cli_leaf: None`. Adding `bitrouter skills show` is not
  needed by this work and would be dead surface.

**[corrected post-implementation]** Four corrections, plus the SEP work the
phase text predates.

- **The root resolution had to become *data*, not a lookup.** "One root
  resolution, shared, including `-g`" is right, but `SkillsRoot::Global`
  resolved `$HOME` inside the method that returned its path. That makes every
  surface's answer depend on the process environment at the moment it is asked,
  and leaves no way to exercise the global root in a test without mutating that
  environment for every other test in the process — so the phase's own
  acceptance criterion ("a user-global skill appears identically in
  `bitrouter skills list` and `skills_search`") was unassertable. `Global` now
  *holds* the home directory, resolved once at `SkillsRoot::global()`. The two
  scopes are `cli_scope(-g)` and `mcp_scope`, and the second is literally the
  first's two cases together, which is what makes "MCP gains user-global skills
  by construction" checkable rather than asserted in prose.
- **The MCP scope needed a URI prefix, which the phase did not mention.** Once
  one server serves both roots, a project `foo` and a user-global `foo` collide
  on `skill://foo/SKILL.md`, and the catalog's existing "two skills resolve to
  the same URI" path would have silently dropped one. The global root
  contributes a `global/` organizational prefix — legal without qualification,
  since SEP-2640 calls preceding segments "a server-chosen organizational
  prefix" — and project URIs are unchanged.
- **`discover_all_skills` had to stop dropping unparseable skills.** The phase
  says `discover_all_skills` "is the survivor", which is true, but it silently
  skipped a `SKILL.md` whose frontmatter would not parse — and that is *the very
  case* the phase exists to fix. It could not be the survivor unchanged. It now
  returns a `DiscoveredSkill` carrying `Result<SkillFrontmatter>`, and
  `DiscoveredSkill::problem()` is the single validation policy the three
  surfaces share.
- **`skills_get` gained a schema after all.** The phase only says its
  `cli_leaf` stays `None`, which held. But `skills_search` becoming
  `Json<SkillsReport>` meant `skills_get` returning an untyped `CallToolResult`
  beside it was the odd one out; typing it as `Json<SkillDetail>` also killed
  `skill_body`'s reason to exist as a *second* frontmatter split. Its `ACTIONS`
  row therefore carries an `output_schema` too. That is a row with one surface
  and a schema, which §3 does not anticipate: the schema pins nothing against a
  second surface, but leaving `None` beside a tool that *does* advertise one
  would have the table understate what it knows. The guard test is unaffected —
  it checks tool-matches-row, not row-implies-two-surfaces.
- **Also deleted:** `skills::root::list_installed` (the second `read_dir`),
  `skills_query.rs` and its `skill_body` (the fourth frontmatter reader),
  `capabilities::skills` (the port moved to `actions::skills`, as phase 3's
  `capabilities::routing` did), `SkillsListReport`/`SkillEntry` in
  `output/reports/skills.rs` (replaced by `impl CliReport for SkillsReport`),
  `skills_catalog::skill_dir_for` (the catalog now keeps the directory beside
  the entry instead of re-running discovery to recover it), and
  `server::json_tool_result`, whose last two callers were the skills tools.

*SEP-2640 alignment, which §6 phase 4 predates.* The SEP was **accepted
2026-09-03**; `crates/bitrouter-sdk/src/mcp/skills.rs` implemented an earlier
draft and was emitting entries a conforming host must refuse. Three divergences
were verified against the accepted text and closed: `SkillResource` gains the
required `size`; `resources` becomes required with `"dynamic"` as an explicit
string marker rather than an omitted key (so `Option<Vec<_>>` becomes a
`SkillResources` enum); and the two fixed per-skill limits (512 entries, 16 MiB
over `size`) are enforced both when publishing a local skill and when the
gateway re-publishes an upstream's. See `CHANGELOG.md` for the wire diff. The
remaining conformance gaps are host-side obligations BitRouter has no host to
discharge them for, plus `resources/directory/read`, which the SEP gates behind
a `directoryRead: true` we deliberately do not declare.

### Phase 5 — the TUI picker, and the docs

- **Validate `route/list` daemon-side.** `route_suggestions` filters model ids
  through `route_chain` — the same check `AcpRouteSet` already runs — so the
  picker cannot offer a route `set` will refuse. This is a daemon fix; the TUI
  is unchanged, and its session-only charter is not touched.
  - Cost note: `route_chain` per model on every `route/list`. If that measures
    badly on a large catalog, cache the validated set per routing-table
    generation rather than dropping the check.
- **Generate the tool table** in `crates/bitrouter-mcp/README.md` and
  `skills/bitrouter/references/` from `ACTIONS`, or — the cheaper half — assert
  in a test that the README's table matches the const table. Prefer the test:
  `dist-helper` generation earns its keep for a 200-entry registry, not a
  6-row table.

### Deferred — `complete`

`complete` has no CLI twin, so it gains nothing from the table today. Its own
problems are real (`LocalBackend`/`CloudBackend` near-duplicates, `stream` not
exposed, every parameter beyond `model`/`messages`/`max_tokens`/`temperature`/
`system` dropped) but they are a *completion* problem, not a drift problem.
Port-ifying it is the last thing that lets `Backend` be deleted, and it should
be its own issue once phases 1–2 have proven the port shape against
multi-tenant HTTP.

## 7. Invariants the refactor must not break

1. **Multi-tenancy.** `CallerAuth` stays a parameter on every port that can
   reach an upstream. `tests/multitenant_http.rs` must keep passing unchanged —
   it is the regression test for the one thing port-ification could silently
   lose.
2. **Capability fencing.** `CAPABILITIES` exists so an HTTP client cannot see
   `route_preview`. Wiring more tools into stdio `mcp serve` (phase 4) must not
   widen the HTTP profile; the guard test asserts the HTTP-built router's tool
   set explicitly.
3. **No app dependency in the crate.** Enforced by the compiler, restated
   because the whole design leans on it.
4. **Stdout stays one JSON value.** Diagnostics to stderr, per
   [`output/mod.rs`](../apps/bitrouter/src/output/mod.rs). Reports gaining
   `Deserialize` must not gain a `Display` that writes to stdout.
5. **No `#[allow]`, no `unwrap`/`expect`/`panic!` in shipped paths**, and no
   type introduced "for the table" that no surface returns (CLAUDE.md 1–4).
6. **Skills root safety.** `is_safe_installed_path` containment survives the
   discovery unification; a global root must not become a path-traversal
   surface for a project-scoped caller.

## 8. Deliberate deviations from the issue

| Issue says | This spec says | Why |
|---|---|---|
| Guard test fails on any tool **or leaf** missing from the table | Only tools must have rows; rows' leaves must resolve | 103 leaves, 6 shared actions; a full mirror of clap is maintenance with no consumer |
| `apps/bitrouter/src/actions/` holds the typed input → report | Types in `bitrouter-mcp`, implementations in `apps/bitrouter/src/actions/` | `#[tool]` needs the type nameable in the crate; the app can't be a dependency of the crate |
| First PR includes "`route_preview` and `route` share one fallback and one report type" | That is phase 3, after the contract exists | Sharing a report type *is* the framework; doing it ad-hoc would produce a seventh bespoke shape |
| Generate the README tool table (dist-helper does this for the registry) | A test that the README matches; generation only if the table grows | Six rows |
| Replace `Backend` with ports (including `complete`) | `status` / `list_models` now; `complete` deferred to its own issue | `complete` is the only tool with no CLI twin and the only one where the cloud profile carries real weight |

## 9. Open decisions

D1, D3 and D4 carry recommendations and await sign-off. **D2 is decided** — see
its own note.

### D1 — does `bitrouter route --json` get to change shape?

> **Decided (human sign-off, 2026-09-04): option (a) — break it, changelog it.**
> No longer a recommendation. The reasoning below stood; it was chosen, not
> merely proposed.

Phase 3 renames `model` → `requested_model` and `chain[].protocol` →
`provider_chain[].api_protocol`, and adds `estimated_cost`. That is a breaking
change to an agent-readable surface. Options: (a) break it, changelog it — the
CLI is pre-1.0 and the richer shape is the right one; (b) keep both keys for one
release with the old ones marked deprecated. **Recommendation: (a).** Duplicated
keys are exactly the drift this spec exists to remove.

**As implemented.** The break is one changelog entry showing both shapes, under
the existing `- **Breaking (CLI):** …` convention, plus a
`- **Breaking (Rust API):** …` entry for the port
(`RoutingQuery::preview → serde_json::Value` becoming
`RouteQuery::route → RouteReport`). The report grew three further fields beyond
what D1 anticipated — `effective_model`, `effective_effort` and
`policy_decision` — which are *additive* to `route_preview`'s vocabulary and
therefore not part of the break; they are what the policy half of phase 3
produces, and `effective_model` is the field a reader should actually act on.

### D2 — the cloud profile

> **Decided (human sign-off, 2026-09-04): a universal spend position.** No
> longer a recommendation. The original reasoning is kept below; the decision
> that overrode it follows it. The anchor keeps its name so the links above
> still resolve.

`StatusInfo::Cloud` returns a credit balance, and #863 §22 has already flagged
"should the HTTP/cloud profile live in OSS at all" as open. Phase 1 needs an
answer only to the narrow question: does `StatusReport` carry an optional
`credits` block, or do local and cloud status stay two actions? **Recommendation:
one report with `credits: Option<Credits>`** — an agent asking "am I OK to spend"
should not need to know which deployment it is talking to. If #863 moves the
cloud profile out, the field goes with it.

**[corrected post-implementation] — decided: one report with a universal spend
position.** Not `credits: Option<Credits>`. The recommendation above got the
*shape* right (one report, not two actions) and the *content* wrong, in a way
only visible once built.

`Credits` was field-for-field `GET /v1/billing/balance`. That models one
deployment's billing flavour instead of the fact underneath it, and it fails the
question it was named for twice over:

- **A BYOK deployment could not answer it at all.** `credits` is prepaid cloud
  balance; a local install pays its providers directly and has none. Yet
  BitRouter *does* know that deployment's spend — `metering::store`'s
  `spend_summary(TimeWindow)` is the same read `LocalCostFooter` already made.
  "Am I OK to spend?" had a real local answer the report simply did not carry.
- **The two surfaces disagreed.** `bitrouter status` never populated `credits`;
  only the MCP tool's cloud profile did. That is exactly the drift this spec
  exists to stop, and it slipped past the guard test — the guard pins the
  *schema* both surfaces advertise, not which fields each one fills. Worth
  stating plainly as a limit of the mechanism: a shared type stops the shapes
  diverging, not the contents.

The replacement is `spend: Option<Spend>` with two **independent** halves, each
separately optional:

```rust
Spend  { currency, spent: Option<Spent>, limit: Option<SpendLimit> }
Spent  { window, estimated_micro_usd, requests, unpriced }
SpendLimit { balance_micro_usd, pending_micro_usd, remaining_micro_usd }
```

- `spent` is money already gone — universal, from the metering store.
- `limit` is money left before a cap — only where a cap exists.

The split is what lets both deployments be honest. The local store knows spend
and no cap; the cloud balance endpoint knows the cap and *not* spend-to-date. A
single flat block would have forced one of them to invent the half it cannot
see. Local fills `spent`; cloud fills `limit`; neither fabricates the other.

Three constraints the shape has to hold, all of them about not costing the user
money:

- **`unpriced` stays visible.** `SpendSummary` excludes rows without charge
  evidence, because summing them would report a floor as a price. The local
  figure is therefore a partial estimate — hence `estimated_micro_usd`, named so
  the reader cannot miss it — and `unpriced` says how much of the window it does
  not cover. A `SpendLimit` is an authoritative ledger. Both are carried
  verbatim: nothing is averaged, rounded away, or hidden.
- **Best-effort, never fatal.** `metering::reader::open_readonly` already returns
  `Option`; an absent or unreadable database is `spend: None`, never a failed
  `status`. `spend` also rides a *stopped* daemon, since what a past daemon spent
  is on disk and stays true after it exits.
- **`spend`, not `cost`.** `cost` is taken: `route_preview` returns
  `estimated_cost`, the prospective per-token rate of a request not yet made.
  `spend` is money already gone, and is the existing vocabulary in `metering/`.

Two things this does **not** do. `spend` is *machine-wide* on the local path —
`spend_summary` rolls up every caller, and `CallerAuth` is ignored there.
Scoped reads exist in the store (`get_spend` per key,
`spend_summary_for_launch`, `spend_summary_for_acp_session`), so closing it is
attribution plumbing, not a new measurement; it is documented at the ignore site
and deliberately deferred, because today's only local `status` surfaces (the CLI
and stdio `mcp serve`) are single-tenant by construction. And a locally issued
API key's `spend_limit_micro_usd` is a *second* kind of cap that would fit
`SpendLimit`, but nothing reaches it from `status` today, so it is left
unmodelled (CLAUDE.md 4) with the extension point noted in the type's doc.

If #863 moves the cloud profile out, `limit` goes with it and `spent` stays —
which is the point: the universal half does not depend on the deployment.

### D3 — invalid skills: marked, or hidden?

Phase 4 proposes surfacing invalid skills with a `problem` string on both
surfaces. The alternative is to hide them everywhere, making CLI and MCP agree
by subtraction. **Recommendation: mark them.** A skill you wrote and cannot use
is a debugging question, and today the CLI answers it by accident while the
agent silently can't see the skill.

**[implemented on the recommendation; still not signed off.]** Phase 4 built
"mark them" and found nothing wrong with it, so it stands as recommended rather
than decided — a human has not weighed in, and this note is here so nobody reads
the implementation as the sign-off.

Two things building it clarified, neither of which changes the recommendation:

- **The two policies are not in tension; they answer different questions.**
  `skills/list` filtering to valid entries is not a competing choice — the SEP
  requires an entry's `frontmatter` to match the `SKILL.md` a host fetches
  field-by-field and its final URI segment to equal `frontmatter.name`, so an
  invalid skill has *no conforming entry to publish*. Marking on the tool and
  CLI surfaces and omitting from the catalog is one validation policy
  (`DiscoveredSkill::problem`) read twice, not the two-policy split the
  alternative was worried about.
- **The "hide them" option would have cost the phase its own headline.** Hiding
  everywhere does make the surfaces agree, but the agreement is that a skill
  with a typo in its frontmatter does not exist — which is the failure mode the
  phase was written to remove, restored in a tidier form.

The residual argument for hiding is that `valid: false` rows are noise in an
agent's context. If that turns out to matter, the cheap answer is a filter
argument on `skills_search`, not a change of policy — the row already carries
what a filter would key on.

### D4 — does `route/list` validation belong on the hot path?

Phase 5 adds a `route_chain` call per candidate model. Unknown cost on a large
catalog. **Recommendation: implement it straight, measure, and cache per
routing-table generation only if it shows up.**

## 10. Acceptance

- `ACTIONS` exists with a row per MCP tool; the guard test fails when a tool is
  added without one, when a row's leaf is renamed, and when a tool's
  `output_schema` diverges from its row's. Rows carrying `output_schema: None`
  are the remaining phases' backlog. *(Met at phase 1; all three failure modes
  were provoked and observed.)*
- For every row with both surfaces: `bitrouter <leaf> --json` and the MCP tool's
  structured content deserialize into the **same** Rust type in a test that runs
  both. *(Met for `status` at phase 1 and `route` at phase 3 —
  `both_surfaces_produce_the_same_report` runs the leaf's path and the port and
  compares the serialized bytes; `both_surfaces_apply_the_policy_table` does the
  same over a config where the policy table selects a different effective model
  than the one requested, which is the case the two surfaces used to disagree
  on.)*
- `bitrouter models` and `list_models` return the same models with the same
  provider lists, with no daemon running. *(Met at phase 2:
  `actions::models::tests::both_surfaces_keep_every_provider_of_a_model` runs
  both surfaces against a config where one model has two providers and asserts
  the two serializations are equal, with nothing listening on the socket.)*
- `status` over MCP with the daemon stopped returns `running: false` and is not
  a tool error.
- A skill with malformed frontmatter, a `./skills/foo` skill, and a user-global
  skill each appear identically in `bitrouter skills list` and `skills_search`.
  *(Met at phase 4: `actions::skills::tests::one_disk_three_surfaces` builds all
  three on one disk and runs the CLI path, the port, and the SEP catalog
  together, asserting every CLI row appears byte-identically in the tool's
  answer and that the tool's scope is exactly the CLI's two scopes together. The
  malformed skill appears on both, marked, and is absent from `skills/list` —
  which is [D3](#d3--invalid-skills-marked-or-hidden) as built.)*
- An `mcp install`-ed client sees the skills tools. *(Met at phase 4:
  `profile_tests::an_installed_client_sees_the_skills_surfaces` pins what
  `mcp install` writes and asserts the profile it launches carries both tools
  and declares the SEP extension.
  `wiring_skills_into_stdio_does_not_widen_the_http_profile` is the other half
  — invariant 2 — over both the tool list and the declared capabilities.)*
- `_bitrouter/route/list` never offers a route `_bitrouter/route/set` refuses.
- `crates/bitrouter-mcp/` contains no `reqwest` call for `status` or
  `list_models`, and no dead doc link.
  - **[corrected post-implementation]** The `list_models` half is unachievable
    and should not have been written. The HTTP profile is built from an
    `Arc<dyn Backend>` and nothing else, and `GET /v1/models` against the cloud
    account with the caller's own bearer is the *correct* answer for that
    deployment — there is nothing app-side to move it to. The reqwest call
    survives, relocated from `Backend::list_models` into
    `impl ModelsQuery for {Local,Cloud}Backend`. What the criterion was reaching
    for does hold: the crate keeps no *second implementation* of the action, and
    the surface an agent reads is one shared type. The same applies to `status`,
    where phase 1 already recorded it. Both close when `complete` is port-ified
    and `Backend` goes away.
- `cargo nextest run --all-features`, `cargo clippy --all-features`,
  `cargo fmt -- --check` clean; `skills/bitrouter/` updated in the same PRs.

## 11. Explicitly out of scope

- The daemon-owns-config question (#863) — this spec assumes today's topology
  and stays correct under either answer.
- Session verbs and `machine::Effect` (#866).
- The TUI's data model. Its session-only charter (`ACP_TUI_SPEC.md` §8.3) is
  unchanged; only the daemon's answer to `route/list` gets stricter.
- Streaming `complete`, and the cloud profile's placement.
