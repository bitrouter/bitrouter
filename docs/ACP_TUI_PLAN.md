# Execution plan: the ACP TUI (v1)

Companion to [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md). The spec holds the
*decisions and rationale*; this file holds the *ordered work and its
completion criteria*. Scope is the spec's **v1 column only** (§12) — every v2
row is out of scope here.

Designed to be driven by `/goal`. See §A for the loop protocol and §B for the
goal conditions to paste.

---

## A. Loop protocol

**The `/goal` evaluator reads only the conversation transcript.** It does not
run commands and does not read files. Anything you want it to judge, you must
**print**. That single fact drives the protocol below.

Each turn:

1. Read this file. Pick the **first unchecked task whose `Depends on` are all
   checked**. Do not batch — one task per turn.
2. Do that task, and only that task.
3. Run the task's **Verify** command. Paste its output into the transcript.
4. Tick the task's checkbox in §D and commit (message under `Commit:`).
5. **Print a status line in exactly this form**, so the evaluator can see it:

   ```
   PLAN STATUS: phase <N> — <done>/<total> tasks complete — next: <task id or NONE>
   ```

6. If a task fails twice in a row, mark it `- [!]` with a one-line reason,
   print `BLOCKED: <id> — <reason>`, and move to the next eligible task. If
   every remaining task is blocked, stop and report.

**Do not re-litigate the spec.** Every decision in §C is settled. If you
believe one is wrong, finish the task as written, then note the objection in
the transcript — do not silently substitute a different design.

**Standing rules** (from `CLAUDE.md`, enforced per task):

- Never `#[allow(...)]` to bypass a check.
- Never `.unwrap()`, `.expect()`, or `panic!` in production paths.
- Never re-export inside a public `mod`.
- No dead code — no type, method, or trait added without a caller in this work.
- Conventional commit messages; title under 60 chars.

---

## B. Goal conditions

One phase per `/goal`. Run them in order; do not start a phase before the
previous one's goal has cleared. Each condition is self-contained and instructs
Claude to surface its own proof.

Pair with auto mode so turns run unattended. Each condition carries a turn
bound — raise it if a phase legitimately needs more.

The exact strings are in §E, ready to paste.

---

## C. Settled decisions (do not re-open)

| # | Decision | Spec |
|---|---|---|
| 1 | `launch --tui`, the PTY host, the VT adapter, and the fidelity matrix are deleted. `launch` (inherited) stays. | §3 |
| 2 | `status --watch` is untouched. | §2 |
| 3 | The differentiated work is in `down.rs`, not the TUI. | §5 |
| 4 | Session cost comes from the metering store, emitted as ACP `UsageUpdate.cost`. | §7 |
| 5 | TUI is in-process in `apps/bitrouter`. **No new crate.** | §9 |
| 6 | ACP **v1** semantics. `providers/*` via raw JSON-RPC + schema types behind `unstable_llm_providers`. Do **not** enable `unstable_protocol_v2`. | §10 |
| 7 | Inline viewport (`ratatui::Viewport::Inline`). No alternate screen. | §8 |
| 8 | A distinct verb sharing `RoutingOptions` via `#[command(flatten)]`. **Working name: `chat`.** | §8.1 |
| 9 | Both stderr streams go to one per-session log file. No permanent log pane. | §8.2 |
| 10 | TUI renders **session-scoped data only**, over the existing `snapshot.rs` / `Scope` layer. | §8.3 |

**The one open choice:** the verb spelling (§16.3). Build it as `chat`. If a
human renames it later that is a one-line clap change — do not block on it, and
do not spend a turn debating it.

**Out of scope — do not do these:** anything in the spec's v2 column;
`session/fork`, `session/list`, `session/resume`, `session/set_config_option`;
`providers/disable`; a second pane; a session list; daemon-wide data in the
TUI; extracting a `bitrouter-tui` crate; ACP v2.

---

## D. Tasks

### Phase 1 — Subtraction

