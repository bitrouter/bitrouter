# Spec: the observability TUI — `bitrouter status --watch` and `launch --tui`

> **SUPERSEDED 2026-08-16 — the live view is gone.** The `launch --tui` half was
> already superseded by [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md). The
> `status --watch` half is now superseded too: the self-refreshing ratatui table
> this spec designs was removed, and `bitrouter status --requests` prints the
> same snapshot as text. §6's argument against a cargo feature is retained and
> still correct, but it no longer applies to `ratatui` — `apps/bitrouter` does
> not depend on it at all, which is a stronger version of the same goal.
>
> What survives and is still authoritative: the data layer (§7, `snapshot.rs`),
> the stream row and footer formats (§8.1), and the honesty rules — including
> the one this spec states and the removed view never kept, that a spend figure
> must say whose spend it is. `status --requests` still does not label its
> scope; see `crate::chat::cost` for the rule applied properly.

Status: **`status --watch` implemented and authoritative; the `launch --tui`
half is superseded by [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md)** · Author: Claude
(with Spikel) · Date: 2026-08-10
Issues: #782 (hosted bar) · #797 (live view) · #795 (attribution) · #796 (startup line)
Supersedes the CLI framing of #782 (`bitrouter top`, `bitrouter tui` deprecation).

> **This spec is half live.** Everything about `bitrouter status --watch` — §1,
> §§4–8, §10.1, §13.2–13.3 — describes what ships today and remains the
> authority for that view.
>
> Everything about `bitrouter launch --tui` — the hosted mode, the emulator,
> the PTY host, the pinned status bar, and the fidelity matrix that gated them
> — is **superseded by [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md)**. The flag,
> `tui/{host,pty,term,conformance}.rs`, `tui/fixtures/`,
> `scripts/record-vt-fixture.sh`, and `TUI_FIDELITY_MATRIX.md` are deleted; the
> replacement is an inline-viewport ACP client, not a terminal emulator. The
> hosted sections below are kept as the record of a decision that was reversed,
> and are marked where they start.

**Context.** #749 ratified that BitRouter is a self-improving LLM router, not an
agent-orchestrator, and #786 executed that: `apps/bitrouter/src/tui/` (~11.5k
lines), `fleet_mcp.rs`, `fleet.rs`, and the `bitrouter tui` command are gone,
along with nine dependencies. This spec defines what replaces them — a router
UX ("what did this cost, which model served it, why did it go there"), not an
orchestration UX.

Two surfaces, one implementation:

| Surface | What it is | Issue |
|---|---|---|
| `bitrouter status --watch` | a live, self-refreshing view of the gateway | #797 |
| `bitrouter launch --tui` | a harness hosted in an emulator with that view's status bar pinned underneath | #782 |

---

## 1. Motivation

`bitrouter status` answers "is the daemon up" in two lines. Nothing answers the
question a user actually has while an agent is running: **what is it doing right
now, and what is it costing me?**

There is, today, **no local spend surface at all** beyond the one-line exit
summary `launch` prints when a harness quits
([spawn.rs:390](../apps/bitrouter/src/spawn.rs:390)). `bitrouter workflow-state
metering-usage` exists but is a benchmark-bundle exporter — it demands an
explicit `--database-url` and writes JSONL to a file. The one machine-wide spend
readout, `fleet_cost`, lived in `fleet_mcp.rs` and was deleted with the
orchestrator in #786.

The *storage* exists: `metering::entities::requests` records `model_id`,
`provider_id`, tokens (prompt/completion/reasoning/cache-read/cache-write),
`estimated_charge_micro_usd`, `latency_ms`, `error`, and `created_at` per settled
request, and `metering::reader::open_readonly` opens the store **without a
running daemon**. The *query layer* does not yet expose what this spec needs —
see §7.2, which is real work, not plumbing.

The test for whether a view belongs in a TUI rather than a command: **does it
change while you watch it?** A spend breakdown does not; a request stream does.
That test sets v1's scope (§4) — the header's rolling total covers the daily
"what did today cost," and a full breakdown can ship later as a view *or* as a
plain command, since it does not need to be live to be useful.

## 2. Goals / non-goals

**Goals**

- A live request stream: time, model, provider *actually* used, tokens, cache,
  cost, latency, status.
- A status bar that renders uniformly across hosted harnesses and **degrades
  honestly** where the data cannot exist (§7.2). *(Superseded — the hosted bar
  is deleted. The honest-degradation rule survives it and carries into
  [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) §8.3.)*
- One renderer and one data layer behind both surfaces.
- `launch --tui` is a strict superset of `launch` in child behavior — true by
  construction, not by test discipline (§5).
- Credential and config management **without the TUI ever handling a secret or
  writing a config file** (§8.4).

**Non-goals**

- Anything the orchestrator did: fleet, subagents, worktree isolation, the
  review queue, permission brokering, autonomy tiers, ACP-rendered panes,
  multi-pane splits. This spec has **no ACP dependency at all**.
- Replacing `status`, `models`, `providers`, or `workflow-state
  metering-usage`. This is a lens over what they already report.
- A second credential path, a second config writer, or a second metering query
  layer.

## 3. CLI surface

### 3.1 `bitrouter status --watch`

```
bitrouter status [--watch] [-c <config>] [--socket <path>]
```

`--watch` / `-w` upgrades the existing one-shot into the live view. **Bare
`bitrouter status` is byte-identical to today** — it stays a script-parseable
probe that works when the daemon is down.

- Non-tty stdout with `--watch` → print **one** snapshot as a table and exit,
  honoring the existing `--json` / `--human` output mode. `bitrouter status
  --watch --json | jq` must work; refusing to run without a tty would be the
  worse default.
- Rejected: `bitrouter top` (accurate for v1's stream, misleading the moment it
  manages anything) and making bare `status` interactive on a tty (the surprise
  runs the wrong way — a tty check protects scripts, not people).

### 3.2 `bitrouter launch --tui`

