# Code TUI adapter smoke evidence

Run on 2026-09-08 for the Code TUI work. This is an ACP `initialize` smoke
only. It records what each adapter actually advertised; it does not establish
that a prompt, tool call, session lifecycle operation, or terminal login
works.

## Scope and isolation

Each probe used a new temporary working directory and an empty `HOME`,
`XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `CODEX_HOME`, and
`CLAUDE_CONFIG_DIR`. The environment was constructed with `env -i`, carried
no API keys or inherited credentials, set `NO_BROWSER=1` and `CI=1`, and used
an empty `OPENCODE_CONFIG` for OpenCode. OpenCode also received `--pure`.

The client sent exactly one newline-delimited JSON-RPC request and no
follow-up request:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"auth":{"terminal":false}}}}
```

There was no `session/new`, prompt, model request, authentication request, or
provider credential. After the matching response, the probe closed stdin and
terminated the adapter's dedicated process group, waiting up to three seconds
before a forced kill. Codex ACP and OpenCode report `-15` after that deliberate
SIGTERM; Claude ACP exited cleanly with status 0. The process snapshots after
each probe found no child from the probe.

## Versions and launch inputs

| Component | Observed version | Launch evidence |
| --- | --- | --- |
| Node.js | `v22.22.3` | `node --version` |
| npm/npx | `10.9.8` | `npx --version` |
| Codex CLI | `codex-cli 0.153.4` | Passed as `CODEX_PATH` to the cached Codex ACP adapter; it meets that adapter's local minimum. |
| `@agentclientprotocol/codex-acp` | `1.10.0` | Exact package already existed in the npm cache. Its cache-local `dist/index.js` reported `@agentclientprotocol/codex-acp 1.10.0`; it was invoked with Node because an isolated `npx --offline` did not expose a package shim. |
| Claude Code | `2.1.175` | Recorded only. It is below the repository's preferred local-Claude threshold, so it was deliberately excluded from the Claude ACP probe's `PATH`. |
| `@agentclientprotocol/claude-agent-acp` | `0.75.1` | Fetched with `npx --yes --package=@agentclientprotocol/claude-agent-acp@0.75.1 claude-agent-acp --version` into the isolated npm cache. It printed `0.75.1`; the subsequent adapter run used its cached entry point and no local `claude` executable. |
| OpenCode | `1.18.3` | Invoked as `opencode --pure acp --cwd "$SMOKE/cwd"`. |

The cache-local entry points are intentionally not treated as stable paths.
For reproduction, create `$SMOKE`, apply the isolation above, use `npx --yes`
to materialize the pinned package in `$SMOKE/npm-cache`, and invoke that
package's `dist/index.js` with Node. The probe's JSON line is the request shown
above.

The version and adapter commands used were:

```text
node --version
npx --version
codex --version
claude --version
opencode --version
node "$CODEX_ACP_ENTRY"
npx --yes --package=@agentclientprotocol/claude-agent-acp@0.75.1 claude-agent-acp --version
node "$CLAUDE_ACP_ENTRY"
opencode --pure acp --cwd "$SMOKE/cwd"
```

`CODEX_ACP_ENTRY` and `CLAUDE_ACP_ENTRY` refer to the respective exact cached
package's `dist/index.js`; neither is a repository path or an installed global
adapter.

## Observed responses

The lists below are the fields actually present in the `initialize` response
for the baseline client capabilities above. Omitted fields are not inferred to
be unsupported in another negotiation.

### Codex ACP 1.10.0

`protocolVersion` was `1`; `agentInfo` was
`@agentclientprotocol/codex-acp` / `Codex` / `1.10.0`.

`agentCapabilities` advertised:

- `loadSession`
- prompt `embeddedContext` and `image`
- session `additionalDirectories`, `close`, `delete`, `fork`, `list`,
  `resume`, and `subagents`
- MCP `http`; it explicitly reported `acp: false` and `sse: false`
- auth `logout`, plus `_meta.authStatus`
- `providers` as an empty object

It offered one auth method, `api-key` ("Use an API key to authenticate"). Its
top-level metadata advertised steering; a goal extension with `set`, `pause`,
`resume`, and `clear`; and JetBrains AIR `sessionFailure`,
`agentFileChangeReport`, `nativeSubagentSessions`, and `asyncTasks`.

### Claude Agent ACP 0.75.1

`protocolVersion` was `1`; `agentInfo` was
`@agentclientprotocol/claude-agent-acp` / `Claude Agent` / `0.75.1`.

`agentCapabilities` advertised:

- `loadSession`
- prompt `embeddedContext` and `image`
- session `additionalDirectories`, `close`, `delete`, `fork`, `list`,
  `resume`, and `subagents`
- MCP `http` and `sse`
- auth `logout`, `_meta.authStatus`, and `_meta.claudeCode.promptQueueing`
- `providers` as an empty object

It offered no auth method in the credential-free isolated environment. Its
top-level metadata advertised steering; a goal extension with `set` and
`clear`; and the same JetBrains AIR capabilities as Codex ACP:
`sessionFailure`, `agentFileChangeReport`, `nativeSubagentSessions`, and
`asyncTasks`.

### OpenCode 1.18.3

`protocolVersion` was `1`; `agentInfo` was `OpenCode` / `1.18.3`.

`agentCapabilities` advertised:

- `loadSession`
- prompt `embeddedContext` and `image`
- session `close`, `fork`, `list`, and `resume`
- MCP `http` and `sse`

It offered `opencode-login` ("Run `opencode auth login` in the terminal").
No other capability or metadata is asserted here because it was absent from
this response.

## Limits

This evidence is deliberately narrower than the fixture-backed protocol tests
and real-PTY journeys required by `CODE_TUI_UX_SPEC.md`. In particular, the
handshake did not negotiate optional client extensions, send a session request,
or exercise the returned capabilities. It prevents a feature matrix from being
derived from native CLI documentation, but it is not an end-to-end adapter
conformance claim.
