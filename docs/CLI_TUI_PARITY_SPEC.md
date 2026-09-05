# Spec: CLI ↔ TUI parity — what "the same command" can and cannot mean

Status: **proposed** · Author: Claude (with Spikel) · Date: 2026-09-05
· Branch: `claude/cli-tui-parity-spec`
· Measured at `43ae57d8` (`claude/actions-table-phase04`)
· Peer-group research and the #866 amendment added 2026-09-05
([§6.9](#69-the-agent-harness-peer-group), [§6.10](#610-the-in-repo-precedent-866))
· `acpx`, the transport survey, and the remote-HTTP requirement added 2026-09-05
([§6.11](#611-acpx-and-what-remote-over-http-can-actually-mean),
[D13](#d13--the-remote-transport-requirement)); D10's reasoning superseded, its
conclusion held
· Refs: [`ACTIONS_SPEC.md`](ACTIONS_SPEC.md) (phases 0–4 implemented),
[`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) §2/§8.3/§9,
[`OBSERVABILITY_TUI_SPEC.md`](OBSERVABILITY_TUI_SPEC.md) §14,
[`ACP_CONTROLLER_SPEC.md`](ACP_CONTROLLER_SPEC.md) §16.3

> **The stated goal is "every headless CLI command has the same interactive TUI
> command." This spec argues it is the wrong target — not because parity is
> undesirable but because "command" is the wrong unit, the CLI is the wrong
> denominator, and the relation is not a subset relation.** The right target is
> stated in [§3](#3-the-goal-restated). It is narrower in what it covers and
> *stricter* in what it guarantees, and it is the only version of the goal that
> can be held by a test.
>
> Where this contradicts the maintainer's framing, it says so in the open. Where
> the maintainer may reasonably choose the broader goal anyway,
> [§16](#16-open-decisions) states the cost rather than hiding it.

**The short version.**

1. There are **103 leaves, not 29**, and 8 of the 29 top-level entries are
   namespaces. Neither reading of "command" survives contact
   ([§2.1](#21-command-is-ambiguous-and-both-readings-fail)).
2. Roughly a quarter of the CLI is **actively hostile** in a session — recursive,
   blocking, substrate-destroying, or TTY-contending — and one case (`stop`) is
   already a decided question, not an open one
   ([§2.2](#22-roughly-a-quarter-of-the-cli-is-actively-hostile-in-a-session)).
3. **No mature tool in the field achieved command parity**, several disclaim it
   in the maintainer's own words, and the two that pursued it — `hub` and
   `aws-shell` — are the only obituaries in the set
   ([§6.1](#61-nobody-achieved-parity-and-the-two-who-tried-are-dead),
   [§6.3](#63-hub-the-only-rigorous-post-mortem-of-a-parity-layer)).
   **[Amended post-research.]** The coding-agent peer group agrees in the
   direction the goal is stated — none of the five surveyed has it, every one
   has CLI surface with no interactive expression, and Codex has *no* shared
   dispatch at all. But two of them **do** share a dispatcher across the
   terminal and the headless CLI: OpenCode through a server-side
   `session.command` port, and aider by making its whole 43-command table
   headlessly invocable. Both got there by **inversion** — the registry
   underneath both surfaces, the headless caller a client of it — which is
   [§6.4](#64-tmux-is-the-one-real-full-parity-system-and-it-works-by-inversion)'s
   result, and the shape of the achievable goal in
   [§3](#3-the-goal-restated) ([§6.9](#69-the-agent-harness-peer-group),
   findings 8, 10 and 16).
4. The topology is **three disjoint sets**, not a subset with a gap. BitRouter
   already has a set-C member — `route/set`, a session-scoped write with no
   headless subject — and the current rules call it a violation
   ([§3](#3-the-goal-restated), [§6.2](#62-psql-the-interactive-layers-vocabulary-is-mostly-net-new)).
5. The real defect underneath "29 versus 2" is **four questions** you cannot ask
   without leaving the session. The answer is ~6 commands
   ([§2.4](#24-the-count-is-not-the-complaint), [§8](#8-the-inventory)).
6. **Mechanism: local dispatch through the same action ports the CLI uses**,
   handed to the chat loop the way `route_control` already is — not
   `AvailableCommandsUpdate` (ACP has no invocation channel but the prompt, and
   BitRouter is a middlebox), and not new extension methods (a JSON round trip
   to talk to yourself, for a consumer that does not exist)
   ([§9](#9-the-mechanism)).
   **[Amended 2026-09-05.]** The registry goes *underneath* both surfaces and the
   headless CLI becomes a client of the same dispatch — OpenCode's inversion,
   taken for the dispatch and not for its openness
   ([D12](#d12--take-693s-inversion)). And the new requirement that both surfaces
   also work **remotely over HTTP** does not reverse this: ACP defines no network
   transport, while `mcp serve --transport http` already carries the same
   `ACTIONS` rows remotely, so the answer is a `Reach` column rather than a new
   protocol ([§6.11](#611-acpx-and-what-remote-over-http-can-actually-mean),
   [D13](#d13--the-remote-transport-requirement)). Four of the six commands do not
   travel at all, and that is a fact about the goal rather than the mechanism.
7. **Source of truth: `ACTIONS` extends** with `tui_command`, `effect`, and a
   tmux-shaped `requires` ([§10](#10-source-of-truth)), guarded by five tests
   ([§13](#13-the-guards)).
8. This **knowingly reverses `ACP_TUI_SPEC.md` §8.3** in a narrowed form, and
   nothing else ([§5](#5-the-prior-record-and-what-parity-would-reverse),
   [D1](#d1--amend-83-or-hold-it)).

## Contents

- [1. Verified starting state](#1-verified-starting-state)
- [2. Why the goal cannot be taken literally](#2-why-the-goal-cannot-be-taken-literally)
- [3. The goal, restated](#3-the-goal-restated)
- [4. Root cause — why 103 versus 2](#4-root-cause--why-103-versus-2)
- [5. The prior record, and what parity would reverse](#5-the-prior-record-and-what-parity-would-reverse)
- [6. What other tools did](#6-what-other-tools-did)
- [7. The membership rule](#7-the-membership-rule)
- [8. The inventory](#8-the-inventory)
- [9. The mechanism](#9-the-mechanism)
- [10. Source of truth](#10-source-of-truth)
- [11. Reads versus writes](#11-reads-versus-writes)
- [12. Rendering](#12-rendering)
- [13. The guards](#13-the-guards)
- [14. Phases](#14-phases)
- [15. Invariants](#15-invariants)
- [16. Open decisions](#16-open-decisions)
- [17. Explicitly out of scope](#17-explicitly-out-of-scope)

---

## 1. Verified starting state

Measured in this worktree at `43ae57d8`, not inferred from the issue. Six of the
claims below correct the brief that commissioned this spec; those are marked
**[correction]**.

| Claim | Evidence |
|---|---|
| The CLI has **29 top-level commands** | [`main.rs:128`](../apps/bitrouter/src/main.rs:128) `enum Command`, confirmed against `bitrouter --help` |
| …and **103 invocable leaves** | walked from `Cli::command()`: `cloud` alone contributes 40, `policy` 13, `eval` 8, `workflow-state` 6 |
| The interactive surface is **two** slash commands | [`machine.rs:375`](../crates/bitrouter-tui/src/machine.rs:375) — `/commands`, and `/route` gated on `state.routable` |
| Both are hardcoded string compares in `submit()`, not a table | same site; there is no command registry in the TUI at all |
| **[correction]** `/commands` does **not** merge BitRouter's own commands with the agent's — it lists *only* the agent's | [`render/session.rs`](../crates/bitrouter-tui/src/render/session.rs) `commands()` takes `&[AvailableCommand]` from the journal and nothing else; [`session.rs:366`](../apps/bitrouter/src/chat/session.rs:366) passes `view.commands()` |
| …so **`/route` is not discoverable from inside the TUI.** The only place it is written down is `docs/CLI.md:532` and the skill | grep for `"/route"` in `crates/bitrouter-tui/` returns the dispatch site and no help text |
| **[correction]** The TUI drives **two** extension methods, not three | `Effect::{ListRoutes, SetRoute}` ([`machine.rs:203`](../crates/bitrouter-tui/src/machine.rs:203)); there is no `Effect::ResetRoute` |
| …and `_bitrouter/route/reset` is fully implemented on both sides with **no** interactive caller | [`client.rs:743`](../crates/bitrouter-sdk/src/acp/client.rs:743) `route_reset`; its only call site outside the controller is a unit test at `client.rs:1987` |
| **[correction]** `claude/mcp-drop-complete` exists **only as a local branch and points at `43ae57d8`** — no `complete` removal is in this tree | `git log --oneline claude/mcp-drop-complete -1` → `43ae57d8`; `git ls-remote --heads origin` does not list it; `ACTIONS` still carries the `complete` row |
| `ACTIONS` has **six** rows, of which **four** have two surfaces | [`actions/mod.rs`](../crates/bitrouter-mcp/src/actions/mod.rs): `status`, `list_models`, `route`, `skills_search` have both; `complete` and `skills_get` have one each |
| Every row is a **read**. No row is a mutation | the four schema'd reports are `StatusReport`, `ModelsReport`, `RouteReport`, `SkillsReport`; `RouteQuery::route` is documented "Read-only: nothing is sent upstream" |
| `route/set` — the only write on any surface — has **no row, no shared type, and no guard** | `AcpRouteSet` → [`daemon.rs:666`](../apps/bitrouter/src/daemon.rs:666); its report shape is `RouteSetResponse` in `bitrouter-sdk`, unknown to `ACTIONS` |
| The guard tests are three, and they assert **tool ⇒ row**, never **leaf ⇒ row** | [`main.rs:5859`](../apps/bitrouter/src/main.rs:5859)–`5934` |
| **ACP has no method for invoking a command.** v1's full method set is `initialize`, `authenticate`, `session/{new,load,list,resume,fork,close,delete,prompt,cancel,set_mode,set_config_option,update,request_permission}`, `fs/*`, `terminal/*` | enumerated from `agent-client-protocol-schema-1.7.0` |
| …so a slash command is invoked by putting its text in a `session/prompt` block | there is nothing else to put it in |
| `AvailableCommand` carries `{name, description, input: Option<AvailableCommandInput>}`, and the only input variant is `Unstructured { hint: String }` | [`client.rs:742`](https://docs.rs/agent-client-protocol-schema) — "All text that was typed after the command name is provided as input" |
| `AvailableCommandsUpdate.available_commands` is documented as *"Commands **the agent** can execute"* | same file, `:697` |
| The `_bitrouter/route/*` capability is negotiated on **three** conditions, per method | [`client.rs:398`](../crates/bitrouter-sdk/src/acp/client.rs:398) `RouteControlCapability::from_init` — `version == "1"`, `scope == "session"`, method listed in `methods` |
| `bitrouter-tui` **cannot** depend on `apps/bitrouter` | `Cargo.toml` comment + the path dependency direction; the reverse edge is a cyclic-package error |
| `apps/bitrouter/src/chat/` **cannot name the daemon, metering, policy, or the control socket** — enforced by a source-scanning test | [`chat/mod.rs`](../apps/bitrouter/src/chat/mod.rs) `the_chat_module_reaches_nothing_daemon_wide`, forbidding `crate::daemon`, `crate::metering`, `crate::policy`, `MeteringStore`, `bitrouter_sdk::config`, `control_socket`, `DaemonRouteControl`, `DaemonSessionCost`, `LocalControllerBinding` |
| …but the *process* has all of them one layer up | [`acp_cli.rs:1010`](../apps/bitrouter/src/acp_cli.rs:1010) `LocalControllerBinding` holds the socket path, principal, and controller instance id |
| The controller is **in-process, over a duplex channel** — the "wire" between the TUI and BitRouter's own surfaces costs no bytes and no process | [`acp_cli.rs:1927`](../apps/bitrouter/src/acp_cli.rs:1927) `agent_client_protocol::Channel::duplex()` |
| Route control is injected as `Arc<dyn AcpRouteControl>`, built app-side, handed to the controller — the *same* port shape `ACTIONS` uses | `binding.route_control()` → `Controller::route_control(..)` |
| A session with `--direct` or an explicit `--base-url` gets **no binding**, so the controller advertises nothing and `/route` is absent rather than dead | [`acp_cli.rs:1025`](../apps/bitrouter/src/acp_cli.rs:1025) |
| `CliReport` renders to a **byte stream** with an ANSI `Theme`, not to `ratatui` lines | [`output/mod.rs`](../apps/bitrouter/src/output/mod.rs) `render(&self, h: &mut Human) -> io::Result<()>`; `render_to_vec` already exists as the palette-free seam |
| `ratatui` is deliberately **not** a dependency of `apps/bitrouter` | `TUI_RENDERER_SPEC.md` implementation note (2026-08-24) |

**[Corrected post-research, 2026-09-05.]** This table was measured before
`878178a4` (#866) landed on `main`, and one row is now understated: the claim
that no BitRouter surface shares an interpreter between the terminal and the
headless CLI is **no longer true**. `apps/bitrouter/src/chat/effects.rs` is
[§9 option C](#c-local-dispatch-through-the-same-action-ports--recommended)
shipped for one verb — permission answering — and the interactive TUI, the piped
`chat`, and `acp prompt` all run it. See [§6.10](#610-the-in-repo-precedent-866)
for the verification and for what it settles.

---

## 2. Why the goal cannot be taken literally

Four objections. The first two are arithmetic, the third is protocol, the fourth
is the one that actually matters.

### 2.1 "Command" is ambiguous, and both readings fail

There are 29 top-level commands and 103 leaves. Only 8 of the 29 are *invocable*
— the rest are namespaces whose leaves are the real commands. So:

- **Read it at the top level** and `/cloud` has to be a nested navigator over 40
  leaves with their own flags. A nested navigator with its own modal stack, over
  account state, is an application inside the chat line. That is the shape of the
  thing #786 deleted.
- **Read it at the leaf** and it is 103 slash commands, each needing an argument
  grammar. ACP gives one `hint: String` per command and nothing else
  ([§1](#1-verified-starting-state)), so every flag — `--provider`, `-c`,
  `--socket`, `--prompt`, `-g` — has to be re-parsed by a second parser living in
  the chat line editor.

Two parsers for one grammar is not a hypothetical risk. It is
[`ACTIONS_SPEC.md`](ACTIONS_SPEC.md) §2 root cause (a) — *two implementations* —
re-created in a surface that has no `--json` and therefore no way to test that
the two agree.

### 2.2 Roughly a quarter of the CLI is actively hostile in a session

Not "less useful" — *hostile*, in four distinct ways, and the brief names three
of them:

- **Recursion.** `chat` inside `chat`; `launch` hosts an interactive harness TUI
  in a terminal a TUI already owns; `spawn --serve` and `acp serve` take stdio
  that the renderer is holding in raw mode.
- **Blocking.** `serve` and `mcp serve` never return. A slash command that never
  returns is a hung session with no way out but the signal path.
- **Substrate destruction.** `stop` and `restart` kill the daemon this session's
  route leases, cost attribution, and next turn all depend on. `update` replaces
  the running binary.
- **Terminal contention.** `init`, `providers login`, `cloud login`, and `update`
  all prompt on a TTY the renderer owns. `cloud billing checkout` opens a
  browser to spend money.

The `stop` case is already a decided question, not an open one:
[`OBSERVABILITY_TUI_SPEC.md`](OBSERVABILITY_TUI_SPEC.md) §14 — *"**No `stop`
keystroke.** The control socket exposes it; a keypress that kills the daemon
behind every running agent is the one action where a mis-press is
unrecoverable."*

### 2.3 The protocol has no slot for this

Whatever the interactive surface ends up containing, ACP will not carry it for
free. There is **no ACP method that invokes a command**: the only invocation
channel is `session/prompt`, the only advertisement channel is
`AvailableCommandsUpdate`, the only argument model is one `hint: String`, and
there is no namespace, origin field, or collision rule
([§6.7](#67-acp-specifically)).

That matters here because it removes the cheapest imagined route to the stated
goal — "advertise all 103 over the wire and let the client render them." It
cannot express the arguments, and since BitRouter is a middlebox rather than the
end of the chain, dispatching them means string-matching its own commands out of
the prompt stream before the harness sees them. Analysed in
[§9 option A](#a-ride-availablecommandsupdate), including the `codex-acp`
counter-precedent that makes the question closer than it first looks.

### 2.4 The count is not the complaint

This is the objection that should decide it. "29 versus 2" is a real asymmetry,
but it is not evidence that 29 commands are *wanted* in a session. The
user-facing defect underneath it is narrower and sharper:

> While a chat session is open, there are questions about **this session** —
> what will the next turn route to, what can it route to, what has this cost, is
> the router even up — that today require killing the session or opening a second
> terminal.

That is four questions. The commands that answer them are `route`, `models`,
`status`, and the cost line (which is already in the footer). Everything else on
the list of 103 is answered *outside* the session by construction: it is about
the daemon, the account, the config on disk, or another session entirely.

**So the honest target is roughly 4–6 commands, not 29 or 103.** If that ratio is
unacceptable, the disagreement is not about the rule — it is about whether the
goal was formed by counting commands or by counting the times someone had to
leave the session. A cheap way to settle it before building anything is in
[D5](#d5--settle-the-scope-empirically-before-phase-2).

---

## 3. The goal, restated

The stated goal presumes a **subset relation with a gap**: the TUI has 2 of the
CLI's 29, and parity closes the difference. [§6.2](#62-psql-the-interactive-layers-vocabulary-is-mostly-net-new)
says that is the wrong topology. In every mature pair there are **three disjoint
sets**, and the third one is where the interactive layer earns its existence:

```
       ┌──────────── everything BitRouter can be asked to do ────────────┐
       │                                                                 │
       │   A. both surfaces        B. CLI-only          C. session-only  │
       │   ────────────────        ───────────          ───────────────  │
       │   status                  serve, stop,         route/set        │
       │   models                  policy *, eval *,    route/reset      │
       │   route (preview)         cloud * (40), …      /commands        │
       │                                                                 │
       │   3 actions               97 leaves            3 verbs          │
       └─────────────────────────────────────────────────────────────────┘
```

Set C is not a parity shortfall. `route/set` leases a route for **a live ACP
session**, and the CLI has no live session to name — the same reason psql's `\e`
and `\r` have no flags (they edit a query buffer that only exists in a session)
and k9s's `:xray` has no `kubectl` twin. Inventing `bitrouter acp route set
--session <id>` to close the "gap" would be new surface nothing asked for.

**[Corroborated post-research.]** All four coding-agent harnesses surveyed in
[§6.9](#69-the-agent-harness-peer-group) have the same three-set shape, and one
of them makes the point sharply in the other direction: **Codex's set A is
empty.** 59 interactive commands, 29 CLI subcommands, nine names on both sides
and not one line of shared dispatch — the whole slash-command registry lives in
`codex-rs/tui/` and `codex exec` cannot reach any of it
([§6.9.2](#692-codex-cli--the-registry-lives-in-the-tui-crate-so-the-overlap-is-zero)).
A subset-with-a-gap model cannot describe that at all.

So, replacing *"every headless CLI command has the same interactive TUI
command"*:

> **P1 (completeness).** Every action whose subject is *the session in front of
> you* is reachable from both surfaces where both surfaces can express it, and
> the two answer with the same typed report. — *set A*
>
> **P2 (containment).** Every command reachable in the TUI is an **inventoried
> action**: it has a row, a shared type, and a guard. A row with a
> `tui_command` and no `cli_leaf` is legal only when its subject is a live
> session — and it must say so. — *sets A and C*

**P2 is a deliberate weakening of a prior rule, and the weakening is the point.**
[`OBSERVABILITY_TUI_SPEC.md`](OBSERVABILITY_TUI_SPEC.md) §14 says *"if there is
no CLI subcommand, there is no keystroke."* Read literally, `/route` already
violates it: `route/set` has no CLI leaf and shipped anyway. §14's real purpose
was *"accretion now requires adding a CLI command first — which gets normal
review. This is the gate the old TUI lacked: its verbs existed nowhere else."*
The gate was **review**, and a CLI leaf was the only mechanism available for
forcing it. An inventoried row with a guard test is a stronger version of the
same gate, and it admits set C honestly instead of by exception. See
[D7](#d7--does-the-route-picker-need-a-cli-leaf).

**Why P1 is stricter than the stated goal, not weaker.** "Every CLI command has a
TUI command" says nothing about whether the two *agree*. `bitrouter route` and
the `/route` picker are both present today and disagree: the picker offers model
ids the daemon's `route_chain` will refuse
([`ACTIONS_SPEC.md`](ACTIONS_SPEC.md) §1, phase 5). Under P1 that is a violated
invariant with a failing test. Command-count parity would have scored today's
`/route` as a success.

---

## 4. Root cause — why 103 versus 2

[`ACTIONS_SPEC.md`](ACTIONS_SPEC.md) §2 named three causes of CLI↔MCP drift. Two
of them apply here unchanged and a third is specific to this pair.

**(a) No inventory of interactive commands.** `ACTIONS` inventories MCP tools and
CLI leaves. Nothing inventories TUI commands, because there is nothing to
inventory *from*: the TUI dispatches on `if line.trim() == "/route"` inside
`submit()`. There is no table, so there is nothing a guard could walk. This is
§2 cause (c), one surface later.

**(b) The help text and the dispatch have different sources.** `/commands`
renders the journal's `AvailableCommandsUpdate`; `submit()` matches string
literals. Nothing connects them, which is why `/route` exists and is
undiscoverable. This is a *new* cause the MCP work never hit, because rmcp
derives a tool's advertisement from the same item that implements it.

**(c) The boundary that stops accretion also stops access.** Two hard walls —
`bitrouter-tui` cannot depend on the app, and `chat/` cannot name the daemon —
were put there deliberately ([§5](#5-the-prior-record-and-what-parity-would-reverse))
and they work. But they mean that adding *any* BitRouter answer to the TUI is a
plumbing problem before it is a product problem, so nobody adds one casually.
The gap between 103 and 2 is partly that friction working as designed.

Cause (c) is the one that determines the mechanism: the fix must **add a
declared channel**, not lower a wall.

---

## 5. The prior record, and what parity would reverse

The brief asked whether prior decisions narrowed the TUI deliberately. They did,
repeatedly, and the record is unusually explicit.

| When | Where | What was decided |
|---|---|---|
| #749 (ratified) | quoted in [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) §1 | *"BitRouter is a self-improving LLM router, not an agent orchestrator."* |
| #786 (`86ee8283`, 2026-08-10) | commit message | Executed it: **19,955 lines** deleted — `apps/bitrouter/src/tui/` (11,323), `fleet.rs`/`fleet_mcp.rs` (2,812), `risk.rs`, 11 subagent MCP tools, the `tui` cargo feature and 9 optional deps, `tui.leader` config, and four TUI specs |
| `OBSERVABILITY_TUI_SPEC.md` §14 | still in tree | Four guardrails "learned from what killed the previous TUI — it accreted mutation surface (spawn, merge, apply, permission decisions) until it became an orchestrator": never writes config; never handles a secret; **no `stop` keystroke**; no per-request retry; **"if there is no CLI subcommand, there is no keystroke"** |
| `ACP_TUI_SPEC.md` §2 | non-goals | Single-session is the line; no fleets, subagents, worktrees, review queues, multi-pane splits |
| `ACP_TUI_SPEC.md` §8.3 | the scope rule | *"The TUI shows **session-scoped** data only. Anything daemon-wide belongs to [the status view]."* |
| `ACP_TUI_SPEC.md` §9 | the extraction trigger | Extract the crate on the first PR that *"adds a session list, or a second pane, or **a verb with no CLI equivalent**"*, or that *"**renders daemon-wide data in the TUI**"* |
| `ACP_TUI_PLAN.md` §C.1 → `crates/bitrouter-tui` | shipped | The extraction happened. The trigger is now a compiler check |
| `crates/bitrouter-tui/src/lib.rs` | module doc | *"The previous terminal UI died of accretion. It lived inside the application, so it could reach any function in it, and it grew verbs with no command-line equivalent until nobody could say what it was. That is still the failure this boundary prevents: what it stops is **reachability, not vocabulary**."* |
| `ACP_CONTROLLER_SPEC.md` §16.3 | 2026-09-01 | *"`bitrouter chat <harness>` remains on the existing single-session engine… TUI/controller convergence is separate product work."* |

**What full parity would and would not reverse.** Precision matters here, because
three of these rules are compatible with the stated goal and two are not.

- **§14's "no keystroke without a CLI subcommand" is *not* reversed.** It asserts
  TUI ⊆ CLI. Parity asserts TUI = CLI, which satisfies it. What parity destroys
  is the rule's *slack*: the containment was cheap to hold because the TUI was
  tiny, and the rule's purpose was to make every new interactive verb pay for a
  headless review first. At equality the rule stops constraining anything.
- **§14's "no `stop` keystroke" *is* reversed**, literally and by name, since
  `stop` is one of the 29.
- **§14's "never handles a secret" is reversed** by `key sign`, `providers
  login`, `cloud login`, and `cloud byok set`.
- **§14's "never writes config" is reversed** by `init` and `mcp add`.
- **§8.3's session-only scope is reversed** by `status`, `models`, `trajectory`,
  `observe`, `policy`, `cloud usage` — every daemon-wide or account-wide read on
  the list. §9 names exactly this as the trigger condition for a boundary
  violation.

The design in this spec **knowingly reverses §8.3, in a narrowed form**, and
reverses none of the others. The narrowing is in [§7](#7-the-membership-rule),
rule R4: daemon-wide data may be *asked for and shown once*; it may not be held,
polled, or put in the footer. That keeps §8.3's teeth — the thing it was
protecting against was a TUI that grows a second data model and a second pane —
while admitting the four questions §2.4 identified.

**This is a reversal the maintainer would be choosing knowingly.** The cost is
stated in [D1](#d1--amend-83-or-hold-it). It is a real cost: §8.3 is the only
scope rule the TUI has, and "transient, on request, never retained" is a softer
line than "never" and will need a test to mean anything (see
[§13](#13-the-guards), G4).

---

## 6. What other tools did

Researched from primary sources — maintainer statements, source, and issue
threads, not blog roundups. Findings that changed this design are marked **⇒**;
the rest is corroboration. §6.1–§6.8 survey general developer tools (findings
1–6); [§6.9](#69-the-agent-harness-peer-group) surveys the coding-agent peer
group (findings 7–14) and [§6.10](#610-the-in-repo-precedent-866) records the
in-repo precedent (finding 15).

### 6.1 Nobody achieved parity, and the two who tried are dead

| Pair | Outcome | Maintainer's own words |
|---|---|---|
| `git` / `lazygit` | **Deliberately not** | *"There's `gh dash` for that, and I'm not sure we should try and incorporate that into lazygit. **I'm a fan of tools that focus on one thing and do it well.**"* — stefanhaller, [#4950](https://github.com/jesseduffield/lazygit/issues/4950) |
| `kubectl` / `k9s` | **Partial, deliberately** | derailed closing the "create/apply objects" request at partial: *"Not able to create manifest as of yet, but now you can edit/view/apply/delete resources via k9s **without resorting to shelling out**."* — [#191](https://github.com/derailed/k9s/issues/191#issuecomment-653287650) |
| `docker` / `lazydocker` | **Deliberately narrower** | README scope is *"every **common** command"*, and states the contrast explicitly: *"lazydocker is more about managing existing containers/services, and docui is more about creating and configuring them."* |
| `gh` / `gh-dash` | **Partial by construction** | A *view* over `gh`. `gh pr create` / `gh issue create` are still absent; [#689](https://github.com/dlvhdr/gh-dash/issues/689) open |
| `psql` | **Different vocabulary** | See 6.2 |
| `git` / **`hub`** | **Tried parity. Repudiated by its author** | See 6.3 |
| aws-cli / **`aws-shell`** | **Tried parity. Abandoned** | 7.3k stars, dead since 2024; and the CLI grew `--cli-auto-prompt` natively, making the parity layer redundant |
| docker / **`docui`** | **Broader scope than lazydocker. Archived** | README: *"This repository is no longer maintenance. Please use lazydocker instead."* The tool that covered *more* of docker lost, head to head |

**⇒ Finding 1. The two projects that pursued surface parity produced the only
two obituaries in the set.** This is the evidence that cuts hardest against the
stated goal, so it leads.

### 6.2 psql: the interactive layer's vocabulary is mostly *net-new*

The brief's hypothesis is correct, and PostgreSQL's docs prove it three ways.
Meta-commands are defined by *who executes them* — *"processed by psql itself…
useful for administration **or scripting**"* — not by whether a human is
watching; they run fine under `-f`. The mapping to CLI flags is almost empty:

| Class | CLI twin? |
|---|---|
| Output formatting (`\a \H \t \x \pset \o`) | **Yes** — `-A -H -t -x -P -o`. The *only* true mirrors |
| Introspection — the ~30-command `\d*` family | **No.** Exactly one: `\l` ↔ `-l`. There is no `psql --describe-table`; the `-E` flag exists to *show* the catalog SQL `\d` generates |
| Session-buffer verbs (`\e \ef \p \r \s \errverbose`) | **No, and cannot be** — they operate on state that only exists in a live session |
| `\gexec` `\gset` `\crosstabview` `\gdesc` `\if/\elif/\endif` `\watch` | **No** — these are verbs psql *invented*. They are not the interactive version of anything |

k9s does the same thing: its `:` prompt accepts kubectl-shaped *nouns*
(`:pod`, `:svc`) but its *verbs* — `:xray`, `:pulses`, `:popeye`, `:dir`,
`:screendump`, `:aliases` — have no kubectl equivalent at all. nushell's
`explore` has a private `:` vocabulary (`:table`, `:try`, `:help`).

**⇒ Finding 2. The relation is not "TUI ⊆ CLI" with a gap. It is three disjoint
sets, and the third one is where the interactive layer earns its existence.**
BitRouter already has a set-C member and this spec initially mis-modelled it:
`route/set` and `route/reset` are session-scoped writes over a lease that *has
no headless subject*, because the CLI has no live ACP session to name. That is
not a parity shortfall to be fixed by inventing `bitrouter acp route set
--session <id>`; it is the normal shape. §3 and [D7](#d7--does-the-route-picker-need-a-cli-leaf)
are written against this finding.

### 6.3 `hub`: the only rigorous post-mortem of a parity layer

`hub` was built to be aliased over `git` — total surface parity by
construction. [Mislav Marohnić's retrospective](https://mislav.net/2020/01/github-cli/):

> *"Expanding the `git` command with new features may sound like a fun gimmick,
> but is in fact surprisingly hard to maintain… **your program needs to behave
> as git in every possible way, and every time it doesn't, you have a bug.**
> Over the years, hub had more than plenty of these."*
>
> *"I wouldn't even *consider* making a git proxy anymore… the way to improve
> git is to design better abstractions around it."*

GitHub then declined to build `gh` on it, explicitly *"without the assumption
that `hub` can be safely aliased to `git`"*
([`docs/gh-vs-hub.md`](https://github.com/cli/cli/blob/trunk/docs/gh-vs-hub.md)).

**⇒ Finding 3. Surface parity converts every behavioural difference into a bug
report, permanently.** This is the argument against
[§9 option D](#d-the-escape-hatch-bitrouter-leaf-args), the `/bitrouter <leaf>`
proxy, and it is stronger than the arguments I had. A proxy inherits an
obligation to be indistinguishable from the thing it proxies, in a terminal
where it demonstrably cannot be (raw mode, no exit code, no stdout).

### 6.4 tmux is the one real full-parity system, and it works by inversion

tmux has **one** command table used identically from the shell (`tmux
new-window`), from the command prompt (`:new-window`), from `bind-key`, and from
`.tmux.conf`. The man page states the seam exactly:

> *"tmux distinguishes between command parsing and execution… If a command is
> run from the shell, the shell parses it; from inside tmux or from a
> configuration file, tmux does."*

`struct cmd_entry` ([tmux.h](https://github.com/tmux/tmux/blob/master/tmux.h))
carries name, alias, `args_parse {template, lower, upper, cb}`, a usage string,
declarative target resolution, an `exec` fn — and a flags word:

```c
#define CMD_STARTSERVER     0x1   /* start the server if it isn't running */
#define CMD_READONLY        0x2   /* safe in a read-only client */
#define CMD_CLIENT_TFLAG   0x10   /* needs a target client, named by -t */
#define CMD_CLIENT_CANFAIL 0x20   /* may run with no client — degrade, don't error */
```

`cmdq_fire_command` resolves the client from the flag, falling back to inferring
it from the originating command queue when `-t` was not given, and erroring only
when `CANFAIL` is unset.

**⇒ Finding 4. tmux does not solve "this command needs a live session" by
excluding it from the scriptable set. It declares the requirement on the
registry entry, infers it when the caller did not supply it, and marks the
commands that may degrade.** That is a better shape than BitRouter's current ad
hoc `State::new(routable: bool)`, and [§10](#10-source-of-truth) adopts it as a
`requires` column.

The inversion is the other half: tmux's parity is real because the **shell is a
client of the interactive command language**, not because the interactive layer
mirrors a CLI. BitRouter's dependency runs the other way — the CLI is primary and
the session is a guest inside it — so tmux's result is not reachable by imitating
its surface. Only its *entry shape* transfers.

### 6.5 Every registry declares which callers may reach an entry

This is unanimous across five unrelated systems:

| System | The declaration |
|---|---|
| Emacs | `(interactive …)` makes a function a command at all; `command-modes` restricts it to a mode; `read-extended-command-predicate` is the pluggable filter |
| VS Code | `menus.commandPalette` `when: false` hides a command from the palette; `enablement` greys it; **neither gates `executeCommand`** — the programmatic path bypasses UI gating |
| Zed | actions are serializable structs in a global registry; a context-predicate *tree* gates dispatch, and the palette filters by whether the action is dispatchable in the current focus |
| Claude Code | `disable-model-invocation: true` and `user-invocable: false` — a per-entry statement of which *caller* may reach it |
| Vim | `-buffer` scopes a command to one buffer |

**⇒ Finding 5. `cli_leaf: Option<..>` / `mcp_tool: Option<..>` / `tui_command:
Option<..>` is the field-standard shape, and the `None` is a *declaration*, not
a gap.** `ACTIONS` already had this and the brief read the `None`s as a backlog.
Some of them are, and some are the Emacs/VS Code "not exposed here, on purpose."
The table should say which — see [§10](#10-source-of-truth).

Also worth carrying: **visibility and executability are separate axes** (VS Code
gates them with different fields). A BitRouter command may be listed-but-explained
when its port is unavailable, rather than hidden — which is a friendlier rendering
of the "absent, not dead" invariant than removal.

### 6.6 The shell-out escape hatch: universal, and its failure modes are documented

Every tool ships one — lazygit `customCommands` + a raw `:` shell prompt, k9s
`plugins.yaml`, lazydocker `customCommands` (its *built-ins* are the same
mechanism — 16 `commandTemplates`), gh-dash `keybindings`, psql `\!`. **None
uses it for its core loop.**

The failure modes, from the projects' own issue trackers:

- **Ambient state does not cross the process boundary, and you will forget a
  piece.** k9s's `runK()` manually re-serializes `--as`, `--as-group`,
  `--insecure-skip-tls-verify`, `--context`, `--kubeconfig` on every call. The
  community `apply` plugin in [#191](https://github.com/derailed/k9s/issues/191)
  shipped **without `$CONTEXT`** — silently applying to the wrong cluster — and
  was corrected three years later.
- **Quoting.** lazygit needed a `| quote` template filter and it is still not
  enough ([#5893](https://github.com/jesseduffield/lazygit/issues/5893)); gh-dash
  shipped a path-mangling regression ([#928](https://github.com/dlvhdr/gh-dash/issues/928));
  psql *refuses* to substitute a variable containing CR/LF because it cannot be
  quoted portably.
- **No structured output.** lazygit's `output:` field is a display destination,
  not a parse; its only post-hoc semantics is one hardcoded
  `after.checkForConflicts` boolean.
- **The config becomes an unversioned public API** you own forever
  ([lazygit #3651](https://github.com/jesseduffield/lazygit/issues/3651), and its
  deprecated-but-retained `SelectedLocalCommit` fields).
- gh-dash's own source shows the safe/unsafe split precisely: built-ins are
  `exec.Command("gh", args...)` — argv, no shell, no quoting bugs — while user
  keybindings are `$SHELL -c "<string>"`, which is where its bugs are.

**⇒ Finding 6. If BitRouter ever ships an escape hatch, it must be argv, not a
shell string, and it must re-inject session state explicitly.** But note that
BitRouter's situation is *strictly better* than any of these: it is a single
binary, so the "shell out to the CLI" tax — process spawn, quoting, state
re-serialization, unstructured output — is pure loss. The equivalent is a
function call. This is the argument that makes [§9 option C](#c-local-dispatch-through-the-same-action-ports--recommended)
obvious rather than merely preferable.

### 6.7 ACP specifically

- `available_commands_update` is documented *"Commands the agent can execute"*,
  and commands are invoked by sending their raw text as an ordinary
  `session/prompt` content block — *"Commands are included as regular user
  messages in prompt requests."*
- **There is no client→agent command advertisement, and no client-command
  capability.** `ClientCapabilities` is `auth.terminal`, `fs.*`, `terminal`,
  `elicitation`, `session.configOptions.boolean`. Nothing else.
- **ACP provides no namespace, no origin field, and no collision rule** for
  commands. Zed's answer is to keep `/` entirely for the agent and put its own
  affordances on other surfaces (`@`-mentions, mode and model pickers). Its
  ACP-agent command handling has open defects
  ([zed#53161](https://github.com/zed-industries/zed/issues/53161),
  [zed#53583](https://github.com/zed-industries/zed/issues/53583)).
- [`extensibility`](https://agentclientprotocol.com/protocol/extensibility):
  *"The protocol reserves any method name starting with an underscore (`_`) for
  custom extensions"*; custom fields at the root of a spec type are forbidden
  (`_meta` only); *"Implementations SHOULD use the `_meta` field in capability
  objects to advertise support for extensions and their methods."* BitRouter's
  `_meta["bitrouter.dev/controller"].routeControl` block is exactly conformant.
- **The counter-precedent, stated fairly:** `codex-acp` advertises `/status`,
  `/mcp`, `/skills`, `/compact`, `/logout` over `available_commands_update` and
  handles them locally, never sending them to the model. So "advertise host
  commands as agent commands and intercept them" is a shipped pattern. It is
  addressed in [§9 option A](#a-ride-availablecommandsupdate) — the reason it
  does not transfer is that `codex-acp` is the *terminus*, and BitRouter is not.
- **Claude Code's precedence table is what ACP lacks and what BitRouter needs:**
  enterprise > personal > project > bundled; plugin skills namespaced; synced
  skills yield to any existing name; MCP prompts may override. irssi is the
  cautionary tale — `command_bind` lets a script silently replace a builtin with
  no namespace at all.

### 6.8 The inverse principle, and whether it is symmetric

ESR's Rule of Separation — *"Separate policy from mechanism; separate interfaces
from engines"* — and the Rule of Composition are one-directional. **No source in
the design literature asserts the converse.** The asymmetry has a stated
rationale worth repeating in review:

1. *Cardinality.* The engine's surface is unbounded; screen and attention are
   not. Every interactive surface is a curated subset — that is the entire
   premise of a command palette.
2. *Direction of derivation.* "Everything in the UI exists in the engine" is a
   tautology if the architecture is right. The converse is an added constraint
   with no architectural payoff.
3. *Asymmetric cost of violation.* A UI action with no scriptable twin is an
   **automation dead end**. A CLI command with no UI presence is a
   **discoverability gap**, remediable by help and search.

The one real counter-pressure is discoverability (recognition over recall) — and
the field's answer to it is not menu presence but a *searchable registry*: `M-x`,
the VS Code palette, tmux `:`, `/commands`. That is what [phase 0](#phase-0--the-discoverability-defect-no-design-required)
builds, and it is the cheapest large win in this document.

Two more transferable primitives, noted without a recommendation:

- **fish** has no interactive/scriptable split at all. Interactive concerns
  (`funced`, `funcsave`, `fish_config`, `bind`, `abbr`) are ordinary commands in
  the ordinary namespace, and "am I interactive?" is a *runtime predicate* —
  `status is-interactive` — not a second vocabulary.
- **nushell's `explore --peek`** — *"When quitting, output the value of the cell
  the cursor was on"* — is the only clean **return path** found: a TUI handing a
  value back to the pipeline. gh-dash's author named its general absence as the
  field's missing primitive: *"it would be awesome to have the equivalent of the
  pipe (`|`) for TUIs."* Out of scope here; worth remembering.

### 6.9 The agent-harness peer group

§6.1–§6.8 surveyed general-purpose developer tools. The obvious objection is
that none of them is a coding-agent harness, which is the peer group BitRouter
actually competes in, and that the newer field might have solved what the older
one gave up on. It has not. This subsection is the check, done against primary
sources — vendor documentation, the harnesses' own source, and the protocol
repository — with counts derived here rather than quoted.

**Method and its limits, stated first.** Where a harness is open source (Codex
CLI, OpenCode, aider) the numbers below are **counted** from the registry in the
tree, at the commit reachable on the default branch on 2026-09-05, and the file
is named so the count can be re-run. Where it is not (Claude Code) nothing is
counted: the findings are qualitative and taken from published documentation and
one maintainer reply on a public issue. Anywhere a number is an estimate it says
so. Five harnesses are covered — the four in scope plus `aider`, reached with
time remaining. `pi` was not reached.

**The direct answer: no coding-agent harness in the survey has CLI/TUI command
parity in the direction the brief asked about** — every one of them has CLI
surface with no interactive expression, and in three of five that surface is
large. But the survey found both degenerate cases of the three-set topology, and
they are more instructive than the negative result:

- **Codex's set A is empty.** 59 interactive commands, 29 CLI subcommands, nine
  shared names, zero shared dispatch
  ([§6.9.2](#692-codex-cli--the-registry-lives-in-the-tui-crate-so-the-overlap-is-zero)).
- **aider's set C is empty.** All 43 chat commands are dispatchable headlessly,
  through one dispatcher with three callers
  ([§6.9.7](#697-aider--set-c-is-empty-and-the-cli-is-a-client-of-the-command-language)).
- **OpenCode is in between and shows the mechanism**: a shared `session.command`
  port under both surfaces, with the registry in the server package
  ([§6.9.3](#693-opencode--the-one-that-got-closest-and-the-inversion-that-did-it)).

**The two harnesses with real cross-surface sharing both got there by the same
inversion that makes tmux work** ([§6.4](#64-tmux-is-the-one-real-full-parity-system-and-it-works-by-inversion)):
the command registry is underneath both surfaces, and the headless caller is a
*client* of the interactive vocabulary rather than its parent. That is the
finding worth carrying, and BitRouter reached it independently for one verb in
#866 ([§6.10](#610-the-in-repo-precedent-866)).

#### 6.9.1 Claude Code — a documented terminal-only class, and a shared class that is not commands

Claude Code is closed-source, so its command registry cannot be walked. What
*can* be established is stronger than a count anyway, because the vendor
documents the split in its own words.

**The vendor names an interactive-only class.** The skills documentation
describes `/help` and `/feedback` as *"terminal-only built-in commands"*, and
records what happens to their names headlessly — they are *"not reserved in
non-interactive sessions, so plugin skills with these names keep their bare
commands in headless mode"* — while *"other terminal-only built-ins like
`/login`"* keep their names *"reserved even in non-interactive sessions **where
commands can't run**"*
([code.claude.com/docs/en/slash-commands](https://code.claude.com/docs/en/slash-commands)).
That is set C, named as a class, by the maintainer, with a documented consequence
for name resolution. `/skill-doctor` is documented the same way and more
bluntly: *"not available in non-interactive mode with the `-p` flag"*.

**But the split is not interactive-versus-headless. It is built-in versus
prompt-expansion.** The clearest statement is a maintainer reply on
[anthropics/claude-code#837](https://github.com/anthropics/claude-code/issues/837)
(*"use slash commands in print/headless/non-interactive mode"*, opened
2025-04-21, closed). The request was for `claude -p "/cost"`. A user reported
that `claude -p /project:my-custom-command` works, and `bcherny` — a repository
collaborator — replied in full:

> *"yep, this is supported"*

So user-defined and skill-backed commands **are** shared across both surfaces,
because they are prompt text and the headless mode takes a prompt. The built-in
commands are not, because they are not prompt text. **The line is drawn by
implementation kind, not by mode**, and it lands where §2.4 predicted: the
commands that need the process's own state — permissions, config, panels — are
the ones that do not cross.

⇒ **Finding 7. The nearest peer's interactive-only set is real, documented, and
named as such — and the boundary it is drawn on is "does this expand to a
prompt", not "is a human watching".** For BitRouter this is the reassuring
result and also the limiting one: BitRouter has no prompt-expansion class of its
own (the harness owns that), so it has no cheap path to a shared set and every
member of set A has to be built like [§9 option C](#c-local-dispatch-through-the-same-action-ports--recommended)
builds it.

**Two documented pairs are worth more than the class boundary, because they show
what happens to names that exist on both sides.**

| Pair | What the documentation says |
|---|---|
| `claude doctor` / `/doctor` | *"Print **read-only** installation and settings diagnostics from the terminal without starting a session… For the in-session setup checkup **that can also apply fixes**, run `/doctor`"* ([CLI reference](https://code.claude.com/docs/en/cli-reference)) |
| `claude mcp login <name>` / `/mcp` | *"Run a configured MCP server's OAuth flow **without opening the interactive `/mcp` panel`**"* (same page) |

The first is a shared **name** whose two members are deliberately *different
actions* — the headless one is read-only, the interactive one can write. Under
this spec's P1 that pair is a violation, not a success, and command-count parity
would have scored it as a win. It is the cleanest external illustration of why
P1 is phrased as *"the two answer with the same typed report"* rather than
*"both surfaces have the command"* ([§3](#3-the-goal-restated)).

The second is a set-C member migrating to set A **because the interactive
affordance was in the way of automation** — the leaf exists precisely to escape
a panel. That is [§6.8](#68-the-inverse-principle-and-whether-it-is-symmetric)'s
"automation dead end" being remediated in public, and it is the shape BitRouter
should expect if `/route`'s picker ever needs a headless twin ([D7](#d7--does-the-route-picker-need-a-cli-leaf)):
the trigger will be a script that cannot get past the modal, not a desire for
symmetry.

**The headless-only side is mostly flags, not commands.** The CLI reference marks
`--init`, `--json-schema`, `--max-budget-usd`, `--max-turns`,
`--no-session-persistence` and `--permission-prompts` as *"print mode only"*.
None of these is a command; each is a flag with no interactive expression at all.
That is [§2.1](#21-command-is-ambiguous-and-both-readings-fail)'s objection
appearing in the nearest peer: the unit "command" does not partition the surface,
and a parity rule stated over commands would score a harness as complete while
six behaviours remain reachable from exactly one side.

**On the maintainer's own first-hand claim** — that `/permissions`, `/config`,
`/doctor` and `/hooks` are interactive-only, and `--print` and `claude mcp add`
headless-only — the published documentation **partly refutes it**:

- **`/doctor` is wrong.** `claude doctor` is a documented CLI command. The pair
  exists; it is the *semantics* that differ, as above.
- **`/permissions` and `/hooks` hold as stated for commands**, with a caveat
  that matters: their headless equivalents exist as *flags and settings files*
  (`--permission-mode`, `--allowedTools`, `--disallowedTools`, `--settings`,
  and `settings.json` hooks), not as commands. "Interactive-only" is true of the
  command and false of the capability.
- **`/config` could not be verified either way.** The published CLI reference
  contains no `claude config` entry, which is consistent with the claim, but
  absence from a reference is not a statement of intent and no maintainer source
  was found. Recorded as unverified.
- **`--print` holds** — *"Print response without interactive mode"*.
- **`claude mcp add` holds as a command**, though `/mcp` covers part of the same
  ground interactively, so this is a partial overlap rather than a clean
  headless-only.

The claim's *shape* survives — all three sets are non-empty in Claude Code — but
two of its five examples were imprecise, and the correction is the useful part:
**the interesting boundary is not the mode, it is whether the command's answer
comes from the process's own state.**

**Not established for Claude Code:** any command count on either side, and
therefore any overlap ratio. The prior figure of "roughly 7 shared names out of
~130" was carried into this work unsourced; **no attempt to reproduce it
succeeded**, the documentation publishes no complete built-in command list, and
the source is closed. It is not repeated here and should not be cited.

#### 6.9.2 Codex CLI — the registry lives in the TUI crate, so the overlap is zero

Codex is open source, so this one is counted. All figures below are from
[`openai/codex`](https://github.com/openai/codex) at the default branch on
2026-09-05 and can be re-derived from two files.

| Measure | Value | Source |
|---|---|---|
| Built-in slash commands | **59** | `enum SlashCommand`, [`codex-rs/tui/src/slash_command.rs`](https://github.com/openai/codex/blob/main/codex-rs/tui/src/slash_command.rs) — counted variants |
| Top-level CLI subcommands | **29** | `enum Subcommand`, [`codex-rs/cli/src/main.rs`](https://github.com/openai/codex/blob/main/codex-rs/cli/src/main.rs) — counted variants, including three `hide = true` internal ones |
| Names present on both sides | **9** | `agents`, `app`, `archive`, `delete`, `fork`, `logout`, `mcp`, `resume`, `review` |
| Union of names | **79** | 50 TUI-only, 20 CLI-only |

Those are counted, not estimated. The one caveat is that "leaves" are not
counted on the CLI side — nine of the 29 are namespaces (`mcp`, `plugin`,
`debug`, `app-server`, `execpolicy`, `cloud`, `features`, `login`, `exec`), so
the true leaf count is higher and the *ratio* would be worse, not better.

**The nine shared names are shared in name only, and the reason is structural:
every file in the repository that mentions `slash_command` is under
`codex-rs/tui/`.** A search of the repository for that symbol returns 30 paths;
all 30 are in the TUI crate, and none is in `codex-rs/exec/`. `codex exec` — the
documented non-interactive mode — has no slash-command dispatch at all. So for
Codex the three sets are:

```
A. both surfaces        B. CLI-only              C. interactive-only
────────────────        ───────────              ───────────────────
(empty — 9 names,       20 subcommands           59 slash commands
 0 shared dispatch)     (exec, serve, doctor,    (model, permissions,
                        update, apply, …)         status, usage, …)
```

⇒ **Finding 8. Set A can be empty.** Codex is the counter-example to any
assumption that a mature harness necessarily has a shared middle. Its
interactive vocabulary is twice the size of its CLI's and structurally
unreachable from it, and nine names collide across the divide without sharing a
line of code. The topology in [§3](#3-the-goal-restated) survives this — three
sets, one of which happens to be empty — but the *stated goal* does not: a
project can ship 59 interactive commands and 29 headless ones with zero
intersection and be, by every other measure, the most successful harness in the
field.

**What Codex declares on each registry entry is the finding that transfers.**
`SlashCommand` carries four per-entry predicates, and they are exactly the shape
[§6.5](#65-every-registry-declares-which-callers-may-reach-an-entry) found
everywhere else and [§6.4](#64-tmux-is-the-one-real-full-parity-system-and-it-works-by-inversion)
found in tmux's `cmd_entry` flags word:

| Predicate | What it declares | Entries |
|---|---|---|
| `supports_inline_args()` | the command takes text after its name | 20 of 59 |
| `available_during_task()` | it may run while a turn is in flight | a hand-written match over **every** variant, no `_` arm |
| `available_in_side_conversation()` | it survives in an ephemeral fork | 10 of 59 |
| `is_visible()` | platform and build gating (`cfg!(target_os = …)`, `cfg!(debug_assertions)`) | 5 special cases |

`available_during_task` is the one to look at closely: it is written as an
exhaustive match with no catch-all, so **adding a variant does not compile until
the author decides whether it is safe mid-turn.** That is a compiler-enforced
version of the `requires` column [§10](#10-source-of-truth) proposes, obtained
without a test, and it is a better mechanism than the one this spec sketched. It
is also direct empirical support for R2 and R4: the harness with the largest
interactive vocabulary in the survey found it necessary to declare, per command,
*when* it may run — and 21 of its 59 commands are marked unavailable while the
agent is working.

**The `codex-acp` counter-precedent, checked and updated.**
[§6.7](#67-acp-specifically) and [§9 option A](#a-ride-availablecommandsupdate)
rest on `codex-acp` advertising host commands over
`available_commands_update`. That is still true, but two facts have changed and
one of them matters:

1. **The repository the spec cited is archived.** `zed-industries/codex-acp` is
   archived as of 2026-07-22 with a README pointing at
   `agentclientprotocol/codex-acp`: *"Development has moved… The new adapter is
   built on the new Codex App Server, and we are pooling implementation and
   maintenance work across teams there."* The pattern was not abandoned; it was
   handed to the protocol organisation, which if anything strengthens it as a
   precedent.
2. **The advertised set is now nine, not five.** The current adapter advertises
   `/status`, `/mcp`, `/skills`, `/goal`, `/review`, `/review-branch`,
   `/review-commit`, `/compact` and `/logout`, *"as well as configured skills"*.
   Every one of the nine is a Codex **TUI** command with no `codex` subcommand —
   so the adapter is re-exposing set C over ACP, which is exactly the move
   [§9 option A](#a-ride-availablecommandsupdate) contemplates for BitRouter.

**And the adapter's implementation is the strongest available evidence for
§9's rejection of option A**, because it had to build, by hand, all three of the
things the option-A objections said ACP does not supply
([`src/CodexCommands.ts`](https://github.com/agentclientprotocol/codex-acp/blob/main/src/CodexCommands.ts)):

- **A prompt-stream string matcher.** `parseCommand()` takes the first content
  block, requires `type == "text"`, trims, checks `startsWith("/")`, splits on
  `/\s+/`, lowercases the name, and hands the remainder on as `rest`. This is
  the `if line == "/route"` compare, relocated into the hot path of every turn —
  precisely what §9 objection 1 predicted a middlebox would have to write.
- **A namespace, invented at the adapter layer.** Skills are advertised under a
  `$` sigil (`name = "$" + skill.name`) and the dispatcher's first act is
  `if (commandName.startsWith("$")) return { handled: false }` — sigil means
  "not mine, forward it". ACP supplies no such rule; the adapter had to mint one.
- **A collision rule, also invented.** `buildAvailableCommands()` seeds a `Map`
  with built-ins and then skips any skill whose name is already present —
  first-writer-wins, built-ins beat skills. Compare
  [§6.9.3](#693-opencode--the-one-that-got-closest-and-the-inversion-that-did-it):
  OpenCode's documented rule is the **opposite** (*"Custom commands can override
  built-in commands"*). Two implementations of the same protocol, two
  incompatible precedence rules, neither wrong, because the protocol has no
  opinion.
- **An out-of-band argument channel.** Two entries carry
  `_meta.commandAction` — `{kind: "setConfigOption", configId, value, resetValue,
  presentation}` for `/plan`, `{kind: "prefixPrompt", presentation: "state"}` for
  `/goal` — because `AvailableCommand` has nowhere else to put a typed action.
  §9 objection 2 said the field cannot carry arguments; the field's most
  invested consumer agrees and routes around it through `_meta`.

⇒ **Finding 9. Option A is not hypothetically awkward — the reference
implementation of it has already had to hand-write a prompt-stream parser, a
namespace sigil, a precedence rule and an `_meta` side-channel, and its
precedence rule disagrees with the other major harness's.** This is
corroboration of §9's rejection rather than a reason to revisit it, and it makes
[§9 objection 3](#a-ride-availablecommandsupdate) concrete: the missing
collision rule is not a theoretical gap, it has already been filled twice,
differently.

#### 6.9.3 OpenCode — the one that got closest, and the inversion that did it

OpenCode is the answer to *"has anyone actually built this?"*, and the answer is
**yes, partially, and by inverting the dependency the same way tmux does.** It is
the most valuable finding in this subsection and the only one that argues
*against* any part of this spec.

Counted from [`sst/opencode`](https://github.com/sst/opencode) at the default
branch (`dev`) on 2026-09-05, plus the published CLI and TUI references:

| Measure | Value | Source |
|---|---|---|
| Top-level CLI commands | **~24** (28 files, some shared helpers) | [`packages/opencode/src/cli/cmd/`](https://github.com/sst/opencode/tree/dev/packages/opencode/src/cli/cmd) — counted files; leaf count is higher (`auth`, `agent`, `mcp`, `github`, `session`, `db`, `debug` are namespaces) |
| TUI built-in slash commands | **17** | [opencode.ai/docs/tui](https://opencode.ai/docs/tui/) — counted rows, aliases folded in |
| Built-ins in the **server** command registry | **2** (`init`, `review`) | `Default` in [`packages/opencode/src/command/index.ts`](https://github.com/sst/opencode/blob/dev/packages/opencode/src/command/index.ts) |
| Registry entries beyond those two | every user `command` config entry, every MCP prompt, every skill | same file: `source: "command" \| "mcp" \| "skill"` |

**The mechanism, which is the point.** OpenCode's server exposes a method whose
entire job is invoking a command by name, and the headless CLI is one of its two
callers. From [`packages/opencode/src/cli/cmd/run.ts`](https://github.com/sst/opencode/blob/dev/packages/opencode/src/cli/cmd/run.ts),
whose header comment says it *"Also supports `--command` for slash-command
execution"*:

```ts
.option("command", { describe: "the command to run, use message for args", type: "string" })
…
if (args.command) {
  const result = await client.session.command({
    sessionID, agent, model: args.model,
    command: args.command,          // the command name
    arguments: message,             // everything else, as one string
    variant: args.variant,
  })
```

So `opencode run --command review HEAD~1` and typing `/review HEAD~1` in the TUI
reach the **same** `session.command` endpoint with the same two fields. That is
not name parity; it is dispatch parity, and it is the only instance of it found
in an agent harness.

⇒ **Finding 10. Parity is achievable in this field, and the way to get it is
[§6.4](#64-tmux-is-the-one-real-full-parity-system-and-it-works-by-inversion)'s
inversion: put the command registry underneath both surfaces and make the
headless CLI a client of it, rather than making the interactive layer mirror a
CLI.** OpenCode's registry lives in the server package, not the TUI package;
`session.command` is a server method; the TUI and `opencode run` are peers over
it. tmux's result reproduced, twenty-five years later, in a coding agent.

**[Decided by the maintainer, 2026-09-05 — BitRouter takes this inversion.]**
The registry goes underneath both surfaces and the headless CLI becomes a client
of the same dispatch. What is *not* taken is the openness that the table two rows
above measures — OpenCode's registry is open data tagged with `source`, and
`ACTIONS` stays a closed `const` table with exhaustive guards. The decision, and
what the closedness costs [D2](#d2--the-user-configured-escape-hatch), are in
[D12](#d12--take-693s-inversion).

**And it is still not full parity, in both directions.** All three sets are
non-empty:

```
A. both surfaces              B. CLI-only                 C. TUI-only
──────────────────            ───────────                 ───────────
init, review, and every       serve, web, attach, auth,   compact, details,
user / MCP / skill command    agent, github, mcp, stats,  editor, exit, help,
— shared through              import, session, upgrade,   new, redo, undo,
session.command               uninstall, pr, plugin, db,  sessions, share,
                              debug, acp, account,        unshare, themes,
2 built-ins + N user          providers, generate, tui    thinking, connect
                              ~22                         ~15
```

Set C survives even here, and its membership is the tell: `/editor`, `/themes`,
`/thinking`, `/details`, `/exit`, `/help` are **renderer** verbs. They cannot be
headless because there is no renderer to address. That is [§6.2](#62-psql-the-interactive-layers-vocabulary-is-mostly-net-new)'s
finding reproduced in the newest tool in the survey: the interactive layer keeps
a private vocabulary about *itself*, and no amount of registry sharing removes
it.

**Two name pairs that are not action pairs**, matching Claude Code's `doctor`
case: `opencode export` *"Export session data as JSON"* against `/export`
*"Export current conversation to Markdown and open in your default editor"*; and
`opencode models` against `/models`, where the CLI prints a filtered list and the
TUI opens a picker. Both are shared names with different outputs and neither goes
through `session.command`. **In every harness surveyed, the shared names that
were not built on a shared port have drifted** — which is the empirical form of
P1's insistence on *"the same typed report"*.

**The collision rule is documented and it is the opposite of `codex-acp`'s.**
[opencode.ai/docs/commands](https://opencode.ai/docs/commands/): *"Custom
commands can override built-in commands. If you define a custom command with the
same name, it will override the built-in command."* Last-writer-wins, user beats
built-in. `codex-acp` seeds built-ins first and skips colliding skills —
first-writer-wins, built-in beats user ([§6.9.2](#692-codex-cli--the-registry-lives-in-the-tui-crate-so-the-overlap-is-zero)).
Claude Code publishes a four-level precedence table
([§6.7](#67-acp-specifically)). Three harnesses, three different answers, and the
protocol they share has none — which is why [§9 option A](#a-ride-availablecommandsupdate)
would be inventing one in a field spelled *"commands the agent can execute"*.

**The one thing OpenCode's design costs, and it is the cost §6.6 predicted.**
`run.ts` rebuilds the argument string by hand before sending it:

```ts
let message = [...args.message, ...(args["--"] || [])]
  .map((arg) => (arg.includes(" ") ? `"${arg.replace(/"/g, '\\"')}"` : arg))
  .join(" ")
```

An argv array is re-joined into one string with hand-rolled quoting, because
`session.command`'s `arguments` field is a single string. That is
[§6.6](#66-the-shell-out-escape-hatch-universal-and-its-failure-modes-are-documented)'s
quoting failure mode — lazygit's `| quote` filter, gh-dash's path mangling —
appearing inside the *shared port*, not at a shell boundary. **The lesson for
[D9](#d9--argument-grammar) is precise: a shared dispatch port does not remove
the argument-grammar problem, it relocates it to the port's signature.** If
BitRouter's `SessionActions::run(id, args)` takes an untyped string, it inherits
this bug the day a second argument appears.

#### 6.9.4 ACP's own position on command ownership

This is the item that decides [§9](#9-the-mechanism), so it is sourced from the
protocol repository rather than the rendered site. Quotations are from
[`docs/protocol/v1/slash-commands.mdx`](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/docs/protocol/v1/slash-commands.mdx)
and its v2 sibling, read on 2026-09-05.

**The ownership question has an unambiguous answer, stated in the first
sentence:**

> *"**Agents** can advertise a set of slash commands that users can invoke."*

and reinforced at the invocation end:

> *"Commands are included as regular user messages in prompt requests… **The
> Agent recognizes the command prefix and processes it accordingly.**"*

**What the page does not contain is as load-bearing as what it does.** Read in
full, across both protocol versions, there is:

- no client→agent command advertisement, and no client-command capability;
- no origin, owner, or provenance field on `AvailableCommand` — it is
  `{name, description, input?}` and nothing else;
- no namespace and no collision rule, in either version;
- no invocation method. `available_commands_update` is a notification;
  `session/prompt` is the only channel a command can travel on.

⇒ **Finding 11. ACP specifies command ownership one way — the agent owns
commands, the client renders and forwards them — and specifies nothing at all
about a third party in the middle.** BitRouter is that third party. There is no
conformant reading of the specification in which a middlebox contributes
commands to the list it forwards; there is only a reading in which it *is* the
agent, which is true of BitRouter's controller and false of the harness behind
it. That is [§9 objection 1](#a-ride-availablecommandsupdate) restated from the
protocol's side rather than from BitRouter's, and it is the single strongest
reason not to take option A.

**v2 exists, and it changes exactly one thing here.** The spec was measured
against schema 1.7 ([§1](#1-verified-starting-state)); `docs/protocol/v2/` is now
in the tree. Its migration table records the delta in one line: *"`available_
commands_update` | **Kept.** Command `input` now carries a required `type`
discriminator"*. `AvailableCommandInput` becomes a tagged union whose only
stable member is `type: "text"` with the same `hint`, plus an extension rule:

> *"Custom input types **MUST** begin with `_`; unknown non-underscore input
> types are reserved for future ACP variants. Clients that cannot render an
> input specification should preserve it when storing, replaying, **proxying, or
> forwarding** command metadata, and otherwise display the command without
> structured input."*

Two consequences, and the second is the one to act on:

1. **§9 objection 2 weakens slightly but does not fall.** A `_bitrouter/…`
   input type is now a sanctioned way to declare a richer argument shape on a
   command entry, so "one `hint` string and nothing else" is no longer the whole
   story. It is still true that *stable* ACP declares strictly less than Emacs,
   Vim, tmux or Claude Code, and still true that inventing a private input type
   is inventing a grammar — but the protocol now has a door for it, and
   `codex-acp` is already using the neighbouring `_meta.commandAction` door
   ([§6.9.2](#692-codex-cli--the-registry-lives-in-the-tui-crate-so-the-overlap-is-zero)).
2. **The "proxying, or forwarding" clause is addressed to BitRouter's exact
   position** and imposes a duty: a middlebox must **preserve** command metadata
   it does not understand. That is an argument for BitRouter forwarding the
   harness's `available_commands_update` untouched, which is what
   [phase 0](#phase-0--the-discoverability-defect-no-design-required) already
   does by rendering BitRouter's own commands as a separate labelled group. The
   design was right for a reason it did not know about.

**ACP's answer to "the interactive layer needs a model picker" is not a command,
and this is new information for [D8](#d8--route-overloading) and
[§9](#9-the-mechanism).** The question was raised as
[agentclientprotocol#77](https://github.com/agentclientprotocol/agent-client-protocol/issues/77)
(*"Extend ACP to allow other features like /slashcommands"*, 2025-09-09, closed),
by a maintainer of a multi-provider Gemini CLI fork who wanted `/profile load
<name>` and `/model <name>` to work inside Zed. `ConradIrwin`, replying as a
repository contributor:

> *"I think selecting the provider/model is a **concrete enough concept we
> should have our own types for it** (in the Zed UI, it's a separate dropdown)."*

and, on the slash-command route:

> *"I guess slash commands in general we have some experimental support for for
> Claude… Right now all our setup/config is done out of band in JSON files."*

The issue was closed with *"Slash Commands and Session Modes are now stable"*,
and the model question moved to its own thread. What it became is
`session/set_config_option` and the `configOptions` list, which in v2 absorbed
modes entirely (*"`current_mode_update` — **Removed**. Modes are config options"*).
The stable schema now carries, as a first-class typed selector:

```json
{ "configId": "model", "name": "Model", "category": "model",
  "type": "select", "currentValue": "model-1", "options": [ … ] }
```

⇒ **Finding 12. The protocol deliberately routes model and mode selection
*away* from the command channel and into a typed, categorised config option that
the client renders as a picker — and an ACP maintainer said so explicitly while
declining the slash-command version of the request.** BitRouter's `/route` is
that request, exactly: a session-scoped selection among models, currently
implemented as a private `_bitrouter/route/*` extension because this spec's §1
measurement predates knowing that `category: "model"` exists in the standard
surface.

**This does not change §9's recommendation, and it is important to say why
not.** `configOptions` is an *agent→client* declaration: the agent offers the
options, the client shows a picker, the client calls `session/set_config_option`.
BitRouter's controller is the agent in that relationship, so it *could* advertise
its route as `configId: "route", category: "model"` and get the TUI's picker for
free — but only for the picker. `status`, `models` and the route *preview* are
reads with reports, not selections among enumerated values, and none of them
fits a `select`. So the honest reading is: **§9's option C remains right for the
four read commands, and a fifth option now exists for `route/set` alone** —
retire the private extension in favour of the standard config-option channel.
That is a real design question this spec did not previously know to ask, and it
is recorded as [D11](#d11--should-routeset-become-an-acp-config-option) rather
than decided here.

**One further v2 signal about who contributes what.** v2 removes
`clientCapabilities.terminal`, `fs/*` and the `terminal/*` methods, with a stated
rationale: *"Clients that want to expose file access, unsaved editor state, or
command execution to agents should do so by providing an **MCP server** to the
session."* The protocol's answer to "the client-side wants to contribute
capability" is MCP, not commands — and BitRouter already ships an MCP surface
over the same `ACTIONS` rows ([§10](#10-source-of-truth)). The two halves of the
answer are consistent: commands belong to the agent, contributed capability
belongs on MCP, and nothing in ACP is shaped like "a middlebox adds a slash
command".

#### 6.9.5 What recurs in set C, across four harnesses

[§7](#7-the-membership-rule)'s R1–R5 were derived from BitRouter's own
constraints and then checked against the general-tool survey. This is the same
check against the peer group, and it is the closest thing to an *empirical*
basis the rule has. Every entry below is a command that exists interactively and
has no headless twin in that harness.

| Category | Claude Code | Codex | OpenCode | psql / k9s ([§6.2](#62-psql-the-interactive-layers-vocabulary-is-mostly-net-new)) |
|---|---|---|---|---|
| **Renderer / display** | fullscreen, transcript | `theme`, `pets`, `statusline`, `title`, `raw`, `vim`, `keymap` | `/themes`, `/details`, `/thinking`, `/editor` | psql `\pset`-family beyond the flags |
| **Conversation buffer** | `/compact`, `/context`, `/clear` | `compact`, `recap`, `clear`, `new`, `copy`, `diff` | `/compact`, `/undo`, `/redo`, `/new` | psql `\e`, `\p`, `\r`, `\s` |
| **Permission / approval** | `/permissions` | `permissions`, `approve` | — (config only) | — |
| **Config editing** | `/config`, `/hooks` | `experimental`, `memories`, `hooks`, `debug-config` | `/connect` | — |
| **Session / model switching** | `/model`, `/cd`, `/add-dir` | `model`, `personality`, `plan`, `side`, `cd`, `mention` | `/sessions`, `/models`, `/share` | k9s `:xray`, `:pulses` |
| **Session lifecycle in place** | — | `quit`, `exit`, `logout`, `stop` | `/exit` | psql `\q` |

Four categories recur in **all four** harnesses: renderer verbs, conversation-
buffer verbs, session/model switching, and lifecycle-in-place. Permission and
config editing recur in three of four. `aider` is absent from the table because
its set C is empty
([§6.9.7](#697-aider--set-c-is-empty-and-the-cli-is-a-client-of-the-command-language))
— but note that it has the same *verbs* (`/editor`, `/voice`, `/multiline-mode`,
`/copy`, `/paste`, `/clear`, `/undo`, `/exit`); they are dispatchable headlessly
and simply have nothing to act on there. Emptiness of set C is a property of the
dispatcher, not of the vocabulary.

⇒ **Finding 13. Set C's membership is not arbitrary and it is not per-project.
Every recurring category is a verb whose *subject is the session's own
apparatus* — the renderer, the buffer, the pending permission, the next turn's
model — none of which exists outside a live session.** That is R1 restated from
observation instead of derivation, and it is the same criterion §6.2 extracted
from psql's `\e`/`\r`. R1 can now be described as measured across six tools in
two generations rather than reasoned from BitRouter's shape.

**Two corrections to the rule fall out of the table.**

- **Permission and approval belong in set C by nature, not by exception.** Two of
  the four harnesses put an interactive permission verb there and none has a
  headless command twin — the headless answer is always a *policy stated up
  front* (Claude Code's `--permission-mode` / `--allowedTools`, Codex's
  `--sandbox` / `--full-auto`, OpenCode's `--auto`). [§6.10](#610-the-in-repo-precedent-866)
  is BitRouter arriving at the same answer independently, and R3 should be read
  as consistent with it rather than as forbidding it.
- **Config editing is set C in the harnesses and set B here, and that difference
  is deliberate.** Three of four harnesses expose an interactive config editor;
  R3 forbids one outright, following `OBSERVABILITY_TUI_SPEC.md` §14's *"never
  writes config"*. The peer group is evidence that the *demand* is real, not that
  the rule is wrong — §14's rule exists because the previous TUI died of exactly
  this accretion ([§5](#5-the-prior-record-and-what-parity-would-reverse)). Worth
  knowing that BitRouter is the outlier; not worth changing on that basis.

#### 6.9.6 Argument grammar, as four harnesses actually solved it

[D9](#d9--argument-grammar) was, before this survey, an invented choice between
`split_whitespace`, clap re-entry, and pickers. None of the four does clap
re-entry. Here is what they do:

| Harness | Grammar | Declaration on the entry |
|---|---|---|
| Claude Code | `$ARGUMENTS`, `$ARGUMENTS[N]`, `$N`, and **named** `$name` args declared in frontmatter | `arguments: [issue, branch]` plus `argument-hint` for autocomplete |
| Codex TUI | name, then the rest of the line, hand-parsed per command | `supports_inline_args() -> bool` — a **boolean**, 20 of 59 |
| `codex-acp` | `text.slice(1).split(/\s+/)`, `rest` hand-parsed per command, per-command usage errors | ACP's `input.hint`, plus `_meta.commandAction` for typed actions |
| OpenCode | `$ARGUMENTS` and `$1..$N` substituted into the command's template | `hints: string[]`, **derived by regex over the template body** |

Three observations, in ascending order of usefulness:

1. **Nobody re-enters their own argument parser.** D9 option (b) has no adopters
   in the peer group, which is weak evidence but all there is.
2. **Everybody declares argument shape on the registry entry**, which is
   [§6.5](#65-every-registry-declares-which-callers-may-reach-an-entry)'s finding
   holding without exception in the newer generation. The declarations range from
   a bare boolean (Codex) to named typed arguments (Claude Code); OpenCode's is
   the interesting middle — `hints` is *computed from the body*, so the entry
   cannot declare an argument the template does not use.
3. **The two harnesses with a real grammar are the two whose commands are
   prompt templates.** `$ARGUMENTS` and `$1` are substitutions into text, which
   is a grammar you get for free when the command *is* text. BitRouter's commands
   are not text — they are typed calls returning typed reports — so this is the
   one place the peer group's answer does **not** transfer, and D9 cannot borrow
   it.

⇒ **Finding 14. D9's recommendation (defer, lean toward pickers, declare the
argument kind on the row rather than writing a parser per command) survives the
survey unchanged, and gains one concrete warning:** OpenCode's shared port takes
`arguments` as a single joined string and its CLI hand-quotes argv back into
that string ([§6.9.3](#693-opencode--the-one-that-got-closest-and-the-inversion-that-did-it)).
**Whatever `SessionActions::run` takes, it must not be one string.** A `Vec<String>`
or a typed per-action request costs nothing today and forecloses the one bug the
only shared-port implementation in the field actually has.

#### 6.9.7 aider — set C is empty, and the CLI is a client of the command language

`aider` was outside the original scope and was reached with time remaining. It is
the most important thing in this subsection, because it is the **counter-example
the brief asked for**: a coding-agent harness where the entire interactive
command vocabulary is reachable headlessly.

Counted from [`Aider-AI/aider`](https://github.com/Aider-AI/aider) on 2026-09-05:

| Measure | Value | Source |
|---|---|---|
| Chat commands | **43** | `def cmd_*` methods in [`aider/commands.py`](https://github.com/Aider-AI/aider/blob/main/aider/commands.py) — counted |
| Of those, reachable headlessly | **43** | one dispatcher, three callers |
| Dispatcher | `Commands.run(inp)` → `matching_commands()` → `do_run(command, rest)` | same file, `:312` |

**Three callers, one dispatcher.** The interactive prompt loop is one.
`aider --message "/add src/foo.py"` is the second: `main.py` does
`coder.run(with_message=args.message)`, and the coder's run path routes anything
starting with `/` or `!` through `Commands.run`. `aider --load <file>` is the
third, and it is a **command script**: `cmd_load` reads the file, skips blanks
and `#` comments, and calls `self.run(cmd)` per line
([`commands.py:1465`](https://github.com/Aider-AI/aider/blob/main/aider/commands.py)).

The tell that this is deliberate rather than incidental is one line in `main.py`:

```python
if args.apply_clipboard_edits:
    args.edit_format = main_model.editor_edit_format
    args.message = "/paste"
```

**A CLI flag implemented by injecting a slash command into the headless message
channel.** The command language is the primary surface and the flag is sugar over
it — [§6.4](#64-tmux-is-the-one-real-full-parity-system-and-it-works-by-inversion)'s
inversion, stated in three lines, in a coding agent.

⇒ **Finding 16. Set C can be empty too, and the harness where it is empty is the
one whose CLI is a client of its interactive command table rather than its
parent.** Codex and aider are the two extremes of the same axis:

| Harness | Set A | Set B | Set C | What decides it |
|---|---|---|---|---|
| Codex | **empty** | 20 subcommands | 59 commands | registry lives in the TUI crate |
| Claude Code | prompt-expansion commands | flags + `claude <verb>` | terminal-only built-ins | registry is split by implementation kind |
| OpenCode | `init`, `review`, user/MCP/skill | ~22 subcommands | ~15 renderer verbs | registry lives in the server; TUI keeps its own |
| aider | all 43 | CLI flags with no command | **empty** | registry is the primary surface; the CLI calls it |

**What this does and does not do to the spec's argument.** It does *not* rescue
the stated goal: aider's set B is large — most of its ~100 CLI flags have no
command — so even the harness with an empty set C is not at parity in the
"every CLI command has a TUI command" direction, which is the direction the brief
asked about. Nobody is. What it does is confirm, for the second time, that the
**achievable** parity is the one this spec's P1 states: a shared *interpreter*
under both surfaces, with each surface feeding it in the idiom it can express.
Two of the four harnesses got there, both by inversion, and BitRouter got there
for one verb in #866 ([§6.10](#610-the-in-repo-precedent-866)) by the same means.

**Two mechanics worth stealing, and one worth avoiding.**

- **`--load` is `/commands` for scripts.** A file of commands, `#` comments
  skipped, executed in order, is a cheap and honest headless surface for an
  interactive vocabulary, and it costs nothing once a dispatcher exists. Not
  proposed here — BitRouter's admitted set is six commands, four of them reads —
  but it is the natural shape if the set ever grows.
- **Prefix disambiguation.** `matching_commands()` accepts any unambiguous
  prefix and errors with *"Ambiguous command: …"* otherwise. That is a recall
  aid the `/commands` registry in [phase 0](#phase-0--the-discoverability-defect-no-design-required)
  could offer for free, and it is only safe *because* there is a registry to
  enumerate — which is the argument for building the registry first.
- **The argument grammar is the same hand-parse everyone else does.**
  `do_run(command, rest_inp)` hands each `cmd_*` the raw remainder of the line.
  Forty-three independent parsers, no declaration on the entry, no completion
  contract beyond an optional `completions_raw_<cmd>` method. This is
  [D9](#d9--argument-grammar) option (a) at full scale and it is the one part of
  aider's design not to copy.

#### 6.9.8 What this survey could not establish

Recorded so that a later reader does not mistake absence for a finding.

- **Any command count for Claude Code**, on either surface, and therefore any
  overlap ratio. The registry is closed and the documentation publishes no
  complete built-in command list. The previously circulated figure of "roughly 7
  shared names out of ~130" could not be reproduced and is not repeated.
- **Whether `/config` has a headless twin.** The published CLI reference has no
  `claude config` entry, which is consistent with the maintainer's claim, but
  no statement of intent was found either way.
- **Any parity post-mortem in this field.** [§6.3](#63-hub-the-only-rigorous-post-mortem-of-a-parity-layer)'s
  `hub` retrospective has no agent-harness equivalent — no harness was found
  that built a parity layer and repudiated it, in a maintainer's own words. The
  field is young enough that the obituaries have not been written. This was the
  single most valuable thing sought and it was not found; the closest substitute
  is `codex-acp`'s archival, which is a *handover* rather than a repudiation
  ([§6.9.2](#692-codex-cli--the-registry-lives-in-the-tui-crate-so-the-overlap-is-zero)).
- **Any maintainer statement in an agent harness that a command was
  *deliberately* withheld from one surface.** The nearest are Claude Code's
  documentation naming a "terminal-only built-in" class
  ([§6.9.1](#691-claude-code--a-documented-terminal-only-class-and-a-shared-class-that-is-not-commands))
  and ACP's `ConradIrwin` declining the slash-command form of model selection in
  favour of typed config options
  ([§6.9.4](#694-acps-own-position-on-command-ownership)). Both are statements
  about *shape*, not about a specific withheld command.
- **`pi`.** Not reached.

### 6.10 The in-repo precedent (#866)

`feat(acp): headless permission policy and output formats` (#866, `878178a4`)
landed on `main` on 2026-09-05, **after** [§1](#1-verified-starting-state) was
measured. §1 is therefore stale in one respect that matters, and the correction
is favourable: BitRouter has already shipped [§9 option C](#c-local-dispatch-through-the-same-action-ports--recommended),
once, for one verb.

Verified in the tree, not from the changelog alone:

| Claim | Evidence |
|---|---|
| A new module holds the wire half of the session's effects | `apps/bitrouter/src/chat/effects.rs`, 140 lines, new in `878178a4` |
| Three loops now run it | its module doc: *"The interactive loop, the piped loop, and the headless `acp prompt` loop used to each hand-roll the wire half… `Wire` is that half, written once, so a permission answered by a keystroke and one answered by a headless policy reach the agent through the same code."* |
| The decision itself is shared, not just the plumbing | `bitrouter_tui::permission::Policy` decides; `machine::decide` turns the decision into the same `Effect::Resolve` a keystroke produces |
| The guard was extended to cover it | *"The chat guard test scans the new file"* — `chat/effects.rs` names the ACP client and nothing else; the daemon, config and metering remain unnameable |
| The maintainer's own framing | CHANGELOG: *"…reaches the agent through one shared interpreter (`chat/effects.rs`) that the interactive TUI, the piped `chat`, and `acp prompt` all run — **the first session-verb parity between the terminal and the headless CLI**."* |
| And the framing in the commit body | *"This is the cheap half of TUI/headless parity, modelled on acpx's surface, with no session persistence and no daemon change."* |

⇒ **Finding 15. The mechanism this spec recommends is not a proposal; it is the
pattern the repository chose the week the spec was written, for the one verb
where the pressure was highest.** Three things about how it was done are worth
carrying into [§14](#14-phases) unchanged:

1. **It moved in the direction §6.8 says is cheap.** Permission answering was a
   set-C verb — a keystroke on a live session — and #866 gave it a headless
   twin. It did *not* take a CLI leaf and give it a keystroke. Every parity move
   in the peer group runs the same way ([§6.9.1](#691-claude-code--a-documented-terminal-only-class-and-a-shared-class-that-is-not-commands)'s
   `claude mcp login`, added to escape the `/mcp` panel), and it is the direction
   with the asymmetric payoff: it closes an automation dead end rather than a
   discoverability gap.
2. **The headless twin is a *policy*, not a command.** `--deny-all`,
   `--approve-reads`, `--approve-all`, `--permission-policy` — the caller states
   a rule up front rather than answering prompts one at a time. That is exactly
   what all four harnesses do with permissions
   ([§6.9.5](#695-what-recurs-in-set-c-across-four-harnesses)), and it is the
   reason the parity was achievable at all: the two surfaces do not share a
   *command*, they share an **interpreter**, and each surface feeds it in the
   idiom it can express. P1's *"where both surfaces can express it"* is doing
   real work here and should not be dropped as hedging.
3. **The shared interpreter was placed where the wall already allowed it.**
   `chat/effects.rs` sits inside `chat/`, names only the ACP client, and is
   scanned by the same guard — [§4 cause (c)](#4-root-cause--why-103-versus-2)'s
   "add a declared channel, don't lower a wall", executed. `SessionActions`
   ([§9 option C](#c-local-dispatch-through-the-same-action-ports--recommended))
   is the same move one seam further out, and #866 is evidence that the seam
   holds under a real feature rather than a sketch.

**One consequence for [D10](#d10--is-there-a-first-party-gui-over-acp-serve) and
the C-versus-B argument.** §9 conceded that C's guarantee is conventional where
B's is structural, and D10 left that risk unpriced. #866 is the first data point
on it: the shared interpreter was written *because* the headless path had already
drifted — the module doc says so in as many words, *"the headless one had
drifted: it could only ever deny."* The drift is real and it happens, and the
thing that caught it was a person noticing, not a test. That is an argument for
treating guard **A1** as load-bearing exactly as D10 says, and a small argument
for building it before the fourth surface rather than after.

### 6.11 `acpx`, and what "remote over HTTP" can actually mean

Written for [D13](#d13--the-remote-transport-requirement), which is the decision
it exists to serve, and for the `acpx` reference in
[§6.10](#610-the-in-repo-precedent-866)'s commit-body quotation, which was
previously unexplained in this document.

#### 6.11.1 `acpx` is a real project, and it is BitRouter's nearest neighbour

#866's commit body says the headless permission work was *"modelled on **acpx**'s
surface."* The name appears exactly once in this repository — in that commit
message — and nowhere in the source, so it is worth writing down what it is.

**[openclaw/acpx](https://github.com/openclaw/acpx)** ([acpx.sh](https://acpx.sh/)),
MIT-licensed, describes itself as a *"headless CLI client for stateful Agent
Client Protocol (ACP) sessions"* — *"talk to coding agents from the command line,
not the PTY."* Read on 2026-09-05 from its published
[`docs/CLI.md`](https://github.com/openclaw/acpx/blob/main/docs/CLI.md):

| Measure | Value |
|---|---|
| Top-level commands | `prompt`, `exec`, `compare`, `flow run`, `cancel`, `set-mode`, `set`, `status`, `sessions`, `config` |
| `sessions` leaves | `list`, `new`, `ensure`, `close`, `show`, `history`, `export`, `import`, `prune` |
| Permission flags | `--approve-all`, `--approve-reads`, `--deny-all`, `--policy` |
| Output formats | text (default), `--format json` (NDJSON ACP events), `--format quiet` (final assistant text only) |
| Transport | **stdio only.** No HTTP, no network mode, no daemon; sessions are files under `~/.acpx/sessions/` with a per-session queue-owner process |
| A command that lists the *agent's* commands | **none** |

Two things follow, and both are load-bearing later.

⇒ **Finding 16. The surface #866 copied is not a coincidence of naming — it is
the same four permission flags and the same three output formats.**
`--approve-all` / `--approve-reads` / `--deny-all` / `--policy` against #866's
`--approve-all` / `--approve-reads` / `--deny-all` / `--permission-policy`, and
`text` / `json` / `quiet` against #866's `text` / `json` / `quiet`. That makes
`acpx` the closest thing this survey found to a control group: an independently
built headless ACP client, converging on the same answers, with **no** router, no
middlebox position, and no interactive TUI to be at parity with.

⇒ **Finding 17. The nearest neighbour has no command-listing command either, and
it is stdio-only.** `acpx` is a pure ACP client — it receives
`available_commands_update` like any client — and still exposes no way to ask
"what can this agent do?". That is a gap in the field rather than a BitRouter
oversight: any headless command-listing surface BitRouter builds has no adopter
to copy. Its being stdio-only is the second half of §6.11.2's point: the
reference headless ACP client has no remote transport because ACP has none to
offer.

#### 6.11.2 ACP has no network transport, and that is the fact that decides D13

Sourced from the protocol repository, read on 2026-09-05.
[`docs/protocol/v1/transports.mdx`](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/docs/protocol/v1/transports.mdx)
and its `v2` sibling are **identical on this point**:

> *"The protocol currently defines the following transport mechanisms for
> agent-client communication: 1. stdio … 2. \_Streamable HTTP (draft proposal in
> progress)\_. Agents and clients **SHOULD** support stdio whenever possible."*

and, under the Streamable HTTP heading itself, the entire section body is:

> *"In discussion, draft proposal in progress."*

So: **stdio is the only transport ACP defines.** The escape clause is
`## Custom Transports`, which grants permission rather than specifying anything —
*"the protocol is transport-agnostic and can be implemented over any
communication channel that supports bidirectional message exchange… Implementers
who choose to support custom transports **MUST** ensure they preserve the
JSON-RPC message format and lifecycle requirements."*

**Work is under way and its status is precisely knowable.** A **Transports
Working Group** was announced 2026-04-22 by Ben Brandt (Zed Industries, ACP lead
maintainer): *"Remote Agent support is a key focus of ACP, and in order to make
this more of a reality, we need to standardize all of the approaches to
transports people have been trying."* The RFD is
[`docs/rfds/streamable-http-websocket-transport.mdx`](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/docs/rfds/streamable-http-websocket-transport.mdx)
(authors alexhancock, jh-block; champion anna239). Its shape, and the quotations
that matter to BitRouter specifically:

- A single `/acp` endpoint; `POST` for client→server, long-lived SSE `GET`
  streams for server→client (one connection-scoped, one per session),
  `Upgrade: websocket` as an alternative on the same endpoint, `DELETE` to
  terminate. **HTTP/2 required.** Clients supporting remote ACP MUST support
  both profiles.
- On the status quo it is blunt: *"ACP only has stdio. There is no standard
  remote transport, which causes fragmentation as implementers invent their own
  HTTP layers, leading to incompatible SDKs and deployments."*
- On BitRouter's exact position, in the "shiny future" section: *"**Proxy
  chains** can route ACP traffic over HTTP for multi-hop agent topologies."*
- On maturity: *"targeted for inclusion in **v1** as an additive feature, with
  more robust durability and reliability primitives coming in **v2**,"* and for
  v1 *"durability and reliability are the implementer's responsibility — the
  protocol provides the building blocks, not the guarantees,"* including
  *"**in-flight messages are not replayed**… server→client messages emitted
  while a client was disconnected are not redelivered on reconnect."*

It is **not landed**: `docs/protocol/v1/draft/transports.mdx` — the draft channel,
where an accepted-but-unreleased change would appear — still reads *"In
discussion, draft proposal in progress."* The RFD is in neither version's
transports page.

⇒ **Finding 18. ACP has no network transport today; an HTTP/WebSocket one is an
RFD in an active working group targeted at v1; and the RFD's own statement of the
status quo is that everyone who needs remote ACP right now is inventing an
incompatible HTTP layer.** BitRouter's `acp serve` is stdio by construction and
says so in its help text — *"Serve one agent session as a vanilla ACP Agent over
**stdio** until the manager disconnects"*
([`main.rs:1271`](../apps/bitrouter/src/main.rs:1271)). Anything BitRouter ships
before that RFD ratifies is one of the incompatible layers the RFD names.

#### 6.11.3 BitRouter has already answered "the same actions, but remote", once, for `ACTIONS`

This is the in-tree precedent [D13](#d13--the-remote-transport-requirement) turns
on, and it is stronger than [§6.10](#610-the-in-repo-precedent-866)'s because it
is about transport rather than about sharing an interpreter. Measured in this
worktree at `0a9537b5`.

| Claim | Evidence |
|---|---|
| `bitrouter mcp serve` already has a **remote transport**, and it is not ACP | [`main.rs:818`](../apps/bitrouter/src/main.rs:818) `enum McpTransport { Stdio, Http }` — *"Streamable HTTP, mounted at `/mcp-control`"*, `--bind` default `127.0.0.1:4357` ([`main.rs:774`](../apps/bitrouter/src/main.rs:774)) |
| It is genuinely multi-tenant, not loopback-only by accident | [`multitenant_http.rs:58`](../crates/bitrouter-mcp/tests/multitenant_http.rs:58) `two_callers_forward_distinct_bearers` — two clients, two bearers, each forwarded to the upstream separately |
| An unauthenticated bind is **forced** to loopback | [`lib.rs:142`](../crates/bitrouter-mcp/src/lib.rs:142) → [`server.rs:786`](../crates/bitrouter-mcp/src/server.rs:786) `ensure_loopback_bind` |
| The two transports serve **two profiles of one table**, not two implementations | [`lib.rs:160`](../crates/bitrouter-mcp/src/lib.rs:160) `stdio_profile` and [`server.rs:880`](../crates/bitrouter-mcp/src/server.rs:880) `http_profile`, both assembling the same `BitrouterMcp::builder()` from the same ports |
| The remote profile is a **strict subset**, and a guard says exactly which | [`server.rs:1403`](../crates/bitrouter-mcp/src/server.rs:1403) `http_profile_never_carries_host_bound_tools` asserts `["complete", "list_models", "status"]` — and `["complete", "list_models"]` for a backend with no status of its own |
| …and a second guard stops the stdio side widening it | [`lib.rs:308`](../crates/bitrouter-mcp/src/lib.rs:308) `wiring_skills_into_stdio_does_not_widen_the_http_profile` |
| The exclusion criterion is **host-boundness**, stated as a crate invariant | [`server.rs:825`](../crates/bitrouter-mcp/src/server.rs:825): *"the remaining non-completion tools are semantically bound to one machine (`route_preview` resolves against the serving host's own config and control socket; `skills_search`/`skills_get` read its installed-skills root), so **no multi-tenant HTTP profile may carry them**"* |
| The guarantee is **structural, not conventional** | [`lib.rs:150`](../crates/bitrouter-mcp/src/lib.rs:150): the HTTP profile *"is built from an `Arc<dyn Backend>` alone and therefore **cannot** reach these ports even by accident"* |
| Statefulness is separately excluded, by the MCP version negotiation | [`server.rs:825`](../crates/bitrouter-mcp/src/server.rs:825): under SEP-2567 a peer negotiating `2026-07-28` is *"**always** served statelessly… each request gets a fresh handler… Adding a stateful tool to this HTTP profile would therefore break under draft-version clients even though it works today against `2025-11-25`"* |
| A degraded answer is a **result**, not a fabrication | same guard's comment: *"A backend with no status of its own gets no `status` tool rather than a fabricated one: the local daemon's liveness is a control-socket question the HTTP profile cannot answer"* |
| The named seam, if a remote client ever needs the host-bound reads | [`server.rs:825`](../crates/bitrouter-mcp/src/server.rs:825): *"this is the single function to change — take a handler factory, keep the profile strictly loopback and incompatible with the cloud backend, and prefer modeling that read-only data as MCP resources over widening the tool surface"* |

⇒ **Finding 19. BitRouter's answer to "the same table, but remote" already exists,
and it is neither a new protocol nor two implementations behind a trait. It is one
table, two *profiles*, with the remote profile constructed from a narrower
dependency so that it cannot reach what it must not carry, and two guard tests
naming the difference.** That is the structural guarantee
[§9](#9-the-mechanism) conceded option C lacked — obtained from a narrower
constructor rather than from a wire boundary, at a cost of two assertions rather
than ~200 lines of protocol.

**And a second in-repo precedent, on the tunnelling question specifically.**
`apps/bitrouter/src/gateways.rs` is *"one spec, two renderers"*: `gateway_servers`
declares the injected MCP gateways once, `to_acp` renders them as an ACP
`session/new` `mcpServers` descriptor and `Harness::launch_overlay` renders them
into a harness's own config surface. The `bitrouter_tools` gateway is *"injected
as a streamable-HTTP server so the harness's own MCP client dials the daemon
directly."* That is descriptor-passing HTTP, and it was chosen **over** tunnelling
MCP through the ACP connection, because the MCP-over-ACP mechanism was behind an
`unstable_mcp_over_acp` schema feature that no shipping harness advertised. The
same shape of choice recurs in D13 with the same answer: **do not tunnel over an
unratified channel; dial the surface that already exists.**

The module also records the one thing the pattern could not do, and it is directly
on point:

> *"there is **no HTTP path to the daemon's own installed skills**. The aggregate
> `/mcp` proxies configured `mcp_servers` upstreams; it does not serve origin
> content… The precondition for retiring it is 'the daemon serves its own skills
> over HTTP', which needs an in-process executor seam (`McpTarget::Direct` assumes
> a dialable transport, and a daemon serving itself has none)."*

⇒ **Finding 20. "Remote" is not a uniform property of a surface; it is a property
of each answer.** The same binary already carries actions that travel (a model
catalog), actions that travel *degraded* (`status`, which reports what the serving
deployment can see), actions that cannot travel because they resolve against one
machine (`route_preview`, the skills roots), and actions that cannot travel
because they are stateful under a stateless negotiation. Choosing a transport
changes none of that.

---

## 7. The membership rule

One rule, five clauses, all of which must hold. A command is admitted to the
interactive surface **iff**:

- **R1 — Subject.** Its subject is *this session*, or the routing that this
  session's next turn will use. (`route`, `models`, `status`, cost pass. `cloud
  policy bind`, `eval subject seal`, `trajectory prune` fail.)
- **R2 — Termination.** It returns a value and gives the terminal back. It does
  not block indefinitely, spawn a nested UI, take stdio, or prompt on the TTY.
  (Fails: `serve`, `start`, `mcp serve`, `acp serve|prompt`, `chat`, `launch`,
  `spawn`, `init`, `update`, `providers login`, `cloud login`.)
- **R3 — Non-destruction.** It cannot destroy the substrate the session runs on,
  handle a secret, or write config. This is `OBSERVABILITY_TUI_SPEC.md` §14's
  three surviving guardrails, unchanged. (Fails: `stop`, `restart`, `key sign`,
  `mcp add`, `cloud keys revoke`, `cloud billing checkout`.)
- **R4 — Transience.** If its answer is daemon-wide or account-wide, it may be
  rendered **once, on request, as a notice** — never held in state, polled, or
  placed in the footer. A command that would need a permanent pane fails.
- **R5 — Invertibility (writes only).** A write is admitted only if the
  inventory contains its inverse. (`route/set` passes: `route/reset` exists.
  Nothing else currently passes.)

**[Corroborated post-research.]** R1 was derived from BitRouter's shape and then
checked against the peer group.
[§6.9.5](#695-what-recurs-in-set-c-across-four-harnesses) tabulates every
interactive-only command in Claude Code, Codex, OpenCode and the §6.2 cohort:
four categories recur in all four harnesses — renderer verbs, conversation-buffer
verbs, session/model switching, and lifecycle-in-place — and every one of them is
a verb whose subject is *the session's own apparatus*. That is R1, measured
rather than reasoned. Two adjustments the table implies, neither of which changes
the clauses: permission and approval sit in set C by nature in every harness (and
their headless twin is always a *policy stated up front*, which is precisely what
#866 shipped — [§6.10](#610-the-in-repo-precedent-866)); and interactive config
editing is set C in three of four harnesses and is forbidden here by R3, so
BitRouter is the deliberate outlier. The demand is real; §14's rule is the answer
anyway, for the reason §5 records.

**Why R5 rather than "writes need a confirmation."** Confirmation is a UI
affordance and it is worth having ([§11](#11-reads-versus-writes)), but it is not
a membership criterion — every destructive CLI command could grow a confirmation
and none of them should be one keystroke from a chat line. Invertibility is the
property that makes a mis-press recoverable, which is exactly what §14's `stop`
guardrail was reasoning about. It also has the pleasant property of being
*checkable from the table* rather than from judgement.

**What the rule is not.** It is not "is this useful?" — `bitrouter status
--requests` is extremely useful mid-session and fails R4 (it is a table of every
caller's requests, which wants a pane). Utility is the reason to *reopen* the
rule, not an exception to it.

**The rule admits set C, and this is where it matters.** R1–R5 say nothing about
whether a CLI leaf exists. A verb that passes all five and has no headless twin
— `route/set`, `route/reset` — is admitted, because the criteria are about the
*session*, not about the CLI's shape. That is deliberate: making CLI presence a
membership criterion is what would force
[D7](#d7--does-the-route-picker-need-a-cli-leaf)'s option (b), and
[§6.2](#62-psql-the-interactive-layers-vocabulary-is-mostly-net-new) says
inventing headless twins for session-only verbs is the wrong instinct.

**The corollary, which is the rule's most useful consequence:** R1–R5 also
license verbs the CLI *cannot* have. Nothing here says the interactive surface
may only contain things that already exist headlessly — psql spent most of its
meta-command budget on `\gexec`, `\watch` and `\crosstabview`, and that is where
its value is. This spec proposes none, because none is needed today (CLAUDE.md
4). But the rule is written so that proposing one later is a normal move rather
than a boundary violation.

---

## 8. The inventory

All 29 top-level commands, with the rule that decides each. Dispositions:

- **S** — session command (a `/` command in the TUI).
- **F** — already reaches the reader through the ACP wire (footer or journal);
  no command needed.
- **C** — CLI-only by rule; the failing clause is named.
- **H** — hostile in-session (a subset of C, called out because the brief asked
  for it explicitly).

| # | Command | Leaves | Disposition | Rule / reasoning |
|---|---|---|---|---|
| 1 | `serve` | 1 | **H** | R2 — never returns; the chat process *is* the foreground |
| 2 | `start` | 1 | **C** | R2 — `chat` already auto-starts a daemon at launch; a mid-session start answers nothing |
| 3 | `stop` | 1 | **H** | R3, and named explicitly by `OBSERVABILITY_TUI_SPEC.md` §14 |
| 4 | `restart` | 1 | **H** | R3 — drops this session's route leases and cost attribution |
| 5 | `reload` | 1 | **C** | R1 (daemon-wide, affects every caller), R5 (no inverse). **The strongest borderline case** — see [D3](#d3--reload) |
| 6 | `status` | 1 | **S** | R1 via "is the router up", R4 transient. Requires the §8.3 amendment |
| 7 | `route` | 1 | **S** | R1 — the *only* action whose subject is literally the next turn |
| 8 | `init` | 1 | **H** | R2 (nested wizard, TTY prompts), R3 (writes config, handles credentials) |
| 9 | `config validate` | 1 | **C** | R1 — about a file, not a session. Read-only and harmless, but nothing asks it here |
| 10 | `key sign` | 1 | **H** | R3 — mints a credential |
| 11 | `models` | 1 | **S** | R1 — "what can this session route to", the superset of what the picker shows |
| 12 | `tools` | 3 | **C** | R1 — upstream MCP servers, not this session |
| 13 | `observe` | 1 | **C** | R1, R4 — exporter state is daemon-wide and wants a pane |
| 14 | `policy` | 13 | **C** | R1 daemon-wide; `create`/`publish`/`rollback` also fail R3/R5 |
| 15 | `eval` | 8 | **C** | R1 — evidence exchange has no session subject |
| 16 | `optimize` | 2 | **C** | R1, R2 (`optimize run` is long-running) |
| 17 | `trajectory` | 3 | **C** | R1, R4 today. Session-scoped replay would pass R1 — see [D4](#d4--session-scoped-trajectory) |
| 18 | `providers` | 3 | **C** | `list` fails R1 only weakly (it is `models` minus the models); `login`/`logout` fail R2 and R3 |
| 19 | `agents` | 3 | **C** | R1 — catalog state, decided before the session started |
| 20 | `launch` | 1 | **H** | R2 — an interactive harness TUI inside a terminal the renderer owns |
| 21 | `spawn` | 1 | **H** | R2, and R1 by #749: spawning sub-agents from a session is the orchestrator |
| 22 | `cloud` | 40 | **C/H** | R1 account-wide; `login` fails R2/R3; `keys revoke`, `policy delete`, `budget delete` fail R5; `billing checkout` spends money |
| 23 | `skills` | 2 | **C** | R1 — the agent's own skills reach the reader as its `AvailableCommands`; `skills init` scaffolds a file (R3). See [D6](#d6--skills-list) |
| 24 | `mcp` | 5 | **H/C** | `serve` fails R2; `install`/`add` fail R3; `search`/`list` fail R1 |
| 25 | `workflow-state` | 6 | **C** | R1 — trace/replay utilities over stored state |
| 26 | `update` | 1 | **H** | R2 (TTY confirm), R3 (replaces the running binary) |
| 27 | `acp` | 2 | **H** | R2 — both take stdio |
| 28 | `chat` | 1 | **H** | R2 — recursion, named in the brief |
| 29 | `help` | — | **S** | The TUI's analogue is `/commands`, which must list BitRouter's commands as well as the agent's |

Plus two things that are not top-level CLI commands but are part of the surface:

| Item | Disposition | Reasoning |
|---|---|---|
| `route/set` (the picker) | **S**, write | R1, R5 (inverse exists). Already shipped; needs a row |
| `route/reset` | **S**, write, **missing today** | R1, R5 (it *is* the inverse). Implemented end to end and unreachable — a one-line defect |
| session cost | **F** | Already on the wire as `UsageUpdate.cost`; the footer is the right rendering |
| session mode / config / title | **F** | `CurrentModeUpdate`, `ConfigOptionUpdate`, `SessionInfoUpdate` already land in the footer |

**Totals.** 4 read actions (`status`, `models`, `route`, plus `/commands` as the
inventory's own reflection), 2 writes (`route/set`, `route/reset`), 2 facts
already carried by the protocol. **6 commands out of 103 leaves.**

**The honest reading of this table.** 97 of 103 leaves are excluded, and 40 of
those are `cloud`. If the maintainer's reaction is that the rule is too strict,
the productive next move is not to loosen R1 — it is to ask which specific leaf
the rule wrongly excludes, and check it against R2/R3/R5. My prediction is that
the answer is `reload` ([D3](#d3--reload)) and possibly `providers list`, and
nothing else.

---

## 9. The mechanism

Three candidates, as the brief framed them, plus the escape hatch the field
evidence keeps producing.

### A. Ride `AvailableCommandsUpdate`

BitRouter's controller injects its own commands into the
`AvailableCommandsUpdate` it forwards from the harness; the TUI needs no change
at all, since it already renders that list.

**There is a shipped precedent for this and it must be dealt with honestly.**
`codex-acp` advertises `/status`, `/mcp`, `/skills`, `/compact`, `/logout` over
`available_commands_update` and handles every one of them locally, never sending
them to the model ([§6.7](#67-acp-specifically)). So "advertise host commands as
agent commands and intercept them" is not a hypothetical abuse — it is what the
reference adapter does. And the naive provenance objection is weaker than it
looks: BitRouter's controller **is** the ACP Agent the TUI is talking to, so
advertising a command it can execute is not, strictly, a lie.

**Rejected anyway.** Three reasons, and the first is the one that separates
BitRouter from `codex-acp`:

1. **BitRouter is a middlebox, not a terminus.** `codex-acp` gets away with this
   because nothing is downstream of it — an unrecognised `/x` reaching the model
   is the model's problem and no one else's. BitRouter sits *between* the client
   and a harness, and the only invocation channel ACP offers is
   `session/prompt`. So BitRouter would have to **string-match its own advertised
   commands out of the prompt stream before forwarding**, in the hot path, on
   every turn. That is the `if line == "/route"` compare this design exists to
   remove, relocated somewhere far worse — where a user typing *"how do I use
   /status"* trips it, and where a harness advertising the same name gets its
   own command eaten.
2. **It cannot carry arguments.** `AvailableCommandInput` is one `hint: String`
   and nothing else. Against Emacs' typed code letters, Vim's
   `-nargs`/`-complete`, tmux's `args_parse`, and Claude Code's `arguments` +
   `argument-hint`, ACP declares strictly less than any registry in
   [§6.5](#65-every-registry-declares-which-callers-may-reach-an-entry).
   `models --provider x` has nowhere to go.
3. **ACP has no namespace, no origin field, and no collision rule**
   ([§6.7](#67-acp-specifically)). irssi is the cautionary tale — `command_bind`
   lets a script silently replace a builtin — and Claude Code's explicit
   precedence table is what a working `/` union actually requires. Merging two
   origins into one unnamespaced list is a decision that needs a rule; ACP
   supplies none, and inventing one inside a field spelled "commands the agent
   can execute" is the wrong place to put it.

**[Corroborated post-research, and the corroboration is unusually direct.]** The
current `codex-acp` — development having moved from `zed-industries/codex-acp`,
now archived, to `agentclientprotocol/codex-acp` — has had to hand-write every
one of the three things objection 1–3 said ACP does not supply: a prompt-stream
string matcher (`parseCommand()`: first text block, `startsWith("/")`, split on
`/\s+/`, hand-parse the rest), a namespace (skills advertised under a `$` sigil,
with `if (commandName.startsWith("$")) return {handled: false}` as the forwarding
rule), and a collision rule (`Map`-seeded built-ins, colliding skills skipped) —
plus an `_meta.commandAction` side-channel for the typed actions
`AvailableCommand` cannot express. Its collision rule is the **opposite** of
OpenCode's documented one. Details and quotations:
[§6.9.2](#692-codex-cli--the-registry-lives-in-the-tui-crate-so-the-overlap-is-zero),
[§6.9.3](#693-opencode--the-one-that-got-closest-and-the-inversion-that-did-it).
And the protocol's own text settles ownership — *"**Agents** can advertise a set
of slash commands"*, with no client-command capability, origin field, namespace,
or collision rule in either v1 or v2
([§6.9.4](#694-acps-own-position-on-command-ownership)).

What *should* ride that channel is nothing of ours. It is the agent's list, and
[phase 0](#phase-0--the-discoverability-defect-no-design-required) keeps it
that way by rendering our commands as a **separate, labelled group** with a
stated precedence, rather than merging into it.

### B. New `_bitrouter/*` extension methods

Follow the `route/*` precedent exactly: `_bitrouter/status`, `_bitrouter/models`,
`_bitrouter/route/preview`, advertised under
`_meta["bitrouter.dev/controller"]` with the same three-condition gate, bridged
to the daemon by an `Arc<dyn AcpActions>` built in `acp_cli.rs` and handed to the
controller alongside `route_control` and `session_cost`.

**Strong on protocol-correctness** — `_`-prefixed extension methods are the
sanctioned place for exactly this, the capability negotiation already exists and
is tested, and per-method gating gives honesty (a control that cannot act is
absent, not dead) for free.

**Strong on testability** — the port is a trait; `ACTIONS`' existing
`both_surfaces_produce_the_same_report` pattern extends to a third surface with
no new machinery.

**Its cost is the one that decides against it:** it is an in-process JSON-RPC
round trip to talk to yourself, for a report the same process already computed,
in service of a consumer that does not exist. The only thing the wire buys over
option C is that an `acp serve` manager — a GUI — would get `status` and `models`
for free. But a GUI is a separate process that can already run `bitrouter status
--json` or call the MCP `status` tool. **There is no consumer gap to close**, and
building for one is CLAUDE.md rule 4.

### C. Local dispatch through the same action ports — **recommended**

The chat process already has everything: `acp_cli.rs` holds the `ConfigSource`,
the socket path, and the principal, and it already constructs one bridge
(`LocalControllerBinding::route_control`) that is a trait object handed downward.
Build a second one the same way:

```
apps/bitrouter/src/acp_cli.rs
    let actions: Arc<dyn SessionActions> = binding.actions(source, config);
    chat::session::run(&mut session, …, actions)          // handed in, not reached for

apps/bitrouter/src/chat/session.rs
    Effect::Action(id, args) => view.notice_lines(render(actions.run(id, args).await))
```

`SessionActions` returns the **`ACTIONS` report types** — `StatusReport`,
`ModelsReport`, `RouteReport` — and its implementation is
`apps/bitrouter/src/actions/{status,models,route}.rs`, the *same functions* the
CLI leaf and the MCP tool call. Not a copy: the same call.

**[This is no longer a proposal.]** #866 (`878178a4`) shipped exactly this shape
for one verb the week the spec was written: `apps/bitrouter/src/chat/effects.rs`
is one interpreter that the interactive TUI, the piped `chat`, and `acp prompt`
all run, placed inside `chat/` and scanned by the same guard. The CHANGELOG calls
it *"the first session-verb parity between the terminal and the headless CLI."*
Verification, and the three properties of how it was done that
[§14](#14-phases) should copy, are in
[§6.10](#610-the-in-repo-precedent-866). It also supplies the first evidence on
C's conceded weakness: the module doc records that *"the headless one had
drifted: it could only ever deny"* — the drift this option risks is not
hypothetical, it had already happened, and a person caught it rather than a test.

**[Strengthened by D12, 2026-09-05.]** The maintainer has taken
[§6.9.3](#693-opencode--the-one-that-got-closest-and-the-inversion-that-did-it)'s
inversion ([D12](#d12--take-693s-inversion)), which raises this option's bar. C
as written above shares an *implementation*: the leaf and the port both call
`actions::status`. The inversion additionally makes [§10](#10-source-of-truth)'s
`ACTIONS` the **dispatcher** — a name resolves to a row and the row runs — so
`submit()`'s string compare becomes a table lookup (which is what **G1** wanted
anyway) and no second name-resolution path is written for a headless caller. It
adds no protocol and no new surface; it is the same port with the lookup moved
into the table.

**Why this satisfies the walls rather than lowering them.** `chat/`'s guard test
forbids naming `crate::daemon`, `control_socket`, `LocalControllerBinding` and
friends. A `dyn SessionActions` it is *handed* names none of them; it is exactly
the shape `route_control` already takes, which is why that guard passes today
with route control fully wired. And `bitrouter-tui` is untouched: it receives
lines, as it does for every other notice.

| | A — `AvailableCommands` | B — extension methods | C — local port |
|---|---|---|---|
| Protocol-correct | ✗ misuses the field | ✓ sanctioned `_` namespace | ✓ nothing on the wire |
| Carries arguments | ✗ one hint string | ✓ typed request | ✓ typed call |
| Testable | ✗ string matching in the prompt path | ✓ stub the trait | ✓ stub the trait |
| Can drift from the CLI | ✓ freely | only via a second impl | only via a second impl |
| Reaches an `acp serve` GUI | ✓ (wrongly) | ✓ | ✗ — but the GUI has the CLI and MCP |
| New surface to version | the agent's own field | a capability block + N methods | none |
| Lines of new protocol | 0 | ~200 + tests | 0 |
| Reaches a **remote** caller over HTTP | ✗ | ✗ — ACP defines no network transport | ✓ — via the existing `/mcp-control` profile |

**The rule that keeps C from becoming two mechanisms:** *the answer travels over
ACP only when the ACP peer is the authority.* `route/set` stays on the wire
because the daemon owns lease state keyed by principal and the controller is the
only thing holding that binding. `status`, `models` and `route` come from the
port because this process can answer them. That is one rule, and it happens to
describe today's code exactly, which is a good sign it is the real seam rather
than a preference.

**The strongest argument against C, stated at full strength.** C's guarantee is
*conventional* where B's is *structural*. Nothing in C's type system forces
`SessionActions::status` to call `actions::status` — a future implementer under
time pressure can satisfy the trait by reading the config directly, and the
result compiles, passes clippy, and drifts. That is precisely
[`ACTIONS_SPEC.md`](ACTIONS_SPEC.md) §2 root cause (a) — *two implementations* —
re-created one layer down, and it is the failure this whole line of work exists
to prevent. B does not have that hole: the report crosses a declared JSON-RPC
boundary with a negotiated capability, so a second implementation would have to
announce itself.

Two things reduce this, and neither eliminates it. First, guard **A1** — run all
three surfaces and compare bytes — catches exactly this class, which is why it is
listed as acceptance rather than optional. Second, `Requires` makes the daemon
dependency explicit in the table, so an implementation that quietly answered from
config while declaring `Requires::Daemon` is a visible lie rather than an
oversight.

**If the maintainer weights structural guarantees over line count, B is the right
answer and this recommendation should be overturned.** The deciding question is
narrow and worth asking directly: *does a first-party GUI over `acp serve` exist
in the plan?* If yes, B's cost is not ceremony — it is the price of the surface
that GUI needs, and C would have to be redone. If no, B is ~200 lines of protocol
for a consumer that never arrives.

**[Corrected, 2026-09-05 — the paragraph above states the deciding question
wrongly, and the recommendation it guards is nevertheless unchanged.]** Two things
happened after it was written. First, [D10](#d10--is-there-a-first-party-gui-over-acp-serve)
answered the question — no first-party GUI — and settled §9 on C. Then the
maintainer added a requirement that **falsifies D10's premise**: the CLI and the
TUI must both work remotely, over HTTP, against a proxy server, which *is* a
remote consumer. So "does a remote consumer exist?" is no longer the deciding
question, because the answer is now yes and the recommendation does not change.

**The deciding question was always the wrong one, and the right one is: is there a
standard remote transport for B to travel on?** There is not.
[§6.11.2](#6112-acp-has-no-network-transport-and-that-is-the-fact-that-decides-d13)
establishes it from the protocol repository: ACP defines **stdio and nothing
else**; Streamable HTTP is *"in discussion, draft proposal in progress"* in v1 and
v2 alike; the HTTP/WebSocket RFD is targeted at v1 and has not landed even in the
draft channel. B over HTTP would therefore mean implementing an unratified RFD or
inventing a private framing — *"a wire protocol you now own and version anyway"*,
which is the cost B existed to avoid paying.

**And C's conceded weakness is now answered rather than merely tolerated.** §9
above concedes that C's guarantee is *conventional* where B's is *structural*.
[§6.11.3](#6113-bitrouter-has-already-answered-the-same-actions-but-remote-once-for-actions)
shows the repository already obtaining a structural guarantee on this exact table
without a wire boundary: `mcp serve --transport http` serves `ACTIONS` rows
remotely, and its profile *"is built from an `Arc<dyn Backend>` alone and therefore
**cannot** reach these ports even by accident"*, with two guard tests naming the
row-set difference. **A narrower constructor is a structural guarantee.** Full
reasoning, the `Reach` column it implies, and the trigger that reopens B are in
[D13](#d13--the-remote-transport-requirement).

### D. The escape hatch: `/bitrouter <leaf> [args]`

One command, literal parity, zero per-command work: re-enter `Cli::parse_from`
in-process and render the resulting `CliReport`. This is what the field actually
does — k9s plugins, lazygit `customCommands`, psql's `\!` — and it deserves to be
on the table rather than dismissed.

**Recommendation: no.** Three reasons, in descending force:

1. **`hub`'s lesson applies directly** ([§6.3](#63-hub-the-only-rigorous-post-mortem-of-a-parity-layer)).
   A proxy inherits an obligation to be indistinguishable from the thing it
   proxies — *"your program needs to behave as git in every possible way, and
   every time it doesn't, you have a bug"* — and here it demonstrably cannot be:
   the terminal is in raw mode and owned by a differential writer, there is no
   exit code to return, `--json` has nowhere to go, and roughly a quarter of the
   leaves prompt on the TTY or never return
   ([§2.2](#22-roughly-a-quarter-of-the-cli-is-actively-hostile-in-a-session)).
   Every one of those differences becomes a defect report against `/bitrouter`.
2. **Filtered, it collapses into option C.** Apply R2 and R3 and the admitted set
   is the same six commands as [§8](#8-the-inventory) — reached via clap
   re-entry, exit-code mapping, and stdout interception rather than a function
   call. Unfiltered, it puts `stop`, `update`, and `cloud billing checkout` one
   line from a chat prompt and restores exactly the unbounded reachability the
   crate boundary exists to prevent.
3. **BitRouter's version of this pattern has no upside to trade for.** k9s,
   lazygit and gh-dash shell out because the engine is *another process* they
   cannot link. BitRouter is one binary; the equivalent is a function call, so
   the whole shell-out tax — spawn cost, quoting
   ([lazygit #5893](https://github.com/jesseduffield/lazygit/issues/5893)),
   ambient-state re-serialization
   ([k9s #191's missing `$CONTEXT`](https://github.com/derailed/k9s/issues/191#issuecomment-1671956952)),
   unstructured output — would be paid for nothing.

See [D2](#d2--the-user-configured-escape-hatch) for the variant that is genuinely
worth considering: the *user-configured* hatch, which is what the field actually
converged on and which trades this cost for something real.

---

## 10. Source of truth

**`ACTIONS` extends. It does not get a sibling.** Three new columns, each of
which has a field precedent:

```rust
pub struct ActionSpec {
    pub id: &'static str,
    pub cli_leaf: Option<&'static str>,
    pub mcp_tool: Option<&'static str>,
    /// The TUI slash command that invokes it, without the leading `/`.
    /// `None` is a *declaration* that this action is not offered in a
    /// session — see `Availability` for why.
    pub tui_command: Option<&'static str>,
    /// Whether this action observes or changes state, and what undoes it.
    pub effect: Effect,
    /// What ambient context the action needs before it can run, and what
    /// happens when it is absent. Resolved by the driver, not the action.
    pub requires: Requires,
    pub output_schema: Option<fn() -> rmcp::model::JsonObject>,
}

pub enum Effect {
    /// Observes. Safe to run at any time, including twice.
    Read,
    /// Changes state. `inverse` names the action id that undoes it —
    /// required by rule R5 for any action with a `tui_command`.
    Write { inverse: Option<&'static str> },
}

pub enum Requires {
    /// Answerable from the config source alone.
    Nothing,
    /// Needs a reachable daemon; degrades to a config answer.
    /// (`status` reports `running: false`; `models`/`route` fall back.)
    Daemon,
    /// Needs the trusted local controller binding. Absent under `--direct`
    /// or an explicit `--base-url`, where the command is not offered.
    Binding,
}
```

`Requires` is [tmux's `cmd_entry` flags](#64-tmux-is-the-one-real-full-parity-system-and-it-works-by-inversion)
— `CMD_CLIENT_TFLAG` plus `CMD_CLIENT_CANFAIL` — with the same division of
labour: **the entry declares what it needs, the framework resolves or degrades,
and the action itself never asks.** It replaces today's ad hoc
`State::new(routable: bool)`, which is one boolean hardcoded for one command and
does not generalise to a second.

It also makes [invariant 3](#15-invariants) — *a control that cannot act is
absent, not dead* — mechanical instead of a review habit: `Requires::Binding`
with no binding means not offered, and that is checkable from the table rather
than re-argued per command.

**`None` is a declaration, not a backlog.** This is
[finding 5](#65-every-registry-declares-which-callers-may-reach-an-entry): every
mature registry has a per-entry statement of which callers may reach it — Emacs'
`command-modes`, VS Code's `when: false`, Claude Code's `disable-model-invocation`
/ `user-invocable: false`, Vim's `-buffer`. The brief read `ACTIONS`' `None`s as
a shortfall. Some are (`complete`'s `output_schema: None` is a real migration
backlog, and `ACTIONS_SPEC.md` §3 says so). But `tui_command: None` on
`skills_search` is the Emacs kind: *not offered here, on purpose, by rule R1*.

**Recommendation: keep them distinguishable in the doc comment, not in the type.**
A `enum Exposure { NotApplicable(&'static str), Backlog }` would be more honest
and is more machinery than six rows earn (CLAUDE.md 4). The rule the guard
enforces is the one that matters: a row that *does* carry a `tui_command` must
satisfy P2.

**Why extend rather than fork.** A sibling table would need its own guard, its
own reconciliation with `ACTIONS`, and a rule for which table a two-surface
action lives in. It would also lose the property that makes `ACTIONS` work — the
schema function pointer, which is what lets a guard compare *shapes* rather than
documentation. The TUI is a third consumer of the same reports.

**The objection, stated fairly — and it is the strongest one against this
recommendation.** `ACTIONS` is a table about *machine*-facing surfaces.
`tui_command`, `requires` and `effect` are human-facing concerns, and
[§6.5](#65-every-registry-declares-which-callers-may-reach-an-entry) shows that a
human command registry keeps growing: Emacs adds per-argument prompt types, Vim
adds `-nargs`/`-complete`/`-range`/`-bang`, tmux adds a usage string and target
resolution, Claude Code adds `argument-hint`, `allowed-tools`, `model`. If
BitRouter follows that trajectory, `ACTIONS` becomes a table with two unrelated
halves and the fork should have happened earlier.

**Recommendation: extend now, with the trigger stated** — fork into a
`TUI_COMMANDS` const in `bitrouter-tui`, reconciled against `ACTIONS` by a guard
rather than merged into it, on the **first field the TUI needs that no other
surface reads**. `argument-hint` is the likely one, and it arrives with
[D9](#d9--argument-grammar). Note that `effect` and `requires` are *not* that
field: `effect` is a fact the MCP tool should arguably declare too (rmcp has
`readOnlyHint`), and `requires` describes the action, not its rendering. This
mirrors `ACP_TUI_SPEC.md` §9's stated-trigger pattern, which is the one
prediction in this codebase's spec history that came true on schedule.

**Six rows against 103 leaves is the correct ratio, not a shortfall.** The
brief's framing — "~5 rows against 29 CLI commands" — reads the ratio as
evidence the table is under-built. It is not. `ACTIONS_SPEC.md` §5 already
decided the direction: *"Not asserted: that every CLI leaf has a row.
`bitrouter policy verify` needs no MCP tool and should not need a table entry to
exist."* The table's size is a measurement of how many questions have more than
one asker, and that number is genuinely small.

---

## 11. Reads versus writes

The brief is right that mutation is an unmodelled category, and right that
`route/set` is the only instance. Four things a write needs that a read does not:

**Confirmation.** *Do not build a new one.* `crates/bitrouter-tui/src/permission.rs`
is a tested modal with an invariant this design needs verbatim — *"Cancelling a
permission prompt can never resolve to consent"* (`lib.rs`) — and
`CHAT_MACHINE_SPEC.md` §1.1 records the bug that gets introduced when a modal
treats control chords as decisions. A write action with `confirm: true` reuses
`Phase::Answering` with a two-option prompt. Adding a second modal type is how
that bug comes back.

**Undo.** R5 makes this a membership criterion rather than an afterthought: the
inverse must be in the table before the write is admitted. This has a concrete
first consequence — `/route reset` ships in the same phase as `/route`'s row,
because it already exists on the wire and its absence is what makes today's
`route/set` un-undoable from the TUI.

**Error surfaces.** A read that fails is a notice. A write that fails has three
distinct outcomes the reader must be able to tell apart, and `RouteError` already
distinguishes them: `Unavailable` (the control was never there), `InvalidRoute`
(refused, old state intact), `Other` (unknown — state indeterminate). The rule:
**a write's rendering must state what is now in force, not what was asked for.**
That is already how `route_set` behaves (`Effect::RouteInForce` carries the
daemon-confirmed route, not the requested one), and it should be the documented
contract for every write rather than an accident of one implementation.

**Idempotence and repetition.** A read may be re-run on a redraw; a write may
not. The machine already separates `Effect::Paint` from state-changing effects,
so this costs nothing — but it needs saying, because a future "refresh" key that
re-runs the last command would silently re-issue a write.

**What this spec does *not* propose.** A general write framework. There is one
write, it has one inverse, and CLAUDE.md rule 4 forbids the rest. `Effect::Write
{ inverse }` in the table is the whole model; if a second write ever qualifies
under R5, that is when the framework earns its keep.

---

## 12. Rendering

There are now three renderings of one report: `--json`, `CliReport::render` into
a byte stream with an ANSI theme, and the TUI's `Vec<Line<'static>>`.

**Recommendation: the TUI reuses `CliReport::render` through `Theme::none()`,
and does not gain a trait of its own.**

```
report ──serde──> --json                       (already exists)
       ──CliReport::render(Theme::for_stdout)─> the CLI's human view
       ──CliReport::render(Theme::none)───────> Vec<u8> ──split──> notice lines
```

`Output::render_to_vec` already exists and is documented as *"the testable seam"*
— it renders with an empty palette to a `Vec<u8>`. That is, precisely, a
plain-text renderer. The chat driver calls it and hands the lines to
`view.notice_lines(..)`, which already takes lines and already renders
`Notice::Say` as plain text.

**Why this is the right trade and not just the cheap one.** Divergence between
the CLI's human view and the TUI's becomes *impossible*, because there is one
function. Nothing to guard, because there is nothing that could differ. That is a
strictly stronger guarantee than any test over two renderers.

**What it costs, stated plainly.**

- No ratatui styling in the TUI's rendering of an action — no colour, no bold, no
  in-place patching. For a transient notice ([§7](#7-the-membership-rule) R4)
  that is acceptable; for anything that wanted to live in the document it would
  not be.
- `Theme::none()` is mandatory. A themed render would inject raw ANSI into a
  differential writer that owns the screen, which would corrupt it. This needs a
  test ([§13](#13-the-guards), G5), because `Theme::for_stdout()` is the default
  everywhere else and is one character away.
- Line wrapping is then the TUI's (`wrap.rs`) over text the CLI laid out for an
  80-column stdout. Tables from `human::Table` are auto-sized to their content
  and will overflow a narrow terminal into the horizontal-scroll case the writer
  does not have. **This is the real risk in this recommendation** — `models` on a
  large catalog is a wide table. Mitigation: the three admitted reads have small
  or narrow reports (`status` is a block, `route` is a chain, `models` is two
  columns), and a report that does not fit is a reason to reconsider R4 for that
  action, not to add a renderer.

**The alternative, and why not.** A `trait TuiReport { fn lines(&self) ->
Vec<Line<'static>> }` in `bitrouter-tui`, implemented for the shared reports,
would give real styling. It requires `bitrouter-tui` to depend on
`bitrouter-mcp` (where the report types live) and therefore on `rmcp` and
`schemars` — a heavy, wrong-shaped edge for a rendering crate, and one that
`TUI_RENDERER_SPEC.md` spent effort keeping clean in the other direction. If
styling is later worth it, the cheaper move is a `Vec<Line>` producer app-side
that consumes `Theme` markers, not a trait across the crate boundary.

---

## 13. The guards

`ACTIONS` has three guards because documentation does not stop drift. This
design needs five, and three of them are extensions of the existing ones rather
than new machinery.

- **G1 — every TUI command has a row.** The mirror of
  `every_mcp_tool_has_an_actions_row`. Requires the TUI's commands to be
  *enumerable*, which they are not today (`if line == "/route"`). This is the
  work: a `const COMMANDS: &[&str]` in `bitrouter-tui::machine` that `submit()`
  dispatches from, and a test in `main.rs` asserting every entry has an `ACTIONS`
  row with a matching `tui_command`.
- **G2 — every row with a `tui_command` is fully inventoried.** P2: it has a
  `cli_leaf` *or* an `Effect::Write` over a live session (set C), it has a
  `requires`, and — where it has two surfaces — an `output_schema`. This is
  `OBSERVABILITY_TUI_SPEC.md` §14's review gate as a test rather than a habit,
  and per [D7](#d7--does-the-route-picker-need-a-cli-leaf) it is the *stronger* form of
  that rule, not a relaxation. It is the single highest-value assertion in this
  document.
- **G3 — every row with a `tui_command` and `Effect::Write` names an `inverse`
  that is itself a row.** Rule R5, mechanically.
- **G4 — the §8.3 amendment holds.** The TUI's retained state
  (`machine::State`, `journal`, the footer) gains no field derived from a
  daemon-wide report. Checkable the way `the_chat_module_reaches_nothing_daemon_wide`
  is checkable: a source scan asserting `StatusReport` and `ModelsReport` appear
  in `session.rs` only in the notice path. Cruder than a type-level guarantee,
  and honest about it — the type-level version would require the reports never to
  be nameable in the crate, which is already true and is what makes the scan
  cheap.
- **G5 — action rendering is palette-free.** `Theme::for_stdout` must not be
  reachable from the chat driver's action path; added to the existing forbidden
  list in `the_chat_module_reaches_nothing_daemon_wide`.

And one *acceptance* test rather than a guard, extending the existing pattern:

- **A1 — three surfaces, one report.** `both_surfaces_produce_the_same_report`
  becomes `all_surfaces_…`: run the CLI leaf's path, the MCP port, and
  `SessionActions`, and compare serialized bytes. For `status`, `models`, and
  `route` this is a three-line extension of a test that already exists.

**What no guard can catch, said once.** `ACTIONS_SPEC.md` D2 recorded it and it
applies here identically: *"a shared type stops the shapes diverging, not the
contents."* Three surfaces sharing `StatusReport` can still fill different fields
if they call different implementations. Only A1 — running all three and comparing
bytes — catches that, and only for the inputs it uses.

---

## 14. Phases

Each ends green, is independently shippable, and fixes something a user can see.
Phases 0 and 1 are worth doing **even if the maintainer rejects everything
else in this spec**, because they are defect fixes rather than design.

### Phase 0 — the discoverability defect (no design required)

Today `/route` exists and is invisible. `/commands` lists only the agent's.

- `/commands` renders BitRouter's own commands **above** the agent's, in a
  labelled group ("BitRouter" / "<agent>"), so a name appearing in both is
  attributed rather than ambiguous. Two labelled groups, not one merged list —
  ACP supplies no origin field, and merging without one is what
  [§6.7](#67-acp-specifically) shows going wrong.
- **State the precedence rule, because there is a real collision.** BitRouter's
  commands are matched in `submit()` *before* the line becomes a prompt, so a
  harness advertising its own `/status` would be shadowed. The rule: **local
  wins, and the shadowed entry is shown as shadowed in `/commands`** rather than
  silently dropped. Claude Code publishes exactly such a table
  (enterprise > personal > project > bundled, plugin skills namespaced, synced
  skills yield); irssi, which has no rule, lets a script replace a builtin with
  no trace. Silent shadowing is the failure to avoid.
- Add `/help` as an alias, because it is what people type.
- The list comes from one place that `submit()` also dispatches from — the
  `const COMMANDS` G1 needs. Two commands do not justify a table; **the guard
  does**, and this is the phase that makes G1 possible at all.
- Update `docs/CLI.md:530`–`537` and `skills/bitrouter/references/cli.md:207`
  in the same change (CLAUDE.md lockstep).

Ships the discoverability fix — the field's answer to the one legitimate
argument for the stated goal
([§6.8](#68-the-inverse-principle-and-whether-it-is-symmetric)) — plus the
mechanism every later phase needs.

### Phase 1 — `/route reset`, and the write model

`_bitrouter/route/reset` is implemented on both sides and has no caller.

- `Effect::ResetRoute` → `client.route_reset`, gated on the capability the same
  way `set` is (all three of list/set/reset must be advertised, or the picker
  offers no reset).
- `ACTIONS` gains `effect`, `requires` and `tui_command`, with rows for
  `route_set` and `route_reset` carrying `Effect::Write { inverse }`,
  `Requires::Binding`, and `cli_leaf: None`.
- `Requires::Binding` replaces `State::new(routable: bool)` — one boolean
  hardcoded for one command becomes a resolved declaration, per
  [§6.4](#64-tmux-is-the-one-real-full-parity-system-and-it-works-by-inversion).
- G2 and G3 land here.

**This phase forces [D7](#d7--does-the-route-picker-need-a-cli-leaf) into the open.**
`route_set` and `route_reset` get `tui_command: Some(..)` and `cli_leaf: None`
— which is set C, legal under P2 and illegal under a literal reading of
`OBSERVABILITY_TUI_SPEC.md` §14. Nothing else in this spec can be built until
that reading is settled, because G2's assertion depends on it. It is also the
cheapest place to settle it: the code is already written and shipped, so the
decision is about a rule, not about a feature.

### Phase 2 — `SessionActions`, proved on `/status`

The cheapest action to prove the port with, exactly as `ACTIONS_SPEC.md` phase 1
used `status` for the same reason.

- `SessionActions` trait app-side beside the existing bridges; `binding.actions()`
  builds it; `session::run` takes it as a parameter.
- `/status` renders `StatusReport` through `render_to_vec` + `Theme::none()`.
- G1, G4, G5, A1 land here.
- A session with no binding (`--direct`, explicit `--base-url`) reports the
  absence honestly, the way `/route` already does — not a dead command.

### Phase 3 — `/models` and `/route <model>`

- `/models [provider]` renders `ModelsReport`.
- `/route <model>` renders `RouteReport` — the *preview*, distinct from the
  picker's write. Naming: `/route` with no argument opens the picker (today's
  behaviour, preserved), `/route <model>` previews. Alternative in
  [D8](#d8--route-overloading).
- This is where `ACTIONS_SPEC.md` phase 5's picker-validation fix pays off: once
  `route/list` is validated daemon-side, `/models` and the picker stop
  disagreeing about what is routable.

### Phase 4 — argument parsing, only if phase 3 needs it

`/models --provider anthropic` implies an argument grammar. **Do not build one
until a command needs more than one positional argument.** Three of the six take
zero or one, so `split_once(' ')` is the whole parser. When a second flag
appears, the question is [D9](#d9--argument-grammar) — and the answer is probably
still not clap.

---

## 15. Invariants

1. **`bitrouter-tui` never depends on `apps/bitrouter`.** Enforced by cargo;
   restated because the whole design leans on the TUI receiving data rather than
   fetching it.
2. **`chat/` names nothing daemon-wide.** `the_chat_module_reaches_nothing_daemon_wide`
   must keep passing *unchanged* — the new port is handed in, exactly as
   `route_control` is.
3. **A control that cannot act is absent, not dead** (`lib.rs`). Carried by
   `Requires`: `Binding` with no binding is not offered; `Daemon` with no daemon
   degrades to the config answer and says which one answered (`resolved_via`,
   which both `ModelsReport` and `RouteReport` already carry). Following VS
   Code's split of *visibility* from *executability*
   ([§6.5](#65-every-registry-declares-which-callers-may-reach-an-entry)), a
   third rendering is legitimate and probably kinder: **listed in `/commands`
   with its unavailability stated**, rather than vanishing with no explanation.
   What is forbidden is the fourth: offered as working and then failing.
4. **Cancelling a confirmation can never resolve to consent.** Inherited from
   `permission.rs`; extends to any write confirmation.
5. **A write reports what is in force, not what was asked.**
6. **No `#[allow]`, no `unwrap`/`expect`/`panic!`, no type or column introduced
   "for the table" that no surface reads** (CLAUDE.md 1–4). In particular
   `Effect::Write { inverse }` must not ship before a write row uses it.
7. **The TUI holds no daemon-wide state.** The amended §8.3: transient render on
   request, never retained, never polled, never in the footer (G4).
8. **Stdout stays one JSON value for every CLI leaf** — inherited from
   `output/mod.rs`; a report gaining a TUI rendering must not gain a `Display`
   that writes to stdout.
9. **`skills/bitrouter/` and `docs/CLI.md` update in the same PR** as any
   change to the interactive command set (CLAUDE.md lockstep).

---

## 16. Open decisions

Each carries a recommendation. None is decided.

### D1 — amend §8.3, or hold it?

`/status` and `/models` render daemon-wide data. `ACP_TUI_SPEC.md` §8.3 forbids
it; §9 names it as a boundary-violation trigger.

- **(a) Amend, narrowly** — daemon-wide data may be *rendered once on request*,
  never held, polled, or placed in the footer, enforced by G4.
- **(b) Hold §8.3** — drop `/status` and `/models`, ship only `/route`,
  `/route reset` and `/commands`. The interactive surface stays session-pure and
  the parity work is three commands.
- **(c) Hold §8.3 and answer the question differently** — put a *session-scoped*
  health signal on the wire (the controller already knows whether its binding is
  live) instead of exposing `status`.

**Recommendation: (a).** The rule was written to prevent a second data model and
a second pane, and a transient notice is neither. But (c) is genuinely
attractive for `status` specifically — "is the router up" is a session question
wearing a daemon-wide report's clothes — and if the maintainer's instinct is that
§8.3 should hold, (c) is the version that costs least.

### D2 — the user-configured escape hatch

lazygit's `customCommands` and k9s's `plugins.yaml` let the *user* bind arbitrary
shell-outs, which is how those tools get unbounded reach without the maintainers
owning it. A `bitrouter.yaml` block could do the same:

```yaml
chat:
  commands:
    - name: usage
      run: ["bitrouter", "cloud", "usage", "--human"]
```

**Recommendation: no, for now** — but this is the option most likely to be right
later, and it deserves a deliberate "not yet" rather than silence. It is what
every tool in [§6.6](#66-the-shell-out-escape-hatch-universal-and-its-failure-modes-are-documented)
converged on, and lazygit's maintainers advertise it as the parity policy itself:
*"If lazygit is missing a feature, there's a good chance you can implement it
yourself with a custom command."*

Against it today: it violates R2/R3 by construction (nothing stops a user binding
`stop`), and it needs subprocess plumbing `chat/` does not have — which
[invariant 2](#15-invariants) would have to be re-argued to admit. **If it is
ever built, three things from §6.6 are non-negotiable:** argv arrays, never a
shell string (gh-dash's own codebase demonstrates the split — its built-ins use
`exec.Command("gh", args...)` and have no quoting bugs, its user commands use
`$SHELL -c` and do); every piece of session state re-injected explicitly (k9s's
missing `$CONTEXT` applied manifests to the wrong cluster for three years); and
an acknowledgement that the template variables become a public API you version
forever ([lazygit #3651](https://github.com/jesseduffield/lazygit/issues/3651)).

The natural trigger is the first request for a seventh command that fails R1 but
that the user legitimately wants.

**[Re-priced by D12, 2026-09-05.]** [D12](#d12--take-693s-inversion) takes
[§6.9.3](#693-opencode--the-one-that-got-closest-and-the-inversion-that-did-it)'s
inversion — the registry underneath both surfaces — but explicitly **not**
OpenCode's openness. That makes D2 cheaper to build (a name-dispatching table has
an insertion point; today's string compare does not) and **more expensive to
allow**: the guards in [§13](#13-the-guards) are exhaustive because the table is
closed, and a row that exists only at runtime cannot be covered by any of them.
Admitting user rows forks the registry into a guarded half and an unguarded one,
and every guarantee in [§3](#3-the-goal-restated) then applies only to the first.
That, rather than the subprocess plumbing, is now the leading argument against
D2. The recommendation is unchanged; the reason is stronger. Full statement in
D12.

### D3 — `reload`

`bitrouter reload` fails R1 (daemon-wide) and R5 (no inverse), but it is the one
excluded command with a genuine session story: you edited `bitrouter.yaml` and
want the next turn to use it. It also has a known bug —
`ACP_TUI_SPEC.md` §6 records that `reload` does not rebuild the policy table, so
today it would be a success-reporting no-op for the case people would use it for.

**Recommendation: exclude, and revisit only after the policy-table reload bug is
fixed.** Shipping an interactive command whose headless twin silently does not
work is worse than not shipping it. If the maintainer wants it anyway, it needs
R5 to be relaxed to "or the operation is idempotent and non-destructive", which
is a real weakening of the rule and should be decided as such.

### D4 — session-scoped `trajectory`

`trajectory inspect` scoped to *this session* would pass R1 cleanly and is
arguably the single most valuable thing a chat user could ask for ("what did
this session actually do"). It fails R4 today only because the report is
daemon-wide.

**Recommendation: out of scope for this spec, and worth its own issue.** It is a
metering/attribution question (does `launch_id`/`api_principal` scoping reach
trajectory rows?) before it is a parity question.

### D5 — settle the scope empirically before phase 2

Phases 0 and 1 are defect fixes. Phase 2 onward encodes §2.4's claim that the
interesting set is four questions. That claim is an inference, not a
measurement.

**Recommendation: before phase 2, answer one question from real use** — *which
`bitrouter` command have you actually run in a second terminal while a chat
session was open?* If the honest answer includes something this spec marks **C**,
the rule is wrong and should be fixed before six commands are built against it.
If it is `status`, `models`, `route`, the spec is right and phases 2–3 proceed.

### D6 — `skills list`

Marked **C** on the grounds that the agent's own skills reach the reader as its
`AvailableCommands`. That is not quite true: SEP-2640 skills and ACP slash
commands are different things, and a user debugging "why can't the agent see my
skill" is exactly the case `ACTIONS_SPEC.md` phase 4 exists to serve.

**Recommendation: exclude for now, on R1** — the question is about the disk, not
the session, and `bitrouter skills list` answers it in another terminal without
disturbing anything. Weak reasoning, and the counter-argument is decent; flagged
rather than buried.

### D7 — does the route picker need a CLI leaf?

`OBSERVABILITY_TUI_SPEC.md` §14 says every keystroke needs a CLI subcommand.
`route_set`/`route_reset` have none, and shipped anyway — so the rule is already
violated in the tree, silently.

- **(a) Weaken to P2** — "every TUI command is an inventoried action", with the
  CLI-leaf requirement dropped for actions whose subject is a live ACP session,
  which the CLI structurally cannot have.
- **(b) Add the leaves** — `bitrouter acp route set --session <id>`.
- **(c) Keep §14 strict and argue the picker is not a "command"** — it is a
  key-driven modal, and modals were never covered.

**Recommendation: (a), and it is the one recommendation here that the field
evidence made *stronger* rather than weaker.** Set C — interactive verbs with no
scriptable twin because their subject only exists in a session — is universal
([§6.2](#62-psql-the-interactive-layers-vocabulary-is-mostly-net-new)): psql's
`\e`, `\r`, `\errverbose`; k9s's `:xray`, `:pulses`; nushell's `:try`. Treating
it as a defect is what would produce (b), and (b) is new surface nothing asked
for (CLAUDE.md 4). (c) will not survive contact — `/route` is typed at a prompt
and is a command by any reading.

**[Corroborated post-research, with one new data point about the trigger.]** Set
C survives in all four coding-agent harnesses, including OpenCode — the one that
built a genuinely shared dispatch port — and its members there are renderer and
buffer verbs that cannot be headless because there is no renderer to address
([§6.9.5](#695-what-recurs-in-set-c-across-four-harnesses)). More useful for this
decision: Claude Code shows what makes a set-C verb acquire a CLI leaf, and it is
not symmetry. `claude mcp login <name>` is documented as running an MCP server's
OAuth flow *"without opening the interactive `/mcp` panel"* — the leaf exists
because the modal was blocking automation
([§6.9.1](#691-claude-code--a-documented-terminal-only-class-and-a-shared-class-that-is-not-commands)).
Expect the same trigger here: if `route/set` ever needs a headless twin it will
be because a script could not get past the picker, and that is the moment to
revisit (b) — not before.

The weakening is small and it keeps §14's actual purpose. Read the rule's own
justification: *"Accretion now requires adding a CLI command first — **which gets
normal review**. This is the gate the old TUI lacked: its verbs existed nowhere
else."* The gate was review; the CLI leaf was the only available forcing
function. A row plus a guard test is a stronger one, and unlike the CLI leaf it
cannot be satisfied by adding a command nobody runs.

### D8 — `/route` overloading

Phase 3 makes `/route` mean two things: bare opens the picker (write), with an
argument previews (read). Overloading a name across the read/write line is
exactly the kind of thing that produces a mis-press.

- **(a) Overload**, as phased.
- **(b) Split** — `/route` picks, `/preview <model>` previews.
- **(c) Split the other way** — `/route <model>` *sets* directly (matching
  `bitrouter route`'s argument shape being a model), `/routes` lists.

**Recommendation: (b).** The CLI's `bitrouter route <model>` is a read, so under
(a) or (c) the same words mean different things on the two surfaces — which is
the drift this whole document is about. (b) costs one more command name and keeps
every name meaning one thing.

**[Reinforced post-research.]** Two harnesses ship a shared *name* whose two
members are different actions, and both are documented as deliberate:
`claude doctor` is read-only while `/doctor` *"can also apply fixes"*, and
`opencode export` writes JSON while `/export` writes Markdown and opens an
editor. In every harness surveyed, **the shared names that were not built on a
shared port have drifted**
([§6.9.1](#691-claude-code--a-documented-terminal-only-class-and-a-shared-class-that-is-not-commands),
[§6.9.3](#693-opencode--the-one-that-got-closest-and-the-inversion-that-did-it)).
That is the failure (a) and (c) would import, observed twice rather than
predicted once. Note also that [D11](#d11--should-routeset-become-an-acp-config-option)
may remove the write side of this question entirely.

### D9 — argument grammar

When a command needs more than one argument, what parses it?

- **(a)** `split_whitespace` and per-command hand parsing.
- **(b)** Re-enter clap with the leaf's own `Command`, so the TUI and the CLI
  share a grammar and `--help` for free.
- **(c)** No arguments ever — every argument becomes a picker.

**Recommendation: defer, and lean (c).** (b) is seductive — one grammar, no drift
— but it drags clap's error rendering, exit codes, and `--help` into a raw-mode
terminal, and clap's errors are written for a shell. (c) is more work per command
and much better interactively; the picker already exists and is tested. Decide
when phase 3 forces it, not before.

Worth knowing before that decision: the field's answer is **neither**. Emacs,
Vim, tmux and Claude Code all declare argument *kinds* on the registry entry and
let the framework prompt or complete for them — Emacs' `(interactive "bBuffer: ")`,
Vim's `-complete=buffer`, tmux's `args_parse.cb`, Claude Code's `argument-hint`
plus named `arguments`. That is (c) with a declaration instead of bespoke code
per command, and it is the form to reach for if a fourth command needs arguments.
It is also the field that triggers the table fork in [§10](#10-source-of-truth).

**[Corroborated and sharpened post-research.]** The coding-agent peer group
answers the same way and adds one warning
([§6.9.6](#696-argument-grammar-as-four-harnesses-actually-solved-it)). None of
the four re-enters its own parser, so (b) has no adopters. All four declare the
argument shape on the registry entry — Codex a bare
`supports_inline_args() -> bool` (20 of 59), Claude Code named `arguments` plus
`argument-hint`, OpenCode a `hints: string[]` **derived by regex from the command
template**. But the two with a real substitution grammar (`$ARGUMENTS`, `$1`)
have it only because their commands *are* prompt text, which BitRouter's are not,
so that half does not transfer. The warning is concrete: OpenCode's shared
`session.command` port takes `arguments` as a single string, and its CLI
hand-quotes argv back into that string before sending — [§6.6](#66-the-shell-out-escape-hatch-universal-and-its-failure-modes-are-documented)'s
quoting failure mode, reappearing *inside* the shared port. **Whatever
`SessionActions::run` takes, it must not be one string.**

### D10 — is there a first-party GUI over `acp serve`?

This is not a design question; it is a fact this spec needs and could not
establish from the tree, and it **decides [§9](#9-the-mechanism)**. C (local
port) forfeits reaching an `acp serve` manager; B (extension methods) reaches it
at the cost of ~200 lines of protocol and a capability block to version.

- If a first-party GUI is planned, **B**, and C would have to be redone.
- If not, **C**, and B is building for an absent consumer (CLAUDE.md 4).

**Recommendation: C, on the evidence available** — nothing in this tree consumes
`acp serve` other than `chat` itself, and a third-party manager already has the
CLI's `--json` and the MCP tools. But the recommendation is only as good as that
fact, and the maintainer knows it better than the tree does.

**[Decided by the maintainer, 2026-09-05 — C.]** There is no first-party GUI.
One was built (a native GPUI renderer in its own repo, consuming ACP through an
`AcpFeed`) and **abandoned**; the direction adopted in its place is exactly the
pair this spec is about — an ACP-compatible headless CLI and an interactive TUI.

Two consequences, and the second matters more than the first:

1. **§9 settles on C.** B would be ~200 lines of protocol and a versioned
   capability block built for a consumer that was tried and dropped — CLAUDE.md
   rule 4 in its plainest form.
2. **The C-versus-B trade-off is no longer a trade-off, so the argument
   against C has to be answered on its own.** §9 conceded that C's guarantee is
   *conventional* where B's is *structural*: nothing in the type system stops a
   future `SessionActions` impl from reading config directly instead of calling
   `actions::*`, which is root cause (a) one layer down. That risk was
   previously priced against B's cost. With B gone, it is unpriced, and the
   guard in [§13](#13-the-guards) is the only thing standing between this
   design and the drift it exists to remove. Treat that guard as load-bearing
   rather than as belt-and-braces — it is now the whole belt.

This also removes the last reason to keep the door open on B "in case a GUI
appears": if one ever does, it consumes the same ACP surface `chat` does, and
the question reopens then with a real consumer to design against.

**[Superseded in its reasoning, 2026-09-05 — the conclusion survives, the
argument does not. Read [D13](#d13--the-remote-transport-requirement) with this.]**
The maintainer has since required that the CLI and the TUI both work remotely over
HTTP against a proxy server. **That is a remote consumer, so the premise this
decision rests on — "there is no consumer for B to serve" — is false as of the
requirement.** Stating it plainly rather than absorbing it: D10's argument for C
is dead.

The conclusion is unchanged, on a different and stronger ground. B is extension
methods on ACP, and **ACP has no network transport to carry them**: stdio is the
only transport it defines, and Streamable HTTP is an unratified RFD
([§6.11.2](#6112-acp-has-no-network-transport-and-that-is-the-fact-that-decides-d13)).
Meanwhile the remote consumer is already served by a surface that exists, is
externally versioned, and carries the same `ACTIONS` rows — `mcp serve --transport
http` ([§6.11.3](#6113-bitrouter-has-already-answered-the-same-actions-but-remote-once-for-actions)).
So B is now rejected for having *no transport*, where D10 rejected it for having
*no consumer*; that reason does not evaporate the next time a consumer appears,
which is the defect D10's reasoning had.

Consequence 2 above — "the risk is unpriced, treat the guard as the whole belt" —
is also superseded, favourably. The risk **is** priced now: §6.11.3 shows the
repository buying a structural guarantee for this table from a narrower
constructor rather than a wire boundary, at a cost of two guard assertions. The
guard remains load-bearing; it is no longer the only thing available.

### D11 — should `route/set` become an ACP config option?

Raised by [§6.9.4](#694-acps-own-position-on-command-ownership), and not
previously on this list because §1's measurement predated it. ACP carries a
first-class, typed `configOptions` channel — `{configId, name, category,
type: "select", currentValue, options[]}` with `category: "model"` as a defined
value — and `session/set_config_option` to write it. v2 folded session modes
into it entirely. An ACP contributor declined the slash-command version of
exactly this request with *"selecting the provider/model is a concrete enough
concept we should have our own types for it"*
([#77](https://github.com/agentclientprotocol/agent-client-protocol/issues/77)).

BitRouter's controller **is** the ACP agent the TUI talks to, so it could
advertise the session's route as a config option and get the client's picker
for free, on the standard surface, instead of the private
`_bitrouter/route/*` extension it uses today.

- **(a) Keep `_bitrouter/route/*`.** It is implemented, tested, capability-gated
  per method, and carries a report shape (`RouteSetResponse`) that a
  `select` cannot express — a route lease is not a value chosen from an
  enumeration, it is a request the daemon may refuse
  ([`ACTIONS_SPEC.md`](ACTIONS_SPEC.md) §1, phase 5).
- **(b) Move to `configOptions`.** Standard surface, no extension to version,
  and any ACP client — not just BitRouter's own TUI — gets a route picker.
- **(c) Both**: advertise the config option for the picker, keep the extension
  method for the report.

**Recommendation: (a) for now, and re-open when a second ACP client exists.**
(b)'s benefit is entirely "other clients get it", which is
[D10](#d10--is-there-a-first-party-gui-over-acp-serve)'s absent consumer wearing
different clothes; and `configOptions` has no slot for "the daemon refused your
lease, here is why", which is the case the picker most needs to render
correctly today. But this is a genuinely closer call than D10 was, because
unlike option B in §9 the channel already exists and costs no new protocol. It
should not be decided silently.

**This decision does not touch the four read commands.** `status`, `models`,
`route <model>` (preview) and `/commands` are reports, not selections among
enumerated values; nothing in `configOptions` fits them, and
[§9 option C](#c-local-dispatch-through-the-same-action-ports--recommended)
remains the mechanism for those regardless of how D11 goes.

### D12 — take §6.9.3's inversion?

**[Decided by the maintainer, 2026-09-05 — take it.]** The command registry sits
**underneath both surfaces**. The headless CLI is a client of the same dispatch
the interactive TUI uses, exactly as OpenCode's `session.command` is
([§6.9.3](#693-opencode--the-one-that-got-closest-and-the-inversion-that-did-it),
finding 10), as tmux's `cmd_entry` table is
([§6.4](#64-tmux-is-the-one-real-full-parity-system-and-it-works-by-inversion)),
and as `chat/effects.rs` already is for permission answering
([§6.10](#610-the-in-repo-precedent-866), finding 15). The interactive layer does
not mirror a CLI; both surfaces resolve a name against one table and run one
implementation.

**What this settles that §9 did not.** §9 option C shares an *implementation* —
`SessionActions::status` and the `status` CLI leaf both call `actions::status`.
The inversion is stronger: it makes [§10](#10-source-of-truth)'s `ACTIONS` the
**dispatcher**, not just the inventory. A name resolves to a row; the row runs.
Concretely, `machine.rs:375`'s `if line.trim() == "/commands"` becomes a table
lookup, which is what guard **G1** was already asking for, and the headless
`--command <name>` entry point that OpenCode's `run.ts` exposes becomes
*available* rather than absent. It is not thereby *scheduled*: nothing today asks
for `bitrouter chat --command status`, and building it before something does is
CLAUDE.md rule 4. What the decision fixes is the direction — when a headless
caller wants a session verb, it goes through the table, and no second dispatcher
is written for it.

**What is explicitly *not* taken with it: the openness.** OpenCode's registry is
**open data**. Its `Default` map holds two built-ins (`init`, `review`); every
other entry is discovered at runtime — every user `command` config entry, every
MCP prompt, every skill — and each carries a `source: "command" | "mcp" |
"skill"` tag because the set is not knowable at compile time. `ACTIONS` is the
opposite: a **closed `const` table** whose guard tests are *exhaustive over it*
([§13](#13-the-guards)). That closedness is not incidental. It is what lets a
guard assert **tool ⇒ row** today and **leaf ⇒ row** tomorrow, and it is what
buys P1's *"the same typed report"*. OpenCode has no such guard, and the two
shared *names* that bypassed `session.command` — `export` and `models` — drifted
into different actions on the two surfaces
([§6.9.3](#693-opencode--the-one-that-got-closest-and-the-inversion-that-did-it)).
**The inversion is taken for the dispatch; the openness is not taken at all.**

**How that lands on [D2](#d2--the-user-configured-escape-hatch), which is the
decision it moves.** D2 is the user-configured escape hatch — a `bitrouter.yaml`
block binding a name to an argv array. Under OpenCode's model D2 would be *free*:
a user command is just another registry row with `source: "command"`, dispatched
by the same port, because the registry was open by design. Taking the inversion
without the openness changes D2's cost in two directions at once, and both should
be written down before anyone reads "we took the inversion" as "D2 got cheaper":

1. **Cheaper to build.** A table that dispatches by name has an obvious insertion
   point that today's `if line == "/route"` does not. The plumbing D2 was
   deferred over is now half-present.
2. **More expensive to allow, and this is the part that decides it.** The guards
   are exhaustive *because* the table is closed. A user-supplied row exists only
   at runtime, so **no guard can cover it** — not G1's dispatch/list agreement,
   not A1's three-surface byte comparison, not P2's "has a row, a shared type,
   and a guard". Admitting user rows therefore forks the registry into a closed
   set the guards are exhaustive over and an open set they are silent about, and
   every guarantee in [§3](#3-the-goal-restated) applies only to the first. That
   fork is the real price of D2, it was not visible before this decision, and it
   is a better argument against D2 than the subprocess-plumbing objection D2
   currently leads with.

**D2's recommendation is unchanged — "no, for now"** — but its reasoning is now
sharper: the objection is not that the mechanism is missing, it is that the
mechanism's value comes from being closed. If D2 is ever taken, the honest form
is OpenCode's: an explicit `source` tag on every row, so that a reader and a
guard can both tell which half of the table they are looking at.

**One thing the inversion does not change.** Set C survives it. OpenCode built
the sharpest shared dispatch in the peer group and still has ~15 TUI-only
commands, all renderer and buffer verbs
([§6.9.5](#695-what-recurs-in-set-c-across-four-harnesses)). Inversion is a claim
about *where the registry lives*, not a claim that the two surfaces end up with
the same rows — which is [§3](#3-the-goal-restated)'s P1/P2 split restated, and
the reason P2 exists separately from P1.

### D13 — the remote transport requirement

**New requirement, 2026-09-05, from the maintainer: the CLI and the TUI must both
work (a) locally over stdio and (b) remotely over HTTP, against a proxy server.**

This is the requirement that could have reversed [§9](#9-the-mechanism), and it
has to be answered in the open rather than designed around, because
[D10](#d10--is-there-a-first-party-gui-over-acp-serve) chose option C over option
B on a premise this requirement attacks directly. **A CLI or TUI talking to a
proxy over HTTP is a remote consumer, and D10's stated ground for C was that no
remote consumer exists.** That ground is gone.

**Verdict: §9's recommendation stands — C, not B — but D10's *argument* for it
is dead and is replaced below. The requirement does not reverse the mechanism; it
shrinks set A whenever the transport is remote, which is a new fact about the
*goal*, not about the mechanism.** Four questions, answered in order.

#### (1) Does the requirement reintroduce B?

No — it argues *against* B, harder than D10 ever did, for a reason D10 did not
know.

Option B was *new `_bitrouter/*` ACP extension methods*. Its entire claimed
advantage over C was a **structural** guarantee: the report crosses a declared
JSON-RPC boundary with a negotiated capability, so a second implementation has to
announce itself. That advantage is only real if the boundary is one somebody else
maintains. **ACP has no network transport**
([§6.11.2](#6112-acp-has-no-network-transport-and-that-is-the-fact-that-decides-d13),
finding 18): stdio is the only transport it defines; Streamable HTTP is *"in
discussion, draft proposal in progress"* in both v1 and v2; the HTTP/WebSocket RFD
is targeted at v1 but has not landed even in the draft channel. `bitrouter acp
serve` is stdio in its help text.

So B-over-HTTP does not exist to be chosen. Choosing it would mean either
implementing an unratified RFD ahead of the working group, or inventing a private
HTTP framing for ACP — which is precisely what that RFD names as the problem it
was written to stop: *"ACP only has stdio. There is no standard remote transport,
which causes fragmentation as implementers invent their own HTTP layers, leading
to incompatible SDKs and deployments."* **A private wire you own and version
yourself is exactly the cost C was rejected for imposing, with none of B's
compensating standardness.** The requirement makes B worse, not better.

#### (2) Can C's injected port be satisfied by one trait with a local and a remote impl?

Yes, and BitRouter has already done it once for the same table — but the answer
is better than "a trait with two impls", and the difference matters.

`bitrouter mcp serve --transport http` already serves `ACTIONS` rows over
streamable HTTP at `POST /mcp-control`, multi-tenant, with per-caller bearer
forwarding proved by an integration test
([§6.11.3](#6113-bitrouter-has-already-answered-the-same-actions-but-remote-once-for-actions),
finding 19). It is **not** two implementations. It is one builder and two
**profiles** — `stdio_profile` and `http_profile` — assembling the same ports, and
the remote profile is a strict subset asserted by a guard
(`http_profile_never_carries_host_bound_tools`).

**"Two impls behind a trait" is the wrong shape and would reintroduce the exact
defect this line of work exists to remove.** Two impls is
[`ACTIONS_SPEC.md`](ACTIONS_SPEC.md) §2 root cause (a) — *two implementations* —
with a `#[cfg]`-free blessing. A profile is not an implementation: it is a
**declaration of which rows this transport may carry**, over one implementation.
The correct extension of [§10](#10-source-of-truth) is therefore a column, not a
trait:

```rust
pub enum Reach {
    /// Answerable for any caller from any host. Travels.
    Portable,
    /// Answerable remotely, but only about the serving deployment; it must
    /// report which deployment answered (`resolved_via`), never fabricate.
    Degraded,
    /// Resolves against the serving machine's own config, control socket, or
    /// installed-skills root. Local transports only.
    HostBound,
    /// Its subject is a live session on the serving process. Local only, and
    /// separately excluded by any stateless negotiation.
    SessionBound,
}
```

This is the same move `Requires` makes for the daemon dependency, one axis over,
and it has the same property: **checkable from the table rather than re-argued per
command.** It is also how the existing HTTP profile already behaves — `Reach`
merely writes down the criterion `server.rs:825` states in prose.

#### (3) What stops the remote impl from being "B with extra steps"?

Three things. Only the third is new, and it is the one that answers §9's strongest
concession.

1. **The wire is not BitRouter's.** MCP's streamable HTTP is versioned by
   somebody else (`2025-11-25`, SEP-2567's `2026-07-28`), implemented by `rmcp`,
   and already spoken by every harness's MCP client. B would have been a private
   `_bitrouter/*` capability block that BitRouter versions alone, forever. "A wire
   protocol you now own and version anyway, but undocumented" is a fair
   description of B; it is not a description of dialing an existing MCP endpoint.
2. **The port's signature is the report type, not an envelope.** Both profiles
   return `StatusReport` / `ModelsReport`; the transport serializes. The seam that
   could drift is the same single seam C already has, not a second one.
3. **The structural guarantee is available without a wire boundary, and is
   already in the tree.** §9 conceded that *"C's guarantee is conventional where
   B's is structural"* and D10 left that risk unpriced. It is now priced, and the
   price is two assertions: the HTTP profile *"is built from an `Arc<dyn Backend>`
   alone and therefore **cannot** reach these ports even by accident"*
   ([`lib.rs:150`](../crates/bitrouter-mcp/src/lib.rs:150)). **A narrower
   constructor is a structural guarantee.** It is enforced by the type system in
   the same way a JSON-RPC boundary would be, and it costs no protocol. This is
   the single most useful thing the transport requirement surfaced, and it
   improves C rather than damaging it.

#### (4) Versioning and capability negotiation, which the local case never needed

Three layers. BitRouter owns exactly one of them, and that is the point.

| Layer | Who versions it | BitRouter's obligation |
|---|---|---|
| Transport and protocol version | MCP (`protocolVersion` handshake), or ACP once the RFD lands | None. Negotiate, do not invent |
| **Which rows this transport carries** | **BitRouter** | The `Reach` column above, plus a guard per transport asserting the exact row set — the shape `http_profile_never_carries_host_bound_tools` already has |
| Report shapes | `ACTIONS`' `output_schema` | Already declared; guard A1 already compares shapes |

**The middle row is the whole capability-negotiation story, and it must be a
column rather than a function**, for the reason [§10](#10-source-of-truth) gives
for `tui_command`: two hand-maintained profile functions are two places to forget.
Today there are two profiles and two guards, which is tractable; a third transport
makes it a table.

A fourth concern the local case never had, recorded so it is not discovered later:
**a remote surface has a threat model.** The existing code already reflects this —
an unauthenticated bind is forced to loopback
([`ensure_loopback_bind`](../crates/bitrouter-mcp/src/server.rs:786)), and the
comment says why: it *"would expose the BYOK daemon's provider keys to the
network."* Anything reachable remotely inherits that constraint, and
[§7](#7-the-membership-rule)'s R3 ("never handles a secret") stops being a
guideline about keystrokes and becomes a network-exposure rule.

#### (5) The consequence that actually matters, and it is not about B

Apply `Reach` to this spec's own inventory
([§8](#8-the-inventory)) and the requirement's real bite appears:

| Command | `Reach` | Why |
|---|---|---|
| `models` | **Portable** | A catalog. Already on the HTTP profile |
| `status` | **Degraded** | On the HTTP profile only where the backend can answer for its own deployment; a backend with no status of its own *"gets no `status` tool rather than a fabricated one"* |
| `route` (preview) | **HostBound** | *"resolves against the serving host's own config and control socket"* — named by the crate invariant as what must never reach a multi-tenant profile |
| `route/set`, `route/reset` | **SessionBound** | A lease over a live ACP session on the serving process. Excluded twice: host-bound, and stateful under a negotiation that serves *"always statelessly"* |
| `/commands` (the agent's list) | **SessionBound** | An artefact of one live ACP connection |

⇒ **Of the six commands [§2.4](#24-the-count-is-not-the-complaint) identified,
one travels, one travels degraded, and four do not travel at all.** Not because
of transport difficulty — because the answers are bound to one machine and one
live session ([§6.11.3](#6113-bitrouter-has-already-answered-the-same-actions-but-remote-once-for-actions),
finding 20). **"The CLI and the TUI must both work remotely" is therefore
satisfiable in full only for the portable subset, and the honest form of the
requirement is: both surfaces work over both transports, and each transport
carries the rows its `Reach` admits, with the excluded ones absent rather than
dead** ([invariant 3](#15-invariants)).

That is not a workaround. The tree already behaves this way: a session started
with an explicit `--base-url` gets **no controller binding**, so `/route` is
absent rather than broken ([`acp_cli.rs:1025`](../apps/bitrouter/src/acp_cli.rs:1025),
[§1](#1-verified-starting-state)). **"Remote means fewer session verbs" is
already encoded in this codebase, deliberately, and predates the requirement.**

#### (6) What "remote via HTTP" means here, concretely

Four readings. Only one is available today.

- **(a) Tunnel ACP over HTTP.** Needs the unratified RFD, or a private framing.
  Not available. This repository has already made this call once in the
  neighbouring case: `gateways.rs` chose descriptor-passing HTTP over
  MCP-over-ACP tunnelling because tunnelling sat behind `unstable_mcp_over_acp`
  and no shipping harness advertised it
  ([§6.11.3](#6113-bitrouter-has-already-answered-the-same-actions-but-remote-once-for-actions)).
  Same reasoning, same answer.
- **(b) A separate BitRouter control protocol over HTTP.** Already exists, twice:
  the OpenAI/Anthropic-compatible routing surface at `127.0.0.1:4356`, and the MCP
  control surface at `/mcp-control` (default `127.0.0.1:4357`). The second is the
  one shaped like *"the same actions, remotely"*, and it is the answer.
- **(c) A new BitRouter-owned wire.** This is B in different clothes and is
  rejected for B's reasons plus (1)'s.
- **(d) The CLI/TUI as a client pointed at a remote base URL.** This is what
  `--base-url` already is. It is the reading the tree supports, and its existing
  behaviour — fewer session verbs, stated rather than hidden — is the behaviour
  (5) prescribes.

**Recommendation: (b) for the portable rows, over the MCP surface that already
exists, extended per the named seam at [`server.rs:825`](../crates/bitrouter-mcp/src/server.rs:825)
if the host-bound reads are ever wanted remotely — and (d) for the session verbs,
which do not travel and should say so.** Build nothing until a remote caller
actually asks: the requirement names a shape, not yet a consumer, and CLAUDE.md
rule 4 applies to shapes too.

#### (7) The trigger that reopens B

Stated explicitly, because D10's version of this promise is what the requirement
just tested. **B reopens when both of these are true**: the ACP Streamable
HTTP/WebSocket RFD ratifies into v1 (so there is a standard remote transport to
carry extension methods over), *and* a real remote consumer of BitRouter-as-ACP-agent
exists. Either alone is insufficient — a transport with no consumer is D10's
mistake, and a consumer with no transport is (1)'s. Until then, C.

---

## 17. Explicitly out of scope

- **Anything #786 removed.** Fleets, sub-agents, worktrees, review queues,
  multi-pane splits, session lists. Single-session remains the line
  (`ACP_TUI_SPEC.md` §2), and #749 remains the charter.
- **`bitrouter launch`.** The harness owns that UX; parity there would mean
  injecting BitRouter commands into someone else's TUI.
- **The `chat`/controller convergence** deferred by `ACP_CONTROLLER_SPEC.md`
  §16.3. This spec assumes today's one-shot engine and stays correct under the
  migration, because `SessionActions` is a parameter either way.
- **`complete`.** Still deferred by `ACTIONS_SPEC.md`; it has no CLI twin and
  gains nothing from a third surface.
- **A general write framework**, per [§11](#11-reads-versus-writes).
- **The daemon-owns-config question (#863)** and the cloud profile's placement.
