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
| 2 | At proposal time, `status --watch` was untouched. It has since been removed; `status --requests` is the settled-request snapshot. | §2 |
| 3 | The differentiated work is in `down.rs`, not the TUI. | §5 |
| 4 | Session cost comes from the metering store, emitted as ACP `UsageUpdate.cost`. | §7 |
| 5 | TUI is **its own crate**, `crates/bitrouter-tui`, depending on neither the `bitrouter` app crate nor its config. | §9 — **amended, see below** |
| 6 | ACP **v1** semantics. `providers/*` via raw JSON-RPC + schema types behind `unstable_llm_providers`. Do **not** enable `unstable_protocol_v2`. | §10 |
| 7 | Inline viewport (`ratatui::Viewport::Inline`). No alternate screen. | §8 |
| | **Superseded** by [`TUI_RENDERER_SPEC.md`](TUI_RENDERER_SPEC.md) §4: still inline and still no alternate screen, but the mechanism is a differential writer over `Backend`, not `Viewport::Inline`. That is the one decision here that spec replaces. | |
| 8 | A distinct verb sharing `RoutingOptions` via `#[command(flatten)]`. **Working name: `chat`.** | §8.1 |
| 9 | Both stderr streams go to one per-session log file. No permanent log pane. | §8.2 |
| 10 | TUI renders **session-scoped data only**, taken **from the ACP wire** — not from `snapshot.rs`. | §8.3 — **amended, see below** |

**The one open choice:** the verb spelling (§16.3). Build it as `chat`. If a
human renames it later that is a one-line clap change — do not block on it, and
do not spend a turn debating it.

### C.1 Amendments after Phase 3 (2026-08-13)

Decisions 5 and 10 were reversed by evidence produced *by Phase 3*, not by
re-litigation. Both spec sections are superseded on these two points.

**Decision 5 — the crate.** Spec §9's load-bearing argument was that being
in-process is *"a capability… it is how the TUI reads the metering store
directly (§7) rather than waiting on the wire."* Phase 3 implemented §7 and
**put cost on the wire** as `UsageUpdate.cost`. The capability that justified
in-process no longer requires in-process. What remains of §9 is its own
strongest counter-argument: a module can silently reach anywhere in the crate,
which is how the previous TUI accreted itself to death, and §9's answer —
"extract on the first accretion attempt" — is backwards. Keeping a module
extractable takes exactly the discipline that a crate enforces mechanically, so
the crate is the cheaper option, not the more expensive one.

**Build it as a crate for the boundary, not for reuse.** A reusable ACP TUI is
a plausible *consequence* of conforming to the protocol, and it should be free.
It is not a design goal to pay for: `providers/*` sits behind an unstable
feature no other agent implements, `UsageUpdate.cost` is optional and rarely
sent, and `bitrouter-gui` is the standing warning about underwriting a design
on a second consumer that never arrives. Negotiate capabilities because ACP
says to, and let generality fall out.

**Decision 10 — not `snapshot.rs`.** That layer was the daemon-wide live-view
shape: request rows, rate metrics, control-socket state. §8.3 forbids the TUI
from rendering nearly all of it. Reusing it would import a daemon-wide data
model into a session-scoped view — the exact mistake §8.3 exists to prevent.
The TUI's data comes from its own ACP session. `Scope` is a two-variant enum;
it moves to the wire (task 4.1), it is not a reason to share a data layer.

**ACP v2 stays out**, and the crate is part of why. v2's `PromptResponse` is
`{}` with no `stop_reason`, so a v2 **gateway** is an `engine.rs`/`turn.rs`
rewrite. But a v2 **client** merely reads a `state_update` notification instead
of a return value. Once the TUI is a separate crate speaking ACP over stdio, it
can gain v2 client support without touching the engine — which makes deferring
v2 cheap, and doing it now unjustified for a draft protocol no shipping agent
speaks. (There is, for the record, **zero** ACP v2 support in `bitrouter-sdk`
today; the `V2026_07_28` constants are MCP's dated protocol version, a
different protocol.)

**Out of scope — do not do these:** anything in the spec's v2 column;
`session/fork`, `session/list`, `session/resume`, `session/set_config_option`;
`providers/disable`; a second pane; a session list; daemon-wide data in the
TUI; ACP v2.

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
    `ACP_TUI_SPEC.md` (the `status --watch` half was superseded later by
    `status --requests`); delete the false *"all eight catalog harnesses"*
    claim at its :439 and decision 8.
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

