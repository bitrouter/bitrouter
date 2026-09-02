# ACP safety invariants: what `engine::Session` guarantees today

Status: **implementation gate** · Date: 2026-09-02

Companion to [`ACP_CONTROLLER_SPEC.md`](ACP_CONTROLLER_SPEC.md). That document
says what the controller *is*; this one says what the code being deleted
*guarantees*, so nothing is lost by omission.

## Why this exists

`engine::Session` is scheduled for deletion in favour of the connection-level
controller. Most of it is transport plumbing that the controller replaces
outright. A minority of it is **safety behaviour** — properties that decide
whether an agent gets consent it was never given, whether a harness hangs
forever, or whether a child process outlives the session that spawned it.

Those properties are not written down anywhere as a set. They are distributed
across doc comments, and each is pinned (or not) by a test somewhere else. A
refactor that moves the transport and forgets one of these does not fail to
compile and does not fail a test — it fails in production, quietly, in the
direction of *more* permission and *fewer* dead processes reaped.

**The rule this file enforces:** no PR in the controller migration may delete a
row's "enforced today" location until that row's "owner after" column names a
merged implementation and its "pinned after" column names a passing test.

**Status.** Phase 2.5 step 4 (the shared client in `bitrouter-sdk::acp::client`,
with `acp prompt` migrated onto it and an in-process controller) re-homed
I1, I2, I4, I6, I7, I8 and I11. Step 6 moved interactive `chat` onto the same
client and controller, so nothing runs on `engine::Session` any more: I5 is
enforced by `chat`'s cancel path against the client's ledger, I8 is the
client's on every path, and I10's strong side (`ControlledSession::shutdown`
awaiting the reap) is the only side left. The rows below record where each
lives now.

## The one that had no equivalent

**I1 is the finding this document exists for.** In the engine, a consumer that
loses interest in a permission request denies it by *dropping* — the parked
upstream handler owns the oneshot, and `Err(_)` on the receiver maps to the
reject option. There is no equivalent under a transparent proxy: the controller
forwards `session/request_permission` to the manager as an open JSON-RPC
request, and only an explicit response closes it. Nothing converts silence into
denial.

So the engine's "deny by dropping" had to become **"deny explicitly, on every
path that can abandon a request"** — and every such path had to be enumerated,
because the compiler will not find them.

Both mechanisms now coexist in `AcpClient`. Drop-denial survives unchanged: the
client's `PermissionLedger` tracks each request by a **weak** handle, so holding
a ledger entry never keeps a request alive and never disables the drop arm. The
ledger exists only for the paths drop-denial cannot reach — a consumer still
holding a live `PendingPermission` it will never answer. Those paths are
enumerated in *Summary of what must be written* at the end of this file.

---

## A. Permission safety

The class where a mistake grants consent that nobody gave.

