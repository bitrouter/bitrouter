# Harness: Claude ACP

BitRouter supplies the terminal UI. `claude-acp` is a built-in ACP agent and
requires no YAML agent entry. Node.js 22+ and `npx` are required.

```bash
bitrouter providers login claude-code
bitrouter init --yes --harness claude --after exit
bitrouter
```

An existing Claude CLI login is adopted by the same
`bitrouter providers login claude-code` command.

For an explicit session or a headless prompt:

```bash
bitrouter chat claude-acp
bitrouter spawn claude-acp -p "summarize this repo"
```

The adapter uses a local Claude CLI when its version is at least 2.1.257.
Older, missing or unresponsive local CLIs use the adapter's bundled worker.
`CLAUDE_CODE_EXECUTABLE` can explicitly select a worker; it does not bypass ACP.
`--direct` keeps the adapter's own provider authentication; ordinary sessions
route through BitRouter and auto-start the local daemon.

`init --model ID` saves the default model. `chat --model ID` overrides it for
that explicit session. No vendor CLI config file is rewritten.

Use `bitrouter status --requests` to inspect settled routed requests. Session
diagnostics live in the BitRouter home under `logs/session-*.log`.