- [x] **1.7 Phase 1 gate**
  - Depends on: 1.1–1.6
  - Verify (paste all three) — **these are the commands CI runs**; anything
    weaker passes locally and fails on the PR, which is exactly what happened
    on #816 (a lint that only fires under `--tests`, and a doc warning that
    only fires under `-D warnings`):
    `cargo nextest run --all-features 2>&1 | tail -20`,
    `cargo clippy --workspace --all-features --tests -- -D warnings 2>&1 | tail -20`,
    `cargo fmt -- --check && echo FMT_OK`
  - Also run before pushing: `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps`
  - Done when: all four are clean.
  - Commit: — (no code change; gate only)

### Phase 2 — Prerequisites

- [x] **2.1 `reload` must rebuild the policy table**
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

- [x] **2.2 Capture both stderr streams to a session log**
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

- [x] **2.3 Structured launch failure instead of `exit(1)`**
  - Depends on: 2.2
  - Files: `apps/bitrouter/src/acp_cli.rs`
  - Do: routing failures currently `eprintln!` + `std::process::exit(1)` before
    any ACP byte, so a client sees no protocol-level error. Return a structured
    failure the caller can render. Preserve the existing fail-fast timing.
  - Done when: the pre-ACP failure path returns an error value rather than
    calling `std::process::exit`.
  - Verify: `cargo nextest run -p bitrouter acp 2>&1 | tail -20`
  - Commit: `feat(acp): surface launch failures structurally`

- [x] **2.4 Phase 2 gate** — same three commands as 1.7.

### Phase 3 — The ACP agent surface

- [x] **3.1 Stop dropping session updates**
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

- [x] **3.2 Emit router-measured cost as `UsageUpdate`**
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

- [x] **3.3 Implement `providers/list` and `providers/set`**
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

- [x] **3.4 Protocol conformance tests**
  - Depends on: 3.1, 3.2, 3.3
  - Files: `apps/bitrouter/tests/acp.rs`
  - Do: extend the existing raw JSON-RPC driver (§13.1) — `providers/list`
    returns the catalog; `providers/set` changes the effective route;
    `UsageUpdate` carries non-null `cost` after a settled turn; the five
    forwarded variants survive round-trip.
  - Done when: all four assertions pass.
  - Verify: `cargo nextest run -p bitrouter acp 2>&1 | tail -20`
  - Commit: `test(acp): cover providers, usage, and forwarded updates`

- [x] **3.5 Phase 3 gate** — same three commands as 1.7.

### Phase 4 — The TUI

Phase 3 surfaced **two prerequisites that must land before any pane renders**.
Both are gaps between what a control *looks like* it does and what it does, so
building the UI on top of them first would ship exactly the dishonesty this
spec spends its length objecting to. They are 4.1 and 4.2, and nothing else
depends on the TUI existing.

- [x] **4.1 Attribute ACP session traffic, and put the scope on the wire**
  - Depends on: —
  - Files: `apps/bitrouter/src/acp_cli.rs`, `apps/bitrouter/src/spawn.rs`
  - Do: **fixes a defect shipped in 3.2.** `measured_usage_update` calls
    `spend_summary(window)` — *every caller* the daemon served during the
    session's time window — and the ACP path mints no launch id, so there is
    nothing to scope by. On a single-user local daemon this coincides with
    session spend; with any concurrent caller it does not, and the wire then
    reports daemon spend as the session's (§8.3's cardinal sin).
    Mint a per-session attribution token on the ACP launch path the way
    `spawn.rs` already does (`mint_launch_token` / `is_launch_token`), query
    `spend_summary_for_launch` when it landed, and carry the resulting
    `Scope` on the wire in `UsageUpdate`'s `_meta` so a client can label it.
    Fall back to `spend_summary` **only** with the scope marked `daemon_wide`.
  - Done when: a test with two callers settling spend in the same window
    asserts the session's `UsageUpdate.cost` counts only its own, and that a
    session whose traffic is unattributable reports `daemon_wide` rather than
    silently over-reporting.
  - Verify: `cargo nextest run -p bitrouter acp 2>&1 | tail -20`
  - Commit: `fix(acp): scope reported cost to the session`

