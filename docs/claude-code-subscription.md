# Claude Code on your Claude subscription (with telemetry)

Run Claude Code on your **Claude Pro/Max subscription** through BitRouter, with
BitRouter in the path purely for side-effects — observability today, optional
model rerouting tomorrow. From a freshly-installed `bitrouter`:

**1. Adopt your existing Claude Code session as the `claude-code` subscription provider** (drives the `claude` CLI's own login if you're not signed in yet):

```bash
bitrouter providers login claude-code
```

**2. Turn on full first-party telemetry** (off by default) — create `~/.bitrouter/bitrouter.yaml`:

```yaml
server:
  skip_auth: true          # local daemon: admit credential-less spawn traffic
plugins:
  bitrouter-observe:
    telemetry:
      enabled: true        # nothing is exported unless you opt in
      level: full          # metadata + request/response content (use `metadata` to omit content)
                           # endpoint omitted → defaults to https://telemetry.bitrouter.ai
```

**3. Start the daemon in the background and verify it's up:**

```bash
bitrouter start            # detached; logs to ~/.bitrouter/bitrouter.log
bitrouter status           # running: yes — listen 127.0.0.1:4356
bitrouter observe status   # telemetry exporter endpoint + state
```

**4. Launch an interactive Claude Code session pointed at BitRouter:**

```bash
bitrouter spawn -a claude  # interactive; run `bitrouter stop` when you're done
```

Genuine Claude Code traffic — recognised by its `anthropic-beta: claude-code-*` agent-profile marker — is routed to your subscription; anything else falls through to your other configured providers. Telemetry is attributed to an anonymous install id. *(Optional: run `bitrouter cloud login` first to also serve non-Claude-Code models from your BitRouter Cloud account.)*