| # | Invariant | Enforced today | Pinned today | Owner after | Pinned after |
|---|---|---|---|---|---|
| **I1** | An abandoned permission resolves to the agent's **reject option**, never consent and never silence | `client.rs` — parked handler `Err(_) => reject`, **plus** `PermissionLedger::deny_outstanding` on every path that abandons a live request | `up.rs` `dropping_pending_permission_defaults_to_deny` (drop arm, end to end; ported onto `AgentProcess` + `AcpClient` when `UpstreamConnection` was deleted) | shared client — **landed** | `client.rs` `dropping_every_clone_still_defaults_to_deny`, `teardown_denies_an_outstanding_permission`, `turn_timeout_cancels_cooperatively_and_denies_the_parked_permission` |
| **I2** | A selection is validated against the offered options; an unknown id becomes the reject option, never the fabricated id | `translate.rs` `sanitize_selection`, called by the shared client's parked handler | `translate.rs` `sanitize_selection_preserves_exact_known_id`, `:625`, `:641` | `bitrouter-sdk::acp::translate` (pure fn, unmoved) — **landed** | existing three |
| **I3** | `Cancelled` passes through as `Cancelled` — it is never upgraded to a selection | `translate.rs` `sanitize_selection` | `translate.rs` `sanitize_selection_cancelled_passes_through` `sanitize_selection_cancelled_passes_through` | shared client | existing |
| **I4** | Each request is answered **exactly once**; later answers are no-ops | `client.rs` `PermissionResolver::answer` — `guard.take()`, first wins | `client.rs` `a_permission_is_answered_exactly_once` | shared client — **landed** | `client.rs` `a_permission_is_answered_exactly_once` |
| **I5** | A permission outstanding when a **turn is cancelled** is denied, not left for whichever keystroke arrives next | `chat/session.rs` cancel path — drain `permission_rx` and `deny` each, then `AcpClient::deny_outstanding_permissions` **while the connection is live** (the ledger answers anything the client emitted that never reached the channel), then clear pending | `chat/session.rs` `an_unanswered_permission_takes_the_reject_option`, `an_unanswered_permission_never_resolves_to_consent` (the rule); the ledger half by `client.rs` `teardown_denies_an_outstanding_permission` | state machine reducer (step 9) | CHAT_MACHINE_SPEC T2 + T3 |
| **I6** | A **headless** path denies rather than hanging the harness | `acp_cli.rs` `prompt` — the deny pump over `AcpClient::subscribe_permissions`; `chat_plain` denies inline | `tests/acp.rs` `prompt_headless_denies_permission_and_completes` | NDJSON + pipe presentations — **landed** | existing, unchanged |
| **I7** | A permission outstanding at **session teardown** is denied | `client.rs` `AcpClient::shutdown` denies, then waits (bounded) for the parked handlers to answer before the transport goes; the command loop and the driver tail repeat it for the drop path | `client.rs` `teardown_denies_an_outstanding_permission` | shared client teardown — **landed** | `client.rs` `teardown_denies_an_outstanding_permission` |

### Deliberately dropped

| Invariant | Enforced today | Pinned today | Why it goes |
|---|---|---|---|
| A manager that detaches and reattaches sees still-outstanding permissions replayed | `PermissionRegistry` — **deleted** with the engine | its two tests went with it | Reattach is not in the controller model. `ACP_CONTROLLER_SPEC.md` §9 instructs managers to assume nothing survives reconnect. Irrelevant to `chat`, which is one process and one connection. |

---

## B. Turn deadlines

| # | Invariant | Enforced today | Pinned today | Owner after | Pinned after |
|---|---|---|---|---|---|
| **I8** | A turn exceeding `--turn-timeout` is cancelled **cooperatively** (`session/cancel`), given `TURN_CANCEL_GRACE` (3s) to comply, then failed | `client.rs` `AcpClient::prompt_typed` — for `prompt`, `chat`, and piped `chat` alike | `client.rs` `turn_timeout_cancels_cooperatively_and_denies_the_parked_permission` | shared client — **landed** | `client.rs` `turn_timeout_cancels_cooperatively_and_denies_the_parked_permission`; `tests/acp.rs` `prompt_turn_timeout_fails_the_turn_instead_of_hanging` |
| **I9** | Turns queued behind a cancelled one resolve to `StopReason::Cancelled` rather than running | `turn::TurnController` — **deleted** with the engine | its queue tests went with it | **none — not load-bearing** | n/a |

