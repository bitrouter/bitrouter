# Spec: ACP as the source of truth for agent authentication

Status: **Proposed — for review** · Date: 2026-09-03

Implementation note: nothing here is built. The wire mechanism this specifies
is already stabilized in ACP and already vendored in BitRouter's pinned
`agent-client-protocol-schema =1.7.0`; what is missing is BitRouter's use of
it. Phase 1 is a capability flag and a retained handshake field. Phase 2 is the
TUI flow. Phase 3 deletes the hardcoded harness login knowledge.

This document is authoritative for **how BitRouter learns that a harness is
unauthenticated, and how a person authenticates one**. It covers the ACP client
in `bitrouter-sdk`, the auth surfaces of `bitrouter-tui`, and the
`providers login` path in `apps/bitrouter` insofar as that path duplicates
knowledge the protocol carries.

It does **not** supersede [`ACP_CONTROLLER_SPEC.md`](ACP_CONTROLLER_SPEC.md).
The controller's forwarding contract, capability gating, and session ownership
are unchanged; this adds one client-side capability and one client-side flow.
It does not change how the daemon acquires provider credentials, and it does
not change `bitrouter launch`.

---

## 1. Executive decision

**The protocol declares how a harness is authenticated; BitRouter stops
guessing.**

Today `bitrouter providers login claude-code` hardcodes the knowledge that the
Claude Code harness is authenticated by running `claude auth login`. ACP
stabilized a mechanism for the harness to declare that itself — a `terminal`
authentication method carrying the arguments and environment to relaunch the
harness's own program in an interactive terminal. BitRouter will consume that
declaration instead of embedding per-harness login recipes.

Two things follow, and they are separable:

- BitRouter **reads** auth state from the protocol (`authMethods`, and an
  auth-required error) and reports it in the terms the protocol used.
- BitRouter **drives** the declared flow, including the out-of-band terminal
  case, and retries what needed authentication.

## 2. Why this is the boundary

### 2.1 Two auth axes, and only one of them is ACP's

| axis | who authenticates to whom | carried by |
|---|---|---|
| **A — agent auth** | harness ↔ its own upstream (`claude-code-acp` ↔ Anthropic) | ACP `initialize.authMethods`, `authenticate`, and the `terminal` method |
| **B — routing auth** | daemon ↔ an upstream provider | provider credentials from `bitrouter providers login`; no ACP participant owns them |

Axis A is what this spec moves onto the protocol. Axis B is not moving: no ACP
participant owns the daemon's upstream credential, and a harness routed through
BitRouter does not and should not know which provider serves it.

The distinction is not academic — it is the difference between the two failures
a session can present, and §6.4 requires them to be reported differently.

### 2.2 What is already on the wire, and is not moving

Provider *configuration* (`providers/list`, `providers/set`,
`providers/disable`) is **unstable** — it lives in the spec's
`schema.unstable.json`, carrying "this capability is not part of the spec yet,
and may be removed or changed at any point," and Zed's client does not
implement it. BitRouter nonetheless uses it, correctly and in advance: the
controller sends `providers/set` downstream to the harness, verifies the result
with `providers/list`, and fails initialize on a mismatch
([`controller.rs:713`](../crates/bitrouter-sdk/src/acp/controller.rs)); it
rejects the same methods upstream from a manager as "not manager-facing"
([`controller.rs:845`](../crates/bitrouter-sdk/src/acp/controller.rs)) and
strips `agentCapabilities.providers` from the response it passes up.

That boundary is correct and this spec does not touch it. Configuring an
endpoint is a preference; acquiring a credential for it is a privilege.

The advance adoption is sound because of how it is *contained*, not because the
draft is nearly stable — its revision history shows four breaking renames in
about two months. Four properties make it safe, and they are the test any
expansion must pass: it is capability-gated (`agent_capabilities.providers`),
verified rather than assumed, confined to one function and one call site, and
invisible to BitRouter's own consumers — no CLI flag, config key, or documented
behavior depends on it. **No user-facing routing mode may be built on
`providers/*` while it carries that warning**, because that would break the
fourth property: promising a mode whose mechanism can be withdrawn.

## 3. Goals and non-goals

