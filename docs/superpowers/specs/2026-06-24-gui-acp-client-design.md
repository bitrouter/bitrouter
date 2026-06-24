# BitRouter GUI — Real ACP Client (Minimal Single Session) — Design Spec

- **Date:** 2026-06-24
- **Status:** Draft — pending review
- **Topic:** Integrate `bitrouter-gui` with the real upstream BitRouter by making
  the GUI a real **ACP client**. Replace the in-process `MockFeed` with an
  `AcpFeed` that drives one live ACP session through `bitrouter agent-proxy`,
  rendering the streaming transcript and answering permission prompts.
- **Related:** prior *BitRouter TUI — Multi-Agent Manager Frontend* design
  (the Substrate/Manager/Router model). This spec builds the **client seam only**
  and defers the orchestrator/cost layers.

---

## 1. Summary

`bitrouter-gui` is already structured for this: `bitrouter-gui-core` is a headless
core (`protocol` = `Command`/`Event`, `state` = pure `reduce`, `feed` = the `Feed`
trait), and `AppModel` simply pumps `Feed` events through `reduce` and calls
`cx.notify()`. The `Feed` trait is the explicit integration seam; today only
`MockFeed` implements it.

This milestone implements a second `Feed` — **`AcpFeed`** — that speaks the Agent
Client Protocol (ACP) as a *client* to a single upstream agent reached via
`bitrouter agent-proxy <id>`. BitRouter sits in the middle (the proxy bridge), so
every LLM call the agent makes still flows through BitRouter's router — the
foundation a future cost HUD builds on, even though cost is **out of scope here**.

The result: a real coding agent's streaming output (messages, thoughts, tool calls
with diffs) renders live in the GUI, and the GUI answers the agent's permission
requests — all behind the existing `Feed`/`state`/views architecture, which stays
otherwise untouched.

## 2. Goals

- Replace `MockFeed` with a real `AcpFeed` for **one** ACP session.
- Drive the session over ACP: `initialize` → `session/new` → `session/prompt`,
  consuming streamed `session/update` notifications and answering
  `session/request_permission`.
- Render **live, token-by-token streaming** of agent messages and thoughts, plus
  tool calls that update in place (status + diff).
- Keep `bitrouter-gui-core` pure and the views unchanged except for one small,
  well-bounded streaming change to `protocol`/`state`.
- Reuse the published ACP Rust SDK rather than hand-rolling JSON-RPC.

## 3. Non-Goals (explicitly deferred)

- **Multi-agent fan-out / broadcast / cross-session selection.** `connect()`
  creates exactly one session; `SpawnAgent` for *additional* sessions is deferred.
- **Worktree isolation.**
- **Cost / routing / failover HUD.** `RequestCompleted` / `RoutingDecided` events
  are not emitted; the HUD and dashboard render zeros/"n/a" for now. Flagged, not
  fixed.
- **`terminal/*` and `fs/write` client capabilities.** Advertise the minimal set
  needed for transcript + permissions; deny/omit the rest.
- **Upstream BitRouter SDK migration.** `bitrouter-sdk/src/acp/` stays hand-rolled
  (see §9). The GUI adopting the SDK does not require upstream to change — ACP is
  the wire contract.

## 4. Decisions (resolved during brainstorming)

