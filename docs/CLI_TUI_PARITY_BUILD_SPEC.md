# Build spec: CLI ↔ TUI parity, track 1 — agent execution plan

Status: **ready to execute** · Date: 2026-09-06
· Design of record: [`CLI_TUI_PARITY_IMPL_SPEC.md`](CLI_TUI_PARITY_IMPL_SPEC.md) (the *impl spec*)
· Rationale of record: [`CLI_TUI_PARITY_SPEC.md`](CLI_TUI_PARITY_SPEC.md) (the *research spec*)

> **What this is.** The impl spec, re-cut as something an autonomous agent can
> run to completion under `/loop`. It adds the three things the impl spec does
> not have: an **iteration protocol** (what one wake-up does), a **resumable
> ledger** (where the work got to), and **every open decision collapsed into an
> instruction** so nothing in the loop requires a human turn.
>
> **What this is not.** A second copy of the design. Every type body, function
> body, guard body and rendering rule stays in the impl spec, cited by section.
> Read that section when a task names it; do not re-derive it.
>
> **Read order for an executing agent:** §0 → the ledger → the one task you are
> on → the impl-spec sections that task names. Nothing else.

---

## The loop invocation

```
/loop Execute docs/CLI_TUI_PARITY_BUILD_SPEC.md. Read §0 for the protocol, read docs/CLI_TUI_PARITY_PROGRESS.md for where the work is, advance exactly one task's sub-steps, then stop.
```