### 3.1 Goals

1. A session that cannot start for want of harness auth says so in a sentence,
   naming what the harness advertised — never a raw JSON-RPC error.
2. A person can complete a harness's interactive login without BitRouter
   knowing the login command.
3. Per-harness login knowledge in `apps/bitrouter` is deleted, not extended.
4. An unauthenticated harness and a provider rejection mid-turn are
   distinguishable from each other on screen.

### 3.2 Non-goals

- **Owning or storing a harness credential.** BitRouter reads the Claude Code
  session live and copies nothing; that is unchanged. Nothing here persists a
  token.
- **Driving the interaction over the protocol.** ACP carries no channel for a
  browser redirect or a pasted code, by design. This spec does not invent one.
- **`bitrouter launch`.** It holds no ACP connection, so the protocol cannot be
  its source of truth. Its auth story stays what it is.
- **Axis B credential acquisition.** `providers login <id>` for a *provider*
  keeps its current shape; only the harness-login special case (§8.3) changes.
- **`auth/status`.** Draft, not stabilized. See §11.1.

## 4. What ACP specifies

Evidence, so review can check the premises rather than the conclusions. Read at
`agentclientprotocol/agent-client-protocol`; schema line is the pinned
`agent-client-protocol-schema =1.7.0`.

### 4.1 Terminal authentication (stabilized 2026-08-20)

The RFD states the problem this spec exists to fix:

> Agents use several authentication flows. Some can handle login over ACP,
> while others require users to interact with the Agent's terminal UI. The
> baseline `agent` authentication method does not tell a Client that it must
> launch an interactive program, so users may need to leave the Client and
> complete setup manually.

Its resolution is out-of-band **execution** with in-protocol **declaration**:

1. The client initializes and receives a `terminal` authentication method.
2. On selection, the client launches a separate interactive process using *the
   configured agent program and base launch configuration*, plus the
   descriptor's `args` and `env`.
3. The client presents the terminal and waits. Exit status zero is success;
   non-zero, no exit status, or cancellation is failure.
4. On success the client reconnects, reinitializes, and retries the operation
   that required authentication.

Two constraints are load-bearing:

- The descriptor **cannot specify a command**. The client builds the invocation
  from the program it already launched, which "prevents the Agent from
  selecting an unrelated program."
- The client **MUST NOT** pass a terminal method to `authenticate`. The
  interactive process is not the ACP connection.

### 4.2 The types are already vendored

In the pinned schema:

- `AuthMethod::Terminal(AuthMethodTerminal)` — `v1/agent.rs:703`, with
  `id`, `name`, `description`, `args: Vec<String>`, `env: HashMap<String,String>`.
- `ClientCapabilities.auth: AuthCapabilities` — `v1/client.rs:2002`, whose
  `terminal: bool` documents: *"The client should set this to `true` only when
  it can reproduce the configured agent invocation in an interactive terminal."*
- `ClientCapabilities.terminal: bool` — `v1/client.rs:1974`. **A different
  field**: the tooling surface that lets an agent run commands in the client's
  terminal. §5.2 depends on these being distinct.

### 4.3 The reference client

Zed (`ed8d600`, 2026-09-03) implements the flow:

- advertises `.auth(acp::AuthCapabilities::new().terminal(true))` —
  `crates/agent_servers/src/acp.rs:782`
- builds the task from the agent's own command plus the descriptor —
  `acp.rs:1539`, behind `AcpBetaFeatureFlag` at `acp.rs:1898`
- spawns it into its **`TerminalPanel`**, a terminal surface it owns, distinct
  from the conversation UI — `crates/agent_ui/src/conversation_view.rs:2124`
- on success calls `reset(…)`, reconnecting and reinitializing
- keeps a pre-stabilization `_meta["terminal-auth"]` fallback carrying
  `label`/`command`/`args`/`env` — `acp.rs:1553`

That a client which owns a windowed UI still gives the login flow *its own
terminal* is the precedent for §7.3.

## 5. Current behavior, and the two defects

### 5.1 Auth state is read and discarded