- [x] **1.1 Remove the `--tui` flag and the hosted exec path**
  - Depends on: —
  - Files: `apps/bitrouter/src/main.rs`, `apps/bitrouter/src/spawn.rs`
  - Do: delete the `--tui` arg, its `conflicts_with`, its tty gate, and
    `spawn::exec_hosted`. **Keep `prepare` and `exec_inherited`** — the
    `Prepared` seam stays (§3).
  - Done when: `rg -n '\-\-tui|exec_hosted' apps/bitrouter/src` returns nothing.
  - Verify: `cargo build -p bitrouter 2>&1 | tail -5`
  - Commit: `feat(launch)!: remove --tui hosted mode`

- [x] **1.2 Delete the emulator and PTY host**
  - Depends on: 1.1
  - Files: `apps/bitrouter/src/tui/{host,pty,term,lifecycle,conformance}.rs`,
    `apps/bitrouter/src/tui/fixtures/`, `scripts/record-vt-fixture.sh`
  - Do: delete those files and their `mod` declarations in `tui/mod.rs`. Keep
    `watch.rs`, `render.rs`, `snapshot.rs`, and `run_watch`/`oneshot_text`.
  - Done when: those paths do not exist and `tui/mod.rs` declares only the
    surviving modules.
  - Verify: `cargo build -p bitrouter 2>&1 | tail -5`
  - Commit: `refactor(tui): delete the PTY host and VT adapter`

- [x] **1.3 Drop the four terminal dependencies**
  - Depends on: 1.2
  - Files: `apps/bitrouter/Cargo.toml`
  - Do: remove `alacritty_terminal`, `portable-pty`, `termwiz`,
    `wezterm-input-types`. Keep `ratatui` and `crossterm`.
  - Done when: `cargo tree -p bitrouter 2>/dev/null | rg -c 'termwiz|alacritty|portable-pty|wezterm'`
    prints `0`.
  - Verify: `cargo build -p bitrouter 2>&1 | tail -5`
  - Commit: `chore(deps): drop the terminal-emulator stack`

- [x] **1.4 Restore full-catalog `launch` support**
  - Depends on: 1.1
  - Files: `apps/bitrouter/src/harness.rs`
  - Do: make `launch_supported()` true for every catalog entry with an
    `interactive_binary`, and rewrite its doc comment — it currently justifies
    the four-harness cut partly by *"(under `--tui`) a hosted terminal"*, which
    no longer exists.
  - Done when: `launch_supported()` no longer hardcodes four ids, and its doc
    comment does not mention `--tui`.
  - Verify: `cargo test -p bitrouter harness 2>&1 | tail -15`
  - Commit: `feat(launch): restore full-catalog harness support`

- [x] **1.5 Retire the fidelity matrix and fix stale claims**
  - Depends on: 1.2
  - Files: `docs/TUI_FIDELITY_MATRIX.md` (delete), `docs/README.md`,
    `docs/OBSERVABILITY_TUI_SPEC.md`
  - Do: delete the matrix; remove its bullet from `docs/README.md`; mark the
    `launch --tui` half of `OBSERVABILITY_TUI_SPEC.md` superseded by
    `ACP_TUI_SPEC.md`, leaving the `status --watch` half authoritative; delete
    the false *"all eight catalog harnesses"* claim at its :439 and decision 8.
  - Done when: `rg -n 'TUI_FIDELITY_MATRIX|all eight catalog harnesses' docs/`
    returns nothing.
  - Verify: `rg -n 'TUI_FIDELITY_MATRIX|eight catalog harnesses' docs/ ; echo "exit=$?"`
  - Commit: `docs: retire the launch --tui fidelity matrix`

- [x] **1.6 Lockstep the CLI surface docs**
  - Depends on: 1.1, 1.4
  - Files: `skills/bitrouter/SKILL.md`,
    `skills/bitrouter/references/cli.md`, `docs/CLI.md`
  - Do: remove every `--tui` reference; correct the harness-support count.
    (Per `CLAUDE.md`, the skill must never describe a CLI that does not exist.)
  - Done when: `rg -n '\-\-tui' skills/ docs/CLI.md` returns nothing.
  - Verify: `rg -n '\-\-tui' skills/ docs/CLI.md ; echo "exit=$?"`
  - Commit: `docs(skill): drop --tui from the CLI surface`

- [ ] **1.7 Phase 1 gate**
  - Depends on: 1.1–1.6
  - Verify (paste all three):
    `cargo nextest run --all-features 2>&1 | tail -20`,
    `cargo clippy --all-features 2>&1 | tail -20`,
    `cargo fmt -- --check && echo FMT_OK`
  - Done when: all three are clean.
  - Commit: — (no code change; gate only)