```
bitrouter launch -a <harness> [--tui] [--model <id>] … [-- <agent args>]
```

- `--tui` is **opt-in** and stays opt-in until it clears the gate in §12 —
  which was redefined once the original "clean across several harness
  releases" proved circular.
- `--tui` **conflicts with `--check`** at the clap level. `--check` preflights
  and exits; there is no display to attach to. Do not silently ignore the flag.
- `--tui` requires a tty on stdout; otherwise it errors, naming plain `launch`.
- Every error raised from inside the emulator names `launch` **without**
  `--tui` as the escape hatch.

**`launch`'s own docstring changes in the same PR.** It currently reads "Launch
a coding-agent harness as an interactive native-TUI child"
([main.rs:373](../apps/bitrouter/src/main.rs:373)), which makes `--tui` read as
"the TUI one, as opposed to the other TUI." The rewrite must make clear the
child is always a TUI, and that `--tui` selects **who owns the screen**.

## 4. Scope by version

| | v1 | v2 |
|---|---|---|
| Request stream | ✅ | |
| Header: daemon state + spend rollup | ✅ | |
| Status bar | ✅ | |
| `r` reload / `e` edit config (the suspend primitive) | ✅ | |
| Spend-by-model view | | ✅ |
| Providers view + `l` login / `L` logout | | ✅ |
| Route-decision detail (Enter on a row) | | ✅ |
| `launch --tui` | ✅ (last PR) | |

v1 is stream-only. The **suspend primitive** (§8.4) ships in v1 anyway, wired to
`reload` and `$EDITOR` — it is the riskiest piece of the management story, and
proving it against a trivial, reversible action beats debuting it under an OAuth
browser flow.

The route-decision detail is the differentiated view — nothing else in the
ecosystem can render "asked for X, chose provider Y over Z because adequacy 0.94
and $3.00/1M vs $3.75." It is deferred to v2 only to keep v1's data layer on the
metering store alone. **Forward-compat requirement:** `MeteringUsageRecord`
already carries `request_id`, and `trajectory::StoredRequest.request_id` is the
same key — do not drop that field when mapping into a stream row. Carrying a
field already fetched is not over-design; re-plumbing it later is a refactor.

## 5. The `prepare` / `exec` seam

[`spawn::run`](../apps/bitrouter/src/spawn.rs:253) splits into three:

```rust
async fn prepare(source, cfg, opts) -> Result<Prepared>  // everything up to the spawn
async fn exec_inherited(Prepared) -> Result<()>          // today: cmd.status() + exit
async fn exec_hosted(Prepared)    -> Result<()>          // tui::host::run(...)

struct Prepared<'a> {
    binary:        PathBuf,
    launch:        ChildLaunch,      // { env, args_prefix } — the routing overlay
    agent_args:    Vec<String>,      // forwarded verbatim, after args_prefix
    base_url:      String,
    harness:       &'static Harness,
    source:        &'a ConfigSource, // for the exit summary's metering read
    session_start: DateTime<Utc>,    // stamped by prepare, used by both execs
}
```

Both `exec_*` diverge via `std::process::exit` after `print_exit_summary`; the
`Result<()>` return is for the error paths before that (an `async fn` cannot
return `!`).

`prepare` keeps the whole existing sequence: codex config-flag conflict check,
install-on-missing, daemon ensure/auto-start, auth precedence, model catalog
fetch, `launch_overlay` assembly, and the #796 startup line.

**`agent_args` is part of the struct, not a separate parameter.** Today
`run` does `cmd.args(&launch.args_prefix)` then `cmd.args(&opts.agent_args)`
([spawn.rs:356](../apps/bitrouter/src/spawn.rs:356)). If the seam carried only
the overlay, everything the user typed after `--` would sit outside the
equality guarantee — precisely the drift class this seam exists to eliminate.

**This is what makes #782's "byte-identical child env and args" requirement true
by construction.** Both modes call one producer, and the test asserts on
`(Prepared.launch, Prepared.agent_args)` — a struct comparison, not a spawned
process. As a separate command it would be a promise policed by tests forever;
as a flag with one `prepare` it cannot drift.

**`launch_id` was deliberately absent from PR 1.** It arrived with #795 (§9.4),
which gave it a consumer in the exit summary; landing an unconsumed field
earlier would have been exactly the unused type CLAUDE.md guideline 4 forbids.

### 5.1 The env contract — the requirement as stated is false

Hosted mode **must** diverge on terminal identity. `pty.rs` already sets
`TERM=xterm-256color` and `COLORTERM=truecolor` before applying the overlay,
with the correct rationale: *TERM must promise only what the emulator renders.*
Passing through `xterm-kitty` invites graphics sequences the emulator cannot
draw. Verified separately: `portable_pty::CommandBuilder::new` inherits the
parent environment via `std::env::vars_os()`, and additionally sets `SHELL` on
unix when absent.

So the requirement is written as two:

1. **`Prepared.launch.env` and `.args_prefix` are identical in both modes.**
   Unit test, direct struct comparison.
2. **The hosted process env may differ only by the union of the three consts
   below.** Test that the observed delta is a subset of that union.

```rust
/// Set by the host (the emulator's real capabilities).
const HOSTED_ENV_SET: &[&str] = &["TERM", "COLORTERM"];
/// Set by portable-pty when absent in the parent.
const HOSTED_ENV_MAY_ADD: &[&str] = &["SHELL"];
/// Unset by the host: inherited values that would lie about the terminal
/// the child is actually talking to, and would steer harnesses onto
/// rendering paths the emulator does not implement.
const HOSTED_ENV_UNSET: &[&str] = &[
    "TERM_PROGRAM", "TERM_PROGRAM_VERSION",
    "KITTY_WINDOW_ID", "KITTY_PID",
    "WEZTERM_EXECUTABLE", "WEZTERM_PANE", "WEZTERM_UNIX_SOCKET",
    "ITERM_SESSION_ID",
    "ALACRITTY_SOCKET", "ALACRITTY_LOG", "ALACRITTY_WINDOW_ID",
    "LINES", "COLUMNS",
];
```