`AcpClient` retains only what it acts on. The doc comment is explicit that the
`initialize` response "is **not** retained: everything this client acts on is
read out of it here, at handshake, and keeping the rest would be state with no
reader" ([`client.rs:528`](../crates/bitrouter-sdk/src/acp/client.rs)). Only
`route_control` survives. `authMethods` arrives and is dropped, so no surface
above can name what a harness offers.

That reasoning was right and stays right: the fix is to give `authMethods` a
reader (§9.1), not to retain the response.

### 5.2 The capability is declined for a reason that does not apply

[`client.rs:1017`](../crates/bitrouter-sdk/src/acp/client.rs):

> Client capabilities are deliberately left at their defaults (no fs / no
> terminal): ACP v2 removes that client surface, and a manager provides such
> tooling via the relayed MCP servers instead.

The rationale is sound for `ClientCapabilities.terminal` — the tooling surface.
It does not cover `ClientCapabilities.auth.terminal`, which was stabilized in
**both** v1 and v2 and is not a tooling surface at all. Two differently-scoped
fields spelled `terminal` were conflated, and a stabilized auth mechanism went
unused as a side effect.

This is the whole of the blocking defect. No dependency changes.

### 5.3 What the defects cost, observed

A `bitrouter chat claude-acp` against an unauthenticated harness renders
nothing and reports `cost unreported`, while the harness retries an upstream
401 behind the scenes with exponential backoff. The renderer is honest — it has
received no message and no priced usage — but the person is told neither which
axis failed nor what to do. Separately, `providers login claude-code` hands the
terminal to a child whose "Paste code here if prompted >" prompt is
indistinguishable from BitRouter's own output, and reports a bare non-zero exit
as "didn't complete".

## 6. Hard invariants

### 6.1 The harness declares; BitRouter does not infer

No login command, argument, or environment variable for a harness's own
authentication may be written in BitRouter. Every one comes from an
`AuthMethodTerminal` descriptor, or the flow is not offered. A harness that
declares nothing gets a pointer to its own documentation, never a guess.

**Scoped to axis A.** This governs agent authentication. It does *not* reach
`providers login <id>`, where a provider's vendor CLI (`claude`, `codex`,
`grok`, `agy`) is the credential's actual source and naming it is unavoidable —
see §8.3.

### 6.2 A control that cannot act is absent, not dead

`auth.terminal` is advertised **only** where BitRouter can both reproduce the
configured invocation *and* run the login. Two conditions, and the second is the
binding one:

- **Reproducible invocation.** Always true on the controlled paths:
  `launch_controlled` spawns the harness itself and `AcpTransport::Stdio` is the
  only transport, so there is no remote-agent case here. An earlier draft
  gated this on `--base-url`; that was wrong — `--base-url` moves where *model
  traffic* goes, not where the harness runs.
- **A flow that exists.** The capability is a promise to *run* the login: an
  agent may offer a `terminal` method only when the client advertised support.
  Advertising it before §7.3 is implemented would surface a method nothing can
  execute — precisely the dead control this codebase refuses.

So it is `false` through phase 1 and flips in phase 2, and the value is a
**caller-supplied parameter** ([`ClientOptions::terminal_auth`]), never an
inference: only the caller knows whether it owns the harness process. This is
the same honesty gate route control applies, checked the same way.

### 6.3 Cancelling never authenticates

Declining the picker, `Esc`, a signal, or a closed terminal leaves the session
unauthenticated. A non-zero exit, no exit status, or cancellation of the login
process is a failure, never a success. No path may treat "the person did not
finish" as "the person is signed in."

### 6.4 The two axes are never conflated on screen

An unauthenticated harness and an upstream provider rejection are different
sentences. Axis A names the harness and its advertised methods; axis B names
the provider, the route, and `providers login`. Neither message may be used for
the other, and this is pinned by test rather than by review.

### 6.5 The renderer's frame is never corrupted

No login process writes into a terminal the renderer believes it owns. The
terminal is restored before the child starts and re-taken after it exits
(§7.3). This is the same rule that sends chat's logs to a file only.

## 7. The flow

### 7.1 Handshake

`initialize` carries `clientCapabilities.auth.terminal`, set from the caller's
declaration and subject to §6.2 — so `false` until §7.3 exists. The response's
`authMethods` are retained alongside `route_control`, and the rest of the
response is discarded as today.

