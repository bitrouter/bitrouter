# Spec: Detach/reattach robustness — session-scoped turn state, v2-shaped

Status: **Phase 1 & 2 implemented (substrate); TUI consumer + Phase 3 deferred**
· Author: Claude (with Spikel) · Date: 2026-07-20

**Implementation note (2026-07-20).** Phases 1 and 2 landed in
`crates/bitrouter-substrate` (default build, no `acp_v2`):
- **Phase 1** — session-scoped [`permissions::PermissionRegistry`]; `PendingPermission`
  made cloneable with a shared once-only resolver; the engine pump is the sole
  consumer of the upstream permission stream and `Session::permissions()` is
  re-subscribable. Fixes the take-once / detach-denies bug.
- **Phase 2** — [`turn_state::TurnState`] (`running`/`idle`/`requires_action`,
  v2-`state_update`-shaped); the engine turn worker emits `running`/`idle` with a
  per-session `turn_seq` and the permission pump emits `requires_action`; the
  transcript gained `TurnStart`/`TurnEnd` (superseding `Result`, legacy-tolerant);
  `down.rs` forwards live turn state and `replay_transcript` re-emits it — both as
  `_bitrouter/turn_state` extension notifications. 87 substrate tests pass, clippy
  + fmt clean, full app builds.
- **Deferred:** the TUI consumer that reads `_bitrouter/turn_state` (separate
  change, by request) and Phase 3 (`acp_v2` wire encoding).