- [x] **4.2 `providers/set` must actually reroute**
  - Depends on: 2.1
  - Files: `apps/bitrouter/src/daemon.rs`, `apps/bitrouter/src/reload.rs`,
    `apps/bitrouter/src/acp_cli.rs`
  - Do: today `providers/set` changes the route the session *reports* and
    nothing more — the substrate is a separate process and the agent child
    talks to the daemon directly. Add a `DaemonCommand` variant that installs a
    **launch-scoped** route override into the live `PolicyTableRouter` (2.1
    made its table swappable; 4.1 supplies the launch id to key it by), and
    have `SessionProviders::set` send it. An override must expire with the
    session and must never alter another caller's routing.
  - Done when: an integration test starts a daemon, issues `providers/set`, and
    asserts a **subsequent request on that launch id** resolves to the new
    provider while a request without it does not.
  - Verify: `cargo nextest run -p bitrouter providers 2>&1 | tail -20`
  - Commit: `feat(daemon): launch-scoped route override`

- [x] **4.3 The `bitrouter-tui` crate**
  - Depends on: —
  - Files: `crates/bitrouter-tui/Cargo.toml`, `crates/bitrouter-tui/src/lib.rs`
  - Do: a new workspace member (`members = ["crates/*"]` picks it up). It
    depends on `ratatui`, `crossterm`, and the two ACP crates — and on
    **neither the `bitrouter` app crate nor its `Config`**. That absence is the
    whole point: the boundary the last TUI failed to hold as prose becomes a
    compiler check. Everything it renders arrives over ACP.
  - Done when: `cargo tree -p bitrouter-tui | rg -c '^bitrouter[ -]'` shows no
    dependency on the app crate, and the crate builds standalone.
  - Verify: `cargo tree -p bitrouter-tui --depth 1 && cargo build -p bitrouter-tui 2>&1 | tail -5`
  - Commit: `feat(tui): add the bitrouter-tui crate`

- [x] **4.4 The verb**
  - Depends on: 2.3, 4.3
  - Files: `apps/bitrouter/src/main.rs`, `apps/bitrouter/src/acp_cli.rs`,
    `apps/bitrouter/Cargo.toml`
  - Do: add `bitrouter chat <agent>`, reusing `RoutingOptions` via clap
    `#[command(flatten)]` (§8.1). The verb owns argument parsing and session
    launch; it hands the running ACP session to `bitrouter_tui`. Do not add an
    interactive mode to `spawn`.
  - Done when: `bitrouter chat --help` lists the shared routing flags and the
    flags have exactly one definition in source.
  - Verify: `cargo run -p bitrouter -- chat --help 2>&1 | head -25`
  - Commit: `feat(cli): add the chat verb for ACP sessions`

- [x] **4.5 Inline renderer over the session's own updates**
  - Depends on: 4.3, 4.4
  - Files: `crates/bitrouter-tui/src/`
  - Do: render inline through the differential writer over `Backend` — no
    alternate screen, but session-long raw mode still has lifecycle restore and a
    panic hook. State is built **from the ACP `session/update` stream**; do not
    reach for `snapshot.rs` or any local store (decision 10, amended).
  - Done when: the session runs, output lands in real scrollback, and `Ctrl-C`
    leaves a readable transcript.
  - Verify: `cargo nextest run -p bitrouter-tui 2>&1 | tail -20`
  - Commit: `feat(tui): inline renderer for ACP sessions`
  - **Superseded** by [`TUI_RENDERER_SPEC.md`](TUI_RENDERER_SPEC.md) §4, shipped
    2026-08-14. What this task built was append-only against a protocol whose
    entities are patchable, so one tool call printed a line per status change.
    The original `Viewport::Inline` mechanism is replaced by a differential
    writer; *inline* itself survives, and so does the done-when above. The
    `Ctrl-C` named there is now one of four bindings — `Esc`, `Ctrl-C`,
    `Ctrl-D`, `Ctrl-L` — because `chat`
    holds raw mode for the session; see that spec's §9 and
    [`CLI.md`](CLI.md).

- [x] **4.6 Message log and tool-call cards**
  - Depends on: 3.1, 4.5
  - Do: render `MessageChunk`, `ThoughtChunk`, `ToolCall`, `ToolCallUpdate` —
    streaming chunks, status, diffs. Plans and available commands (3.1) render
    here too, or degrade to nothing when the agent never sends them.
  - Done when: `TestBackend` assertions cover each variant.
  - Verify: `cargo nextest run -p bitrouter-tui 2>&1 | tail -20`
  - Commit: `feat(tui): render messages and tool calls`

- [x] **4.7 Permission modal**
  - Depends on: 4.6
  - Do: render `session/request_permission` and return the chosen option.
  - Done when: a `TestBackend` test drives a permission request to a decision.
  - Verify: `cargo nextest run -p bitrouter-tui 2>&1 | tail -20`
  - Commit: `feat(tui): permission prompt`

