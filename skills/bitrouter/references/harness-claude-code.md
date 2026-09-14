# Harness: Claude

Claude has two deliberate BitRouter facets:

- `bro code claude` drives the built-in `claude-acp` adapter inside
  BitRouter's full-screen ACP lifecycle UI.
- `bro claude` launches Claude Code's own native interface with
  reversible per-process routing overrides.

For local ACP sessions, install Node.js 22+ and `npx`. No `agents:` YAML is
required. A Claude Code subscription can be adopted with:

```bash
bro providers login claude-code
bro init --yes --harness claude --after exit
```

## ACP session

```bash
bro code claude
bro run claude "summarize this repo"
bro acp serve claude
```

The pinned `@agentclientprotocol/claude-agent-acp@0.75.1` adapter uses a local
Claude CLI when its version is at least 2.1.257. Older, missing, failing, or
unresponsive CLIs use the adapter's bundled worker. `CLAUDE_CODE_EXECUTABLE`
can select a worker explicitly; it does not bypass ACP. `--direct` keeps the
adapter's own provider authentication; ordinary sessions route through
BitRouter and can auto-start the local daemon.

`init --model ID` saves the default model. `code claude --model ID` overrides
it for one session. No vendor CLI config file is rewritten.

## Native interface

```bash
bro claude
bro claude -- -p "summarize this repo"
```

The native launcher starts Claude Code with `ANTHROPIC_BASE_URL` pointed at
BitRouter and sets `ANTHROPIC_AUTH_TOKEN`, never silently editing
`~/.claude/settings.json`. Everything after `--` is forwarded verbatim. A
missing local daemon is auto-started unless `--no-start` is set.

Use `ANTHROPIC_AUTH_TOKEN`, not `ANTHROPIC_API_KEY`, for BitRouter inbound
authentication. Token precedence is an exported `ANTHROPIC_AUTH_TOKEN`, then
`BITROUTER_API_KEY`, then the `bitrouter-local` placeholder used by the
`skip_auth: true` local default.

Claude sends bare Anthropic model ids such as `claude-sonnet-4-6`; verify their
effective route with `bro route claude-sonnet-4-6`. Existing Claude Code
processes must be restarted before changed environment routing takes effect.
Inspect routed traffic with `bro requests`; ACP session diagnostics live
under the BitRouter home in `logs/session-*.log`.
