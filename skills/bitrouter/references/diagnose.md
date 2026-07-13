# Diagnose

Real diagnostic flow against the v1 binary. There is no `bitrouter doctor` — diagnostics are composed from `status`, `route`, `models`, `providers list`, and the log file.

> **Work one hypothesis at a time.** After each change, re-run `bitrouter status` and the command that originally failed. Don't try the next fix until you've confirmed the current one didn't resolve it — shotgunning multiple changes makes it impossible to attribute the eventual success and often introduces new failures.

## First six checks

```bash
bitrouter --version                           # 1. installed?
bitrouter status                              # 2. daemon running?
bitrouter providers list                      # 3. providers configured + active?
bitrouter models                              # 4. routing table populated?
bitrouter route openai/gpt-4o                 # 5. one specific model resolves?
tail -n 80 ~/.bitrouter/bitrouter.log         # 6. recent daemon output
```

`bitrouter status` prints `bitrouter is stopped` (exit 0) when no daemon answers the control socket — that's the answer to the question, not a failure. Look at the log next.

## Symptom: "command not found" after install

```bash
command -v bitrouter
echo $PATH
ls "$HOME/.cargo/bin/bitrouter" 2>/dev/null
```

The shell installer drops the binary in `$CARGO_HOME/bin` (default `~/.cargo/bin`). Add to PATH:

```bash
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc
exec $SHELL -l
```

For Homebrew, check `brew doctor`. For npm, ensure `npm bin -g` is on PATH.

## Symptom: shell installer fails with SSL error

`curl: (60) SSL certificate problem` means your system trust store is out of date. Update root certs or fall back to `brew install bitrouter/tap/bitrouter` or `npm install -g bitrouter`.

## Symptom: brew can't find the formula

`brew install bitrouter/tap/bitrouter` should auto-tap. If it doesn't:

```bash
brew tap bitrouter/tap
brew install bitrouter
```

## Uninstall

| Installed via | Remove with |
|---|---|
| Shell installer | `rm "$(command -v bitrouter)"` |
| Homebrew | `brew uninstall bitrouter` (optionally `brew untap bitrouter/tap`) |
| npm | `npm uninstall -g bitrouter` |

State under `~/.bitrouter/` survives uninstall — `rm -rf ~/.bitrouter` for a clean slate.

## Symptom: daemon won't start

```bash
bitrouter start
# error: bitrouter is already running (pid 12345); use `restart` or `stop` first
```

A live daemon already holds the socket. Either `bitrouter restart` or `bitrouter stop && bitrouter start`. The `start` path automatically cleans up stale pid files when no process matches.

If the daemon exits within 250ms of spawn, `bitrouter start` quotes the failure log tail back to stderr — read it. Common causes:

- **Port collision.** `lsof -i :4356` to see what's bound. Edit `server.listen` to a free port, restart.
- **Config parse error.** The log tail shows the YAML line; fix and retry.
- **Database lock.** Stale sqlite lock from a SIGKILL'd previous run — `rm ~/.bitrouter/bitrouter.db-shm ~/.bitrouter/bitrouter.db-wal` (the main `.db` file is safe to keep).

## Symptom: "connection refused" from a client

```bash
curl -v http://localhost:4356/health      # daemon health
curl -v http://localhost:4356/v1/models   # routing table over HTTP
```

If `bitrouter status` says running but curl fails:

- **Wrong port.** Old skill versions said 8787 — the real default is **4356**.
- **`0.0.0.0` vs `127.0.0.1`.** `bitrouter init` writes `127.0.0.1:4356` for safety. Code default for `ServerConfig` is `0.0.0.0:4356`. Both serve localhost clients — but if the client is in a container/VM hitting the host, you need `0.0.0.0`.
- **Firewall.** macOS: `sudo pfctl -d` to test (temporary). Linux: `sudo ufw allow 4356`.

## Symptom: provider errors at request time

```bash
bitrouter providers list                  # ACTIVE column
bitrouter route openai/gpt-4o             # does the chain resolve?
```

If `ACTIVE` is `no` for a registry provider, its env var wasn't set when the daemon started — or its OAuth token is missing (`github-copilot`). Two fixes:

```bash
# A) re-export and hot-reload (no restart needed)
export OPENAI_API_KEY=sk-...
bitrouter reload

# B) for github-copilot, run the device flow
bitrouter providers login github-copilot
bitrouter reload
```

If the upstream itself is the problem, test it directly:

```bash
curl https://api.openai.com/v1/models \
  -H "Authorization: Bearer $OPENAI_API_KEY"
```

## Symptom: "model not found"

```bash
bitrouter models                          # what's actually routable
bitrouter models --provider openai        # filter
bitrouter route <exact model id>          # resolution + chain
```

Canonical model identifiers are slash-form ids such as **`openai/gpt-4o`**. A colon-form id such as **`openrouter:openai/gpt-4o`** is a deliberate provider pin: it routes directly through the named provider when that provider is active. The exact canonical id strings come from the registry or your config's `models:` list — `bitrouter models` is authoritative.

## Symptom: env var change didn't propagate

```bash
export OPENAI_API_KEY=sk-new-...
bitrouter reload          # re-pushes provider env vars into daemon
```

`reload` snapshots every env-var-credentialed provider's value from your shell and hands them to the daemon. SIGHUP reloads daemon-side config, but it cannot forward newly exported shell variables. If `bitrouter reload` does not pick up the new value, restart: `bitrouter restart`.

## Symptom: MCP / ACP not working

```bash
bitrouter tools status              # MCP server liveness + latency
bitrouter tools list                # advertised tools
bitrouter agents check              # spawn each ACP agent, verify `initialize`
```

`tools status` shows per-server latency or the error inline. `agents check` will exit with a non-success row when an ACP agent's stdio bridge fails — check that the `command` / `args` in your `agents:` config resolve on PATH (`npx`, `uvx`, etc.).

## Symptom: degraded latency

```bash
time curl -sS http://localhost:4356/health
time curl -sS http://localhost:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"openai/gpt-4o-mini","messages":[{"role":"user","content":"hi"}],"max_tokens":5}'
```

Compare to a direct upstream call. If `/health` is fast but completions are slow, the upstream is the bottleneck — pick a different provider for the same model via routing or by adjusting account_strategy.

## Where everything lives

```
~/.bitrouter/
├── bitrouter.yaml         # if you ran `bitrouter init`
├── bitrouter.db           # sqlite, virtual keys + metering
├── bitrouter.sock         # daemon control socket
├── bitrouter.pid          # daemon pid (best-effort, cleaned on graceful exit)
└── bitrouter.log          # stdout+stderr of the detached daemon
```

Per-provider OAuth tokens (github-copilot today) live under `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`. The BitRouter Cloud session created by `bitrouter cloud login` lives at `$XDG_DATA_HOME/bitrouter/account-credentials.json` (mode 0600 on Unix). On macOS both default to `~/Library/Application Support/bitrouter/`.

If `bitrouter cloud whoami` shows an expired token and the refresh exchange fails (network outage, server-side revocation), the fix is to re-run `bitrouter cloud login`. Re-login is idempotent — it overwrites the existing credentials file in place.

## Clean reset

```bash
bitrouter stop || true
rm -rf ~/.bitrouter
# reinstall is not required — state was the only thing wiped
bitrouter start
```

## Observability snapshot

```bash
bitrouter observe status            # OTel exporter wired? endpoint? cardinality?
bitrouter observe status --json     # machine-readable
```

`compiled: no` means the binary was built without the OTel feature — install from a release build, not a custom `cargo install --no-default-features` invocation.