## 6. Architecture

```
apps/bitrouter/src/tui/
├── mod.rs          run_watch(…) · run_hosted(…)
├── lifecycle.rs    raw mode, alt screen, kitty push/pop, XTWINOPS title
│                   save/restore, panic hook, restore(), suspend()
├── term.rs         restored verbatim (967)      — the emulator
├── pty.rs          restored (265), adapted      — PTY spawn + I/O
├── data/
│   ├── snapshot.rs Snapshot — the one struct both surfaces render
│   ├── source.rs   trait SnapshotSource
│   └── poll.rs     metering-store poll + control-socket Status
├── render/
│   ├── bar.rs      the one-row status bar — shared by both surfaces
│   └── stream.rs   the request stream list
└── host.rs         hosted loop: PTY pane rows 0..h-2, bar at h-1
```

The module is `tui/` again, not `term/`: it *is* a TUI, just not the one that
was removed.

**Always compiled — no cargo feature.** #786 removed the `tui` feature along
with nine deps; six come back (§11.2). At ~2.5k lines instead of 11.5k, a
feature flag that sometimes makes a documented CLI flag vanish costs more
confusion than it saves build time.

## 7. The data layer

### 7.1 `Snapshot` and `SnapshotSource`

```rust
trait SnapshotSource {
    async fn poll(&mut self) -> Snapshot;
}
```

v1 implements it by polling `metering::reader::open_readonly` at **1 Hz** plus a
control-socket `Status`. The trait exists so a daemon-pushed SSE endpoint can
replace polling later without touching the renderer — it is one method on one
type, not speculative abstraction.

`Snapshot` carries exactly what the two surfaces render:

| Field | Source |
|---|---|
| `daemon: Option<{pid, listen, models}>` | control socket `Status` |
| `mode: Live \| HistoryOnly \| Empty` | §7.3 |
| `rows: Vec<RequestRow>` | **new** `recent_requests` (§7.2) |
| `summary: SpendSummary` | `spend_summary(window)` — `{spend_micro_usd, requests}` |
| `rate: RateMetrics` | **new** all-caller rate (§7.2) |
| `scope: DaemonWide \| Launch(LaunchId)` | §9.4 |

`RequestRow` keeps `request_id`, `created_at`, `model_id`, `provider_id`, the
token breakdown, `estimated_charge_micro_usd`, `latency_ms`, `status`,
`error_code`.

### 7.2 The query layer is new work, not plumbing

**The existing read side cannot serve §8.1's stream.** Two verified gaps:

1. **`export_usage` drops the three columns every row of the stream renders.**
   `MeteringUsageRecord` ([store.rs:72](../apps/bitrouter/src/metering/store.rs:72))
   carries tokens, `request_id`, `status`, `error_code`, and
   `final_charge_micro_usd` — and **no `created_at`, no `latency_ms`, and no
   `estimated_charge_micro_usd`**. The entity has all three
   ([requests.rs](../apps/bitrouter/src/metering/entities/requests.rs)); the
   export shape discards them.
2. **`get_rate`, `get_spend`, and `get_token_usage` are scoped to one
   `api_key_id`** ([store.rs:425](../apps/bitrouter/src/metering/store.rs:425),
   :305, :397). The footer's daemon-wide req/min and tok/min cannot come from
   them. Only `export_usage` and `spend_summary` are all-caller.

Additionally `export_usage` is an **unbounded ascending full scan** with no
`LIMIT` ([store.rs:449](../apps/bitrouter/src/metering/store.rs:449)) — correct
for a benchmark export, wrong to run once a second against a day-long window.

So PR 3 adds two read-side queries, and the spec budgets them as such:

```rust
/// Newest-first page of settled requests, for the live stream.
async fn recent_requests(&self, window: TimeWindow, limit: u64) -> Result<Vec<RequestRow>>;
/// All-caller request/token rate over the trailing minute.
async fn get_total_rate(&self) -> Result<RateMetrics>;
```

`recent_requests` orders **descending with a `LIMIT`** (the view renders one
screen plus scrollback margin), which fixes the scan cost as a side effect. It
selects the entity columns directly rather than widening `MeteringUsageRecord` —
that record is a stable export artifact consumed by benchmark bundles, and this
is a different read with a different shape.

### 7.3 Prerequisite: SQLite journal mode

Verified in `sqlx-sqlite-0.8.6`: `busy_timeout` **already defaults to 5 s**
(`options/mod.rs:201`), so that half needs nothing. But sqlx deliberately leaves
`journal_mode` alone — *"Don't set `journal_mode` unless the user requested it"*
(`options/mod.rs:177`) — so the store runs on a rollback journal, where a reader's
SHARED lock blocks the daemon's writes. Both surfaces add a second process
polling at 1 Hz.

Three constraints make this **not** a one-line change in the reader:

- WAL is a **permanent property of the database file**, and switching into it
  takes an exclusive lock that `sqlite3_busy_timeout` cannot wait on. It must be
  set once, by a writer.
- The reader pins `?mode=ro`
  ([reader.rs](../apps/bitrouter/src/metering/reader.rs)) and **cannot** flip it.
- [`db::connect`](../apps/bitrouter/src/db/mod.rs:34) is multi-backend, and
  sea-orm's `ConnectOptions` exposes no pragma hook — so this is a URL parameter
  or an `execute_unprepared`, gated to `sqlite://` URLs only.

**Therefore it belongs on the daemon's writer connection, not in the reader-side
PR.** Existing installs adopt WAL on the next daemon restart; until then, a
running old daemon plus a new reader still contend. That is acceptable
degradation (brief write stalls, not corruption) and must not block the view.

### 7.4 Modes, including a dead daemon

`open_readonly` gives history for free, and the moment you most want this view is
right after the daemon died. The mode is **stated in the header**, never implied
by an empty list:

| daemon | store | header |
|---|---|---|
| up | any | `● live` |
| down | has rows | `○ history only — daemon not running` |
| down | none/absent | `○ nothing recorded yet — try bitrouter serve` |

## 8. `bitrouter status --watch`

### 8.1 Layout

```
┌ bitrouter · ● live · pid 4412 · 127.0.0.1:4356 · 47 models · otel off ──┐
│ 14:32:07  claude-sonnet-4-5 → openrouter  ↑12.4k ↓891  $0.042  1.8s  ok │
│ 14:32:01  gpt-5             → openai      ↑3.1k  ↓204  $0.011  0.9s  ok │
│ 14:31:44  gpt-5             → openai      ↑2.8k  ↓0    —       0.4s  429│
│ …                                                                        │
├──────────────────────────────────────────────────────────────────────────┤
│ today $4.71 · 128 req · 3.2 req/min · 12.4k tok/min                      │
└──────────────────────────────────────────────────────────────────────────┘
```

Header carries the spend rollup so "what did today cost" stays answerable
without the deferred spend view. The bottom row is `render/bar.rs` — the same
widget `launch --tui` pins under the harness, scoped daemon-wide here.

### 8.2 The re-sorting list problem

A 1 Hz refresh over a recency-sorted list means a cursor on row 3 points at a
different request every second. Two rules, both required:

1. **The cursor is keyed to `request_id`, never to a row index.** On refresh,
   re-find the id; if it aged out, clamp to the nearest surviving neighbour.
2. **Auto-follow is pinned off when the cursor is not at the live edge** —
   the identical rule `term.rs` already applies to scrollback (`is_scrolled()`
   suppresses the jump to bottom). Same semantics across both surfaces.

`claude/tui-manager-reference` solved cursor clamping against a re-sorting
collection; that reducer is worth reading (its *content* — agents, decide/review
verbs — is dead orchestration; its shell is not).

### 8.3 Keys (v1)

| Key | Action |
|---|---|
| `↑`/`↓`, `j`/`k` | move cursor (pins auto-follow off) |
| `g`/`G` | top / live edge (re-arms auto-follow) |
| `r` | **runs `bitrouter reload`** |
| `e` | **opens `$EDITOR` on `bitrouter.yaml`, then reloads on exit** |
| `q`, `Ctrl-C` | quit |
| `?` | help |

Every mutating key echoes its CLI equivalent in the footer: `↩ ran: bitrouter
reload`. That makes the TUI a discovery surface for a 30-subcommand CLI rather
than a replacement for it.

### 8.4 Management without handling secrets

The rule:

> **The TUI never handles a secret and never writes config itself. Mutating
> actions suspend the TUI and run the existing interactive command on the real
> terminal.**

This is not a restriction on the management story — it is how the management
story gets to be good. `bitrouter providers login <id>` already **is** the
credential editor, with per-provider methods auto-derived from the catalog
(Claude Code session adoption, ChatGPT PKCE, GitHub device flow, API-key paste),
writing to `bitrouter_providers::oauth::credential_store::Credential` — **not**
to `bitrouter.yaml`. `providers logout` exists alongside it.

So v2's providers view gets `l`/`L` and a full credential manager with **zero
new secret-handling code**. What must never appear is a ratatui text field that
accepts an API key: it would be a second credential path ignorant of the OAuth
flows and the credential store, with the secret sitting in a redrawable buffer
that can reach a panic dump or the log.

For non-credential config, `e` → `$EDITOR` → `reload` is likewise the correct
answer, not a cop-out: `bitrouter.yaml` is hand-maintained and commented, and a
TUI that round-trips it through serde_yaml **will silently delete every
comment**. Note that even `bitrouter init`, which does own a config writer,
refuses to overwrite an existing config without `--force`.

**`lifecycle::suspend()`** is the primitive: leave alt screen, restore cooked
mode, run the child on the real tty, re-enter, force a redraw. ~40 lines given
lifecycle is already factored out, and the same shape as hosting a child — which
makes it unusually cheap for this codebase specifically.

## 9. `bitrouter launch --tui`

> **§§9–12 are superseded by [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md).** The flag
> and every file described here are deleted. Kept as the record of a reversed
> decision — read them as history, not as a description of the codebase.

### 9.1 Why an emulator, not a reserved line

The cheap design — reserve a line with `DECSTBM` and let the harness write to
the real tty — does not survive the requirement. `term.rs` tracks alt-screen
state (`\x1b[?1049h`) because some harnesses render inline on the main screen and
others take the alternate screen, and an alt-screen app owns the whole display
and clobbers a reserved line. Uniform chrome was taken as the product
requirement, so BitRouter must own the screen and composite. The emulator's
cost is the price of uniformity specifically.

*(That premise is what [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) rejects: uniformity
across the catalog was never delivered — `launch --tui` was only ever offered
for four of the eight harnesses — and it was not worth an emulator.)*

### 9.2 Geometry and input

- Outer terminal is `(cols, rows)`. The PTY child gets `(cols, rows - 1)`; the
  bar owns the last row. `SIGWINCH` recomputes and calls `pty.resize`.
- **Zero BitRouter keybindings in v1.** Every prefix key collides with something
  across eight harnesses — that is why tmux users remap `C-b` — and none is
  needed: `Ctrl-C` correctly reaches the child, and the child exiting ends the
  session. Pure passthrough is both the safest v1 and the least surprising.
- The one gesture that branches is already implemented in `term.rs`: wheel →
  `encode_mouse()` when the inner app enabled mouse reporting
  (`mouse_enabled()`), else page BitRouter's own scrollback.

If v2 wants "expand the bar into the full view in place," that is when a prefix
key earns its complexity, and it should be config-opt-in.

### 9.3 Accepted tradeoffs

