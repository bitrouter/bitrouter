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

## The one that has no equivalent yet

**I1 is the finding this document exists for.** In the engine, a consumer that
loses interest in a permission request denies it by *dropping* — the parked
upstream handler owns the oneshot, and `Err(_)` on the receiver maps to the
reject option. There is no equivalent under a transparent proxy: the controller
forwards `session/request_permission` to the manager as an open JSON-RPC
request, and only an explicit response closes it. Nothing converts silence into
denial.

So the engine's "deny by dropping" has to become **"deny explicitly, on every
path that can abandon a request"** — and every such path has to be enumerated,
because the compiler will not find them.

---

## A. Permission safety

The class where a mistake grants consent that nobody gave.

| # | Invariant | Enforced today | Pinned today | Owner after | Pinned after |
|---|---|---|---|---|---|
| **I1** | An abandoned permission resolves to the agent's **reject option**, never consent and never silence | `up.rs:867-869` — parked handler, `Err(_) => select_option(Deny, &options)` | `up.rs:1235` `dropping_pending_permission_defaults_to_deny` | **shared client — NEW CODE** | *must be written* |
| **I2** | A selection is validated against the offered options; an unknown id becomes the reject option, never the fabricated id | `translate.rs:338-351` `sanitize_selection` | `translate.rs:601`, `:625`, `:641` | shared client (pure fn, moves unchanged) | existing three, re-pointed |
| **I3** | `Cancelled` passes through as `Cancelled` — it is never upgraded to a selection | `translate.rs:343` | `translate.rs:641` `sanitize_selection_cancelled_passes_through` | shared client | existing |
| **I4** | Each request is answered **exactly once**; later answers are no-ops | `up.rs:148-154` — `guard.take()`, first wins | implied via `is_resolved` (`up.rs:158`) | shared client | *needs an explicit test* |
| **I5** | A permission outstanding when a **turn is cancelled** is denied, not left for whichever keystroke arrives next | `chat/session.rs` cancel path — drain `permission_rx`, `deny`, clear pending | `chat/session.rs:668`, `:690` | state machine reducer | CHAT_MACHINE_SPEC T2 + T3 |
| **I6** | A **headless** path denies rather than hanging the harness | `acp_cli.rs:1147` `pending.deny()`; `chat_plain` denies inline | `tests/acp.rs:778` `prompt_headless_denies_permission_and_completes` | NDJSON + pipe presentations | existing, re-pointed |
| **I7** | A permission outstanding at **session teardown** is denied | last-clone drop → `up.rs:867` `Err` branch; the registry is the last holder | — (inverse pinned at `permissions.rs:212`) | shared client teardown | *must be written* |

### Deliberately dropped

| Invariant | Enforced today | Pinned today | Why it goes |
|---|---|---|---|
| A manager that detaches and reattaches sees still-outstanding permissions replayed | `PermissionRegistry` (re-subscribable, sole consumer of the take-once stream) | `permissions.rs:174`, `engine.rs:787` | Reattach is not in the controller model. `ACP_CONTROLLER_SPEC.md` §9 instructs managers to assume nothing survives reconnect. Irrelevant to `chat`, which is one process and one connection. **Delete the tests in the same commit, with this row cited.** |

---

## B. Turn deadlines

| # | Invariant | Enforced today | Pinned today | Owner after | Pinned after |
|---|---|---|---|---|---|
| **I8** | A turn exceeding `--turn-timeout` is cancelled **cooperatively** (`session/cancel`), given `TURN_CANCEL_GRACE` (3s) to comply, then failed | `engine.rs:387-410` | `engine.rs:889` | **shared client — NEW CODE** (~20 lines) | *must be written* |
| **I9** | Turns queued behind a cancelled one resolve to `StopReason::Cancelled` rather than running | `turn.rs:92-99` `flush()` + `engine.rs:414` flushed value | `turn.rs:147`, `:175`, `:224`, `:239`, `:258` | **none — not load-bearing** | n/a |

I9 is safe to drop: every production caller sends one prompt at a time —
`chat` (`session.rs:189`, next read only after the outcome), `chat_plain`,
and `prompt`'s `run_turn` (at most two *sequential* turns via the repair
re-prompt). The only path that could have issued concurrent prompts on one
session was `down.rs`'s `SessionAgent`, now unreachable. **Say so in the
commit rather than letting it disappear.**