### 7.2 Detecting that auth is needed

Two triggers, in this order:

1. An `auth_required` error from `session/new` or a later authentication-gated
   request.
2. An explicit request from the person (§8.2).

`session/new` is **not** a reliable detector — ACP does not require it to check
authorization, and some harnesses validate lazily on the first model call. So a
`session/new` that succeeds is not evidence of authentication, and the
auth-required error must be recognized wherever it can arrive, not only at
session creation. §11.1 is the eventual fix.

### 7.3 Answering a `terminal` method

1. Restore the terminal (`lifecycle::restore`), leaving the transcript in
   scrollback.
2. Spawn the configured harness program with the descriptor's `args` appended
   and `env` overlaid, with inherited stdio, and wait.
3. Exit zero → reconnect, reinitialize, retry what needed auth. Anything else →
   report failure and stay unauthenticated (§6.3).
4. Re-take the terminal and repaint.

Steps 1 and 4 are what make this safe inside an inline renderer: the child gets
a real cooked terminal, which is what its browser-and-paste flow needs, and the
renderer's model of the screen is rebuilt rather than desynchronized. Zed
achieves the same separation with a dedicated pane (§4.3).

### 7.4 Answering an `agent` method

`authenticate` with the method id, then retry. No terminal handoff. This is the
protocol's own path and needs no BitRouter mechanism.

## 8. Surfaces

### 8.1 The picker

Auth methods render in the existing numbered-modal idiom — the same grammar as
the permission prompt and the route picker, so no new vocabulary is introduced:

```
sign in? claude-acp   [1]Log in from the terminal [2]API key   [esc] cancel
```

Zero advertised methods draws **no picker**, per §6.2, and a notice naming the
harness instead. One method still draws the picker: confirming a step that
takes over the terminal is worth one keystroke.

### 8.2 In-session command

`/login` opens the picker at an idle prompt, and reports "this harness
advertises no authentication methods" when there are none — the same shape as
`/route`'s refusal on a non-routable session.

### 8.3 `providers login claude-code`

**The `claude auth login` spawn stays.** An earlier draft had it deleted and
routed through §7. That was wrong, on two counts:

- **It is not this axis.** `claude-code` here is a *provider*, and the `claude`
  CLI is its vendor tool — the same relationship `import_cli_for` already has
  with the Codex, Grok, and Antigravity CLIs. The credential it acquires serves
  the daemon's routing, not any ACP session.
- **It would couple provider login to ACP.** Reaching the descriptor means
  launching an adapter — `npx`, a download, a controller — to learn a command,
  in order to acquire a credential that has nothing to do with ACP. A user with
  no adapter installed could no longer log in to a provider.

Doing it would also re-conflate the two axes §6.4 exists to separate.

What *was* wrong is the seam, and that is fixed on its own terms: the handoff
is announced before it happens and ruled off at both ends, so a person can see
that the prompts belong to another program; and a failure states that the
sign-in did not complete, notes the browser-code step, and names
`claude auth status` — rather than relaying an exit code.

The provider-credential meaning of `providers login <id>` is untouched.

## 9. Component changes

### 9.1 `bitrouter-sdk`

- `ClientOptions` gains a way for the caller to declare terminal-auth support;
  the client sets `clientCapabilities.auth.terminal` from it. Defaults to
  false, so no existing caller changes behavior.
- A retained, parsed `auth_methods` on `AcpClient`, read at handshake, with an
  accessor. Parsed like `route_control` is: read once, off the response,
  never re-derived from the agent's name.
- An `auth_required` predicate over ACP errors, so every consumer agrees on
  what the state looks like.
- The controller is **unchanged**. `authenticate` already rides the verbatim
  forwarding path, and `authMethods` already arrives from the harness
  untouched.

### 9.2 `bitrouter-tui`

- `auth::Prompt` — the picker, built from the advertised methods. `open`
  returns `None` for an empty list, mirroring `picker::Picker::open`.
- The footer gains no row: the picker is a modal, and the modal slot exists.
- `machine`: a `Phase::Authenticating(auth::Prompt)` reachable only from
  `Phase::Idle`, and the `Effect`s for selecting, handing over the terminal,
  and reconnecting.
