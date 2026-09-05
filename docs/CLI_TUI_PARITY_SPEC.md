# Spec: CLI ↔ TUI parity — what "the same command" can and cannot mean

Status: **proposed** · Author: Claude (with Spikel) · Date: 2026-09-05
· Branch: `claude/cli-tui-parity-spec`
· Measured at `43ae57d8` (`claude/actions-table-phase04`)
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
threads, not blog roundups. Six findings changed this design; they are marked
**⇒**. The rest is corroboration.

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
   guard in [§12](#12-what-guards-it) is the only thing standing between this
   design and the drift it exists to remove. Treat that guard as load-bearing
   rather than as belt-and-braces — it is now the whole belt.

This also removes the last reason to keep the door open on B "in case a GUI
appears": if one ever does, it consumes the same ACP surface `chat` does, and
the question reopens then with a real consumer to design against.

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