- [x] **4.8 Cost line, honest about scope**
  - Depends on: 4.1, 4.5
  - Do: render session cost from `UsageUpdate.cost`, labelled with the scope
    4.1 puts on the wire. Three states, all distinct: attributed session spend;
    `daemon_wide` rendered **visibly** as such; and *no cost reported at all* —
    which must read as "not reported", never as `$0.00`.
  - Done when: a test asserts a distinct rendering for each of the three.
  - Verify: `cargo nextest run -p bitrouter-tui 2>&1 | tail -20`
  - Commit: `feat(tui): session cost line with honest scope`

- [x] **4.9 Log tail on abnormal exit**
  - Depends on: 2.2, 4.5
  - Do: on abnormal exit, print the last N lines of the session log inline and
    name its path (§8.2). No permanent pane. The crate cannot read the app's
    paths, so the log location arrives from the caller — `chat` passes it in.
  - Done when: a test asserts the tail and path appear on a non-zero exit.
  - Verify: `cargo nextest run -p bitrouter-tui 2>&1 | tail -20`
  - Commit: `feat(tui): surface the session log on abnormal exit`

- [x] **4.10 Provider/model picker**
  - Depends on: 3.3, 4.2, 4.6
  - Do: render `providers/list` and issue `providers/set` on selection. Blocked
    on 4.2 by design — without it the control does not do what it appears to.
    Hide the picker entirely when the agent does not advertise `providers/*`;
    a dead control is worse than an absent one.
  - Done when: a test asserts selection issues `providers/set`, and a second
    asserts the picker is absent for an agent without the capability.
  - Verify: `cargo nextest run -p bitrouter-tui 2>&1 | tail -20`
  - Commit: `feat(tui): provider and model picker`

- [x] **4.11 Document the new surface**
  - Depends on: 4.4–4.10
  - Files: `skills/bitrouter/SKILL.md`, `skills/bitrouter/references/cli.md`,
    `docs/CLI.md`, `docs/DEVELOPMENT.md`
  - Do: document `chat`, its flags, and the session log location. Add
    `bitrouter-tui` to the workspace architecture guide, stating the dependency
    rule (no edge to the app crate) so the boundary is written down as well as
    compiled.
  - Done when: all four describe the shipped command and the new crate.
  - Verify: `rg -n 'chat|bitrouter-tui' docs/CLI.md docs/DEVELOPMENT.md skills/bitrouter/SKILL.md | head`
  - Commit: `docs(skill): document the chat verb`

- [x] **4.12 Phase 4 gate** — same three commands as 1.7.

---

## E. Goal strings

Paste one at a time, in order. Each is well under the 4,000-char limit.

**Phase 1**

```
Work through Phase 1 of docs/ACP_TUI_PLAN.md following its §A loop protocol: one task per turn, first unchecked task whose dependencies are met, tick its checkbox and commit. Done when every Phase 1 task 1.1-1.7 is checked in the file AND the final turn shows `cargo nextest run --all-features`, `cargo clippy --all-features`, and `cargo fmt -- --check` all passing with no failures. Print the full Phase 1 checklist and a `PLAN STATUS:` line every turn, and paste each Verify command's real output — do not summarise it. Do not reintroduce status --watch, do not start Phase 2, do not change any file outside the paths named in Phase 1 tasks. Stop after 25 turns.
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
Work through Phase 4 of docs/ACP_TUI_PLAN.md following its §A loop protocol: one task per turn, first unchecked task whose dependencies are met, tick its checkbox and commit. Done when tasks 4.1-4.12 are checked AND the final turn shows `cargo nextest run --all-features`, `cargo clippy --all-features`, and `cargo fmt -- --check` all passing, AND the transcript shows the 4.1 cost-scoping test and the 4.2 route-override test passing by name. Build the TUI as the crate crates/bitrouter-tui with no dependency on the bitrouter app crate — if you find yourself needing one, stop and report instead of adding it. The TUI renders session-scoped data taken from the ACP wire only: no snapshot.rs, no session list, no second pane, no daemon-wide data. Target ACP v1 — never enable unstable_protocol_v2. Never render a control that does not do what it appears to: no cost figure without its scope, no provider picker before 4.2 lands. Print the full Phase 4 checklist and a `PLAN STATUS:` line every turn, and paste each Verify command's real output — do not summarise it. Stop after 45 turns.
```

---

## F. Progress

Phase 1 ☑ · Phase 2 ☑ · Phase 3 ☑ · Phase 4 ☑

Tick a phase when its gate task passes. A phase whose goal cleared but whose
gate is unticked was **not** finished — re-read §D before trusting it.
