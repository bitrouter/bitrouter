# Repoint `AcpFeed` from `agent-proxy` to `bitrouter acp serve`

**Issue:** [bitrouter-gui#3](https://github.com/bitrouter/bitrouter-gui/issues/3)
**Date:** 2026-06-28
**Status:** Approved design — pending spec review

## Context

bitrouter [#613](https://github.com/bitrouter/bitrouter/pull/613) replaces the opaque
`bitrouter agent-proxy` with a per-session ACP substrate exposed as `bitrouter acp serve|prompt`.
In the new model the GUI is a **manager**: it spawns one `bitrouter acp serve --agent <id>`
process per session and drives it as a vanilla ACP client. `agent-proxy` is removed in #613,
so the GUI's `AcpFeed` must repoint or it breaks once #613 lands.

`AcpFeed` is already single-session, already spawns a subprocess, and already speaks the exact
ACP wire interaction `acp serve` exposes (initialize / session/new / session/prompt /
session/cancel + forwarded session/update + request_permission). So this is a small change.

### Sequencing reality (verified 2026-06-28)

- **#613 is still OPEN**, not merged (head branch `claude/beautiful-kare-6a0049`).
- The locally installed `bitrouter` binary has **no `acp` subcommand** (only `agent-proxy`).
- The target file `crates/bitrouter-gui/src/acp/feed.rs` lives on the unmerged GUI branch
  `claude/confident-gagarin-4dbd25`; this repoint rides that branch.

Consequence: the code edit + unit test can land now, but the **live round-trip** (acceptance #2)
and the **no-orphan PID check** (acceptance #3) are gated on #613 landing.

## The change

Four edits in `crates/bitrouter-gui/src/acp/feed.rs`, no structural change:

1. **L61** — `format!("{bin} agent-proxy {agent_id}")` → `format!("{bin} acp serve --agent {agent_id}")`
2. **L2** — module doc: "drives one ACP session through `bitrouter agent-proxy <id>`" →
   "... through `bitrouter acp serve --agent <id>`".
3. **`from_env` comment (L68–71)** — keep `claude-acp` as the default catalog id; update wording
   to reference `acp serve` rather than `agent-proxy`.
4. **Test `new_builds_proxy_command` (L353–357)** — rename to reflect the new command and assert
   `agent_command == "bitrouter acp serve --agent claude-code"`.

The wire interaction is unchanged. `record_id` (the manager-facing id `session/new` now returns)
is treated as opaque — the GUI already uses a fixed display `SessionId` (`GUI_SESSION`) and only
uses the ACP `session_id` to address prompts, so no identity changes are needed.

## Lifecycle / teardown

**Decision:** ship the pure repoint; do not attempt graceful stdin-close teardown in this issue.

### Why the existing teardown is sufficient for conformant agents

The `agent-client-protocol` 1.0.0 SDK does not expose a graceful "close stdin then await child
exit" shutdown. Its `AcpAgent` `connect_to` races the protocol driver against a `child_monitor`
that owns a `ChildGuard`; when the GUI's command loop returns (`StopAgent → break → connect_with
returns Ok`), the protocol future resolves first and `child_monitor` is dropped, which calls
`child.kill()` — a **SIGKILL** of the `acp serve` process.

Cascade to the upstream agent (serve's child, the GUI's grandchild):

- #613's `serve` spawns the upstream agent via the same `AcpAgent`/`ChildGuard`, and is designed
  to tear it down on manager disconnect (its `serve_on` future unwinds → `Arc<Session>` drops →
  upstream `ChildGuard` kills the agent).
- The GUI SIGKILLs `serve` before serve can run that cascade. The upstream agent therefore dies
  because serve's death closes the pipe to the agent's stdin → a conformant ACP agent
  (e.g. claude-code-acp) exits on stdin EOF.

So acceptance #3 ("no orphans") holds for conformant agents via stdin-EOF propagation, not via
serve's graceful cascade. The issue's "close stdin (disconnect)" wording describes an ideal not
reachable through the current SDK without forking it.

### Follow-up (separate issue)

File a follow-up to harden teardown once #613 lands:

- Expose / fork an SDK graceful-shutdown path (close stdin, await child exit with a SIGKILL
  backstop) so serve runs its own cascade and the no-orphan guarantee no longer depends on the
  upstream agent's EOF behavior.
- Add an integration test: spawn `acp serve`, close the GUI session, assert both the serve PID
  and the upstream agent PID are reaped within a timeout.

## Testing

- **Now (lands with the change):** update the existing unit test to assert the new command
  string. No new harness — it is a pure construction assertion.
- **Gated on #613 landing (documented, not implemented here):**
  - Acceptance #2 — a GUI session round-trips a prompt + streamed updates against a configured
    agent.
  - Acceptance #3 — closing a GUI session reaps the `acp serve` process and the upstream agent
    (no orphans). Belongs to the teardown-hardening follow-up.

## Acceptance criteria (from the issue)

- [x] `AcpFeed` spawns `bitrouter acp serve --agent <id>` (not `agent-proxy`) — this change.
- [ ] A GUI session round-trips a prompt + streamed updates — **gated on #613**.
- [ ] Closing a GUI session terminates its `acp serve` process and the upstream agent —
      satisfied for conformant agents by SIGKILL + stdin-EOF; hardened in the follow-up.

## Out of scope

- Multi-agent aggregation (running N `acp serve` processes) — the substrate stays per-session;
  the GUI remains single-session here.
- SDK graceful-shutdown fork and the orphan integration test — separate follow-up issue.
- Any translate/permission changes — the wire interaction is unchanged.
