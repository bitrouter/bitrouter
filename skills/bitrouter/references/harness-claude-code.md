# Harness: Claude

Claude has three deliberate BitRouter facets:

- `bro connect claude --model ID` adds a persistent gateway model option
  (default `bitrouter/automatos`), including in builds without `harness`;
  see [persistent connections](connect.md) for desktop behavior and restore.

- `bro run claude` drives the maintained ACP adapter for one headless turn;
  `--background` gives controller ownership to the daemon.
- `bro claude` launches Claude Code's own native interface with
  reversible per-process routing overrides.

For local ACP sessions, install Node.js 22+ and `npx`. No `agents:` YAML is
required. A Claude Code subscription can be adopted with:

```bash
bro providers login claude-code
bro init --yes --harness claude --after exit
```

## ACP session

For daemon skill allocation, use `bro run claude "review changes" --background
--skill project:DIRECTORY_ID` (one command). Obtain directory IDs with
`bro skills list`; repeat `--skill` or `--exclude-skill` with project/global IDs.
Delivery requires that
connection's `embeddedContext` capability and uses embedded text resources as
user context. Directory manifest, content fingerprint, path and input identity are
retained in `agents show RUN`. Only explicit includes are delivered; no decision
backend is called. Acquisition belongs to `npx skills`.
This proves prompt delivery; native activation, protected supporting-file access
and vendor token/history admission are not verified. Earlier text may remain in
agent history after later exclusion. MCP-origin required entries, required skills
on native slash commands, and non-background allocation are unsupported. Tool
approval remains independent. Real Claude sessions remain unverified; local
independent fixtures establish the carrier contract.

```bash
bro run claude "summarize this repo"
bro acp serve claude
```

The pinned `@agentclientprotocol/claude-agent-acp@0.75.1` adapter uses a local
Claude CLI when its version is at least 2.1.257. Older, missing, failing, or
unresponsive CLIs use the adapter's bundled worker. `CLAUDE_CODE_EXECUTABLE`
can select a worker explicitly; it does not bypass ACP. `--direct` keeps the
adapter's own provider authentication; ordinary sessions route through
BitRouter and can auto-start the local daemon.

`init --model ID` saves the default model. `run claude PROMPT --model ID` overrides
it for one session. These launch/ACP commands do not rewrite vendor CLI config files.

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
