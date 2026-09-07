# Harness: Codex ACP

BitRouter supplies the terminal UI. `codex-acp` is a built-in ACP agent and
requires no YAML agent entry. Node.js 22+ and `npx` are required.

```bash
bitrouter providers login openai-codex
bitrouter init --yes --harness codex --after exit
bitrouter
```

An existing vendor CLI login can be imported with
`bitrouter providers login openai-codex --import-existing`.

For an explicit session or a headless prompt:

```bash
bitrouter chat codex-acp
bitrouter spawn codex-acp -p "summarize this repo"
```

The adapter uses a local Codex CLI when its version is at least 0.153.3.
Older, missing or unresponsive local CLIs use the adapter's bundled worker.
`CODEX_PATH` can explicitly select a worker; it does not bypass ACP.
`--direct` keeps the adapter's own provider authentication; ordinary sessions
route through BitRouter and auto-start the local daemon.

`init --model ID` saves the default model. `chat --model ID` overrides it for
that explicit session. No vendor CLI config file is rewritten.

Use `bitrouter status --requests` to inspect settled routed requests. Session
diagnostics live in the BitRouter home under `logs/session-*.log`.