### Phase 2 — Prerequisites

- [ ] **2.1 `reload` must rebuild the policy table**
  - Depends on: —
  - Files: `apps/bitrouter/src/reload.rs`, `apps/bitrouter/src/daemon.rs`,
    `apps/bitrouter/src/policy_table_router.rs`
  - Do: reconstruct `PolicyTableRouter` on reload alongside the routing table.
    Today `reload` returns `{"status":"reloaded"}` and silently keeps the old
    tiers; only `restart` applies them (spec §6.1, probe 5).
  - Done when: a new integration test starts a daemon with
    `policy_table.tiers` routing to one provider, reloads with the tier changed
    to another, issues a request, and asserts the **newly** chosen provider.
  - Verify: `cargo nextest run -p bitrouter reload 2>&1 | tail -20`
  - Commit: `fix(daemon): rebuild the policy table on reload`

- [ ] **2.2 Capture both stderr streams to a session log**
  - Depends on: —
  - Files: `crates/bitrouter-sdk/src/acp/up.rs`, `apps/bitrouter/src/main.rs`
  - Do: change the agent child from `Stdio::inherit()` to a captured pipe; add a
    **fourth** tracing-subscriber initializer that writes to a file, alongside
    `basic` / `stderr` / `serve`. Both streams interleave into one per-session
    log under `~/.bitrouter/logs/` (§8.2).
  - Done when: `rg -n 'Stdio::inherit' crates/bitrouter-sdk/src/acp/up.rs`
    returns nothing, and a file-writing subscriber initializer exists.
  - Verify: `cargo nextest run --all-features 2>&1 | tail -20`
  - Commit: `feat(acp): capture agent stderr to a session log`

- [ ] **2.3 Structured launch failure instead of `exit(1)`**
  - Depends on: 2.2
  - Files: `apps/bitrouter/src/acp_cli.rs`
  - Do: routing failures currently `eprintln!` + `std::process::exit(1)` before
    any ACP byte, so a client sees no protocol-level error. Return a structured
    failure the caller can render. Preserve the existing fail-fast timing.
  - Done when: the pre-ACP failure path returns an error value rather than
    calling `std::process::exit`.
  - Verify: `cargo nextest run -p bitrouter acp 2>&1 | tail -20`
  - Commit: `feat(acp): surface launch failures structurally`

- [ ] **2.4 Phase 2 gate** — same three commands as 1.7.

### Phase 3 — The ACP agent surface

- [ ] **3.1 Stop dropping session updates**
  - Depends on: —
  - Files: `crates/bitrouter-sdk/src/acp/translate.rs`
  - Do: `translate`'s `_ => None` arm discards `Plan` and every variant the
    gateway does not itself act on. Forward `PlanUpdate`, `PlanRemoved`,
    `AvailableCommandsUpdate`, `ConfigOptionUpdate`, and `StateUpdate` to the
    manager (§5.1). Prefer the existing raw stream where reverse-mapping would
    lose fidelity.
  - Done when: those five variants survive a translate round-trip, with a unit
    test per variant.
  - Verify: `cargo nextest run -p bitrouter-sdk translate 2>&1 | tail -20`
  - Commit: `feat(acp): forward plan and command session updates`

- [ ] **3.2 Emit router-measured cost as `UsageUpdate`**
  - Depends on: —
  - Files: `crates/bitrouter-sdk/src/acp/down.rs`, `apps/bitrouter/src/acp_cli.rs`
  - Do: synthesize `UsageUpdate` from the metering store on every settled turn
    rather than forwarding only what upstream reports (§7). The per-turn
    `RequestCompleted` telemetry already exists but is drained to stderr/OTel
    and never put on the down-facing wire.
  - Done when: a test drives a settled turn and asserts a non-null
    `UsageUpdate.cost` reaches the manager.
  - Verify: `cargo nextest run -p bitrouter acp 2>&1 | tail -20`
  - Commit: `feat(acp): emit router-measured cost on the wire`

