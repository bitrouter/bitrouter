# Amendment 1 to the ACP controller spec: one stack, one client, and the cost model

Status: **proposed, for review** · Date: 2026-09-02 · Amends
[`ACP_CONTROLLER_SPEC.md`](ACP_CONTROLLER_SPEC.md)

Written as a separate document rather than as edits, because the spec it amends
is under active authorship. On approval, §§2–6 below are applied to the named
sections and this file is deleted.

Gated by [`ACP_SAFETY_INVARIANTS.md`](ACP_SAFETY_INVARIANTS.md), which
enumerates what the retired code guarantees. Nothing here authorizes deleting a
guarantee; it authorizes moving one.

---

## 1. What prompted this

The spec unified the **server**. It left the **client** unaddressed, and it left
one product decision — what `UsageUpdate.cost` means — to fall out of a refactor.

Three facts made that untenable:

- **Two ACP stacks ship today.** `acp serve` runs the controller (#849, #852);
  `chat` and `prompt` still run `engine::Session`. Client capabilities are
  declared by the client, so the same harness behaves differently depending on
  which command launched it. §16.2 anticipates the migration but not the
  fragmentation it would cause.
- **`UsageUpdate.used` and `.size` are required `u64`**, not optional. A
  synthesized cost-only update must invent context-window figures.
  `Journal::apply` is last-writer-wins, so the zeros the current dead code emits
  would erase the real occupancy. Cost cannot be sent on its own.
- **The scope marker at §15.2 has no writer.** `measured_usage_update` builds a
  correctly-scoped `UsageUpdate`, has three tests, and is unreachable — every
  caller passes `None`. The marker's requirement stands; its implementation was
  orphaned when #849 removed the only injection point.

---

## 2. Amends §16.2 — the engine is retired, and here is its disposition

Replace the section. `engine::Session` is not "retained until equivalent tests
pass"; it is retired part by part, each with a named successor.

| Part | Disposition |
|---|---|
| `UpstreamConnection` | Superseded by `up::AgentProcess`, already the controller's |
| `record_id` manager alias | Deleted — §5.2 removes it from the wire |
| `launch_deferred` / `open` | Deleted — the controller forwards the manager's `session/new` verbatim |
| ACP `Pipeline`, `executor`, pinned `AcpTarget` | Deleted — the executor ignores the target, and no consumer exists in `apps/bitrouter`. Its one live output, `TelemetryHook`'s `RequestCompleted`, is re-derived by the shared client from its own prompt round-trip |
| `settlement` seam | Deleted — no consumer |
| `turn::TurnController` | Deleted. The FIFO is not load-bearing (see invariant I9); `--turn-timeout` and cancel-with-grace move to the shared client (I8) |
| `permissions::PermissionRegistry` | Deleted. Reattach replay is outside the controller model (§9 of the P2 contract) |
| `translate` | **Kept in `bitrouter-sdk::acp`.** Pure over `schema::v1`, and it is the published wire contract of `acp prompt`'s NDJSON output |
| `down.rs` `serve` / `serve_with` / `ServeExtensions` | Deleted — no callers since #849. `ProviderSurface` is deleted with the picker migration (§5) |

The ACP hook traits (`acp::PreRequestHook` / `RouteHook` / `ExecutionHook`) are
`pub` in a published crate. Their removal is semver-breaking and lands as
`refactor(sdk)!` with a changelog entry. `AcpTransport`, `AcpTarget`,
`AcpAgentConfig`, and `RoutingTable` are **retained** — config and `agents.rs`
use them.

---

## 3. Amends §16.3 and §6.3 — one client, three presentations

§16.3 says `chat` "uses the same controller library as an in-process manager
client." That is right about the controller and silent about the client, and it
does not mention `prompt` at all. Three commands each growing their own ACP
`Client` would put three capability declarations in the binary — and decision 1
("the TUI supports what the controller supports") is only checkable if there is
one place the answer lives.

**There is exactly one BitRouter ACP client.** It lives in
`bitrouter-sdk::acp` behind the `acp` feature, because it needs the
`agent-client-protocol` runtime that `bitrouter-tui` deliberately does not have.
It is built by generalizing `up::UpstreamConnection` over a transport rather
than written fresh: that type already exposes broadcast raw updates, a
permission stream, `prompt_typed`, `cancel`, and `context_usage` — it is only
hard-wired to spawning a child.

Its surface: connect · initialize (reading `_meta["bitrouter.dev/controller"]`)
· `session/new` · `prompt` · `cancel` · raw `SessionUpdate` stream · permission
requests · `_bitrouter/route/*`.

Three consumers, which are **not** three renderings of one driver — they
consume different things and must stay distinct:

| Consumer | Consumes | Owns |
|---|---|---|
| `chat` (TUI) | **raw** `SessionUpdate` → `Journal` | raw mode, the event loop, modals |
| `chat_plain` (pipe) | raw → `Journal` → `plain` | per-turn batching |
| `acp prompt` (NDJSON) | `translate::SessionUpdateKind` | the repair re-prompt, the result contract |

`Journal` stays in `bitrouter-tui`; `ratatui` never enters the SDK; `prompt`
never depends on the renderer crate.

**Transport for `chat`: in-process duplex.** `Controller::run` is
transport-generic and `Send`. Recorded weakness: the child process-group kill
runs in a task, so the panic exit can end the process before it fires, and
`kill_on_drop` reaches `npx` but not the `node` it spawns. This is not a
regression against the engine — which already owns the child in-process — but it
is the one axis where a subprocess `acp serve` would be stronger. See invariant
I10.

`chat` moves to `RoutingCredentialMode::ControllerIssuedLocal`. Its traffic then
meters under `controller_instance_id` rather than `launch_id`, which is also
what makes `_bitrouter/route/*` available to it.

---

## 4. Amends §15.2 and §15.5 — the cost model, decided

This is the section the amendment exists for. Five decisions.

### C1 — The controller **decorates**; it never synthesizes

BitRouter's cost figure rides on the harness's own `UsageUpdate` as the
controller forwards it. `used` and `size` pass through **untouched** — they are
the harness's, always, because it owns the context window.

A harness that never emits `UsageUpdate` therefore shows no cost. That is
accepted: it is strictly better than fabricating an occupancy of zero, which is
what a synthesized update must do and what the current dead code does.

*Deferred, not rejected:* synthesizing a cost-only update from a last-seen
`used`/`size` retained in `LiveSessionIndex`. That requires adding a row to
§5.1's permitted-state list and is out of scope here.

**On the lag, which is inherent rather than a defect.** The implementation
caches the figure and refreshes off the forward path, so a usage update carries
what a previous refresh confirmed. That reads like a one-update delay
introduced by caching, and it was first recorded as something to follow up —
wrongly. A usage update arrives *mid-turn*, before the model request it belongs
to has settled in metering, so a synchronous query at that moment would return
the same previous-turns total the cache holds. The lag is when spend settles
versus when ACP reports occupancy; removing the cache would buy a stall on the
forward path and no fresher number.

The visible consequence stands and is correct: a session that ends after one
turn may show no figure, because BitRouter had not finished metering it. That
is C5 — absent rather than invented — reached by a different route.

### C2 — `UsageUpdate.cost` carries session-attributed cost, or nothing

The field is specified as *cumulative session cost*. A third-party ACP client
reads it per specification and has no reason to look for a BitRouter `_meta`
key. Placing daemon-wide spend there behind a private qualifier is
[#847](https://github.com/bitrouter/bitrouter/issues/847): our client is safe,
every other client is not.

`Scope::Wider` is therefore **removed from the ACP surface**. Daemon-wide spend
remains available through `bro status --requests` — which is already where
`bitrouter-tui`'s own charter says that question is answered.

This closes #847 by making the value match the field's definition, rather than
by omitting the field or by qualifying it privately.

### C3 — The `_meta` marker survives, and marks **provenance**, not scope

Once `cost` is always session-scoped, "scope" is redundant with the
specification. What is not redundant: **two parties can write that field.** The
harness may report its own provider relationship; BitRouter may report its
meter. They are different numbers, and a subscription harness's figure is not
BitRouter's spend.

So the marker says *whose number this is*:

- present, `"router"` — BitRouter metered this session's traffic and attributed
  it;
- absent — the figure is the harness's own, forwarded untouched, or there is no
  figure.

Key: `bitrouter.dev/cost`, matching the `bitrouter.dev/controller` namespace
#849 established. It supersedes `bitrouter/costScope`, whose writer is already
dead. This satisfies §15.2's requirement that cost carry an honest marker, with
the marker's job narrowed to something the specification does not already say.

### C4 — The attributed-cost query is new work, and it gates C1–C3

#852 added `controller_instance_id` and `acp_session_id` to metering, but the
only scoped query is `spend_summary_for_launch`, keyed by `launch_id`. A
`spend_summary_for_acp_session` does not exist. No part of C1–C3 may ship before
it does, with tests.

Per the P2 contract §8, attributed cost is a capability: the initialize response
`_meta` gains a `usage` block beside `routeControl`, gated the same way —
version, scope, and method presence all checked, and availability never inferred
from the BitRouter agent name.

### C5 — Unattributable traffic renders **nothing**

`--direct`; an explicit `--base-url` with no reviewed trusted binding; own-auth
harnesses; a subscription harness that ignores the injected credential. In every
case BitRouter did not meter that traffic, so `cost` is absent and the footer
shows no figure — not `$0.00`, and not a daemon-wide number wearing a label.

`ACP_TUI_SPEC.md:489-497` reaches these cases through `Scope::Wider`. Under this
amendment they are reached through **absence**, which is the same honesty by a
narrower route.

### C6 — `used` and `size` are rendered

Independent of everything above and blocked by nothing: context-window
occupancy arrives from the harness, needs no attribution, and is currently
stored in `Journal` and never drawn. It ships first.

---

## 5. Amends §11.5 — route control replaces the picker in one step

§11.5 allows a compatibility window in which manager-side `providers/*` is still
accepted. The controller does not implement that window — it rejects those
methods with `method_not_found` — and the P2 contract states the existing local
`providers/*` behaviour "must not be copied into the new client."

So there is no window. `SessionProviders`, its `ProviderSurface` implementation,
and the `launch_id`-scoped route override are deleted **in the same change** that
migrates `chat` to the shared client. `picker::Picker` survives: its render and
choose logic are unchanged, and its `available: bool` parameter becomes the
contract's three-condition `routeControl` gate.

Ordering consequence: route control cannot be a later step. `SessionProviders`
is the last consumer of both `ProviderSurface` and `launch_id`-scoped routing;
migrating `chat` while keeping it would mean porting a surface onto the new
client in order to delete it.

---

## 6. Amends §20 — delivery phases

Insert **Phase 2.5 — client consolidation**, between the shipped Phase 2 and
Phase 3's product surfaces:

1. ~~Pure deletion~~ — landed.
2. ~~`spend_summary_for_acp_session`~~ — landed, folded into the decoration
   commit because the query alone would have been dead code.
3. ~~Render `used`/`size`~~ — landed.
4. ~~The shared client, with `acp prompt` migrated first~~ — landed. Built by
   generalizing `up::UpstreamConnection` over a transport rather than written
   fresh, which shed 749 lines: `AgentProcess` already did the child spawn,
   reaper and death race the old driver duplicated.
5. ~~`chat_plain`~~ — landed.
6. ~~`chat` + in-process controller + `_bitrouter/route/*`~~ — landed as two
   commits: the client's route-control surface first (additive, no behaviour
   change), then the migration and the deletion of `SessionProviders`.
7. ~~Delete the engine and everything in §2's table~~ — landed. Wider than
   planned: `ConfigAcpRoutingTable` and `AcpTarget` had no consumers outside
   the dead pipeline either, and two `pub use` re-exports in `acp/mod.rs` that
   broke the house rule went with them.
8. ~~Cost decoration (C1–C3, C5)~~ — landed across steps 2 and 6 rather than
   as its own step: the server half rode with the query that feeds it, and the
   client half had to land with `chat`'s migration or every attributed figure
   would have rendered `cost unreported`. C4's capability and C6's occupancy
   line are in too; all six are pinned.
9. ~~The chat state machine ([`CHAT_MACHINE_SPEC.md`](CHAT_MACHINE_SPEC.md)),
   once, on the shared client~~ — landed. 2A and 2B went together rather than
   as two steps: a `Phase` without `Turn` has no reader for the turn, so
   landing 2A alone would have left the driver consulting the machine for two
   of five loops and bypassing it for the other three, which is the defect the
   spec exists to remove. The spec's §2.3 and §4.2 were re-derived against the
   shared client first (§7's last row); both conclusions survived, and its
   deletion of `Phase::Routing` was corrected — §5 keeps the picker, and step 6
   rebuilt it.

`prompt` leads because **`chat` has no integration test** — `tests/acp.rs`
covers `prompt`, `serve`, and the *pipe* branch of chat, never the interactive
one. The step that changes chat's transport, credential mode, cost source, route
surface, and event loop must not also be the step that proves the client works.

---

## 7. Consequential edits, required in the same changes

Per `CLAUDE.md`, skills move in lockstep. Each is falsified by this amendment:

| File | Claim |
|---|---|
| `skills/bitrouter/references/cli.md:143` | "`acp prompt` is the local one-shot session engine" |
| `cli.md:147`, `:148`, `:177`, `:324` | `--turn-timeout` as "a local-engine option" |
| `cli.md:163` | "`prompt` and `chat` retain the local `record_id`, OTel spans, FIFO queue, cooperative cancellation, and `--turn-timeout`" |
| `cli.md:165`, `:329` | NDJSON `session` line carries `record_id` |
| `cli.md:198`, `docs/CLI.md:344` | the `all callers` cost line — already false for `chat` |
| `cli.md:200`, `docs/CLI.md:346` | `/route` gated on an attributable launch id; re-reads `providers/list` |
| `skills/bitrouter/references/sessions.md:23-26`, `:140-144` | engine description; NDJSON `record_id` |
| `skills/bitrouter/SKILL.md:201` | "ACP controller vs one-shot engine" |
| `docs/CLI.md:304` | `.bitrouter/sessions/` — **already stale**, no such path exists |
| `docs/DEVELOPMENT.md:29-30`, `:46-63` | ACP as "down / engine / up"; the `costScope` key |
| `docs/ACP_TUI_SPEC.md:300-330`, `:489-497` | Decision 4 and the wider-scope requirement — superseded by C2/C5 |
| `docs/CHAT_MACHINE_SPEC.md` §2.3, §4.2 | written against `PendingPermission` and `Session::prompt` — **rewritten**, both conclusions intact |

---

## 8. What this does not change

- Harness session ownership (§5.1). No BitRouter session store, catalog, or
  transcript copy.
- Transparent identifiers (§5.2), honest capability negotiation (§5.3),
  namespace meaning (§5.4), authenticated routing scope (§5.5).
- The controller's no-TUI-dependency rule (§5.6) — the shared client is in the
  SDK precisely to keep it.
- The wire contract in the P2 TUI integration handoff. Every decision here is a
  BitRouter-internal disposition; no JSON-RPC method, error code, or `_meta`
  block visible to a third-party manager changes, except the addition of the
  `usage` capability in C4 and the `bitrouter.dev/cost` marker in C3.
