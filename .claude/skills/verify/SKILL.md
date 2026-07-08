---
name: verify
description: Verify bitrouter acp/agents CLI changes end-to-end against a real ACP agent (claude-code-acp). Use when verifying changes under crates/bitrouter-substrate or apps/bitrouter's acp_cli/agents surfaces.
---

# Verifying the ACP substrate CLI

Build: `cargo build -p bitrouter` → `target/debug/bitrouter`.

## Real-agent workspace (claude-acp via Claude Code login)

```bash
E2E=$(mktemp -d /tmp/acp-e2e.XXXX) && cd $E2E && git init -q && git commit -qm init --allow-empty
```

**Gotcha — nested Claude Code env breaks the agent.** When verifying from
inside a Claude Code session, the spawned claude-code-acp inherits
`ANTHROPIC_BASE_URL` + `CLAUDE_CODE_*` vars and its `session/new` fails with
"Query closed before response received". The agent config can only SET env,
not unset — wrap the command in `env -u`:

```yaml
agents:
  claude-acp:
    name: claude-acp
    transport:
      type: stdio
      command: env
      args: ["-u", "CLAUDECODE", "-u", "CLAUDE_CODE_ENTRYPOINT", "-u", "ANTHROPIC_BASE_URL",
             "-u", "CLAUDE_CODE_SDK_HAS_OAUTH_REFRESH", "-u", "CLAUDE_CODE_SDK_HAS_HOST_AUTH_REFRESH",
             "-u", "CLAUDE_CODE_EXECPATH", "-u", "CLAUDE_CODE_SESSION_ID", "-u", "CLAUDE_CODE_CHILD_SESSION",
             "-u", "CLAUDE_CODE_OAUTH_SCOPES", "-u", "CLAUDE_AGENT_SDK_VERSION",
             "npx", "-y", "@zed-industries/claude-code-acp@latest"]
```

Prewarm the npx cache once (`npx -y @zed-industries/claude-code-acp@latest </dev/null`)
or the first `agents check` blows the 10s initialize budget on download.

## Flows worth driving

- `agents list` / `list --remote` (live registry) / `check` (real initialize) / `install <id>`.
- `acp prompt --agent claude-acp "Reply with exactly PONG"` → NDJSON on stdout,
  `acp turn completed` telemetry on stderr, record + transcript under
  `.bitrouter/sessions/`.
- `--worktree <name>`: agent runs in `.bitrouter/worktrees/<name>`; retained after exit.
- Headless permissions: a file-write prompt makes the agent request permission —
  prompt mode must auto-DENY and complete (regression: it used to hang).
- `acp serve --warm --idle-timeout 10`: drive stdio JSON-RPC (initialize →
  session/new → prompt, answering `session/request_permission` with the
  allow_once optionId), close stdin, then `acp attach <record-prefix>` and run
  initialize → session/load (transcript replay) → live prompt; verify idle reap
  + record settled (`socket: null`). A python line-driver beats bash here (the
  permission response needs the request's id echoed back).
- `acp sessions`: crashed sessions show `dead` (pid liveness).

**Cleanup gotcha:** killing the CLI with a signal leaks the npx/node agent
children (no destructors run) — `pkill -f claude-code-acp` after crashes.