- [ ] **3.3 Implement `providers/list` and `providers/set`**
  - Depends on: 2.1
  - Files: `crates/bitrouter-sdk/src/acp/down.rs`, `apps/bitrouter/Cargo.toml`
  - Do: depend on `agent-client-protocol-schema` with `unstable_llm_providers`
    for the types; dispatch the two methods as **raw JSON-RPC** (the 1.2 runtime
    crate does not forward that feature — §10). `providers/list` returns the
    routing catalog with `current` reflecting the effective route;
    `providers/set` changes the live session's route via the policy-table
    mechanism verified in §6.1. **Secrets never cross this wire** —
    `ProviderCurrentConfig` is non-secret only.
  - Done when: both methods answer, and no credential value appears in any
    response.
  - Verify: `cargo nextest run -p bitrouter acp 2>&1 | tail -20`
  - Commit: `feat(acp): expose routing via providers/list and providers/set`

- [ ] **3.4 Protocol conformance tests**
  - Depends on: 3.1, 3.2, 3.3
  - Files: `apps/bitrouter/tests/acp.rs`
  - Do: extend the existing raw JSON-RPC driver (§13.1) — `providers/list`
    returns the catalog; `providers/set` changes the effective route;
    `UsageUpdate` carries non-null `cost` after a settled turn; the five
    forwarded variants survive round-trip.
  - Done when: all four assertions pass.
  - Verify: `cargo nextest run -p bitrouter acp 2>&1 | tail -20`
  - Commit: `test(acp): cover providers, usage, and forwarded updates`

- [ ] **3.5 Phase 3 gate** — same three commands as 1.7.

### Phase 4 — The TUI

- [ ] **4.1 The verb**
  - Depends on: 2.3
  - Files: `apps/bitrouter/src/main.rs`, `apps/bitrouter/src/acp_cli.rs`
  - Do: add `bitrouter chat <agent>`, reusing `RoutingOptions` via clap
    `#[command(flatten)]` (§8.1). Do not add an interactive mode to `spawn`.
  - Done when: `bitrouter chat --help` lists the shared routing flags and the
    flags have exactly one definition in source.
  - Verify: `cargo run -p bitrouter -- chat --help 2>&1 | head -25`
  - Commit: `feat(cli): add the chat verb for ACP sessions`

- [ ] **4.2 Inline viewport over the shared snapshot layer**
  - Depends on: 2.2, 4.1
  - Files: `apps/bitrouter/src/tui/`
  - Do: `ratatui::Viewport::Inline` — no alternate screen, no lifecycle restore,
    no panic hook (§8). Render over the existing `snapshot.rs` / `Scope`; do
    **not** add a second data layer (§8.3).
  - Done when: the session runs, output lands in real scrollback, and `Ctrl-C`
    leaves a readable transcript.
  - Verify: `cargo nextest run -p bitrouter tui 2>&1 | tail -20`
  - Commit: `feat(tui): inline viewport for ACP sessions`

- [ ] **4.3 Message log and tool-call cards**
  - Depends on: 3.1, 4.2
  - Do: render `MessageChunk`, `ThoughtChunk`, `ToolCall`, `ToolCallUpdate` —
    streaming chunks, status, diffs.
  - Done when: `TestBackend` assertions cover each variant.
  - Verify: `cargo nextest run -p bitrouter tui 2>&1 | tail -20`
  - Commit: `feat(tui): render messages and tool calls`

- [ ] **4.4 Permission modal**
  - Depends on: 4.3
  - Do: render `session/request_permission` and return the chosen option.
  - Done when: a `TestBackend` test drives a permission request to a decision.
  - Verify: `cargo nextest run -p bitrouter tui 2>&1 | tail -20`
  - Commit: `feat(tui): permission prompt`

- [ ] **4.5 Provider/model picker**
  - Depends on: 3.3, 4.3
  - Do: render `providers/list` and issue `providers/set` on selection.
  - Done when: a test asserts selection issues `providers/set`.
  - Verify: `cargo nextest run -p bitrouter tui 2>&1 | tail -20`
  - Commit: `feat(tui): provider and model picker`

- [ ] **4.6 Cost line, honest about scope**
  - Depends on: 3.2, 4.2
  - Do: render session cost. When attribution is unavailable (a subscription
    harness that ignores the injected credential), render `Scope::DaemonWide`
    **visibly** — never present daemon spend as the session's (§8.3).
  - Done when: a test asserts the scope label renders in both scopes.
  - Verify: `cargo nextest run -p bitrouter tui 2>&1 | tail -20`
  - Commit: `feat(tui): session cost line with honest scope`