I8 is **not** safe to drop. `--turn-timeout` is a documented flag on `chat`,
`acp prompt`, `acp serve`, and `spawn`, and
`skills/bitrouter/references/cli.md:163` promises `prompt` and `chat` retain it.

---

## C. Process lifetime

| # | Invariant | Enforced today | Pinned today | Owner after | Pinned after |
|---|---|---|---|---|---|
| **I10** | The harness child **and its process group** are killed on teardown, on child exit, and on reaper-handle drop | `up.rs:477-502` `spawn_child_reaper` → `kill_process_group`; `AgentProcess` awaits a 2s confirm | `up.rs:1330` `shutdown_kills_wrapper_chain_process_group` | `AgentProcess` — **already the controller's, unchanged** | existing |
| **I11** | A harness that dies mid-prompt **fails the turn** instead of hanging it | `AgentProcess::connect_to` — `select!` on `dead_rx`, "agent process exited while the ACP controller was connected" | — | unchanged | *needs a test* |

**Known weakness in I10, unchanged by this work but worth recording:** the
group kill runs in a tokio task, so the **panic** exit can end the process
before it executes, and `kill_on_drop` reaches `npx` but not the `node` it
spawns. A subprocess `acp serve` would get stdin EOF and kill its own group;
in-process does not. This is not a regression against today — the engine
already owns the child in-process — but it is the one axis on which
in-process is strictly weaker than subprocess.

---

## D. Terminal restoration

| # | Invariant | Enforced today | Pinned today | Owner after | Pinned after |
|---|---|---|---|---|---|
| **I12** | Raw mode is released on **all three exits** — normal (and every `?`), panic, and signal | `Stdin::drop` → `lifecycle::restore`; `lifecycle::install_panic_restore`; `signals::Shutdown` as a `select!` arm | `lifecycle.rs:76` `the_panic_hook_restores_and_still_reports`; `signals.rs` arm test; the terminal itself by the documented manual checks in `chat/mod.rs` | TUI presentation — unchanged | existing |

Unaffected by the controller migration, but listed because the state-machine
work touches the loop that owns all three, and because I10's weakness
interacts with the panic path.

---

## E. Route leases and secrets

| # | Invariant | Enforced today | Pinned today | Owner after | Pinned after |
|---|---|---|---|---|---|
| **I13** | A route lease is removed when its session closes **or** the controller disconnects; harness success precedes lease cleanup on close/delete | `controller.rs:596`, `:635` `session_closed`; `:362` `disconnected` | controller tests | unchanged — already the controller's | existing |
| **I14** | Credentials never reach `Debug`, logs, or responses | hand-written `Debug`: `ProviderEndpointPlan` (`controller.rs:87`), `HarnessEndpointPlan` (`harness.rs:194`), `AgentProcess` (`up.rs`) | `up.rs` `agent_process_debug_redacts_arguments_and_environment_values` | unchanged | existing |

---

## The coverage problem behind all of it

`chat` — the surface this migration changes most — has **no integration test**.
Its only tests are the two pure-function permission tests at
`chat/session.rs:668` and `:690`. `tests/acp.rs` covers `prompt`
(`prompt_ndjson`, `prompt_headless_denies_permission_and_completes`), `serve`
(`serve_subprocess_e2e`, `conformance_forwarded_update_variants_survive_round_trip`),
and the **pipe** branch of chat (`chat_on_a_pipe_is_plain_text`) — never the
interactive one.

The migration changes chat's transport, credential mode, cost source, route
surface, and event loop. With no integration coverage, the first regression
signal is a person at a terminal.

**Consequence for sequencing:** migrate `prompt` first. It is headless, it has
byte-level NDJSON assertions, and it exercises I1, I2, I6, I8 and I11 — five of
the seven invariants that need new code. It is the parity oracle `chat` does
not have.

## Summary of what must be written

Four invariants need new implementations, and three of those need new tests:

- **I1** — explicit denial on every abandonment path *(no equivalent exists; the highest-risk item)*
- **I7** — denial at session teardown
- **I8** — turn timeout with cooperative cancel and grace
- **I4** — answered-exactly-once *(mechanism likely survives; needs an explicit test)*

Two are deliberately dropped and must be named in their commit: reattach replay,
and the turn queue flush.

Everything else moves unchanged or is already the controller's.