| | `launch` | `launch --tui` |
|---|---|---|
| Rendering | child owns the real tty — what harness authors tested | recomposited; mouse encoding, OSC-52, kitty flags, bracketed paste, SIGWINCH, alt-screen all proxied |
| Scrollback | your terminal's — native search, selection, configured depth | **BitRouter's.** `Cmd-F` stops finding agent output; copy routes through the OSC-52 relay |
| Nesting | terminal → (tmux) → harness | terminal → (tmux) → **bitrouter** → harness |
| Blast radius | a BitRouter bug cannot affect a running harness | a panic in the render loop takes the session |
| Maintenance | ~0 lines of terminal code | ~1,200 lines of terminal protocol, permanently |

**Scrollback is the cost weighted heaviest** — daily friction, not one-time
risk. That is why `launch` stays the default and the recommended daily driver.

### 9.4 Scope of the bar's numbers

Hosted mode scopes the bar to **this launch** via `Prepared.launch_id`,
consuming #795. Until #795 lands, the honest fallback is the same time-window
read [`print_exit_summary`](../apps/bitrouter/src/spawn.rs:390) already
performs — `TimeWindow::Custom { session_start..now }`, daemon-wide — labelled
as such (`since launch · all callers`), never as "this session."

**Shipped (#795).** Attribution rides the **credential slot, not a header** —
header injection would reproduce the same 4-of-8 ceiling as MCP, whereas
`launch` already sets a bearer for every routed harness through whatever
mechanism that harness has. `resolve_launch_token`'s last rung mints an opaque
`brl_<uuid>` instead of the old fixed placeholder; `skip_auth` ingress
recognises it and produces a *tagged local* caller — attribution without
authentication — recorded in a nullable `launch_id` column with its own index.

Three properties worth keeping when this is extended:

- **A user-supplied credential always wins and is never tagged.** The upper two
  precedence rungs are real authentication; rewriting them would break
  `skip_auth: false` outright. Those launches fall back to the window summary,
  and the exit line *says* `spend since launch (all callers)` rather than
  claiming a precision it does not have.
- **The tag grants nothing.** It is read only on the `skip_auth` path, where
  the daemon already serves every local caller without credentials — a client
  that could forge a tag could equally well just spend. No authorization
  decision may ever read it.
- **`launch_id` is its own column, not an overloaded `api_key_id`.** Under
  `skip_auth` that column is the synthetic `local` caller; putting an
  unauthenticated tag in a field named for keys would mislead every later
  reader of the schema.

## 10. The status bar

### 10.1 Content

Scoped to **LLM token observability only**: tokens in/out, cache read/write,
context-window occupancy, cumulative cost, model, the provider that actually
served, latency.

```
claude · anthropic/claude-sonnet-4-5 → openrouter · ↑12.4k ↓3.1k · cache 8.2k/1.1k · ctx 31% · $0.42 · p50 1.8s ●
```

Explicitly **not** in the bar: skills/MCP capability flags. Those belong in the
#796 one-line startup message before the harness takes the screen, where they
are actionable — not in a row the user stares at all day.

Two honesty constraints:

- **Context occupancy is derived from the *last* request's prompt tokens**
  against the registry's context length. It is not live. The label must not
  imply otherwise.
- **Suppress `p50 latency` below a request-count threshold.** A p50 over three
  requests is noise, and a wrong number is worse than no number.

### 10.2 Uniform chrome, not uniform capability

The bar renders identically for every hosted harness. What it can *say* does
not. Verified in `harness.rs`: the `grok` and `antigravity` arms return
`RoutingOverlay { env: Vec::new(), args: <model flag> }` and ignore the `mcp`
parameter; the `pi-acp` arm carries the comment *"No MCP mechanism — `mcp` is
ignored"*; `openclaw` likewise injects none.

| Harness | Routed → metered | MCP/skills injectable |
|---|---|---|
| claude | ✅ | ✅ |
| codex | ✅ | ✅ |
| opencode | ✅ | ✅ |
| hermes | ✅ | ✅ |
| pi | ✅ | ❌ no MCP mechanism |
| openclaw | ✅ | ❌ no MCP mechanism |
| grok | ❌ own-auth | ❌ |
| agy | ❌ own-auth | ❌ |

grok and agy are subscription clients whose own traffic never traverses the
daemon — the daemon *borrows* their sessions to serve other requests as
providers — so no metering rows exist for them.

**Degrade honestly.** A blank cost field on grok reads as broken;
`own-auth · not routed · not metered` reads as true.

```
grok   own-auth · not routed · not metered
pi     routed · no requests yet
*      daemon unreachable
```

## 11. Restore manifest

A working PTY host and emulator exist in history. Do not rebuild them.

| Ref | Holds |
|---|---|
| tag `orchestrator-final` | the full orchestrator, including #781's harness synthesis |
| branch `claude/tui-manager-reference` | the later single-screen manager — better chrome |

### 11.1 What comes back

```bash
git show orchestrator-final:apps/bitrouter/src/tui/term.rs > apps/bitrouter/src/tui/term.rs
git show orchestrator-final:apps/bitrouter/src/tui/pty.rs  > apps/bitrouter/src/tui/pty.rs
```

- **`term.rs` (967) restores verbatim.** Imports are only `alacritty_terminal`,
  `crossterm`, `ratatui`, `termwiz`, `wezterm-input-types` — zero coupling to
  anything deleted. Its `TerminalBackend` trait already covers the hard
  decisions: `alt_screen()`, `mouse_enabled()`, `encode_mouse()` in SGR-1006 or
  legacy X10 at pane-relative coordinates, `encode_paste()` gated on DEC 2004,
  `drain_responses()` so the emulator answers device queries with what it
  actually renders, host-owned scrollback, `NO_COLOR`.
- **`pty.rs` (265) needs two adaptations, not one.**
  1. It imports `crate::tui::event::Incoming` — the deleted event enum. Replace
     with a `HostEvent` carrying `PtyOutput`/`PtyExited`.
  2. **It cannot propagate an exit code.** The reader thread emits
     `PtyExited { record_id }` on EOF with no status, and `PtyPane` holds
     `child: Box<dyn Child>` while exposing only `kill()` — never `wait()`.
     §13 promises the child's exit code is propagated exactly as `launch` does
     today, so the host must `wait()` the child (reaping it) and carry the
     status on `PtyExited`. Note also that **EOF is not exit**: a child can close
     its output and linger, so "the child exiting ends the session" (§9.2) must
     key off `wait()`, not off the reader thread's EOF.
- **`lifecycle.rs` is new-but-not-written.** Lift terminal setup/teardown from
  `claude/tui-manager-reference:apps/bitrouter/src/tui/mod.rs`: `enable_raw_mode`
  / alt-screen enter, `EnableMouseCapture`, `EnableBracketedPaste`, the kitty
  keyboard push/pop, the XTWINOPS title save/restore, `restore_terminal()`
  (handle-free so any thread can call it), and `install_panic_restore`. All
  content-agnostic.

### 11.2 Dependencies

Restore to `apps/bitrouter/Cargo.toml`: `ratatui`, `crossterm`,
`alacritty_terminal`, `portable-pty`, `termwiz`, `wezterm-input-types`.

**Do not restore `similar`, `syntect`, or `two-face`** — their sole consumers
were `tui/state/diff.rs` (tool-call diff rendering) and `tui/highlight.rs`
(code-block highlighting), both ACP Monitor-pane rendering, i.e. orchestration.

### 11.3 What is not restored

Everything in `tui/state/` and most of `tui/ui.rs`: the fleet list, decide and
review verbs, ownership routing, permission brokering, splits, ACP Monitor
panes. And **not the old status bar** — it was fed by `SessionUpdateKind::Usage`,
an ACP update, so `pane.usage` / `pane.cost` were only ever populated for ACP
panes and a PTY-hosted native harness rendered an empty left zone. The bar here
is a new feature on a new data source.

## 12. Acceptance gate: the fidelity matrix

*(Superseded. `TUI_FIDELITY_MATRIX.md` was deleted with the mode it gated; what
it specified is summarized below for the record.)*

- **Layer 1, input conformance** (`tui/conformance.rs`): hosts `cat -v` under
  the real PTY so everything the wrapper *sends* is echoed back and asserted.
  This is the half fixture replay cannot reach, and where the silent failures
  live. Validated by mutation.
- **Layer 2, recorded harness output** (`tui/fixtures/harness-*.vt`): real byte
  streams from all eight, replayed with loose sanity assertions. Directory-
  driven, so re-recording needs no code change.
- **Layer 3, manual**: physical mouse drag and an OSC-52 clipboard round-trip.

**The gate was redefined.** "Opt-in until the full matrix runs clean across
several harness releases" was circular — clearing it required standing
machinery that only pays off if someone owns triaging it, and an ignored red
job launders "unverified" into "we have CI for that." `--tui` now clears when
layers 1–2 are green, layer 3 has been run once across the eight, and the flag
has been opt-in for two release cycles with no unresolved rendering reports.

Scheduled live-harness smoke is the only automatic detector of *upstream*
regressions and is deliberately deferred: the first harness-specific rendering
report is the evidence that its recurring cost is justified.

## 13. Failure modes, signals, and platform

### 13.1 Terminal restoration is a three-path problem

A panic hook is **not** sufficient. There are three ways out of raw mode + alt
screen, and only one of them runs Rust unwinding:

| Path | Mechanism |
|---|---|
| Normal exit | the loop's teardown calls `restore_terminal()` |
| Panic | `install_panic_restore` chains `restore_terminal()` ahead of the existing hook and echoes the message to the restored screen |
| **Signal (SIGTERM, SIGHUP)** | **panic hooks do not run.** A signal handler (or a `tokio::signal` branch in the select loop) must call the same handle-free `restore_terminal()` before exiting |

The third row is the one most likely to be skipped and the one users will hit —
a closed terminal window sends SIGHUP, and leaving the shell in raw mode with no
echo is the worst possible failure for a tool whose pitch is "safe wrapper."
`restore_terminal()` is already handle-free precisely so any context can call it.

### 13.2 Job control (`status --watch` only)

Raw mode swallows `^Z`, so SIGTSTP either never fires or suspends the process
with the terminal still in raw mode. `--watch` must either handle
SIGTSTP/SIGCONT explicitly — restore on stop, re-enter and redraw on continue —
or document that `^Z` is inert. **Decide this in PR 3**; silently broken job
control is a bug report waiting to happen.

Hosted mode is unaffected: `^Z` is a byte forwarded to the child, which owns
its own job-control behavior.

### 13.3 Suspending with a live child

`lifecycle::suspend()` (§8.4) hands the real tty to `$EDITOR` or `providers
login`. Two things keep running underneath and must be handled, which is why the
"~40 lines" estimate applies to the terminal choreography only:

- **The PTY reader thread keeps pumping.** In hosted mode its bytes must still
  be fed to the emulator (so the grid is correct on return) while **nothing is
  drawn**. Dropping them corrupts the screen; drawing them fights the child for
  the terminal.
- **The 1 Hz poll must pause**, or the first frame after resume renders a
  snapshot taken while the screen belonged to someone else.

v1 sidesteps the first case entirely: suspend exists only in `status --watch`,
which hosts no child. If v2 ever offers suspend inside `--tui`, this is the work.

### 13.4 Platform

**Unix only in v1.** `--tui` and `status --watch` error on Windows with a clear
message; every other `launch` and `status` path keeps working there.

This is a real constraint, not an oversight: the repo does support Windows
(`spawn.rs` carries `#[cfg(windows)]` installers and `.exe`/`.cmd`/`.bat`
probing). But SIGWINCH, SIGTSTP, `$EDITOR` conventions, `SHELL` (portable-pty
sets it on unix only), and the whole `TERM` contract in §5.1 are unix
semantics; ConPTY is a separate design with its own fidelity matrix. Shipping
"works on Windows" untested would be worse than declining it.

### 13.5 Failure-mode table

| Mode | Handling |
|---|---|
| `--tui` with `--check` | clap-level conflict, explicit error |
| `--tui` without a tty | error naming plain `launch` |
| `status --watch` without a tty | one snapshot, table or `--json`, exit 0 |
| Metering DB absent | `open_readonly` → `None` → `Empty` mode, never an error |
| Daemon unreachable | `HistoryOnly` mode in `--watch`; `daemon unreachable` in the bar |
| Child exits | `wait()` the child, propagate its exit code exactly as `launch` does today (§11.1) |
| Wrapper dies abnormally | dropping the PTY master sends SIGHUP to the child's session — the child dies with the wrapper rather than orphaning. State it, and verify it. |
| Any error inside the emulator | names `launch` without `--tui` as the escape hatch |
| Windows | `--tui` / `--watch` refuse with a clear message (§13.4) |

## 14. Guardrails

Learned from what killed the previous TUI — it accreted mutation surface
(spawn, merge, apply, permission decisions) until it became an orchestrator.

- **Never writes config.** Routing rules, providers, and policy are files the
  user owns.
- **Never handles a secret.** Delegate to `providers login`.
- **No `stop` keystroke.** The control socket exposes it; a keypress that kills
  the daemon behind every running agent is the one action where a mis-press is
  unrecoverable.
- **No per-request retry.** That is an agent action; BitRouter routes, it does
  not re-drive.
- **If there is no CLI subcommand, there is no keystroke.** Accretion now
  requires adding a CLI command first — which gets normal review. This is the
  gate the old TUI lacked: its verbs existed nowhere else.

## 15. PR sequence

| # | PR | Depends on | Status |
|---|---|---|---|
| 1 | `refactor(launch): split prepare/exec` + #796 startup line | — | **done** |
| 2 | `feat(metering): recent_requests + all-caller rate queries` (§7.2), and WAL on the daemon writer (§7.3) | — | **done** |
| 3 | `feat(status): --watch live view` — `Snapshot`, footer, stream, suspend primitive (#797) | 2 | **done** |
| 4 | `feat(metering): per-launch attribution` (#795) | 2 | **done** |
| 5 | `feat(launch): --tui` — restore `term.rs`/`pty.rs`, `host.rs`, the four emulator deps | 1, 3, 4 | **done** |
| 6 | Fidelity matrix: real harness recordings + CI smoke + the manual pass | 5 | open |

**Correction (2026-08-10, found while building).** The draft sequenced the
emulator restore as its own PR *before* the view, with "no user-facing surface"
stated as if it were a virtue. It is not: `term.rs` and `pty.rs` with no
consumer are ~1,200 lines of dead code, which CLAUDE.md guideline 4 forbids
outright. And the live view never needed them — it hosts no child, so it wants
`ratatui` + `crossterm` + `lifecycle.rs` and nothing else.

So the restore **moves into the `--tui` PR**, where its consumer is, and the
four emulator-only dependencies (`alacritty_terminal`, `portable-pty`,
`termwiz`, `wezterm-input-types`) arrive with it. Only `ratatui` and `crossterm`
land early, with the view that uses them.

The same rule applies elsewhere in this spec: `SnapshotSource` lands with its
one implementation, and `launch_id` reached `Prepared` in PR 4 — with #795,
which gave it a consumer (the exit summary) — rather than in PR 1, where it
would have been an unused field.

## 16. Testing

- `Prepared.launch` **and `Prepared.agent_args`** identical across modes (§5).
- Hosted env delta ⊆ `HOSTED_ENV_SET ∪ HOSTED_ENV_MAY_ADD ∪ HOSTED_ENV_UNSET`
  (§5.1).
- `restore_terminal()` runs on the signal path, not only the panic path (§13.1)
  — assert via a child process sent SIGTERM mid-render, checking the tty is out
  of raw mode.
- Child exit code propagates through the hosted path, including a child that
  closes stdout before exiting (§11.1).
- `recent_requests` returns newest-first and honors `limit` (§7.2); rows carry
  `created_at`, `latency_ms`, and `estimated_charge_micro_usd`.
- `get_total_rate` counts every caller, not one `api_key_id` (§7.2).
- `--tui` + `--check` rejected at parse time; `--tui` without a tty rejected.
- Bare `bitrouter status` output unchanged — a golden test, since this is the
  compatibility promise `--watch` is built on.
- `status --watch` non-tty emits one snapshot and exits 0 under both `--json`
  and `--human`.
- Cursor keyed to `request_id` survives a re-sort that drops the cursored row.
- Auto-follow pins off when the cursor leaves the live edge and re-arms on `G`.
- Bar degradation strings for: own-auth harness, routed-but-no-requests,
  daemon-unreachable, sub-threshold latency.
- `Snapshot` modes: live / history-only / empty.
- Fixture replay snapshots per harness (§12).

## 17. Migration and lockstep

`bitrouter tui` was **already removed** by #786 — there is no deprecation
period to run, and #782's migration section is obsolete.

Per CLAUDE.md, the same change must update:

- [`docs/CLI.md`](CLI.md) — `status --watch`, `launch --tui`, the changed
  `launch` docstring.
- [`skills/bitrouter/`](../skills/bitrouter/) — the skill must never describe a
  CLI that does not match `apps/bitrouter`. Keep `SKILL.md` under ~200 lines;
  detail goes in `references/`.
- `.claude-plugin/`, `.codex-plugin/`, `.agents/plugins/marketplace.json` — only
  if the MCP invocation changes (it does not here, but verify).
- `README.md` — only if it still frames the product around a TUI.

## 18. Decisions log

| # | Decision | Rationale / rejected |
|---|---|---|
| 1 | **Flag, not a command**, for both surfaces | The wrapped mode must be a strict superset of `launch`; as a flag over one `prepare` that is true by construction. Same argument gives `status --watch` over a 31st subcommand. |
| 2 | **`status --watch`**, not `bitrouter top` | `status` already exists as a script-parseable probe and stays byte-identical; `--watch` is the established idiom for "the live version of this"; `top` would be accurate for v1 and misleading once it manages. Defers the naming call to when the identity is proven. |
| 3 | **`--tui` kept as the flag name; `launch`'s docstring changes** | `--tui` is conventional as a mode selector and describes the real consequence (who owns the screen). Rejected `--hud` (invented jargon), `--metrics` (Prometheus collision), `--status`/`--observe` (subcommand collision). The docstring's "interactive native-TUI child" was the actual ambiguity. |
| 4 | **Always compiled, no cargo feature** | ~2.5k lines, not 11.5k. A feature that makes a documented flag vanish costs more than the build time it saves. |
| 5 | **Zero BitRouter keybindings in hosted mode (v1)** | Any prefix collides across eight harnesses; none is needed since the child owns `Ctrl-C` and its own exit. |
| 6 | **Stream first; spend, providers, and route-detail in v2** | A view earns a TUI only if it changes while you watch it. The header's rolling total answers the daily spend question; a full breakdown does not need to be live, so it can ship later as a view or a command. |
| 7 | **The TUI never handles a secret and never writes config** | `providers login` already is the credential editor (OAuth flows, credential store); a TUI form would be a second, worse credential path. serde_yaml round-tripping would destroy the user's config comments. |
| 8 | ~~**The emulator is required, not preferred**~~ — **reversed** | Rested on uniformity across the catalog being the requirement. It was never delivered: `launch --tui` shipped for four of the eight harnesses. [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) deletes the emulator rather than paying for it. |
| 9 | **`--tui` stays opt-in** | Scrollback moving to BitRouter is daily friction. Revisit only after the matrix runs clean across several harness releases. |
| 10 | **Suspend primitive ships in v1** wired to `reload`/`$EDITOR` | It is the riskiest part of the management story; prove it on a trivial reversible action, not on an OAuth browser flow. v1 confines it to `--watch`, which hosts no child (§13.3). |
| 11 | **Unix only in v1** | SIGWINCH, SIGTSTP, `$EDITOR`, `SHELL`, and the whole `TERM` contract are unix semantics; ConPTY is a separate design with its own matrix. The repo does support Windows, so this must be an explicit refusal with a clear message, not an untested claim. |
| 12 | **`recent_requests` is a new query, not a widened `MeteringUsageRecord`** | That record is a stable export artifact consumed by benchmark bundles; the stream is a different read with a different shape, and a `LIMIT`ed descending order fixes the 1 Hz scan cost as a side effect. |

## 19. Open questions

1. **v1 management scope.** This spec assumes v1 ships the suspend primitive
   with `r`/`e` (decision 10) while the providers view waits for v2. The
   alternative — a pure read-only v1 — ships sooner and lands the primitive
   with the view that needs it. Flag if that is preferred.
2. **Adequacy join.** Request → trajectory joins on `request_id` (verified: the
   key is on both `MeteringUsageRecord` and `trajectory::StoredRequest`).
   Whether adequacy reliability events join to a single request or only to a
   route fingerprint is **unverified**, and is the first thing to check before
   committing to v2's route-decision detail.
3. **Push vs poll.** 1 Hz polling is v1. A daemon SSE endpoint behind
   `SnapshotSource` would be better and is not scoped here.
4. **Skills via the filesystem rails.** `pi` and `openclaw` may be reachable
   through `npx skills add` / the plugin manifests rather than the
   `bitrouter_skills` MCP gateway — worth an hour before accepting ❌ in §10.2.
   Separate issue, not this one.
5. **`^Z` in `status --watch`** — handle SIGTSTP/SIGCONT with save/restore, or
   document it as inert (§13.2). Decide in PR 4; do not let it ship undecided.
6. **Stale doc-comments** in `metering/mod.rs:46`, `reader.rs:10`, and
   `store.rs:279` cite a `status --agent` flag that does not exist on the
   `Status` subcommand. Not a defect in this design, but PR 2 touches those
   files and should clean them.

## 20. #795 acceptance, as built

| Criterion | Where it is held |
|---|---|
| Two concurrent launches on a default (`skip_auth: true`) install report separate spend | `metering::tests::concurrent_launches_report_separate_spend` — two tagged callers plus one untagged, all three the same synthetic `local` identity |
| Existing daemon-wide queries unchanged | the same test asserts `spend_summary` still totals all three rows |
| No requirement to enable auth | the tag is read only on the `skip_auth` branch; `caller::launch_tag` rejects every non-`brl_` credential |
| Requests outside a launch still attributed sanely | `an_untagged_caller_is_still_recorded_just_not_attributed` |

Verified end-to-end against a running daemon: two `POST /v1/chat/completions`
calls differing only in their bearer recorded identical `api_key_id = local`
with `launch_id` set on one and `NULL` on the other.

**Known limitation, stated rather than hidden.** A launch where the user
exported their own `BITROUTER_API_KEY` (or a harness-native token) is not
attributed — BitRouter does not own the credential slot, and rewriting a real
credential to carry a tag would break `skip_auth: false`. Those launches fall
back to the time-window summary and the exit line changes wording to match.
Closing that gap needs a second channel (a header where the harness supports
one), which reopens the 4-of-8 ceiling and belongs in its own issue.

## 21. Review record

An adversarial review of the first draft (2026-08-10) found, and this revision
fixes: `export_usage` cannot serve the stream (§7.2 — the load-bearing defect,
budgeted as plumbing, actually two new queries); `get_rate` is key-scoped so
there was no daemon-wide rate; `Prepared` dropped `agent_args`, leaving the very
thing the seam guarantees outside the guarantee (§5); `pty.rs` cannot propagate
an exit code and never reaps the child (§11.1); `busy_timeout` already defaults
in sqlx while WAL cannot be set from the `mode=ro` reader (§7.3); `bitrouter
metering-usage` was cited as a user-facing spend table when it is a
`workflow-state` benchmark exporter (§1); signals, job control, and Windows were
absent entirely (§13); `LaunchId` in PR 1 would have violated the no-dead-code
rule (§5, §15).

The terminal half — the restore manifest, the env contract, the harness
capability table — survived unchanged.