| Decision | Choice | Rationale |
| --- | --- | --- |
| Scope | Minimal single real ACP session, replacing `MockFeed` | Proves the client end-to-end with least surface |
| ACP library | `agentclientprotocol/rust-sdk` family: `agent-client-protocol` (typed `Client` + schema) + `agent-client-protocol-tokio` (process spawning) | A real client must construct/interpret every payload; typed schema earns its keep |
| Upstream migration | Deferred | Asymmetric benefit (proxy routes opaque payloads); v1.0.0 is brand-new; separate blast radius. Revisit as its own design |
| `AcpFeed` location | `bitrouter-gui` app crate (next to `ai.rs`) | Keeps `bitrouter-gui-core` pure/threadless/deterministic |
| Async model | Dedicated thread running a tokio runtime, bridged to the `Feed`'s `futures` channels | Mirrors the existing `ai.rs` pattern; ACP stdio I/O needs a real runtime |
| Binary/agent discovery | `bitrouter` on `PATH` (override `BITROUTER_BIN`); single agent id from `BITROUTER_GUI_AGENT` (default e.g. `claude-code`) | Convention + minimal config; least magic, still real |
| `model` field | Display-only; BitRouter routes | Base ACP `session/new` has no model field |
| Streaming | Stream live; coalesce chunks in `reduce` | Streaming is a core reason to be an ACP client; the core change is small and bounded |

## 5. Architecture

```
   views ─dispatch─▶ AppModel ─Command─▶ [tokio thread: ACP Client] ─stdio─▶ bitrouter agent-proxy ─▶ agent
   views ◀─notify── AppModel ◀─Event─── reduce ◀── translate ◀─ session/update ◀──────────────────────────┘
```

- The boundary between the GUI and the rest of the world stays the `Feed` trait:
  `events: Stream<Item = Event> + Send` out, `commands: mpsc::Sender<Command>` in.
- `bitrouter agent-proxy <id>` is **standalone** — it loads `bitrouter.yaml`
  directly, so no `bitrouter serve` daemon needs to be running.

## 6. Component: core change (`bitrouter-gui-core`)

The single intrusive change, kept tight. Faithful ACP rendering needs a small
cluster of additions (not literally one variant):

**`protocol.rs`**
- Add `SessionUpdateKind::MessageChunk { text: String }`.
- Add `SessionUpdateKind::ThoughtChunk { text: String }`.
- Add an `id: String` field to `SessionUpdateKind::ToolCall` (and to the
  corresponding `TranscriptItem::ToolCall`).
- Add `SessionUpdateKind::ToolCallUpdate { id: String, status: ToolStatus, diff: Option<String> }`.
- Existing `Message { text }` / `Thought { text }` variants are retained (used by
  `MockFeed` and for any final/non-streamed text).

**`state.rs` (`reduce`)**
- `MessageChunk` / `ThoughtChunk`: append `text` to the trailing `TranscriptItem`
  if it is the same kind and belongs to the current turn; otherwise push a new
  item. A `ToolCall`, a different chunk kind, or a new turn boundary starts a
  fresh bubble.
- `ToolCallUpdate`: find the `TranscriptItem::ToolCall` with the matching `id` and
  update its `status` / `diff` in place; no-op if absent (mirrors the existing
  "unknown session is a no-op" discipline).

All other core modules and **all views remain unchanged.**

## 7. Component: `AcpFeed` (`bitrouter-gui/src/acp_feed.rs`)

New module in the app crate, implementing `Feed`. Owns a tokio runtime on a
dedicated thread (the `ai.rs` pattern). `connect()` returns a `FeedHandle` whose
`events` stream and `commands` sender bridge to that thread.

**Startup (on the tokio thread):**
1. Resolve the `bitrouter` binary (`BITROUTER_BIN` or `PATH`) and agent id
   (`BITROUTER_GUI_AGENT`, default `claude-code`).
2. Spawn `bitrouter agent-proxy <agent_id>` via `agent-client-protocol-tokio`.
3. Run ACP `initialize`, advertising minimal client capabilities (permission
   handling; `fs`/`terminal` deferred).
4. `session/new` → emit `Event::AgentSpawned { session }` with
   `render_mode = Acp`, `status = Running`.

**ACP `Client` trait handlers:**
- `session/update` → `translate(update)` → `Event::SessionUpdate { session, update }`.
- `session/request_permission` → emit `Event::PermissionRequested { request_id, summary, diff }`
  and **park** the ACP response on a `oneshot`, keyed by request id, in a
  correlation map. The handler awaits the `oneshot` before returning its ACP
  result.