I9 is safe to drop: every production caller sends one prompt at a time —
`chat` (`session.rs` (the turn loop`, next read only after the outcome), `chat_plain`,
and `prompt`'s `run_turn` (at most two *sequential* turns via the repair
re-prompt). The only path that could have issued concurrent prompts on one
session was `down.rs`'s `SessionAgent`, now unreachable. **Say so in the
commit rather than letting it disappear.**

I8 is **not** safe to drop. `--turn-timeout` is a documented flag on `chat`,
`acp prompt`, `acp serve`, and `spawn`, and `skills/bitrouter/references/cli.md`
promises `prompt` and `chat` retain it. It is the client's on both; `acp serve`
says on stderr that it does not enforce one, because deadlines belong to the
manager on a transparent path.

---

## C. Process lifetime

| # | Invariant | Enforced today | Pinned today | Owner after | Pinned after |
|---|---|---|---|---|---|
| **I10** | The harness child **and its process group** are killed on teardown, on child exit, and on reaper-handle drop — and teardown **waits for it** | `up.rs` `spawn_child_reaper` → `kill_process_group`; the confirmation is handed to the owner by `AgentProcess::reaped`, and every command's owner is now `ControlledSession::shutdown` — `chat`'s three exits all route through it before the terminal is given back | `up.rs` `shutdown_kills_wrapper_chain_process_group` | `AgentProcess` + `ControlledSession::shutdown` (the one owner left) | existing, plus the owner's wait |
| **I11** | A harness that dies mid-prompt **fails the turn** instead of hanging it | `AgentProcess::connect_to` — `select!` on `dead_rx`, "agent process exited while the ACP controller was connected" | `tests/acp.rs` `prompt_fails_fast_when_the_harness_dies_mid_turn`; `up.rs` `agent_crash_fails_pending_commands_fast` | unchanged — **now the only child-owning path** | those two |

**History, because the shape of this one moved twice.** The group kill runs in
a tokio task, so the **panic** exit can still end the process before it
executes, and `kill_on_drop` reaches `npx` but not the `node` it spawns. That
much is unchanged and remains the one axis on which in-process is weaker than a
subprocess `acp serve`, which gets stdin EOF and kills its own group.

What *did* change: moving to the shared client briefly weakened this on the
**normal** path too. `connect_to` ordered the kill and awaited a confirmation
in its tail — but when a connection closes the SDK drops the transport task
mid-`select!`, so that tail never ran, and an owner dropping its runtime in the
same breath could lose the wrapped grandchild. The kill still happened (the
dropped `kill_tx` wakes the reaper) but nothing waited for it. It is fixed by
handing the confirmation to the owner via `AgentProcess::reaped`, which lives
outside the connection and so survives the drop. Note that
`shutdown_kills_wrapper_chain_process_group` polls for two seconds *after*
shutdown returns, so it passed throughout — it cannot distinguish a confirmed
kill from a race that was usually won. It is **left as it is**: tightening the
tolerance would buy a flaky test rather than a guarantee.

**The guarantee was asymmetric, and the weak side is now unreachable.**
`ControlledSession::shutdown` awaits the reap directly, so a caller that has
awaited it knows the child is gone. `UpstreamConnection` could not: its reaper
runs on the runtime its own thread owns, so the wait sits inside that thread
rather than in `shutdown()`. With `chat` on the controlled path nothing
constructs an `UpstreamConnection` any more; it is deleted with the engine in
step 7. **Nothing pins the strong side yet** — see the unpinned list below.

---

## D. Terminal restoration

| # | Invariant | Enforced today | Pinned today | Owner after | Pinned after |
|---|---|---|---|---|---|
| **I12** | Raw mode is released on **all three exits** — normal (and every `?`), panic, and signal | `Stdin::drop` → `lifecycle::restore`; `lifecycle::install_panic_restore`; `signals::Shutdown` as a `select!` arm | `lifecycle.rs` `the_panic_hook_restores_and_still_reports` `the_panic_hook_restores_and_still_reports`; `signals.rs` arm test; the terminal itself by the documented manual checks in `chat/mod.rs` | TUI presentation — unchanged | existing |

Unaffected by the controller migration, but listed because the state-machine
work touches the loop that owns all three, and because I10's weakness
interacts with the panic path.

---

## E. Route leases and secrets

| # | Invariant | Enforced today | Pinned today | Owner after | Pinned after |
|---|---|---|---|---|---|
| **I13** | A route lease is removed when its session closes **or** the controller disconnects; harness success precedes lease cleanup on close/delete | `controller.rs` (`session_closed` on close/delete`, `:635` `session_closed`; `:362` `disconnected` | controller tests | unchanged — already the controller's | existing |
| **I14** | Credentials never reach `Debug`, logs, or responses | hand-written `Debug`: `ProviderEndpointPlan` (`controller.rs` (`ProviderEndpointPlan`'s hand-written `Debug``), `HarnessEndpointPlan` (`harness.rs` (`HarnessEndpointPlan`'s hand-written `Debug``), `AgentProcess` (`up.rs`) | `up.rs` `agent_process_debug_redacts_arguments_and_environment_values` | unchanged | existing |

---

## The coverage problem behind all of it

`chat` — the surface this migration changed most — has **no integration test**.
Its only tests are the two pure-function permission tests in
`chat/session.rs` (`an_unanswered_permission_takes_the_reject_option`,
`an_unanswered_permission_never_resolves_to_consent`). `tests/acp.rs` covers
`prompt` (`prompt_ndjson`, `prompt_headless_denies_permission_and_completes`),
`serve` (`serve_subprocess_e2e`,
`conformance_forwarded_update_variants_survive_round_trip`), and the **pipe**
branch of chat (`chat_on_a_pipe_is_plain_text`) — never the interactive one.

Step 6 changed chat's transport, credential mode, cost source, route surface,
and teardown. What it could pin without a pty, it did: the route capability
gate and error mapping against the real controller (`client.rs`
`route_calls_round_trip_through_the_controller`,
`route_control_is_absent_without_a_binding`,
`route_control_capability_needs_all_three_conditions`,
`route_errors_are_classified_by_code_not_text`), the cost marker
(`bitrouter-tui` `cost::tests`, plus `acp_cli.rs`
`the_cost_marker_is_spelled_the_same_on_both_sides`), and the cached cost
bridge staying off the forward path (`acp_cli.rs`
`the_cached_cost_answers_without_awaiting_the_daemon`,
`an_unmeasured_session_stays_absent_and_a_held_figure_survives`). The event
loop, the key table, and the three exits remain checked by the manual steps in
`chat/mod.rs`; with no integration coverage there, the first regression signal
is still a person at a terminal.

**Consequence for sequencing (as applied):** `prompt` went first. It is
headless, it has byte-level NDJSON assertions, and it exercises I1, I2, I6, I8
and I11 — five of the seven invariants that needed new code. It was the parity
oracle `chat` did not have.

## Known-unpinned, as of this revision

Recorded rather than left to be rediscovered:

- `ControlledSession::shutdown` returning only after the child is reaped. The
  behaviour is implemented; no test drives a real harness through teardown and
  asserts no survivor.
- Delivery — as opposed to resolution — of a denial issued at teardown. Covered
  only over an in-memory transport, and see I1's note on why byte delivery is
  best-effort.
- `report_turn` and `spawn_tool_spans` emissions on the prompt, piped-chat,
  and interactive-chat paths.
- Revocation of `chat`'s controller credential on each exit.
  `ControlledSession::shutdown` and the launch failure paths call it, and a
  panic exit relies on the daemon-side TTL instead; nothing drives a live
  daemon through `chat`'s teardown and asserts the credential is gone.
- The interactive picker's behaviour end to end: the route the footer shows
  after a `set` is the response's `current`, by construction rather than by
  a test that drives keys.
- The exit status of `acp prompt` when teardown fails: it now logs rather than
  failing the command, which was an undeclared change in the migration.

## Summary of what must be written

Four invariants needed new implementations, and three of those needed new
tests. All four landed with the shared client:

- **I1** — explicit denial on every abandonment path *(the highest-risk item)*.
  The enumerated paths in `AcpClient` are: no consumer on the permission stream
  (the item drops, the parked handler denies); every clone dropped (same); a
  turn abandoned at `turn_timeout`, denied before `session/cancel` goes out; a
  turn that returned an error, denied on the way out of `prompt_typed`; the
  command loop ending, by explicit shutdown or by the last client handle
  dropping; the connection driver returning for any reason; and
  `shutdown`, which denies and then waits for the responses before the
  transport closes.
- **I7** — denial at session teardown
- **I8** — turn timeout with cooperative cancel and grace
- **I4** — answered-exactly-once

Two are deliberately dropped and must be named in their commit: reattach replay,
and the turn queue flush.

Everything else moves unchanged or is already the controller's.