- No harness knowledge enters this crate. The descriptor arrives as data.

### 9.3 `apps/bitrouter`

- Declares terminal-auth support for `chat` and `spawn`, and not under an
  explicit `--base-url` (§6.2).
- Owns the process spawn, since it owns the harness command — the crate
  boundary already forbids the renderer from reaching it.
- Loses the per-harness login recipe (§8.3).

## 10. Phasing

| Phase | Content | Independently useful? |
|---|---|---|
| 1 — **shipped** | Wire `auth.terminal` as a caller parameter (sent `false`, per §6.2); retain `authMethods`; `is_auth_required` predicate; report auth state in a sentence naming the harness and its methods | Yes — kills the raw-error failure mode with no new UI |
| 2 — **shipped** | The method prompt, the terminal handoff, reconnect-and-retry; `terminal_auth` opted into by interactive `chat` | Yes — completes axis A |
| 3 — **shipped, revised** | Fix the `providers login claude-code` handoff seam. *Not* the original "delete the hardcoded login and route through §7", which §8.3 explains was wrong | Yes — fixes the reported UX failure |

### 10.1 What phase 2 proved

`claude-agent-acp@0.70.0`, probed directly, advertises exactly one method:

```json
{ "id": "claude-login", "name": "Log in with Claude",
  "description": "Run `claude /login` in the terminal",
  "type": "terminal", "args": ["--cli"] }
```

…**and only when the client advertised `auth.terminal`**. Probed without it,
`authMethods` is `[]`. Two things follow, and both are load-bearing:

- The capability gate in §6.2 is enforced by real adapters, not just specified.
  A client that declines the capability is told nothing at all, which is why
  phase 1 alone would have reported "advertises no authentication method" for
  every Claude session.
- The terminal flow is not speculative. The one harness this repo configures by
  default uses it, and `agentCapabilities.providers` on that same response is
  `null` — so claude-acp is routed by env injection only, and
  `configure_provider` never fires for it.

Phase 1 is worth landing alone: most of the observed pain in §5.3 is a missing
sentence, not a missing flow.

## 11. Open questions

1. **`auth/status`.** The Draft RFD (accepted 2026-07-21) adds an `auth/status`
   method precisely because `session/new` is an unreliable detector, and its
   status quo section names the failure we observed: harnesses that "do so
   lazily (e.g., on the first LLM call)" leave users to "encounter errors
   mid-flow." It is not stabilized, so §7.2 does not depend on it. Adopt when
   it stabilizes, and treat §7.2's error-driven detection as the fallback.
2. **`_meta["terminal-auth"]` compatibility.** Zed carries a pre-stabilization
   fallback (§4.3). Whether any harness BitRouter launches still relies on it,
   rather than the stabilized descriptor, is unmeasured. Do not implement the
   fallback speculatively.
3. **Does the `spawn`/`serve` path want the flow at all, or only the report?**
   A headless sub-agent has no person at a terminal. The likely answer is that
   `spawn` advertises the capability (it can reproduce the invocation) but a
   non-interactive session reports auth-required as a structured NDJSON error
   and exits, rather than taking a terminal nobody is watching. Unresolved.
4. **Reconnect cost.** §7.3 step 3 reconnects and reinitializes, per the RFD.
   For `chat` this discards the ACP connection but not the transcript, which is
   in scrollback. Whether anything else in a live session must survive the
   reconnect is unaudited.

## 12. Acceptance

Pinned by test, in the house's style — a failing build rather than a missed
review:

1. A client that cannot reproduce the invocation does not advertise
   `auth.terminal` (§6.2).
2. A harness advertising no methods draws no picker (§6.2, §8.1).
3. Cancelling the picker, and a non-zero login exit, both leave the session
   unauthenticated (§6.3).
4. An axis-A message never names a provider or `providers login`, and an
   axis-B message never names an auth method (§6.4).
5. No string matching a harness login command appears outside a test in
   `apps/bitrouter` after Phase 3 (§6.1) — checked against the sources the way
   `crate::chat`'s reachability guard is.
6. A `terminal` method is never passed to `authenticate` (§4.1).