**Inbound `Command` drain:**
- `SendPrompt { text }` → `session/prompt` with a text content block.
- `ResolvePending { request_id, outcome }` → resolve the parked `oneshot` with
  `outcome_to_acp_option(outcome)`.
- `StopAgent` → terminate the child; emit `Event::AgentExited`.
- `SpawnAgent` → out of scope this milestone (logged); `connect()` already owns the
  single session.

**Errors / lifecycle:**
- Binary-not-found, handshake failure, or child crash → emit an `AgentExited`
  (and/or set the session to `Errored`) plus a final transcript message describing
  the failure, so the UI shows a clear dead-session rather than hanging.

## 8. Component: pure translation seam (TDD anchor)

Keep the protocol-mapping logic out of the I/O thread so it is unit-testable
without spawning processes:

- `fn translate(update: acp::SessionUpdate) -> Option<SessionUpdateKind>` — maps
  `agent_message_chunk` → `MessageChunk`, `agent_thought_chunk` → `ThoughtChunk`,
  `tool_call` → `ToolCall { id, .. }`, `tool_call_update` → `ToolCallUpdate`.
  Unmapped/ignored update kinds return `None`.
- `fn outcome_to_acp_option(outcome: PermissionOutcome) -> <acp option id>` — maps
  `AllowOnce` / `AllowAlways` / `Deny` onto the option ids offered in the ACP
  `request_permission` payload.

The tokio thread stays thin: read wire → `translate` → push `Event`; pull
`Command` → build request → write wire.

## 9. Upstream note (deferred, not done here)

`bitrouter-sdk/src/acp/` hand-rolls JSON-RPC because the proxy only *routes*
opaque payloads. The rust-sdk ships first-class `Proxy`/`Conductor` roles that map
onto `bitrouter agent-proxy` and a future router, so migrating upstream is
attractive — but it is a separate, larger change with asymmetric benefit and a
day-old v1.0.0 dependency. Decision: revisit as its own design after (a) this GUI
client is proven end-to-end and (b) the SDK has stabilized. **Verify during this
build:** whether rust-sdk v1.0's `initialize` capability negotiation interoperates
cleanly with bitrouter's hand-rolled proxy (protocol-version skew). A clean
handshake is the precondition; a mismatch becomes the strongest argument for
aligning both on the SDK sooner.

## 10. Testing

- **Pure-fn unit tests:** `translate` over representative ACP `session/update`
  JSON; `outcome_to_acp_option` for each `PermissionOutcome`.
- **`reduce` tests:** chunk coalescing (consecutive `MessageChunk` append into one
  bubble; a `ThoughtChunk`/`ToolCall`/new turn starts a new bubble);
  `ToolCallUpdate` mutates the matching tool call by id; unknown id is a no-op.
- **Permission round-trip:** a parked request is resolved by a `ResolvePending`
  command and produces the mapped ACP option.
- **Optional integration:** a scripted fake ACP agent (a small stdio child, or the
  SDK's `-test` utilities) exercises `connect()` → `session/new` → `session/prompt`
  → streamed updates end-to-end with no real BitRouter.

## 11. Wiring (`main.rs`)

Select `AcpFeed` when configured (binary + agent resolvable), else fall back to
`MockFeed` for dev. `AppModel::new` is already feed-generic, so this is a single
construction-site change.

## 12. Risks / open questions

- **Protocol-version skew** (see §9) — the top empirical unknown; verify the
  handshake early.
- **Client capability surface** — if the chosen agent insists on `fs/*` or
  `terminal/*` capabilities we declined, some tool calls may fail. Mitigation:
  advertise read-only `fs` if needed; keep `terminal` deferred and observe.
- **Turn-boundary detection for coalescing** — `reduce` needs a reliable signal
  for "new assistant turn" so chunks don't merge across turns. Driven by the
  ordering of `session/update` kinds (a user `SendPrompt`, or a non-message update,
  closes the current message bubble).
