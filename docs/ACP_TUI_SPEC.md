# Spec: the ACP TUI — BitRouter owns the agent UX

Status: **proposed** · Author: Claude (with Spikel) · Date: 2026-08-13

Supersedes the `launch --tui` half of
[`OBSERVABILITY_TUI_SPEC.md`](OBSERVABILITY_TUI_SPEC.md) (#782) and retires
[`TUI_FIDELITY_MATRIX.md`](TUI_FIDELITY_MATRIX.md) with it. Leaves
`bitrouter status --watch` (#797) untouched. Builds on #613 (per-session ACP
substrate) and the ACP rightsizing of #745–#749.

**Context.** #749 ratified that BitRouter is a self-improving LLM router, not
an agent orchestrator, and #786 executed it — 19,955 lines of fleet/worktree/
review-queue machinery deleted. #803 then shipped two surfaces in one module:
`status --watch` (a live router view, no PTY) and `launch --tui` (a harness
hosted in a VT emulator with a one-row status bar pinned underneath).

This spec argues that `launch --tui` is the wrong half, deletes it, and
replaces it with a thin ratatui **ACP client** over BitRouter's existing
down-facing ACP agent endpoint. The load-bearing claim is not cost. It is that
PTY-hosting a harness's native TUI is **strategically incoherent with
harness-agnosticism**, and that ACP already has first-class vocabulary for
exactly what BitRouter sells.

---

## 1. Motivation

BitRouter's product is being harness-agnostic and model-agnostic. `launch
--tui` is the one surface in the codebase that is structurally the opposite:
it requires a per-harness fidelity matrix, a hardcoded four-entry allowlist
([`harness.rs:417`](../apps/bitrouter/src/harness.rs:417)), and a **manual**
verification pass — physical mouse drag, OSC-52 clipboard round-trip — per
harness per upstream release.

Lay the options out and the middle one is dominated on every axis:

| Surface | Who owns the UX | Harnesses reached | Per-harness cost |
|---|---|---|---|
| `launch` (inherited) | the harness | **9** | zero |
| `launch --tui` (PTY) | the harness, minus one row | **4** | fidelity matrix + manual QA |
| ACP TUI (this spec) | **BitRouter** | any ACP agent | zero |

`launch --tui` is worse than inherited `launch` on coverage *and* cost, and
worse than an ACP client on ownership *and* cost. It is the option that tries
to have both and gets neither.

**The justifying premise was already false at merge.** The emulator rests on
one sentence — *"Uniformity across all eight catalog harnesses is the product
requirement"* ([`host.rs:11`](../apps/bitrouter/src/tui/host.rs:11),
`OBSERVABILITY_TUI_SPEC.md:439`, decision 8 at :810). The same commit that
shipped the spec (`25f69b71`) cut `launch` to four. Nobody re-derived the
decision at n=4, and the stale text is still in source in two places.

### 1.1 The two surfaces are not competitors

They are a deliberate split, and each is coherent alone:

- Want Claude Code's own plan mode, slash commands, and native affordances?
  `bitrouter launch` — inherited, zero BitRouter terminal code, all nine
  harnesses.
- Want to swap **agent and model** freely with router-measured cost? The ACP
  TUI.

Nothing needs to sit between them.

## 2. Goals / non-goals

**Goals**

- BitRouter renders the agent session itself, from a normalized ACP stream —
  one renderer, N agents, no per-harness verification.
- BitRouter's routing surface (providers, models, effective route, spend) is
  expressed in **ACP's own vocabulary**, not smuggled through `_meta`, so any
  ACP client gets it.
- The TUI stays **single-session** by construction.
- Delete the emulator, its four terminal crates, and the fidelity matrix.

**Non-goals**

- Anything #786 removed: fleets, subagents, worktree isolation, review queues,
  autonomy tiers, multi-pane splits. **Single-session is the line.**
- Replacing `bitrouter status --watch`. That view is the router lens over the
  metering store and is untouched by this spec.
- Replacing `bitrouter launch`. Inherited launch stays, and regains the four
  harnesses `--tui` cost it.
- A reusable TUI widget crate. See §9 — deferred, with a stated trigger.
- ACP v2. See §10.

## 3. Decision 1 — delete `launch --tui`

Remove the `--tui` flag, `tui/host.rs`, `tui/pty.rs`, `tui/term.rs`,
`tui/lifecycle.rs`, `tui/conformance.rs`, `tui/fixtures/`,
`scripts/record-vt-fixture.sh`, and `TUI_FIDELITY_MATRIX.md`. Restore
`Harness::launch_supported()` to the full catalog.

Keep `spawn::prepare`/`exec_inherited` — the `Prepared` seam
([`spawn.rs:253`](../apps/bitrouter/src/spawn.rs:253)) is good design
independent of hosting; only `exec_hosted` goes.

Removed dependencies: `alacritty_terminal`, `portable-pty`, `termwiz`,
`wezterm-input-types` (`termwiz` alone resolves ~107 crates for three imported
symbols). `ratatui` and `crossterm` stay — `status --watch` and the new TUI
both need them.

**Supporting evidence, verified.** These are not the reason to delete it, but
they establish that the code was not load-bearing:

- The binary currently links **three** VT parsers: `vte` (under
  `alacritty_terminal`), `vtparse` (under `termwiz`), and a hand-written
  84-line `Osc52Scanner` ([`term.rs:560`](../apps/bitrouter/src/tui/term.rs:560))
  that re-scans every byte immediately before feeding the identical bytes to
  alacritty — which already decodes OSC 52 and dispatches it to the very
  listener `term.rs` installs.
- Three CLAUDE.md rule-4 violations: `TerminalBackend::alt_screen()` has zero
  production callers (all six hits are `#[cfg(test)]`); the `page: true` arm of
  `scroll()` is unreachable (sole caller hardcodes `false`); `TerminalBackend`
  is a 13-method object-safe trait with **one** implementor, kept explicitly as
  a hypothetical escape hatch.
- `Proxy::send_event` matches only `Event::PtyWrite`, silently discarding
  `ColorRequest` (OSC 4/10/11 — standard light/dark detection),
  `TextAreaSizeRequest`, and `ClipboardStore`, while the trait doc claims the
  opposite.
- The acceptance gate is unmet by its own criteria: all three checkboxes
  unchecked, both manual columns blank for all four harnesses, and the only
  automatic upstream-drift detector *"deliberately not built."*

**Honest counter, recorded.** The alt-screen argument survives the cut to four
— `opencode` is confirmed alt-screen, so if a status row pinned under a
harness were the requirement, an emulator really is the only mechanism. The
mechanism is sound; the **requirement** is what this spec rejects. §9.3 of the
prior spec never priced it against `status --watch` in a second pane, which
answers the same question with zero terminal-protocol surface.

## 4. The protocol surface BitRouter is not using

Enumerated from the pinned crates (`agent-client-protocol` 1.2,
`agent-client-protocol-schema` 1.4), not from prose.

**Agent-side (client → agent), v2:** `auth/login`, `auth/logout`,
`session/new`, `session/list`, `session/resume`, `session/close`,
`session/delete`, `session/fork`, `session/prompt`, `session/cancel`,
`session/set_config_option`, `providers/list`, `providers/set`,
`providers/disable`, `tools/list`, `mcp/message`, `notifications/progress`.

**Client-side (agent → client), v2:** `session/update`,
`session/request_permission`, `elicitation/create`, `elicitation/complete`,
`mcp/connect`, `mcp/disconnect`, `mcp/message`, `tools/list`,
`notifications/progress`.

**`SessionUpdate` variants (v2), 15:** `UserMessageChunk`, `UserMessage`,
`AgentMessageChunk`, `AgentMessage`, `AgentThoughtChunk`, `AgentThought`,
`StateUpdate`, `ToolCallContentChunk`, `ToolCallUpdate`, `PlanUpdate`,
`PlanRemoved`, `AvailableCommandsUpdate`, `ConfigOptionUpdate`,
`SessionInfoUpdate`, `UsageUpdate`.

v1 additionally places `fs/read_text_file`, `fs/write_text_file`, and
`terminal/{create,output,kill,release,wait_for_exit}` on the **client**; v2
deletes all seven. The client role shrinks in v2 while the agent role does
not — an asymmetry that favors this design.

**What this corrects.** Slash commands (`AvailableCommandsUpdate`), modes
(`ConfigOptionUpdate` + `session/set_config_option`), and plans (`PlanUpdate`)
are all in the protocol. What lacks them is BitRouter's
[`down.rs`](../crates/bitrouter-sdk/src/acp/down.rs), which answers
`method_not_found` to everything outside `initialize` / `session/new` /
`session/prompt` / `session/cancel`
([`down.rs:319`](../crates/bitrouter-sdk/src/acp/down.rs:319)), and
[`translate.rs`](../crates/bitrouter-sdk/src/acp/translate.rs), whose `_ =>
None` arm drops `Plan` and every variant above.

## 5. Decision 2 — the work is in `down.rs`, not the TUI

This is the central architectural claim of the spec.

BitRouter already terminates **both** ACP roles: `up.rs` speaks the Client role
to the spawned agent, `down.rs` re-exposes the session as a vanilla Agent over
stdio for *"a manager (GUI / CLI / orchestrating agent)"*
([`down.rs:4`](../crates/bitrouter-sdk/src/acp/down.rs:4)). A working
end-to-end ACP client already exists in the test suite — 
[`tests/acp.rs`](../apps/bitrouter/tests/acp.rs) drives `initialize` →
`session/new` → `session/prompt` → `session/update` over raw JSON-RPC.

Therefore: **invest in the agent endpoint, and every client gets better —
including clients BitRouter does not write.** Zed becomes a BitRouter frontend
without shipping a line of Zed's code. That is a strictly better third-party
story than publishing ratatui widgets.

Work items, in dependency order:

1. **Stop dropping updates.** `translate.rs` gains the variants it discards —
   at minimum `PlanUpdate`/`PlanRemoved`, `AvailableCommandsUpdate`,
   `ConfigOptionUpdate`, `StateUpdate`. Forward them to the manager verbatim
   via the existing raw stream (`subscribe_raw_updates`) so no reverse-mapping
   loss is introduced.
2. **Unmask what the session can honor.** `relayed_caps.load_session = false`
   ([`down.rs:201`](../crates/bitrouter-sdk/src/acp/down.rs:201)) and the
   `method_not_found` catch-all are correct *today* because the session cannot
   serve those methods. Each one unmasked is a separate, testable change —
   `session/list`, `session/resume`, `session/set_config_option`.
3. **`providers/*`** — §6.
4. **`UsageUpdate.cost` from metering** — §7.

The TUI is written last and stays thin.

## 6. Decision 3 — BitRouter's routing surface, in ACP's nouns

`providers/list` returns `ListProvidersResponse { providers: Vec<ProviderInfo> }`,
and `ProviderInfo` is documented as *"Configurable providers with current
routing info **suitable for UI display**"*:

```rust
pub struct ProviderInfo {
    pub provider_id: ProviderId,               // "main", "openai", …
    pub supported: Vec<LlmProtocol>,
    pub required: bool,                        // cannot be disabled
    pub current: Option<ProviderCurrentConfig>,// effective non-secret routing config
    pub meta: Option<Meta>,
}
```

ACP has a **native vocabulary for provider and model routing, designed to be
rendered by a client.** That is BitRouter's entire product, in the protocol's
own nouns.

`down.rs` implements:

- `providers/list` → BitRouter's routing catalog, with `current` reflecting the
  effective route.
- `providers/set` → switch provider/model for the live session.
- `providers/disable` → drop a provider from consideration, honoring `required`.

**Secrets never cross this wire.** `ProviderCurrentConfig` is specified as
*non-secret* routing config. Credential management stays on the CLI, exactly as
`OBSERVABILITY_TUI_SPEC` §8.4 required of the previous design — the TUI never
handles a secret and never writes a config file.

**This is the differentiated demo.** Swapping provider mid-session is the one
thing no harness and no other ACP agent can do.

### 6.1 Verified: the mechanism exists, and the risk was misidentified

Probed against a live daemon on an isolated config (two providers, `alpha` and
`beta`, both declaring `probe/m1`; zero upstream tokens spent):

| # | Behaviour | Result |
|---|---|---|
| 1 | Cascade + fallback — `probe/m1` → try `alpha`, fall back to `beta` | ✅ works |
| 2 | Provider-prefix direct route — `alpha:m1-alpha` resolves to `alpha` only | ✅ works |
| 3 | Deactivate `alpha` in config + `reload` → live daemon reroutes to `beta`, no restart | ✅ works |
| 4 | `policy_table.tiers` rewriting the model server-side to `beta:m1-beta` | ✅ works (after restart) |
| 5 | `reload` picking up a changed `policy_table.tiers` | ❌ **silent no-op** |

**The `providers/set` substrate is `policy_table`, not the routing table.**
`PolicyTableRouter::apply(&self, prompt: &mut Prompt)` rewrites the request's
model before routing, and `PolicyTableConfig.tiers` maps a tier to a bare id
*or* an explicit `provider:model` direct route. Probe 4 confirmed it end to
end: with `tiers: { forced: "beta:m1-beta" }` and `default_tier: forced`, a
request for `probe/m1` was logged as `route="beta:m1-beta" provider="beta"`.

**The risk this spec originally flagged was the wrong one.** §16's first
question worried that an upstream adapter caching its model at `session/new`
would defeat a mid-session switch. It cannot: the rewrite happens **inside the
daemon, on the request the adapter already sent**. The adapter never learns the
route changed and needs no cooperation. `is_explicitly_routed` exempts a model
the caller pinned itself, which is the only case where the caller wins.

Two real gaps replace it:

1. **`reload` does not rebuild the policy table.** Probe 5: with the daemon
   live and serving `beta:m1-beta`, editing `tiers` to `alpha:m1-alpha` and
   running `bitrouter reload` returned `{"action":"reload","status":"reloaded"}`
   and kept routing to `beta`. Only `restart` picked up the change. Providers
   *are* hot-reloaded (probe 3), so this is specific to `policy_table` — a
   success-reporting silent no-op, and a bug on its own merits. `providers/set`
   cannot ship on a control path that requires a daemon restart.
2. **Routing overrides are global, not session-scoped.** `policy_table` is
   daemon-wide, keyed on an agent-trace fingerprint derived from request
   headers — not on an ACP session, a caller, or an API key. A `providers/set`
   implemented naively would change routing for **every** concurrent caller.
   The nearest existing session-shaped surface is the DB-backed
   `adequacy_pins` table (keyed on `fingerprint`), plus the session-scoped
   virtual-key minting that `SPAWN_SPEC` §10 deferred to v1.5. One of those has
   to carry the scope before `providers/set` is safe.

This moves `providers/set` from "unknown upstream risk" to "known BitRouter
work": fix the reload gap, then scope the override. Both are in BitRouter's own
code, which is a far better position than depending on adapter behaviour.

## 7. Decision 4 — spend comes from metering, over the wire

`UsageUpdate` carries `used`, `size`, and `cost: Option<Cost>` (*"cumulative
session cost"*). Today `translate.rs` forwards only what the **upstream**
reports, and upstream reporting is optional — which is exactly why the previous
TUI's status bar *"presented the daemon's spend as the session's"* and why a
native harness rendered an empty left zone
([`TUI_FIDELITY_MATRIX.md:103`](TUI_FIDELITY_MATRIX.md)).

That is not a protocol limitation. **BitRouter sits in the agent seat.** It
synthesizes `UsageUpdate` from its own metering store on every settled turn,
regardless of what upstream emits.

The data already exists and is already drained — per-turn `RequestCompleted`
telemetry is written to stderr and OTel
([`acp_cli.rs:855`](../apps/bitrouter/src/acp_cli.rs:855)) and never put on the
down-facing wire. This change routes it there.

**The general rule this establishes:** BitRouter *forwards* what the upstream
agent knows (messages, tool calls, plans, slash commands) and *adds* what only
the router knows (providers, effective route, measured cost). Neither the
upstream harness nor a generic ACP client can do the second half. That is the
defensible position, and it is the reason this is a router feature rather than
scope creep back toward the orchestrator.

## 8. Decision 5 — the TUI: ratatui, inline viewport

Built in `apps/bitrouter` (see §9 on why not a crate), rendering the normalized
stream.

**Inline, not alternate screen.** `ratatui::Viewport::Inline(n)`: prompt and
status render in a small inline region, agent output scrolls into the
terminal's **real scrollback**, and `Ctrl-C` leaves a readable transcript
behind. This is the design stance worth taking from `pi-tui`'s
`TuiMainScreen`/`TuiAltScreen` split — and it is the exact opposite of what a
PTY host does.

It also deletes a category of failure: no alt-screen enter/leave, no
`lifecycle.rs` restore dance, no panic hook whose job is recovering a terminal
BitRouter took custody of.

**Views (v1):** streaming message log (message/thought chunks), tool-call cards
with status and diff, permission modal (`session/request_permission`), a
provider/model picker over `providers/*`, and a status line carrying
router-measured cost.

**Client duties are small:** handle `session/update` and
`session/request_permission`; call `initialize`, `session/new`,
`session/prompt`, `session/cancel`, `providers/*`. The protocol half is
~300–500 lines; the rendering half is the bulk.

**Honest sizing.** A decent v1 is ~2,000–3,000 lines of ratatui — **more than
the emulator it does not replace.** This spec is not a net subtraction. It is a
strategy change that also permits a subtraction. Anyone reading §3 as "delete
2,287 lines and we're done" has misread it.

### 8.1 The command: a distinct verb sharing `spawn`'s options

**Not `bitrouter tui`.** Three reasons, the third decisive: it resurrects the
deleted orchestrator's name and expectations; it names the medium rather than
the function; and **`status --watch` is also a TUI**, so the name would be
permanently ambiguous inside BitRouter's own surface.

The real fork was verb-vs-flag:

- **`spawn --tui`** inherits spawn's whole option surface — routing, `--model`,
  `--direct`, `--check`, cwd, MCP servers — for free, and §3 frees the `--tui`
  spelling. But `spawn` is documented as a **headless sub-agent**, and
  `SPAWN_SPEC` §53–72 split interactive (`launch`) from headless (`spawn`)
  deliberately. An interactive mode on `spawn` re-muddies that on purpose-drawn
  line.
- **A distinct verb** keeps the split clean but duplicates spawn's options, with
  the drift risk that implies.

**Decision: a distinct verb that shares the option struct.**
[`RoutingOptions`](../apps/bitrouter/src/acp_cli.rs:165) is already a
standalone struct; clap's `#[command(flatten)]` gives spawn's flag reuse with
none of the semantic muddle. The verb split stays honest and there is one
definition of the routing flags.

The taxonomy this fills is a 2×2 — who owns the UX × interactive vs headless —
and the new command is the **interactive sibling of `spawn`**:

| | Harness owns UX | BitRouter owns UX |
|---|---|---|
| Interactive | `launch` | **this command** |
| Headless | — | `spawn -p` / `acp prompt` |

`bitrouter chat <agent>` is the working name: function-describing, no
collision, no inherited expectations. Its weakness is real — "chat" undersells
tool calls, diffs, and plans — so the name is not frozen here. Two constraints
that are: do not name it after the medium, and do not reuse `attach`, which was
removed with #745 and would resurrect a second dead name.

### 8.2 Diagnostics: three streams, one file

An inline viewport shares the terminal, so every other writer is a corruption
source. There are **three** streams, not one:

| Stream | Destination today | Collides? |
|---|---|---|
| Agent child stderr | inherited ([`up.rs:283`](../crates/bitrouter-sdk/src/acp/up.rs:283)) | **yes** |
| BitRouter's own tracing | stderr, for *every* command ([`main.rs:2489`](../apps/bitrouter/src/main.rs:2489)) | **yes** |
| TUI rendering | stdout | — |

`init_basic_tracing_subscriber` hardcodes `.with_writer(std::io::stderr)`
because stdout must stay a pure JSON result surface. That reasoning is correct
for every existing command and wrong for this one. There are already three
subscriber initializers (`basic`, `stderr`, `serve`); **this command needs a
fourth that writes to a file.**

Because BitRouter's own tracing needs a file regardless, the log-pane-vs-file
question dissolves — a file exists either way. So:

- **One per-session log file** under `~/.bitrouter/logs/`, carrying **both**
  the captured agent stderr and BitRouter's tracing, interleaved.
- **No permanent pane.** It would spend screen continuously for something read
  rarely.
- **On abnormal exit, surface the last N lines inline, automatically.** That is
  the only moment anyone wants this, and precisely the moment the user does not
  know to go looking. Discoverability comes from the error naming the path, not
  from a pane.
- **One key to tail it live**, for the rare interactive debugging case.

This also unblocks §11.2: a structured launch failure cannot be rendered if the
text explaining it was never captured. Switching `inherit` → a captured pipe is
the same change.

### 8.3 Scope: session-only, over the shared snapshot layer

**The rule:** the TUI shows **session-scoped** data only. Anything daemon-wide
belongs to `status --watch`. This is the *same* line as single-session in §2 —
one scope rule governing both, not two to remember — and it is checkable in
review.

The merge question largely dissolves because §7 puts router-measured cost on
the wire per session; the TUI never needs `status --watch` to answer "what is
this costing me."

**Do not build a second data layer — the scope seam already exists.**
[`snapshot.rs:54`](../apps/bitrouter/src/tui/snapshot.rs:54) defines
`Scope::{DaemonWide, Launch}`, with `spend_summary_for_launch` and
`recent_requests(…, Some(launch))`; `launch_id` is plumbed end to end from
`caller.launch_id()` to the metering column
([`recorder.rs:190`](../apps/bitrouter/src/metering/recorder.rs:190)).

The honesty problem is solved there too
([`snapshot.rs:117`](../apps/bitrouter/src/tui/snapshot.rs:117)):

> *Prefer launch scope, but only once it has something to show. Minting a token
> is a request, not a guarantee: the harness has to send it back, and some do
> not. Falling back — and saying so via `Scope` — keeps the bar honest either
> way, instead of showing an empty session forever or, worse, showing the
> daemon's numbers as if they were the session's.*

That is the fix for the fidelity matrix's defect #1, already written. So:
**share `snapshot.rs` and `Scope`; keep `watch.rs` and this TUI as separate
renderers over it.** That yields what merging would actually buy — one data
layer, consistent numbers, honest attribution — without merging the views.

**Carry the caveat.** For a subscription harness that ignores the injected
credential (Claude Code on Max), requests never reach the daemon, so session
cost is **structurally unknowable**. The TUI must render `Scope::DaemonWide`
visibly rather than silently. Inherit that behaviour; do not reinvent it.

## 9. Decision 6 — no crate yet, with a stated trigger

Build in `apps/bitrouter/src/tui/`. Do **not** extract `crates/bitrouter-tui`
in this work.

- The reuse argument is void: `bitrouter-gui` is deprecated and unscheduled, so
  the shared view model has exactly one consumer.
- [`DEVELOPMENT.md:26`](DEVELOPMENT.md:26) states the workspace rule — *"MCP is
  a standalone crate while both ACP and the API ride inside the SDK"* — because
  MCP **owns an external protocol**. A TUI owns none.
- The precedent is weaker than it looks: `bitrouter-mcp` is a non-optional
  dependency with no `[features]` section (it removes zero crates from the
  binary), was born a crate rather than extracted, and `apps/bitrouter` still
  declares an `rmcp` server dependency with **zero `rmcp::` references** in
  source — residue of the deleted #715 bridge.
- Being in-process is a **capability**, not a compromise: it is how the TUI
  reads the metering store directly (§7) rather than waiting on the wire.

**The one real pro-crate argument, and its trigger.**
[`tui/mod.rs:4`](../apps/bitrouter/src/tui/mod.rs:4) encodes the lesson from the
last TUI's death — *"if there is no CLI subcommand for it, there is no
keystroke for it"* — as **prose**. The previous TUI accreted verbs precisely
because a module can silently reach anywhere in the crate. A crate cannot.

So: extract on the **first accretion attempt**. §8.3 makes that trigger
concrete rather than a judgement call — extract on the first PR that:

- adds a session list, or a second pane, or a verb with no CLI equivalent; or
- **renders daemon-wide data in the TUI** (the §8.3 scope rule, violated).

At that point the boundary has proven it needs enforcing, and extraction
converts the rule that failed into a compiler check. Extracting earlier
formalizes a property that already holds and buys nothing.

## 10. Decision 7 — v1 semantics, `providers/*` by raw dispatch

**Target ACP v1. Do not adopt v2 in this work.**

- Both pinned crates gate v2 behind `unstable_protocol_v2`, and
  `agent-client-protocol-schema` 1.4 labels it *"v2 draft schema types"*. There
  are **zero** references to that feature anywhere in the workspace.
- Under v2 the non-conformant party is **BitRouter**, not the TUI: v2's agent
  baseline is `session/{new,list,resume,close,prompt,cancel,update}`, and v2
  inverts the turn lifecycle (`session/prompt` acks `{}`, completion arrives as
  a `state_update` notification) against `engine.rs`/`turn.rs`, which return
  `PromptResponse` with a `stop_reason`. That is an engine rewrite, not a TUI
  change.
- v2's only relevant gift is a smaller **client** role (`fs/*` and `terminal/*`
  deleted) — and `up.rs` already declines both, *"ACP v2 removes that client
  surface"* ([`up.rs:764`](../crates/bitrouter-sdk/src/acp/up.rs:764)).

**`providers/*` does not require v2.** It appears in both `v1/agent.rs` and
`v2/agent.rs`, gated only on `unstable_llm_providers`.

**But the runtime crate does not forward it.** `agent-client-protocol` 1.2
forwards `unstable_auth_methods`, `unstable_elicitation`,
`unstable_end_turn_token_usage`, `unstable_mcp_over_acp`,
`unstable_session_fork`, `unstable_protocol_v2` — and **not**
`unstable_llm_providers`, `unstable_nes`, or `unstable_plan_operations`. So
`providers/*` is **types-only** today: depend on
`agent-client-protocol-schema` with `unstable_llm_providers` for the serde
structs, and dispatch the three methods as raw JSON-RPC, as
[`tests/acp.rs`](../apps/bitrouter/tests/acp.rs) already does.

Accept that this capability is marked *"not part of the spec yet, and may be
removed or changed at any point."* Implementing it early is how BitRouter gets
to influence its shape rather than adapt to it — and the blast radius is three
methods behind one feature flag.

## 11. Prerequisites — five verified blockers

These are BitRouter-side and must land before any inline TUI, or the TUI will
look broken in ways that read as its own bug.

0. **`bitrouter reload` silently ignores `policy_table` changes** (§6.1, probe
   5). It returns `{"status":"reloaded"}` and keeps the old routing; only
   `restart` applies the edit. Provider activation *is* hot-reloaded, so the
   gap is specific to the policy table. This blocks `providers/set` outright
   and is a bug independent of this spec — fix it first, with a regression test
   that reloads a changed tier and asserts the next request's chosen provider.

1. **Two stderr writers will corrupt the viewport, not one** (§8.2). The agent
   child's stderr is inherited —
   [`up.rs:283`](../crates/bitrouter-sdk/src/acp/up.rs:283)
   (`.stderr(Stdio::inherit())`), stated at :300 — *and* BitRouter's own
   tracing goes to stderr for every command
   ([`main.rs:2489`](../apps/bitrouter/src/main.rs:2489)). Capture the child's
   stderr to a pipe, and add a **fourth** tracing-subscriber initializer that
   writes to a file, alongside the existing `basic` / `stderr` / `serve` three.
   Both streams land in one per-session log under `~/.bitrouter/logs/`.
2. **Routing failures `eprintln!` + `exit(1)` before any ACP byte** —
   [`acp_cli.rs:546`](../apps/bitrouter/src/acp_cli.rs:546), *"Fail fast to
   stderr — before speaking any ACP."* A client gets no protocol-level error
   for a failed launch. Emit a structured failure the TUI can render.
3. **One session per `acp serve` process.** Acceptable while single-session is
   the line (§2), and it is a natural enforcement of it. Recorded so the
   constraint is chosen rather than discovered.
4. **`AcpTransport` is a single-variant enum (`Stdio` only)** —
   [`transport.rs:22`](../crates/bitrouter-sdk/src/acp/transport.rs:22). Any
   "embeddable over a socket" claim has no transport today, and ACP lists
   Streamable HTTP as a draft proposal. Out of scope for v1; do not promise it.

## 12. Scope by version

| | v1 | v2 |
|---|---|---|
| Delete `launch --tui` + fidelity matrix (§3) | ✅ | |
| `translate.rs` stops dropping variants (§5.1) | ✅ | |
| `UsageUpdate.cost` from metering (§7) | ✅ | |
| `providers/list` + `providers/set` (§6) | ✅ | |
| Prerequisites 0–2 (§11) | ✅ | |
| File-writing tracing subscriber + captured child stderr (§8.2) | ✅ | |
| New verb sharing `RoutingOptions` via `#[command(flatten)]` (§8.1) | ✅ | |
| TUI renders over the shared `snapshot.rs` / `Scope` layer (§8.3) | ✅ | |
| TUI: message log, tool cards, permission modal, cost line (§8) | ✅ | |
| TUI: provider/model picker (§6) | ✅ | |
| Inline surfacing of the log tail on abnormal exit (§8.2) | ✅ | |
| Key to tail the session log live (§8.2) | | ✅ |
| `providers/disable` | | ✅ |
| `session/list` / `session/resume` unmasking | | ✅ |
| `session/set_config_option` (modes) | | ✅ |
| `session/fork` | | ✅ |
| ACP v2 migration (§10) | | ✅ |

## 13. Acceptance

Unlike its predecessor, this spec's gate contains no manual, per-harness,
per-release step. That is the point.

1. **Protocol conformance is automated.** Extend
   [`tests/acp.rs`](../apps/bitrouter/tests/acp.rs)'s raw JSON-RPC driver:
   `providers/list` returns the catalog; `providers/set` changes the effective
   route; `UsageUpdate` carries a non-null `cost` after a settled turn;
   forwarded variants survive `translate` round-trip.
2. **Renderer tests are pure.** The view is a function of the normalized
   stream, so assertions run on a `ratatui::TestBackend` with no child process,
   no PTY, and no live agent.
3. **One live-agent smoke, and it is the risky one:** `providers/set`
   mid-session against a real upstream (§6). Manual for v1, and the honest
   place to discover that an adapter has opinions.
4. Full workspace `cargo nextest run --all-features`, `cargo clippy
   --all-features`, `cargo fmt -- --check` clean.

## 14. Rejected alternatives

- **Keep `launch --tui`, extract `crates/bitrouter-term`.** The extraction is
  genuinely cheap — `pty.rs`, `term.rs`, `lifecycle.rs`, `conformance.rs`
  contain zero `crate::` references, so it is a `git mv` plus a manifest. But
  a clean boundary around a strategically incoherent feature is still that
  feature. Rejected on §1, not on cost.
- **Keep `launch --tui`, just fix rule-4 and feature-gate the deps.** ~50 lines
  of deletion plus `optional = true` on six dependencies. This is the correct
  action *if* §3 is rejected, and it is the fallback recommendation. It does
  not address §1.
- **An ACP TUI as a replacement for `launch --tui`.** It is not one. An ACP
  client renders BitRouter's UI for agents that speak ACP; `launch`'s premise
  is a human driving the harness's **own** native TUI. Adopting this spec does
  not "port" `launch --tui` — it deletes one product and starts another.
- **A `pi-tui`-style TUI toolkit crate.** `pi-tui` is TypeScript,
  agent-agnostic, and protocol-free; its Rust analogue is `ratatui`, already a
  direct dependency. Only the *design philosophy* transfers (§8), not the
  artifact.
- **Wait for ACP v2.** §10 — `providers/*` does not need it, and v2 costs an
  engine rewrite for a client-role shrink BitRouter already took voluntarily.

## 15. Lockstep obligations

Per `CLAUDE.md`, the `--tui` removal in §3 is a CLI surface change and must
land with:

- `skills/bitrouter/SKILL.md` and `skills/bitrouter/references/cli.md` — drop
  `--tui`, add the new TUI command.
- [`CLI.md`](CLI.md) — same.
- [`README.md`](README.md) — delete the `TUI_FIDELITY_MATRIX.md` entry and the
  "eight harnesses" line describing it.
- [`OBSERVABILITY_TUI_SPEC.md`](OBSERVABILITY_TUI_SPEC.md) — mark the
  `launch --tui` half superseded by this spec; leave the `status --watch` half
  authoritative.
- [`harness.rs:401`](../apps/bitrouter/src/harness.rs:401) — rewrite the
  `launch_supported` doc comment, which currently justifies the four-harness
  cut partly by *"(under `--tui`) a hosted terminal."*
- Independently and immediately: the stale *"all eight catalog harnesses"*
  claim at [`host.rs:11`](../apps/bitrouter/src/tui/host.rs:11) and
  `OBSERVABILITY_TUI_SPEC.md:439` is false today and should not survive this
  spec's review regardless of which way §3 is decided.

## 16. Open questions

1. ~~**Does `providers/set` mid-session actually work against a live
   upstream?**~~ **Answered — see §6.1.** The mechanism exists (`policy_table`
   server-side model rewrite, verified end to end) and is transparent to the
   upstream adapter, so the adapter-cooperation risk was misidentified. Two
   BitRouter-side gaps replace it: `reload` silently ignores `policy_table`
   changes, and routing overrides are daemon-global rather than session-scoped.
   Both must close before `providers/set` ships.
2. ~~**Where does captured agent stderr go?**~~ **Answered — see §8.2.** The
   question was under-scoped: there are two competing stderr writers, not one,
   because BitRouter's own tracing also goes to stderr on every command. Since
   a file is required for tracing regardless, the pane-vs-file dilemma
   dissolves — both streams go to one per-session file, with no permanent pane,
   the tail surfaced inline automatically on abnormal exit, and a key to tail it
   live. This needs a fourth tracing-subscriber initializer.
3. ~~**Does the TUI get its own command, or a flag?**~~ **Answered — see
   §8.1.** A distinct verb, sharing `spawn`'s options via `RoutingOptions` +
   `#[command(flatten)]` — flag reuse without collapsing SPAWN_SPEC's
   interactive/headless verb split. Not `bitrouter tui`: it names the medium,
   inherits the orchestrator's expectations, and is ambiguous against
   `status --watch`, which is also a TUI. `bitrouter chat <agent>` is the
   working name; **the spelling is the one thing still open here**, subject to
   two constraints — do not name it after the medium, and do not reuse `attach`
   (removed with #745).
4. ~~**Is `status --watch` a view inside this TUI, or permanently separate?**~~
   **Answered — see §8.3.** Separate views, shared data layer. The rule: the
   TUI renders session-scoped data only; daemon-wide belongs to
   `status --watch` — the same scope line as §2's single-session rule. The seam
   already exists (`snapshot.rs`'s `Scope::{DaemonWide, Launch}`, `launch_id`
   plumbed to the metering column), including the honest-fallback behaviour that
   fixed the fidelity matrix's defect #1. Reuse it; do not build a second data
   layer, and do not merge the views.

**Still open.** Only the verb spelling in §16.3, plus the two `providers/set`
gaps in §16.1 — which are tracked as work in §11.0 and §6.1 rather than as
design questions.
