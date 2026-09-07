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

With an active `openai-codex` provider, no model pin is needed: the ACP
adapter keeps the Codex CLI's native default and model picker. BitRouter maps
its declared native model names to the Codex subscription at gateway ingress.
Generic API calls still require an explicit subscription route; canonical ids,
provider-qualified routes, presets, and user-defined virtual models retain
their normal routing behavior. A daemon reload updates this mapping too.

`init --model ID` saves the default model. `chat --model ID` overrides it for
that explicit session. No vendor CLI config file is rewritten.

Use `bitrouter status --requests` to inspect settled routed requests. Session
diagnostics live in the BitRouter home under `logs/session-*.log`.
