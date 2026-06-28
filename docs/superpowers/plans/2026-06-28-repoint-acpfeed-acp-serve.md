# Repoint `AcpFeed` to `bitrouter acp serve` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Repoint `AcpFeed` from the removed `bitrouter agent-proxy <id>` to the new per-session `bitrouter acp serve --agent <id>` so the GUI keeps working once bitrouter #613 lands.

**Architecture:** `AcpFeed` already spawns one subprocess per GUI session and drives it as a vanilla ACP client. Only the command string it builds changes; the ACP wire interaction, identity handling, and teardown are unchanged. Teardown hardening is explicitly a separate follow-up (see spec).

**Tech Stack:** Rust, `agent-client-protocol` 1.0.0, tokio. Build/test via `cargo`.

**Spec:** `docs/superpowers/specs/2026-06-28-repoint-acpfeed-acp-serve-design.md`

**Branch base:** `claude/confident-gagarin-4dbd25` (where `AcpFeed` lives).

---

### Task 1: Repoint the command string + docs and update the unit test

**Files:**
- Modify: `crates/bitrouter-gui/src/acp/feed.rs:2` (module doc)
- Modify: `crates/bitrouter-gui/src/acp/feed.rs:61` (command builder)
- Modify: `crates/bitrouter-gui/src/acp/feed.rs:68-71` (`from_env` comment)
- Test: `crates/bitrouter-gui/src/acp/feed.rs:353-357` (unit test, same file)

- [ ] **Step 1: Update the failing unit test first**

In `crates/bitrouter-gui/src/acp/feed.rs`, replace the existing test (currently lines 353-357):

```rust
    #[test]
    fn new_builds_serve_command() {
        let feed = AcpFeed::new("bitrouter", "claude-code");
        assert_eq!(feed.agent_command, "bitrouter acp serve --agent claude-code");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p bitrouter-gui new_builds_serve_command`
Expected: FAIL — assertion mismatch, `left: "bitrouter agent-proxy claude-code"`, `right: "bitrouter acp serve --agent claude-code"` (the old `new_builds_proxy_command` name no longer exists, and the production code still builds the `agent-proxy` string).

- [ ] **Step 3: Repoint the command builder**

In `crates/bitrouter-gui/src/acp/feed.rs`, change line 61 inside `AcpFeed::new`:

```rust
            agent_command: format!("{bin} acp serve --agent {agent_id}"),
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p bitrouter-gui new_builds_serve_command`
Expected: PASS.

- [ ] **Step 5: Update the module doc (line 2)**

In `crates/bitrouter-gui/src/acp/feed.rs`, change the first doc line from:

```rust
//! `AcpFeed` — a real `Feed` that drives one ACP session through
//! `bitrouter agent-proxy <id>`. Owns a tokio runtime on a dedicated thread
```

to:

```rust
//! `AcpFeed` — a real `Feed` that drives one ACP session through
//! `bitrouter acp serve --agent <id>`. Owns a tokio runtime on a dedicated thread
```

- [ ] **Step 6: Update the `from_env` comment (lines 68-71)**

In `crates/bitrouter-gui/src/acp/feed.rs`, the comment above the `agent` binding currently reads:

```rust
        // `claude-acp` is the bitrouter catalog id for Anthropic Claude (Zed's
        // `claude-code-acp`); verified against `bitrouter agents list`. Override
        // with BITROUTER_GUI_AGENT for any other configured agent.
```

Update it to reference the new subcommand (the catalog id is unchanged; only the surrounding command changes):

```rust
        // `claude-acp` is the bitrouter catalog id for Anthropic Claude (Zed's
        // `claude-code-acp`), passed as `acp serve --agent <id>`; verified against
        // `bitrouter agents list`. Override with BITROUTER_GUI_AGENT for any other
        // configured agent.
```

- [ ] **Step 7: Build and run the full crate test suite**

Run: `cargo test -p bitrouter-gui`
Expected: PASS — no other test references `agent-proxy` (confirmed: the only match is the test updated in Step 1).

- [ ] **Step 8: Verify no stray `agent-proxy` references remain in the GUI crate**

Run: `grep -rn "agent-proxy" crates/bitrouter-gui/src`
Expected: no output (all references repointed).

- [ ] **Step 9: Commit**

```bash
git add crates/bitrouter-gui/src/acp/feed.rs
git commit -m "feat(gui): repoint AcpFeed to bitrouter acp serve --agent

Replaces the removed agent-proxy invocation (bitrouter #613) with the
per-session acp serve substrate. Wire interaction, identity, and teardown
are unchanged.

Refs: https://github.com/bitrouter/bitrouter-gui/issues/3

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: File the teardown-hardening follow-up issue

Per the spec, true graceful teardown isn't reachable through the current ACP SDK and is out of scope here. Capture it so it isn't lost.

**Files:** none (GitHub issue only).

- [ ] **Step 1: Open the follow-up issue**

Run:

```bash
gh issue create --repo bitrouter/bitrouter-gui \
  --title "Harden AcpFeed session teardown (graceful close + orphan test)" \
  --body "$(cat <<'EOF'
Follow-up to #3.

Today, closing a GUI session SIGKILLs the `acp serve` process (via the
`agent-client-protocol` SDK's `ChildGuard` on connection drop). The upstream
agent dies only because serve's death closes the pipe to its stdin and a
conformant ACP agent exits on stdin EOF — serve never gets to run its own
teardown cascade. The SDK does not expose a graceful "close stdin, await child
exit" shutdown.

### Work
- Expose / fork an SDK graceful-shutdown path: close the child's stdin, await
  its exit with a SIGKILL backstop, so `acp serve` runs its own cascade and the
  no-orphan guarantee no longer depends on the upstream agent's EOF behavior.
- Add an integration test: spawn `acp serve`, close the GUI session, assert both
  the serve PID and the upstream agent PID are reaped within a timeout
  (acceptance criterion #3 from #3).

### Gating
Integration test is gated on bitrouter #613 landing (the `acp serve` CLI).
EOF
)"
```

Expected: prints the new issue URL.

- [ ] **Step 2: (Optional) note the follow-up issue number in the spec**

If desired, add the returned issue number to the "Follow-up (separate issue)" section of
`docs/superpowers/specs/2026-06-28-repoint-acpfeed-acp-serve-design.md`, then:

```bash
git add docs/superpowers/specs/2026-06-28-repoint-acpfeed-acp-serve-design.md
git commit -m "docs: link teardown-hardening follow-up issue"
```

---

## Gated verification (do NOT block this plan)

These acceptance criteria from issue #3 require bitrouter #613 to land first (it is still an open
PR, head branch `claude/beautiful-kare-6a0049`; the locally installed `bitrouter` has no `acp`
subcommand). Run them once `bitrouter acp serve` exists:

- **Acceptance #2 — live round-trip:** launch the GUI against a configured agent, send a prompt,
  confirm streamed `session/update`s render.
- **Acceptance #3 — no orphans:** close a GUI session; confirm the `acp serve` process and the
  upstream agent process both exit (no orphan PIDs). This belongs to the Task 2 follow-up.

## Notes

- `record_id` (returned by `session/new`) stays opaque — the GUI already uses a fixed display
  `SessionId` and only uses the ACP `session_id` to address prompts. No identity change.
- Multi-agent aggregation (N `acp serve` processes) is out of scope; `AcpFeed` stays single-session.