- **Deviation from §7:** the `_bitrouter/turn_state` params also carry
  `sessionId` (the §7 table's shorthand omitted it), matching how ACP's
  `session/update` always carries the session id via its notification wrapper.

**Change note (2026-07-20).** Revised after an ACP deep-dive (website docs +
vendored crate source `agent-client-protocol{,-schema}` 1.2.0/1.4.0 + the
`agentclientprotocol/agent-client-protocol` repo). The five open questions from
draft v1 are now **resolved** (§11) and their conclusions are woven into §4/§7/§10.
Net: the research confirmed the architecture and closed the ambiguities; it did
not overturn anything.

**One-line:** Make a manager able to detach from a running substrate session and
reattach later without losing the turn's outcome, the "a turn is running" signal,
or a pending permission — by lifting turn-completion, turn-state, and permission
round-trips off the live connection and onto the durable, replayable session
stream. Shape the new events like ACP v2's `state_update` so the eventual v2 wire
adoption is an encoding swap, not a rewrite. No remote/network transport is in
scope.

## 0. Protocol facts this design leans on (verified)

These are load-bearing; sources in §11.

- **ACP defines no session-durability contract.** Persistence, restart survival,
  and re-surfacing an outstanding permission are entirely implementation-defined.
  De-facto precedent: session state lives in the running agent process; a client
  persists only the `sessionId` and reattaches.
- **There is no "is a turn running?" query and no state field in the resume /
  initialize response.** The only way a reconnecting client learns a turn is live
  is by replaying the update stream and seeing a `running` with no matching `idle`.
  (Upstream RFD #986 `session/status` proposes a liveness probe; unmerged.)
- **ACP has no turn id.** Identity is per-*message* via an opaque, agent-generated
  `messageId` with upsert/patch semantics; turns are delimited only by the
  `running → idle` bracket.
- **v2 is `schema-v2.0.0-alpha.1` (dated 2026-07-20), marked EXPERIMENTAL and
  "may change at any time."** `ProtocolVersion::LATEST` is still V1 and the
  reference crate's production path negotiates to V1. → We do **not** ship v2 on
  the wire; we ship v1 + a bitrouter-internal, v2-*shaped* model, and gate the v2
  wire encoding behind an off-by-default feature.
- **In v2, advertising the session capability makes `session/new`, `session/list`,
  `session/resume`, `session/close`, `session/prompt`, `session/cancel`,
  `session/update` all baseline.** `session/load` is *removed* (folded into
  `session/resume` + `replayFrom`). Relevant to Phase 3 scope (§10).

## 1. Motivation

`bitrouter acp serve --warm` already keeps the `Session` (and its upstream agent
child) alive across a manager disconnect and re-serves reattach connections on a
per-session unix socket (`apps/bitrouter/src/acp_cli.rs`). The **up-edge**
(bitrouter ↔ real agent) never disconnects during this; only the **down-edge**
(manager ↔ bitrouter) detaches. So reattach robustness is entirely a *down-edge +
session-state* problem, and the real agent's ACP version is irrelevant to it.

Three things break on reattach today. All three share one root cause: **ACP v1
binds turn-completion, turn-state, and permission round-trips to the live
connection, not to the durable session stream.** Everything that flows as a
*notification* (message/thought chunks, tool calls, diffs, usage) already goes
stream → transcript → replay and reattaches cleanly. Only the connection-scoped
request/response interactions fall on the floor.

### 1.1 Verified failure modes (current `main`)

1. **Turn completion is invisible on reattach.** The turn *outcome* is persisted
   — `transcript::TranscriptEvent::Result { stop_reason }`
   (`crates/bitrouter-substrate/src/transcript.rs:34`) — but the replay path drops
   it. `down::replay_transcript` (`crates/bitrouter-substrate/src/down.rs:488`)
   matches `result`/`error` lines with `_ => {}` and the comment *"result / error
   lines mark turn boundaries; nothing to replay."* The reason: **v1 has no
   `session/update` that represents "turn ended,"** so completion can be recorded
   but not re-expressed as a notification on reattach. A reattached manager sees
   every chunk and tool call but never learns the turn finished, or its
   `stopReason`.

2. **No "a turn is running" signal on reattach.** `session::SessionStatus::Running`
   exists internally but has no ACP notification form in v1. Reattaching mid-turn
   replays history and returns, with nothing saying "a turn is in flight."

3. **Permissions die after reattach.** `up::UpstreamConnection::subscribe_permissions`
   (`crates/bitrouter-substrate/src/up.rs:478`) is single-consumer take-once
   (`Mutex<Option<..>>.take()`). The first connection's forwarder took the
   receiver; a reattached forwarder gets `futures::stream::empty()`, so upstream
   permission prompts are silently never re-forwarded and the parked up-side
   handler defaults to **Deny** on resolver drop. Any permission in-flight *at*
   detach is likewise dropped → Deny. (Raw updates ride a re-subscribable
   broadcast and reattach fine — permissions is the odd one out, and this is a
   latent bug independent of ACP versioning.)

## 2. Goals / non-goals

Goals:

1. A manager that detaches (mid-turn or between turns) and reattaches sees, via
   replay: the full history **plus** a faithful current turn state — running,
   idle (with `stopReason`), or blocked-on-permission.
2. A turn that completes **while no manager is attached** is not lost: its
   outcome is a durable, replayable event.
3. A permission pending at detach is **not** auto-denied; it survives at session
   scope and is re-issued to the reattached manager, resolvable by that manager.
4. Permission forwarding works on the second and subsequent connections (fix the
   take-once bug).
5. The new turn-state events are modeled on ACP v2 `state_update` so turning on a
   future `acp_v2` wire encoding is a swap at the encoder, not a producer rewrite.
6. Zero dependency on the upstream agent speaking v2, and zero dependency on the
   still-`unstable_protocol_v2` schema for the default build.
7. **Bitrouter's own managers treat the turn-state stream as the authoritative
   turn lifecycle** (the v1 `session/prompt` response is consumed as a bare ack),
   so the eventual v2 flip is a no-op for the TUI/GUI (§4, §7). This is the one
   design *commitment* the reviewer must sign off on.

Non-goals:

- **Remote/network transport.** ACP defines no finalized remote transport in v1
  *or* v2 (stdio only; "Streamable HTTP" is a draft proposal). Out of scope here;
  tracked separately. Reattach in this spec is local (stdio / unix socket).
- **Third-party v2 clients reattaching to bitrouter sessions.** The default
  encoding is bitrouter-proprietary (`_`-namespaced, ignored by other clients).
  Standards-compliant reattach for arbitrary v2 clients arrives with the `acp_v2`
  wire encoding (§7), gated and deferred (Phase 3), which also owes the now-baseline
  `session/list` + `session/close` (§0, §10).
- **Cursor-based partial replay** (`replayFrom` at an arbitrary message cursor).
  v1 replays from start. Note the boundary: partial replay requires
  `messageId`-keyed upsert dedup, and v1 `messageId` is *optional* and set by the
  **upstream** agent, not us — so idempotent partial replay is not something
  bitrouter can guarantee on v1 without synthesizing stable ids (§11 Q5). Deferred.
- **Session survival across a substrate *process* restart.** The warm session is
  in-memory and the upstream child dies with the process; durable-across-restart
  sessions are a separate, larger feature (§11 Q3).
- **Rewriting the internal model onto v2 types.** We keep v1 schema types on both
  wires; only the *new* turn-state events are v2-shaped.

## 3. Design overview

Introduce one internal, version-agnostic stream that both the live down-forwarder
and the transcript writer consume:

```rust
/// The single ordered event stream for a session, feeding both the live
/// manager forwarder and the durable transcript.
enum SessionEvent {
    /// A raw upstream `session/update`, forwarded verbatim (today's behavior).
    Update(SessionUpdate),
    /// A synthetic, v2-shaped turn-state transition produced by the engine.
    TurnState(TurnState),
}

/// Mirrors ACP v2 `StateUpdate` field-for-field so the `acp_v2` encoder is a
/// direct map. `turn_seq` is a per-session monotonic turn counter minted by the
/// engine when a turn starts; it is an INTERNAL correlation index (which
/// running/requires_action/idle belong to one turn), NOT a wire identity — ACP
/// has no turn id (§11 Q5). `stop_reason` is Option to match v2 `IdleStateUpdate`
/// (idle need not carry a stop reason); bitrouter always populates it at turn end.
enum TurnState {
    Running        { turn_seq: u64 },
    Idle           { turn_seq: u64, stop_reason: Option<StopReason> },
    RequiresAction { turn_seq: u64, request_id: String },
}
```

**Producer (engine):** the engine emits `TurnState::Running` when it begins
driving a turn upstream, `TurnState::Idle { stop_reason }` when the turn resolves
(success or synthesized cancel), and `RequiresAction`/`Running` around a pending
permission. These go onto the same non-lossy transcript feed and the live
broadcast that upstream updates already use.

**Persistence (transcript):** turn-state transitions are recorded as durable
transcript lines (§6), so a turn that ends while detached is on disk. Current
turn state is *reconstructable from the replayed sequence* — if the tail is a
`Running` with no matching `Idle`, the session is mid-turn (matches how ACP v2
itself expects a reconnecting client to infer liveness — §0).

**Encoding (down-edge), behind a seam:** the down-forwarder and `replay_transcript`
map each `SessionEvent` to the wire:

- `Update(u)` → `session/update` with `u`, verbatim (unchanged).
- `TurnState(..)` → **default:** a bitrouter-custom `_bitrouter/turn_state`
  JSON-RPC notification. Custom `_`-prefixed notifications are sanctioned by ACP
  extensibility, and conformant clients **SHOULD ignore unknown notifications**,
  so third-party v1 clients degrade safely. **`acp_v2` on:** a `session/update`
  whose payload is the corresponding v2 `state_update` variant.

The same producer serves both encodings; only the encoder branches.

**Permissions (session-scoped):** replace the take-once permission handoff with a
session-owned permission registry that (a) survives detach, (b) re-issues
outstanding permissions on reattach, and (c) is consumable by each successive
connection (§5).

## 4. Turn-state production (engine) & the authoritative-lifecycle contract

`engine::Session` gains a per-session monotonic `turn_seq: AtomicU64` and a
`SessionEvent` fan-out (broadcast for live consumers + the existing unbounded
transcript feed). Emission points, all inside the turn runner that wraps
`up::UpstreamConnection::prompt_typed`:

| Moment | Event | Notes |
|---|---|---|
| Turn dequeued & driving upstream begins | `Running { turn_seq }` | `turn_seq = fetch_add(1)`. Also flips `SessionStatus::Running`. |
| Upstream `request_permission` arrives (turn parked) | `RequiresAction { turn_seq, request_id }` | Paired with the permission registry (§5). |
| Permission resolved, turn resumes | `Running { turn_seq }` | Same `turn_seq`. |
| Turn resolves (`PromptResponse`) | `Idle { turn_seq, stop_reason }` | Emitted **before** the down-edge prompt response is returned. |
| Turn cancelled (`session/cancel`) | `Idle { turn_seq, stop_reason: Cancelled }` | Consistent with the synthesized cancel response in `turn.rs`. |
| Turn fails (transport/pipeline error) | `Idle { turn_seq, stop_reason: <mapped> }` + `Error` transcript line | Keeps a reattached manager from hanging on a dead turn. |

**Authoritative-lifecycle contract (Goal 7).** `Idle` is emitted **both live and
on replay**. Bitrouter's own managers (TUI, GUI, `bitrouter acp attach`) key the
turn lifecycle off the `turn_state` stream and treat the v1 `session/prompt`
response purely as an **ack** (turn accepted). Consequences:

- One completion code path in our managers, identical live and on reattach.
- A live turn emits one extra small `_bitrouter/turn_state{idle}` notification;
  third-party v1 clients ignore it and use the `PromptResponse` as usual.
- **The v2 flip is free for us:** under `acp_v2` the `session/prompt` response
  becomes a literal empty ack and `Idle` is the sole completion signal — our
  managers are already written that way, so nothing changes UI-side.

The existing `session/prompt` down-handler still *returns* a v1 `PromptResponse`
in the default build (third-party v1 clients require it); under `acp_v2` it acks
(§7).

## 5. Session-scoped permissions

Replace the take-once model with a session-owned registry:

- The engine spawns **one** permission pump that takes `subscribe_permissions()`
  exactly once (up-side stays single-producer) and inserts each `PendingPermission`
  into `pending: Mutex<IndexMap<request_id, PendingPermission>>`, then signals a
  `tokio::sync::watch`/`Notify` "pending set changed."
- A `PendingPermission` **is not dropped on manager detach.** It lives in the
  registry until it is resolved by *some* manager or the whole session tears down.
  This removes the accidental Deny-on-detach.
- `down::spawn_permission_forwarder` becomes reattach-safe: on (re)attach it first
  re-issues every entry currently in `pending` (as `session/request_permission`),
  then streams new ones via the change signal. Resolving removes from the registry
  and answers the up-side resolver with the exact `optionId` (unchanged
  `sanitize_selection` discipline).
- Emit `RequiresAction { request_id }` when an entry is inserted and `Running`
  when the set empties mid-turn, so the turn-state stream reflects blocked/unblocked.

`up::subscribe_permissions` keeps its single-consumer contract (the engine is the
sole consumer); the re-subscribable surface moves up to the `Session`.

## 6. Transcript / durable format (delta)

Current `transcript::TranscriptEvent`
(`crates/bitrouter-substrate/src/transcript.rs:27`):

```rust
enum TranscriptEvent { Prompt{blocks}, Update{update}, Result{stop_reason}, Error{message} }
```

Change to:

```rust
enum TranscriptEvent {
    Prompt    { blocks: Vec<ContentBlock> },                        // unchanged
    Update    { update: Box<SessionUpdate> },                       // unchanged
    TurnStart { turn_seq: u64 },                                    // new  (Running)
    TurnEnd   { turn_seq: u64, stop_reason: Option<StopReason> },   // was `Result`
    Error     { message: String, turn_seq: Option<u64> },          // + turn_seq
}
```

- `TurnEnd` supersedes `Result`. **Back-compat:** the reader accepts a legacy
  `result` line (no `turn_seq`) and treats it as a `TurnEnd` with a synthesized
  seq, so existing transcripts still replay (mirrors the
  `legacy_record_without_new_fields_still_parses` pattern in `record.rs`).
- `RequiresAction` is **not** persisted — it is live-only, reconstructed on
  reattach from the in-memory permission registry (§5). Rationale: ACP defines no
  durability contract (§0); persisting a permission's `tool_call` payload
  duplicates the registry and risks replaying a stale prompt for a permission
  resolved out-of-band.
- The writer stays single-writer/monotonic-`seq` and flush-per-event (durable
  record contract), unchanged.

## 7. Wire encoding seam (`acp_v2`)

A new `acp_v2` cargo feature on `bitrouter-substrate` forwards
`agent-client-protocol/unstable_protocol_v2` (+ schema). Default **off**. It gates
only the **encoding** of `TurnState`, in `down.rs` (live forwarder) and
`replay_transcript`:

| | default (v1) | `acp_v2` on |
|---|---|---|
| `Update(u)` | `session/update { u }` | `session/update { u }` (shim handles field remap) |
| `TurnState::Running` | `_bitrouter/turn_state {state:"running", turnSeq}` | `session/update { state_update: running }` |
| `TurnState::Idle` | `_bitrouter/turn_state {state:"idle", turnSeq, stopReason?}` | `session/update { state_update: idle { stopReason? } }` |
| `TurnState::RequiresAction` | `_bitrouter/turn_state {state:"requires_action", turnSeq, requestId}` | `session/update { state_update: requires_action }` |
| `session/prompt` response | v1 `PromptResponse { stopReason }` | ack `{}` (completion via the idle update) |
| reattach method | `session/load` | `session/resume { replayFrom: start }` + baseline `session/list`/`session/close` |

Notes:

- The `_bitrouter/turn_state` params struct mirrors `TurnState` 1:1. Bitrouter's
  own managers read it (§4 contract); third-party v1 clients ignore the unknown
  notification (correct JSON-RPC behavior).
- `stopReason` is optional on idle in both encodings (`Option<StopReason>` /
  v2 `IdleStateUpdate.stop_reason`); bitrouter populates it at turn end but the
  type tolerates its absence (e.g. a future v2 upstream reporting a non-work idle).
- Keeping `session/load` on the v1 wire is *correct*, not a stopgap: v2
  `resume{replayFrom:start}` maps back to v1 `session/load` in the reference
  conversion, and the *stabilized* v1 `session/resume` is a no-replay reconnect —
  the wrong semantics for our replay. The `acp_v2` column therefore also owes the
  now-baseline `session/list` + `session/close` handlers (today answered
  method-not-found).

This keeps the default build on **stable** schemas while making the v2 flip a
localized encoder change plus the three v2-baseline session methods.

## 8. Reattach flow (end-to-end, default build)

1. Manager A drives a turn. Engine emits `TurnStart`, streams updates, (maybe)
   `RequiresAction`, then `TurnEnd`. Each is persisted and forwarded live.
   Manager A keys completion off the `turn_state{idle}` notification (§4), not the
   `PromptResponse`.
2. Manager A detaches mid-turn. `EofSignaling` fires; `serve_on` returns; the
   warm loop keeps the `Arc<Session>` (upstream keeps running). Pending
   permissions stay in the registry (§5).
3. The turn finishes while detached. Engine emits `TurnEnd { stop_reason }` →
   persisted to the transcript (durable); the live broadcast has no subscriber.
4. Manager B reattaches on the socket. Sends `initialize` then `session/load`.
   `replay_transcript` now emits `Prompt` → `user_message_chunk`, `Update` →
   verbatim, `TurnStart`/`TurnEnd` → `_bitrouter/turn_state`. Manager B
   reconstructs: turn N ended `end_turn`.
5. The permission forwarder re-issues any still-pending permission to Manager B,
   which can now answer it; the exact `optionId` reaches the upstream.
6. If a turn were still running at step 4, the replayed tail is a `TurnStart` with
   no `TurnEnd` → Manager B shows "running," and subsequent live events flow.

## 9. Testing

Unit / integration (the `up.rs`/`down.rs` bash-ACP-stub pattern already in the
crate):

1. **Completion survives detach** — stub completes a turn after the manager drops;
   reattach + `session/load` replays a `TurnEnd` with the right `stopReason`.
2. **Mid-turn reattach shows running** — reattach while the stub is mid-turn;
   replay tail is `TurnStart` with no `TurnEnd`.
3. **Permission survives detach** — stub issues `request_permission`; manager
   detaches without answering; reattached manager receives the re-issued request
   and its answer reaches the stub (asserts *not* auto-Denied).
4. **Permissions work on the 2nd connection** — regression for the take-once bug:
   two sequential connections, permission delivered on the second.
5. **Legacy transcript replays** — a transcript with an old `result` line replays
   as `TurnEnd`.
6. **Authoritative-lifecycle contract** — a manager keying off `turn_state{idle}`
   sees exactly one completion per `turn_seq` whether the turn ended live or while
   detached; the `PromptResponse` is treated as an ack (not double-counted).
7. **`acp_v2` encoding** (feature build) — `TurnState` serializes as a v2
   `state_update`, the prompt response is an ack, and `session/list`/`session/close`
   are answered (not method-not-found).

House checks: `cargo nextest run --all-features`, `cargo clippy --all-features`,
`cargo fmt -- --check`. No `#[allow]`, no `unwrap/expect/panic`, no dead/unused
types (the `acp_v2` encoder path is feature-gated, not dead).

## 10. Rollout

- **Phase 1 — DONE (bug fix, no format change):** session-scoped permission
  registry + reattach re-issue (§5). Fixes failure mode 3 alone; shipped
  independently. (Implemented as `TurnState` type in §3 via two broadcasts +
  transcript rather than a literal `SessionEvent` enum — same event model.)
- **Phase 2 — DONE (substrate):** [`turn_state::TurnState`], engine emission (§4),
  the authoritative-lifecycle contract (§4, Goal 7), transcript delta (§6),
  `replay_transcript` + live forwarder emit `_bitrouter/turn_state` (§7 default
  column). Fixes failure modes 1 & 2.
  - **Pending (separate change, by request):** update the TUI to consume
    `_bitrouter/turn_state` (key completion off it; render running/blocked). The
    GUI is a separate repo, out of scope.
- **Phase 3 (v2 encoding, deferred):** `acp_v2` feature — encode `TurnState` as
  v2 `state_update`, ack the prompt, switch reattach to `session/resume{replayFrom}`,
  and add the v2-baseline `session/list` + `session/close` handlers. Ships off by
  default until `unstable_protocol_v2` stabilizes upstream (it is `alpha.1`, §0).

Skill/docs lockstep: no CLI flag, port, or env changes in Phase 1–2, so
`skills/bitrouter/` is unaffected. If Phase 3 adds an `--acp-v2`-style flag or a
config toggle, update the skill in the same change (CLAUDE.md rule).

## 11. Resolved decisions (were open questions)

Each conclusion cites the ACP research. `schema-1.4.0` =
`agent-client-protocol-schema-1.4.0/src`; `acp-1.2.0` =
`agent-client-protocol-1.2.0/src`.

**Q1 — custom `_bitrouter/turn_state` notification, not `_meta` on a chunk.**
Custom `_`-prefixed **notifications** are sanctioned (`ExtNotification`,
`schema-1.4.0/v2/ext.rs`), and conformant clients **SHOULD ignore unknown
notifications** (unknown *requests* must error `-32601`) — safe degradation for
third-party v1 clients. Extension data may **not** be a root field on a spec type;
it must live in `_meta`, and a streamed chunk's `_meta` is *chunk-scoped*
(`schema-1.4.0/v2/client.rs`). A turn that completes while detached has no final
chunk to host `_meta`. Rule of thumb from the docs: `_meta` for metadata about a
message you're already sending; a custom `_`-notification for an independent event
— turn completion is the latter. **Confidence: high.**

**Q2 — keep the live v1 `PromptResponse`, but our managers treat `turn_state` as
authoritative (Goal 7 / §4).** v2's `session/prompt` response is an empty ack;
completion is a separate `state_update{idle}`, and the crate **hard-errors**
converting one to the other (`schema-1.4.0/v2/conversion.rs:1052` — *"v2
SessionUpdate variant `state_update` cannot be represented in v1 because v1
reports completion in the session/prompt response"*). Rather than dedup
"response vs idle," we adopt the v2 model now: managers key off `turn_state{idle}`
(live and replayed) and read the `PromptResponse` as an ack. Third-party v1
clients still get their `PromptResponse`. Makes the v2 flip a UI no-op.
`idle.stopReason` is `Option` in v2 (`schema-1.4.0/v2/client.rs:524`). **This is
the one commitment to confirm on review.** **Confidence: high.**

**Q3 — `RequiresAction` is live-only; do not persist.** ACP defines no
durability/persistence/restart contract (absent from the v2 session-setup &
migration docs; the reference precedent keeps session state in the running agent
process and persists only the `sessionId`). The realistic socket-drop case is
handled by the in-memory registry (§5); full-restart durability is a larger,
separate feature (§2 non-goal). **Confidence: high.**

**Q4 — keep `session/load` on the v1 wire; add `session/resume{replayFrom}` +
baseline `session/list`/`session/close` in Phase 3.** v2 *removes* `session/load`
(PR #1584) and folds it into `session/resume` with `replayFrom`; the reference
conversion maps v2 `resume{replayFrom:start}` **back to v1 `session/load`**
(`schema-1.4.0/v2/conversion.rs:~3406/3419`). The *stabilized* v1 `session/resume`
is a **no-replay**, capability-gated reconnect — wrong semantics for our replay.
So `session/load` is exactly right on v1 today; no early alias. New scope: v2
makes `session/list`/`session/close` baseline (`schema-1.4.0/v2/agent.rs:~4190`),
so Phase 3 must add them. `ResumeSessionRequest` =
`{sessionId, cwd, additionalDirectories, mcpServers, replayFrom?, _meta}`;
`ReplayFrom` = `Start | Other` (only `start` exists today; cursors are future).
**Confidence: high.**

**Q5 — no turn id in ACP; `turn_seq` stays an internal correlation index.**
Identity is per-*message* via opaque agent-generated `messageId` with upsert/patch
dedup (`schema-1.4.0/v2/client.rs` message upserts; v2 makes `messageId` required,
v1 optional). Our `turn_seq` correlates `running`/`requires_action`/`idle` for one
turn; it is not a wire identity and never dedups *messages* — reattach does a full
replay into a *fresh* connection, so there is no partial-overlap to dedup. The
`messageId`-keyed dedup only matters for future **cursor-based partial replay**,
which on v1 would require synthesizing stable ids (upstream `messageId` is optional
and not ours) — see §2 non-goal. **Confidence: high.**

### 11.2 Known limitation (implementation)

- **`idle`/`TurnEnd` can be emitted before a turn's trailing `session/update`(s)**
  (found in review). Updates and turn-state reach the transcript (and the live
  wire) via **two unsynchronized paths** — updates through the `take_transcript_feed`
  pump, `TurnEnd` directly from the turn worker — so on a multi-thread runtime the
  worker can write `TurnEnd` before the pump drains a turn's last update (commonly
  a trailing `usage_update`). On replay a manager then sees `idle` before that
  update, weakening the §4 "idle = authoritative completion" contract. This is a
  race Phase 2 *exposed*: pre-Phase-2 the `result` line was never replayed, so its
  ordering was invisible. Practical impact is low (usually a trailing
  `usage_update` after `idle`; content chunks almost always precede it), and there
  is no live consumer yet (TUI deferred). **Proper fix** = §3's single ordered
  `SessionEvent` stream: make the update pump the sole writer and route turn-state
  through it with a biased drain-before-`TurnEnd`, so one orderer serves both the
  transcript and the live forwarder. Deferred to the TUI-consumer work (the first
  live consumer), where the single-stream design lands; tracked here.


- **No `running`-again after a permission resolves.** §4 lists a `running`
  transition when a parked turn resumes. As built, the sequence for a turn with a
  permission is `running → requires_action → idle` — the brief `running` between
  the manager's answer and turn end is **not** emitted. Reason: resolution happens
  on a `PendingPermission` clone (down-forwarder / registry) that the engine's
  turn worker and permission pump don't observe, so emitting it cleanly needs a
  resolution hook. It is cosmetic — a manager shows "blocked" until "done", and
  completion (`idle`) is unaffected. Deferred; revisit with the TUI consumer.

### 11.1 Still genuinely open (for review)

- **Q2 sign-off:** confirm bitrouter managers should treat `turn_state` as the
  authoritative turn lifecycle (the load-bearing commitment above). Everything
  else follows from it.
- **Track upstream:** RFD #986 `session/status` (a side-effect-free liveness
  probe) is the community's proposed answer to the "is a turn running on
  reconnect?" gap. Unmerged; returns live/not-found only (not running/idle). If it
  lands, it becomes a cleaner primitive than our replay-tail reconstruction (§3) —
  revisit then. No action now.
- **Replay-includes-idle is unverified upstream** (the docs don't say whether a
  v2 agent replays terminal `state_update{idle}` on resume). Irrelevant to us
  because *we* control `replay_transcript` and will replay `TurnEnd`; noted only
  for the future v2 up-edge.