Invoke **without an interval** — the work is compute-bound, not wait-bound, so
pacing is the agent's own. On each iteration that ends with work remaining, the
agent schedules the next wake-up at the **60-second minimum** (`ScheduleWakeup`,
`noop: false`, reason naming the task just advanced). On the iteration that ties
off the last ledger row, or that fires a **STOP** condition in [§5](#5-stop-conditions),
it calls `ScheduleWakeup` with `stop: true` instead.

---

## Contents

- [0. The iteration protocol](#0-the-iteration-protocol)
- [1. Preflight — the tree this builds on](#1-preflight--the-tree-this-builds-on)
- [2. The ledger](#2-the-ledger)
- [3. The tasks](#3-the-tasks)
- [4. Decisions — already made, do not reopen](#4-decisions--already-made-do-not-reopen)
- [5. Stop conditions](#5-stop-conditions)
- [6. Never do](#6-never-do)
- [7. Final acceptance](#7-final-acceptance)

---

## 0. The iteration protocol

One iteration does this, in order, and nothing else.

1. **Orient.** Read `docs/CLI_TUI_PARITY_PROGRESS.md`. It names the current task
   and the last sub-step completed. If the file does not exist, the current task
   is **T0** and the first act is to create it from the template in [§2](#2-the-ledger).
2. **Check for a STOP.** Run the four checks in [§5](#5-stop-conditions). If any
   fires, append the reason to the ledger's **Blocked** section, call
   `ScheduleWakeup({stop: true})`, and report to the user. Do not work.
3. **Advance one task.** Take the current task from [§3](#3-the-tasks) and work
   its sub-steps in order from where the ledger says you are. Read the impl-spec
   sections that task names before writing any code.
4. **Verify.** Run the task's own **verify** line, then the three project checks:

   ```bash
   cargo fmt && cargo clippy --all-features --all-targets && cargo nextest run --all-features
   ```

   `cargo test --all-features` substitutes if `cargo-nextest` is absent.
5. **Land or hold.**
   - **All green and every sub-step of the task done** → commit with the task's
     stated conventional title, tick the task in the ledger, set the next task
     as current.
   - **All green but sub-steps remain** → do **not** commit. Record the last
     sub-step reached in the ledger, leave the working tree as it is. The next
     iteration continues from there.
   - **Not green** → fix it in this iteration if the fix is mechanical
     (a signature, an import, a moved literal). If it is not mechanical, record
     the failure verbatim in the ledger's **Blocked** section and apply the
     thrash rule in [§5](#5-stop-conditions).
6. **Schedule.** `ScheduleWakeup` per [the loop invocation](#the-loop-invocation).

**One task per iteration. Never two.** A task that finishes early ends the
iteration; it does not start the next task. This is what keeps a failure
attributable to one diff.

**Commits.** Conventional format, title under 60 characters, and:

```
Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
```

Commit only at a task boundary, only when green. Never `--amend` a commit from
an earlier iteration; never force-push.

---

## 1. Preflight — the tree this builds on

**The base is not `main`.** `origin/main` has no `crates/bitrouter-mcp/src/actions/`.
The `ACTIONS` table this design extends exists only on an open three-PR stack:

| PR | Branch | Base | State at 2026-09-06 |
|---|---|---|---|
| #869 | `claude/actions-table-phases-1-3` | `main` | open |
| #870 | `claude/actions-table-phase04` | #869 | open |
| #875 | `claude/mcp-drop-complete` | #870 | open |

**Base every commit of this plan on `origin/claude/mcp-drop-complete`** — the
stack tip, `07a2096d` as measured. Work on a new branch
`claude/cli-tui-parity-impl` cut from it. When the stack merges to `main`, the
branch rebases; that rebase is a **STOP** (a human decides when), not something
the loop does.

**Anchors, verified at `07a2096d`.** If any of these does not hold, the tip has
moved and [§5](#5-stop-conditions) STOP-1 fires:

| Check | Expected |
|---|---|
| `git rev-parse --short origin/claude/mcp-drop-complete` | `07a2096d` |
| `ACTIONS` rows in `crates/bitrouter-mcp/src/actions/mod.rs` | 5: `status`, `list_models`, `route`, `skills_search`, `skills_get` |
| `crates/bitrouter-tui/src/machine.rs:387` | `if line.trim() == "/commands"` |
| `crates/bitrouter-tui/src/machine.rs:394` | `if line.trim() == "/route"` |
| `crates/bitrouter-tui/src/machine.rs:115` | `pub fn new(routable: bool)` |
| `crates/bitrouter-tui/src/machine.rs:72` | `const NOT_ROUTABLE` |
| `apps/bitrouter/src/main.rs:5893` | `fn every_mcp_tool_has_an_actions_row` |
| `apps/bitrouter/src/main.rs:5912` | `fn every_actions_row_resolves_to_a_cli_leaf` |
| `apps/bitrouter/src/main.rs:5949` | `fn every_actions_row_matches_its_tools_output_schema` |
| `crates/bitrouter-mcp/src/server.rs:1258` | `fn http_profile_never_carries_host_bound_tools` |

Two facts that make T1 safe and that the plan depends on:
`every_actions_row_resolves_to_a_cli_leaf` `continue`s on `cli_leaf: None`, and
`every_actions_row_matches_its_tools_output_schema` `continue`s on
`mcp_tool: None`. So the three new set-C rows added in T1 pass both unchanged.

The full anchor inventory — every `file:line` an implementer touches — is impl
spec [§1.2](CLI_TUI_PARITY_IMPL_SPEC.md#12-anchors-an-implementer-touches).
[Appendix A](CLI_TUI_PARITY_IMPL_SPEC.md#appendix-a--research-spec-references-that-moved)
lists the ~25 research-spec references that have since moved; trust §1.2 over
the research spec on every number.

---

## 2. The ledger

`docs/CLI_TUI_PARITY_PROGRESS.md` is the loop's only mutable state. **T0 creates
it from this template.** It is committed, so `git log` and the ledger agree.

```markdown
# CLI/TUI parity — build progress

Base: `origin/claude/mcp-drop-complete` @ 07a2096d
Branch: `claude/cli-tui-parity-impl`
Spec: docs/CLI_TUI_PARITY_BUILD_SPEC.md

## Current

Task: T0
Sub-step: none
Consecutive non-green iterations on this task: 0

## Ledger

- [ ] T0 — preflight and branch
- [ ] T1 — ActionSpec columns, enums, eight rows, G2, G3
- [ ] T2 — G6, the HTTP profile guard read from the table
- [ ] T3 — the resolver, /commands, /help, /route reset
- [ ] T4 — SessionPorts, /status, G4, G5, A1
- [ ] T5 — /models and /preview
- [ ] T6 — CommandsReport and its builder
- [ ] T7 — bitrouter acp commands, /commands through the shared report
- [ ] T8 — the prompt-expansion registry
- [ ] T9 — acceptance sweep

## Notes

(one line per iteration: task, sub-step reached, commit sha if any)

## Blocked

(empty)
```

Rules: tick a row only after its commit lands green. Never re-order rows. Never
tick ahead. The **Notes** section is append-only.

---

## 3. The tasks

Every task states its **read** (the impl-spec sections carrying the code), its
**files**, its **sub-steps** in order, its **verify** line, and its **commit**
title. Where a task's design is fully written in the impl spec, this document
does not repeat it — read the section.

Tasks T1–T8 correspond to impl spec [§7](CLI_TUI_PARITY_IMPL_SPEC.md#7-phases)'s
five phases: T1–T3 are phase 0, T4 is phase 1, T5 is phase 2, T6–T7 are phase 3,
T8 is phase 4.

---

### T0 — preflight and branch

**Read:** [§1](#1-preflight--the-tree-this-builds-on) above.

**Sub-steps**

1. `git fetch origin`. Verify every anchor in [§1](#1-preflight--the-tree-this-builds-on).
   Any mismatch → STOP-1.
2. `git switch -c claude/cli-tui-parity-impl origin/claude/mcp-drop-complete`.
   If the branch already exists, switch to it and confirm its merge-base is the
   stack tip.
3. Confirm the baseline is green: run the three project checks on a clean tree.
   Red at baseline → STOP-2 (the base is broken; not this plan's to fix).
4. Write `docs/CLI_TUI_PARITY_PROGRESS.md` from [§2](#2-the-ledger)'s template.
5. Copy `docs/CLI_TUI_PARITY_BUILD_SPEC.md`, `docs/CLI_TUI_PARITY_IMPL_SPEC.md`
   and `docs/CLI_TUI_PARITY_SPEC.md` onto this branch if they are not already
   present (they live on `claude/cli-tui-parity-spec-docs`, cut from `main`).
   Add the three to `docs/README.md`'s index if absent.

**Verify:** the three project checks are green and the ledger exists.

**Commit:** `docs(spec): add the CLI/TUI parity build plan and ledger`

---

### T1 — `ActionSpec` columns, enums, eight rows, G2, G3

**Read:** impl spec [§3.1](CLI_TUI_PARITY_IMPL_SPEC.md#31-actionspec--cratesbitrouter-mcpsrcactionsmodrs)
(the full type bodies and the eight-row table), [§6 G2](CLI_TUI_PARITY_IMPL_SPEC.md#g2--every-row-with-a-tui_command-is-fully-inventoried-p2),
[§6 G3](CLI_TUI_PARITY_IMPL_SPEC.md#g3--every-write-names-an-inverse-row-that-names-it-back-r5).

**Files:** `crates/bitrouter-mcp/src/actions/mod.rs` only. This task is
crate-local and compiles alone.

**Sub-steps**

1. Add `pub enum Effect { Read, Write { inverse: &'static str } }`,
   `pub enum Requires { Nothing, Binding }`, `pub enum Reach { Portable, HostBound, SessionBound }`
   with the doc comments from §3.1. All three derive
   `Debug, Clone, Copy, PartialEq, Eq`. `Effect::Write.inverse` is **not**
   `Option` — a write with no inverse is not admitted.
2. Add `tui_command: Option<&'static str>`, `effect: Effect`, `requires: Requires`,
   `reach: Reach` to `ActionSpec`, with §3.1's doc comments.
3. Fill the five existing rows per §3.1's table. Every `tui_command` is `None`
   at the end of this task — `status` gains `Some("status")` in T4,
   `list_models` and `route` in T5.
4. Add the three new rows: `commands` (`cli_leaf: None`, `mcp_tool: None`,
   `tui_command: Some("commands")`, `Read`, `Nothing`, `SessionBound`,
   `output_schema: None`), `route_set` (`tui_command: Some("route")`,
   `Write { inverse: "route_reset" }`, `Binding`, `SessionBound`, no schema),
   `route_reset` (`tui_command: Some("route reset")`,
   `Write { inverse: "route_set" }`, `Binding`, `SessionBound`, no schema).
5. Rewrite the module doc's closing sentence — *"the backlog is empty today, so
   every row below carries a real schema"* is now false. Say which rows carry
   no schema and why: `route_set` and `route_reset` have one surface each, so
   there is no second shape to hold them to; `commands` gains one in T7.
6. Add **G2** and **G3** as `#[cfg(test)]` tests in this file. Each must name
   the offending row in its failure message — see §6 for the exact assertions
   and the "breaking it" cases.
7. Provoke each new guard once by hand (give `skills_get` a `tui_command`; make
   `route_reset` a `Read`), confirm the message names the row, then revert.

**Verify:** `cargo nextest run -p bitrouter-mcp --all-features`, then the three
project checks. `every_mcp_tool_has_an_actions_row`,
`every_actions_row_resolves_to_a_cli_leaf` and
`every_actions_row_matches_its_tools_output_schema` must pass **unchanged in
text** — if one needed editing, the row set is wrong.

**Commit:** `feat(actions): add tui_command, effect, requires, reach`

---

### T2 — G6, the HTTP profile guard read from the table

**Read:** impl spec [§6 G6](CLI_TUI_PARITY_IMPL_SPEC.md#g6--the-http-profile-carries-only-portable-rows).

**Files:** `crates/bitrouter-mcp/src/server.rs`.

**Sub-steps**

1. Inside `http_profile_never_carries_host_bound_tools` (`:1258`), add the
   assertion: every name in `tool_names(&http_profile(Arc::new(StubBackend)))`
   is the `mcp_tool` of a row whose `reach` is `Reach::Portable`.
2. **Keep both existing literal assertions.** They are the belt; G6 is the
   braces. Weakening either is an invariant violation ([§6](#6-never-do)).
3. Provoke: flip `list_models`'s `reach` to `HostBound`, confirm the failure
   names the row, revert.

**Verify:** `cargo nextest run -p bitrouter-mcp --all-features`; then
`git diff origin/claude/mcp-drop-complete..HEAD --stat -- crates/bitrouter-mcp/tests/multitenant_http.rs`
prints nothing.

**Commit:** `test(mcp): derive the HTTP profile guard from Reach`

---

### T3 — the resolver, `/commands`, `/help`, `/route reset`

**The largest task. Expect two to four iterations.** It crosses the
`bitrouter-tui` ↔ `apps/bitrouter` boundary, and `State::new`'s signature change
means the workspace is red between sub-step 2 and sub-step 7. That is expected;
**do not commit until sub-step 10 is green.**

**Read:** impl spec [§3.2](CLI_TUI_PARITY_IMPL_SPEC.md#32-the-reducers-data--cratesbitrouter-tuisrcmachiners)
(`Command`, `REDUCER_OWNED`, `ALIASES`, `State`, `Effect`, `Action`),
[§4](CLI_TUI_PARITY_IMPL_SPEC.md#4-the-dispatcher) (`Resolution`, `resolve`,
`submit`, the `Wire::apply` arm, the three-caller table, the reducer test
table), [§3.3](CLI_TUI_PARITY_IMPL_SPEC.md#33-sessionports--appsbitroutersrcactionssessionrs-new-phase-1)'s
`offered_commands` block only (skip `SessionPorts` — that is T4),
[§6 G1](CLI_TUI_PARITY_IMPL_SPEC.md#g1--every-tui-command-has-a-row),
[§7 phase 0](CLI_TUI_PARITY_IMPL_SPEC.md#phase-0--the-tables-new-columns-the-resolver-commands-grouped-help-route-reset)'s
file table.

**Sub-steps**

1. `machine.rs`: add `Command`, `PromptCommand` is **not** added yet (T8 owns
   it — it has no producer before then), `REDUCER_OWNED`, `ALIASES`,
   `Resolution` **without** the `Action` and `Expand` variants (T4 and T8 add
   them with their first producers), and `resolve(commands, line)`.
2. `machine.rs`: `State.commands: Vec<Command>` replaces `routable: bool`;
   `State::new(Vec<Command>)`; `State::available(&self, action)`. Delete
   `NOT_ROUTABLE` (`:72`) — it moves app-side in sub-step 6.
3. `machine.rs`: rewrite `submit` as the `match` over `Resolution` from §4,
   minus the `Action` and `Expand` arms. Delete the two string compares at
   `:387` and `:394`.
4. `machine.rs`: add `Effect::ResetRoute`; widen
   `Action::Routed(Result<Option<String>, String>)`; `routed()` reports
   `Ok(None)` as *"route reset to the daemon's default"* and emits
   `RouteInForce(None)`.
5. `machine.rs`: update every `State::new(true)` / `State::new(false)` in the
   existing tests via a test-module helper `fn routable(bool) -> Vec<Command>`,
   one line each. Add the resolver tests from §4's table (the rows that do not
   depend on T4 or T8).
6. `apps/bitrouter/src/actions/session.rs` (new) + `pub mod session;` in
   `apps/bitrouter/src/actions/mod.rs`: `offered_commands(&AcpClient)`,
   `summary_for(id) -> &'static str`, `NOT_ROUTABLE`, `NOT_RESETTABLE`. Delete
   `can_reroute` from `chat/session.rs:88` — `offered_commands` subsumes it.
7. `crates/bitrouter-tui/src/render/session.rs`: `commands(bitrouter: &[Command], agent: &[AvailableCommand])`
   — a "BitRouter" group first carrying each row's `summary` and any
   `unavailable` reason, then the agent's group with same-named rows marked
   shadowed. This renderer is deleted in T7; it exists so phase 0 ships
   something visible.
8. `crates/bitrouter-tui/src/picker.rs`: `Picker::open`'s first parameter comes
   from `state.available("route_set")`.
9. `apps/bitrouter/src/chat/effects.rs`: the `Effect::ResetRoute` arm from §4;
   the `SetRoute` arm returns `Ok(Some(in_force))`.
   `apps/bitrouter/src/chat/session.rs`: `run` and `chat_plain` take
   `commands: Vec<Command>`; the `Notice::Commands` arm passes `&state.commands`;
   `chat_plain` runs `resolve` (`Owned` → *"/<name> needs a terminal"*,
   `Unavailable` → the reason, `Prompt` → the turn).
   `apps/bitrouter/src/acp_cli.rs`: build `offered_commands(&session.client)`
   at `:1497`, pass it to `run` (`:1501`) and `chat_plain` (`:1771`); keep the
   *"type /route to change the route"* hint, now printed when the `route_set`
   command is available.
10. `apps/bitrouter/src/main.rs`: **G1**, beside `every_mcp_tool_has_an_actions_row`.
    Four assertions per §6. Provoke each of the four once.
11. Docs lockstep, same commit (CLAUDE.md): `docs/CLI.md:535`–`:537` gains
    `/help` and `/route reset`, and says BitRouter's commands are listed above
    the agent's with same-named agent commands shown as shadowed;
    `skills/bitrouter/references/cli.md:207` the same.

**Verify:** the three project checks; G1, G2, G3, G6 pass; the picker's existing
tests pass unchanged in meaning. By hand, in a routed `chat`: `/commands` shows
`route`, `route reset`, `commands` under a BitRouter heading with the agent's
list beneath; `/help` does the same; `/route reset` after `/route` clears the
footer's route and says so. Under `--direct`: `/commands` lists `route` with its
reason and `/route` answers with that reason rather than a bare refusal.

**Commit:** `feat(chat): resolve slash commands against the actions table`

---

### T4 — `SessionPorts`, `/status`, G4, G5, A1

**Read:** impl spec [§3.3](CLI_TUI_PARITY_IMPL_SPEC.md#33-sessionports--appsbitroutersrcactionssessionrs-new-phase-1)
(`SessionPorts`, `open`, `run`, the construction site),
[§5](CLI_TUI_PARITY_IMPL_SPEC.md#5-rendering) (the driver arm, `plain_lines`,
and the four rendering rules), [§6 G4/G5/A1](CLI_TUI_PARITY_IMPL_SPEC.md#g4--the-driver-names-no-report-type-the-83-amendment-holds),
[§7 phase 1](CLI_TUI_PARITY_IMPL_SPEC.md#phase-1--sessionports-proved-on-status).

**Sub-steps**

1. `crates/bitrouter-tui/src/machine.rs`: add `Resolution::Action` and
   `Effect::Action { action, args: Vec<String> }`, and `submit`'s `Action` arm.
   `args` is whitespace-split into a `Vec<String>` — **never one string**.
   Add the `/status extra words` resolver test.
2. `apps/bitrouter/src/actions/session.rs`: `SessionPorts { status, models, route }`,
   `open(source, socket)`, and `run` with the **`"status"` arm only** — the
   other two arms arrive in T5 with their rows. `summary_for("status")`.
3. `apps/bitrouter/src/acp_cli.rs`: construct `SessionPorts::open(source.clone(), crate::daemon::socket_path_for(source, &config))`
   beside the binding at `:1295`; pass `&SessionPorts` to `run` and `chat_plain`.
4. `apps/bitrouter/src/chat/session.rs`: the `ports: &SessionPorts` parameter on
   both loops; the `Effect::Action` arm and `plain_lines` from §5;
   `chat_plain`'s `Action` arm writes `render_to_vec` bytes to stdout.
5. `apps/bitrouter/src/chat/mod.rs`: **G4 + G5** — six strings appended to
   `the_chat_module_reaches_nothing_daemon_wide`'s `forbidden` list:
   `"StatusReport"`, `"ModelsReport"`, `"RouteReport"`, `"SkillsReport"`,
   `"CommandsReport"`, `"for_stdout"`. **Append only.** The nine existing
   strings and the three scanned files stay.
6. `crates/bitrouter-mcp/src/actions/mod.rs`: `status` row gains
   `tui_command: Some("status")`.
7. **A1** in `actions/session.rs`'s test module, `status` only: the JSON of
   `SessionPorts::open(source, socket).run("status", &[])` equals the JSON of
   `DaemonStatus::new(socket, Some(source)).report()`. Use the same temp-dir
   `bitrouter.yaml` fixture `actions/route.rs`'s tests use and a socket nothing
   listens on.
8. A driver test with a stub `SessionPorts` asserting the notice's bytes equal
   `Output::new(Format::Human).render_to_vec(&report)`. **The stub is the one
   place a second `impl StatusQuery` is legitimate.**
9. Docs lockstep: a `/status` row in `docs/CLI.md` and
   `skills/bitrouter/references/cli.md`, plus a sentence that its output is
   `bitrouter status --human`'s.

**Verify:** the three project checks; A1, G4, G5 pass. By hand: `/status` with
the daemon up shows the running block; with the daemon down shows
`running: false` as a notice, not an error; `echo /status | bitrouter chat <agent>`
prints the same block to stdout.

**This task enacts D1(a)** — see [§4](#4-decisions--already-made-do-not-reopen).
G4 is the enforcement. Do not reopen it.

**Commit:** `feat(chat): render /status through the shared action ports`

---

### T5 — `/models` and `/preview`

**Read:** impl spec [§3.3](CLI_TUI_PARITY_IMPL_SPEC.md#33-sessionports--appsbitroutersrcactionssessionrs-new-phase-1)'s
`run` arms, [§5](CLI_TUI_PARITY_IMPL_SPEC.md#5-rendering)'s width rule,
[§7 phase 2](CLI_TUI_PARITY_IMPL_SPEC.md#phase-2--models-provider-and-preview-model).

**Sub-steps**

1. `crates/bitrouter-mcp/src/actions/mod.rs`: `list_models` →
   `tui_command: Some("models")`; `route` → `tui_command: Some("preview")`.
   The second name is **D8(b)**, decided: `/route` keeps meaning the picker.
2. `apps/bitrouter/src/actions/session.rs`: `run`'s `"list_models"` and
   `"route"` arms; two `summary_for` arms; **A1 extended** to both.
   `/preview` with no argument returns the usage line as a `ToolError` — never
   a panic.
3. Docs lockstep: two rows, plus the sentence distinguishing `/route` from
   `/preview`.
4. **The width check, by hand at 60 columns.** `models` on the default catalog
   is the one report at risk (`status` is a block, `route` a chain). If it does
   not wrap acceptably, the pre-decided fallback applies: **drop `list_models`'s
   `tui_command`, record the reason in the ledger's Notes, and ship `/preview`
   alone.** That is a decided outcome, not a STOP — do not add a renderer.

**Verify:** the three project checks; A1 covers all three reads. By hand:
`/models anthropic` renders what `bitrouter models --provider anthropic --human`
renders; `/preview gpt-5` renders what `bitrouter route gpt-5 --human` renders,
palette off.

**Commit:** `feat(chat): add /models and /preview to the session`

---

### T6 — `CommandsReport` and its builder

**Read:** impl spec [§3.4](CLI_TUI_PARITY_IMPL_SPEC.md#34-the-commands-report--cratesbitrouter-mcpsrcactionscommandsrs-new-phase-3).

**Files:** `crates/bitrouter-mcp/src/actions/commands.rs` (new),
`crates/bitrouter-mcp/src/actions/mod.rs` (`pub mod commands;`),
`apps/bitrouter/src/actions/commands.rs` (new),
`apps/bitrouter/src/output/reports/commands.rs` (new).

**Sub-steps**

1. `CommandSource { Bitrouter, Config, Agent }`, `CommandRow`, `CommandsReport`
   per §3.4. `CommandRow.name` is **never rewritten** — no sigil, no prefix
   (D14, decided).
2. `commands_report(bitrouter, config, agent, received) -> CommandsReport`
   app-side. It takes ACP's own `AvailableCommand` (which carries `input`), not
   the SDK's `AgentCommand`, so `hint` is available without touching
   `translate.rs`.
3. `impl CliReport for CommandsReport` — three headed groups in source order,
   shadowed rows marked, and `received: false` rendered as a **distinct line**
   from an empty agent list.
4. Unit tests on `commands_report`: shadowing, source order, and the `received`
   flag. `config` is passed empty until T8.

**Verify:** `cargo nextest run -p bitrouter-mcp -p bitrouter --all-features`;
the three project checks. The `commands` row still has `output_schema: None`
at the end of this task — T7 sets it.

**Commit:** `feat(actions): add the shared commands report`

---

### T7 — `bitrouter acp commands`, `/commands` through the shared report

**Read:** impl spec [§7 phase 3](CLI_TUI_PARITY_IMPL_SPEC.md#phase-3--bitrouter-acp-commands-and-the-shared-commands-report)'s
file table, [§5](CLI_TUI_PARITY_IMPL_SPEC.md#5-rendering)'s closing paragraph.

**Sub-steps**

1. `crates/bitrouter-mcp/src/actions/mod.rs`: `commands` row →
   `cli_leaf: Some("acp commands")`, `output_schema: Some(..)`.
2. `apps/bitrouter/src/main.rs`: `AcpCmd::Commands { agent, routing: RoutingOptions, wait_ms: u64 /* default 2000 */, source: Option<CommandSource> }`.
   `every_actions_row_resolves_to_a_cli_leaf` then walks `acp commands` with no
   change to the guard.
3. `apps/bitrouter/src/acp_cli.rs`: the runner — `acp prompt`'s launch preamble,
   `initialize`, `session/new`, collect `AvailableCommandsUpdate` from
   `client.subscribe_raw_updates()` for `wait_ms` (last update wins;
   `received = any arrived`), build
   `commands_report(offered_commands(&client), &[], &agent_list, received)`,
   emit, tear down. **No prompt is sent.**
4. `crates/bitrouter-sdk/src/acp/translate.rs:59`: `AgentCommand` gains
   `hint: Option<String>` from `input` — for the `--format json` consumer, not
   for the report.
5. `crates/bitrouter-tui/src/journal.rs:179`: `commands_received: bool`, set on
   the first `AvailableCommandsUpdate`, exposed beside `commands()`.
6. `apps/bitrouter/src/chat/session.rs`: the `Notice::Commands` arm builds
   `commands_report(&state.commands, &[], journal.commands(), journal.commands_received())`
   and renders it through `render_to_vec`.
7. `crates/bitrouter-tui/src/render/session.rs`: **delete** `commands(..)`. It
   was T3's scaffolding and now has no caller.
8. Docs lockstep: the `acp commands` leaf; `/commands` now documents three
   outcomes.

**Verify:** the three project checks. `bitrouter acp commands --agent <id> --json`
prints a `CommandsReport`; against a harness that never sends the update it
prints `received: false` and an empty agent group after `wait_ms`; `--source agent`
filters. `every_actions_row_matches_its_tools_output_schema` still passes — the
row has no tool so it is skipped, and the test still reports a non-empty checked
set.

**Commit:** `feat(acp): add the headless commands leaf`

---

### T8 — the prompt-expansion registry

**Read:** impl spec [§3.5](CLI_TUI_PARITY_IMPL_SPEC.md#35-prompt-expansion-config--cratesbitrouter-sdksrcconfigmodrs-phase-4),
[§4](CLI_TUI_PARITY_IMPL_SPEC.md#4-the-dispatcher)'s `Expand` arm and the
three-caller table, [§2.1](CLI_TUI_PARITY_IMPL_SPEC.md#21-d2-decided-two-registries)
(why the two registries are never held in one collection),
[§7 phase 4](CLI_TUI_PARITY_IMPL_SPEC.md#phase-4--the-prompt-expansion-registry-d2).

**Sub-steps**

1. `crates/bitrouter-sdk/src/config/mod.rs:46`: `pub chat: ChatConfig`;
   `ChatConfig { commands: Vec<PromptCommandConfig> }`;
   `PromptCommandConfig { name, description, prompt }`. **There is deliberately
   no key that runs anything** — no `run:`, no `command:`, no `shell:`. A
   deserialization test in `config/tests.rs`.
2. `crates/bitrouter-tui/src/machine.rs`: `PromptCommand`;
   `State.prompt_commands: Vec<PromptCommand>`; `resolve` gains its
   `prompt_commands` parameter and `Resolution::Expand`; `submit` treats
   `Expand` exactly as `Prompt`. Resolver tests for substitution and for
   precedence (`/status` with a `status` prompt command resolves to the
   BitRouter row, never to `Expand`).
3. `apps/bitrouter/src/actions/session.rs`:
   `prompt_commands(&ChatConfig) -> anyhow::Result<Vec<PromptCommand>>` — unique
   names, and **none equal to any `tui_command` or alias**. The error names the
   offender and the BitRouter command it collides with. This load-time check is
   what keeps G1–G3 exhaustive over `ACTIONS`; it cannot live in the SDK,
   which cannot see `ACTIONS`.
4. `apps/bitrouter/src/acp_cli.rs`: the `chat` launch passes
   `prompt_commands(&config.chat)?` to `State`; `acp prompt` runs its text
   through `resolve(&[], &prompt_commands, text)` and an `Expand` replaces it
   before `session/prompt` — **`&[]` for the BitRouter commands is deliberate**
   (`acp prompt "/status"` is not a scheduled surface); `acp commands` passes
   the list as the `config` group.
5. `apps/bitrouter/src/main.rs`: `config validate` also runs `prompt_commands`,
   so a collision is reported without launching a session.
6. Docs lockstep: the `chat.commands` block, `$ARGUMENTS`, the no-`run:`
   statement, the collision rule.

**Verify:** the three project checks. With
`chat.commands: [{name: review, prompt: "Review: $ARGUMENTS"}]`, typing
`/review the diff` in `chat` and running `acp prompt --agent x "/review the diff"`
both send `Review: the diff` — assert the expanded text in the `session/prompt`
request, extending the existing NDJSON tests. `/commands` shows a "config"
group. A config naming `status` fails both `bitrouter chat` and
`bitrouter config validate` with a message naming both names.

**Commit:** `feat(config): add prompt-expansion commands to chat`

---

### T9 — acceptance sweep

**Read:** [§7](#7-final-acceptance) below; impl spec
[§10](CLI_TUI_PARITY_IMPL_SPEC.md#10-acceptance).

No code. Run [§7](#7-final-acceptance) in full, record each line's result in the
ledger's Notes, then `ScheduleWakeup({stop: true})` and report to the user with
the commit range and anything §7 found. If a §7 line fails, record it and stop —
do not open a new task to fix it without the user.

**Commit:** `docs(spec): record the CLI/TUI parity acceptance sweep`

---

## 4. Decisions — already made, do not reopen

The impl spec's [§8](CLI_TUI_PARITY_IMPL_SPEC.md#8-open-decisions--what-each-blocks)
lists fifteen decisions with defaults. **In this plan every one is settled.** The
table below is the instruction; the impl spec's column *"what changes if decided
otherwise"* is context for a human, not a branch for the loop. An agent that
finds itself weighing an alternative has left the plan — that is STOP-4.

| | Decision | Build this |
|---|---|---|
| **D1** | `ACP_TUI_SPEC.md` §8.3 restricts the TUI to session-scoped data | **Amend narrowly.** `/status` renders daemon-wide data once, on request, never retained, never polled, never in the footer. G4 enforces it: the driver cannot name a report type. Enacted in T4 |
| **D2** | user-configured commands | **Prompt-expansion only.** T8. No `run:` key, ever |
| **D3** | a `reload` command | **Excluded.** No row |
| **D4** | session-scoped `trajectory` | **Out of scope.** Its own issue |
| **D5** | settle the scope empirically | **Proceed.** T4–T5 build exactly `status`, `models`, `route`. A further non-hostile member found later is one row + one `summary_for` arm + one `run` arm |
| **D6** | `skills list` in-session | **Excluded.** No `tui_command` on `skills_search` |
| **D7** | a CLI leaf for `route/set` | **Blocked — B-D7.** `route_set` and `route_reset` ship as set-C rows (`cli_leaf: None`, `reach: SessionBound`). The tree lacks session discovery, cross-process lease identity, and a daemon→controller lease-change notification; without the third, a route set elsewhere leaves the footer stale, violating invariant 5. Needs #863. The stated need is met at launch today by `--model provider:model`. **Do not build the leaf** |
| **D8** | `/route` overloading | **(b)** `/preview <model>` reads; `/route` keeps meaning the picker. T5 |
| **D9** | argument grammar | **Whitespace split into `Vec<String>`.** Never one string. No arity field on `Command` until a command needs two positionals |
| **D10** | a first-party GUI | **None** |
| **D11** | `route/set` as an ACP config option | **(a)** keep `_bitrouter/route/*` |
| **D12** | take the inversion | **Yes.** `SessionPorts` + `resolve` are it. No new `SessionActions` trait — the four port traits already exist |
| **D13** | remote over HTTP | **`Reach` with G6 as its reader; no widening.** Only `Portable` rows may reach the HTTP profile |
| **D14** | distinguishing command sources | **A `source` field, rendered as a group.** Never a sigil, never a rewritten name. Shadowed rows are listed, not dropped |
| **D15** | `/list` vs `/commands` | **Keep `/commands`; the leaf is `acp commands`** |

---

## 5. Stop conditions

Check these at the start of every iteration ([§0](#0-the-iteration-protocol)
step 2). Any one firing ends the loop: append the reason to the ledger's
**Blocked** section, call `ScheduleWakeup({stop: true})`, and report to the user.

- **STOP-1 — the base moved.** `origin/claude/mcp-drop-complete` is no longer
  `07a2096d`, or any [§1](#1-preflight--the-tree-this-builds-on) anchor fails,
  or #869/#870/#875 merged to `main`. A rebase is a human's call.
- **STOP-2 — the baseline is red.** The three project checks fail on a tree this
  plan has not modified. Not this plan's to fix.
- **STOP-3 — thrash.** Three consecutive iterations on the same task end
  non-green. Record the last failure verbatim. Something in the design does not
  meet the tree, and guessing makes it worse.
- **STOP-4 — a decision is in play.** Any moment the right next edit depends on
  an alternative in [§4](#4-decisions--already-made-do-not-reopen), or a guard
  can only be made to pass by weakening it, or the plan calls for something the
  tree cannot express. Name the decision and stop.

Two things that are explicitly **not** stops, because their outcome is already
decided: `models` failing the 60-column check (T5 sub-step 4 says what to do),
and `route_set`/`route_reset` having no CLI leaf (B-D7 is the decision).

---

## 6. Never do

Impl spec [§9](CLI_TUI_PARITY_IMPL_SPEC.md#9-invariants)'s thirteen invariants,
stated as prohibitions. Each is checkable on a single diff.

1. **Never add a `bitrouter-*` dependency to `crates/bitrouter-tui/Cargo.toml`.**
   Everything the reducer knows arrives as `Vec<Command>` / `Vec<PromptCommand>`.
2. **Never weaken `the_chat_module_reaches_nothing_daemon_wide`.** Its nine
   strings stay; T4 appends six. A new file under `chat/` joins the scanned list
   in the same commit that creates it.
3. **Never touch `crates/bitrouter-mcp/tests/multitenant_http.rs`.** It must be
   byte-identical to the base at every commit.
4. **Never remove `CallerAuth` from `StatusQuery`/`ModelsQuery`,** and never add
   a caller-less method. `SessionPorts::run` passes `CallerAuth::default()`.
5. **Never widen the HTTP profile.** It is built from an `Arc<dyn Backend>`
   alone; `wiring_skills_into_stdio_does_not_widen_the_http_profile` and the two
   literal assertions in `http_profile_never_carries_host_bound_tools` stay.
6. **Never emit more than one JSON value on stdout from a CLI leaf.**
   `acp commands`'s `wait_ms` settle writes nothing.
7. **Never `#[allow(..)]`, `.unwrap()`, `.expect()` or `panic!` in a shipped
   path** (CLAUDE.md 1, 3). **Never add a type, field or variant with no reader**
   (CLAUDE.md 4) — this is why `Resolution::Action` waits for T4 and
   `PromptCommand` for T8, and why `Requires::Daemon`, `Reach::Degraded` and
   `Option` on `Effect::Write::inverse` were cut.
8. **Never render a control as dead.** A command whose `requires` is unmet is
   listed with its reason and answers with that reason.
9. **Never add a confirmation modal.** `permission.rs` is untouched. R5 —
   every write names an inverse — is the safety.
10. **Never report an asked-for route as in force.** The footer follows
    `RouteInForce` and nothing else.
11. **Never retain a report.** `view.notice_lines` replaces; `submit` emits
    `Effect::ClearNotice` before every command.
12. **Never put `SkillsQuery` on `SessionPorts`** (D6 excluded).
13. **Never ship an interactive-command change without `docs/CLI.md` and
    `skills/bitrouter/references/cli.md` in the same commit** (CLAUDE.md
    lockstep). Each task's sub-steps name the lines.

Two more, specific to this plan:

14. **Never use `Theme::for_stdout` under `chat/`.** `render_to_vec` hard-codes
    `Theme::none()`; a themed render writes raw ANSI into the differential
    writer's screen. G5 is the enforcement.
15. **Never commit red, and never commit mid-task.** [§0](#0-the-iteration-protocol)
    step 5.

---

## 7. Final acceptance

T9 runs this in full and records each line.

- `cargo fmt -- --check`, `cargo clippy --all-features`,
  `cargo nextest run --all-features` clean — and clean at **every** commit in
  the range, checked with `git rebase --exec` or by walking the shas.
- G1, G2, G3, G6 exist and pass from T3's commit; G4, G5, A1 from T4's. Each was
  provoked once and the failure named the row or the file.
- `every_mcp_tool_has_an_actions_row`, `every_actions_row_resolves_to_a_cli_leaf`
  and `every_actions_row_matches_its_tools_output_schema` pass **unchanged in
  text** across the whole range.
- `git diff origin/claude/mcp-drop-complete..HEAD --stat -- crates/bitrouter-mcp/tests/multitenant_http.rs`
  prints nothing.
- `git diff origin/claude/mcp-drop-complete..HEAD -- crates/bitrouter-tui/Cargo.toml`
  shows no `bitrouter-*` dependency added.
- For every row with both a `tui_command` and a `cli_leaf`: the TUI notice's
  bytes equal `Output::new(Format::Human).render_to_vec(&report)` for the same
  report. By construction for the three reads; by unit test for `commands`.
- By hand, once per task that added a command: a routed `chat`, a `--direct`
  session, and `echo '/<cmd>' | bitrouter chat <agent>`, each at 60 columns.
  `stty -a` reports a restored terminal after each exit.
- `bitrouter acp commands --agent <id>` against three harnesses — one that
  advertises commands, one that advertises an empty list, one that never sends
  the update — produces three distinguishable reports.
- The ledger's every row is ticked and its Blocked section is empty, or its
  Blocked section names exactly the outcomes [§5](#5-stop-conditions) permits
  (B-D7; and the 60-column fallback if it fired).

**Out of scope, and staying that way:** track 2 (the ~25 commands hostile in a
session), D7(b)'s CLI leaf, carrying `HostBound` reads over HTTP, a headless
entry to the action ports, the `//` escape for a shadowed agent command, D11(b),
#863's daemon-owns-config question, session-scoped `trajectory`, a general write
framework, and everything #786 removed. Impl spec
[§11](CLI_TUI_PARITY_IMPL_SPEC.md#11-out-of-scope) has the full list.