- [ ] **4.7 Log tail on abnormal exit**
  - Depends on: 2.2, 4.2
  - Do: on abnormal exit, print the last N lines of the session log inline and
    name its path (§8.2). No permanent pane.
  - Done when: a test asserts the tail and path appear on a non-zero exit.
  - Verify: `cargo nextest run -p bitrouter tui 2>&1 | tail -20`
  - Commit: `feat(tui): surface the session log on abnormal exit`

- [ ] **4.8 Document the new surface**
  - Depends on: 4.1–4.7
  - Files: `skills/bitrouter/SKILL.md`, `skills/bitrouter/references/cli.md`,
    `docs/CLI.md`
  - Do: document `chat`, its flags, and the session log location.
  - Done when: all three describe the shipped command.
  - Verify: `rg -n 'chat' docs/CLI.md skills/bitrouter/SKILL.md | head`
  - Commit: `docs(skill): document the chat verb`

- [ ] **4.9 Phase 4 gate** — same three commands as 1.7.

---

## E. Goal strings

Paste one at a time, in order. Each is well under the 4,000-char limit.

**Phase 1**

```
Work through Phase 1 of docs/ACP_TUI_PLAN.md following its §A loop protocol: one task per turn, first unchecked task whose dependencies are met, tick its checkbox and commit. Done when every Phase 1 task 1.1-1.7 is checked in the file AND the final turn shows `cargo nextest run --all-features`, `cargo clippy --all-features`, and `cargo fmt -- --check` all passing with no failures. Print the full Phase 1 checklist and a `PLAN STATUS:` line every turn, and paste each Verify command's real output — do not summarise it. Do not touch status --watch, do not start Phase 2, do not change any file outside the paths named in Phase 1 tasks. Stop after 25 turns.
```

**Phase 2**

```
Work through Phase 2 of docs/ACP_TUI_PLAN.md following its §A loop protocol: one task per turn, first unchecked task whose dependencies are met, tick its checkbox and commit. Done when tasks 2.1-2.4 are checked AND the final turn shows `cargo nextest run --all-features`, `cargo clippy --all-features`, and `cargo fmt -- --check` all passing, AND the transcript shows the new reload regression test from 2.1 passing by name. Print the full Phase 2 checklist and a `PLAN STATUS:` line every turn, and paste each Verify command's real output — do not summarise it. Never use #[allow], .unwrap, .expect or panic!. Do not start Phase 3. Stop after 25 turns.
```

**Phase 3**

```
Work through Phase 3 of docs/ACP_TUI_PLAN.md following its §A loop protocol: one task per turn, first unchecked task whose dependencies are met, tick its checkbox and commit. Done when tasks 3.1-3.5 are checked AND the final turn shows `cargo nextest run --all-features`, `cargo clippy --all-features`, and `cargo fmt -- --check` all passing, AND the transcript shows the four conformance assertions from 3.4 passing by name. Target ACP v1 only — never enable unstable_protocol_v2. No credential value may appear in any providers/* response. Print the full Phase 3 checklist and a `PLAN STATUS:` line every turn, and paste each Verify command's real output — do not summarise it. Do not start Phase 4. Stop after 35 turns.
```

**Phase 4**

```
Work through Phase 4 of docs/ACP_TUI_PLAN.md following its §A loop protocol: one task per turn, first unchecked task whose dependencies are met, tick its checkbox and commit. Done when tasks 4.1-4.9 are checked AND the final turn shows `cargo nextest run --all-features`, `cargo clippy --all-features`, and `cargo fmt -- --check` all passing. The TUI must render session-scoped data only and must not add a session list, a second pane, or a bitrouter-tui crate — if you find yourself wanting any of those, stop and report instead. Print the full Phase 4 checklist and a `PLAN STATUS:` line every turn, and paste each Verify command's real output — do not summarise it. Stop after 45 turns.
```

---

## F. Progress

Phase 1 ☐ · Phase 2 ☐ · Phase 3 ☐ · Phase 4 ☐

Tick a phase when its gate task passes. A phase whose goal cleared but whose
gate is unticked was **not** finished — re-read §D before trusting it.
