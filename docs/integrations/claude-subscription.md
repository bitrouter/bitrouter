---
title: Claude subscription
description: Route your Claude Pro or Max plan through BitRouter — OAuth, no Anthropic API key, no per-token bill.
sourceHash: 2ede6363832e91c518560ddcd0a44a5e496bb5fbf021f60d8675abdf75a194fa
---

Already paying for Claude Pro or Max? Use that plan as a model source. `bitrouter providers login claude-code` adopts the live Claude Code session, stores a refreshing credential, and attaches it to requests routed to the `claude-code` provider — so your subscription's usage covers the tokens and there's no `ANTHROPIC_API_KEY` to manage.

<Callout type="info">
**Subscription, not API key.** This is the OAuth path — it bills against your Claude plan. If you'd rather pay per token with an Anthropic API key, skip the login and set `ANTHROPIC_API_KEY` in the environment instead; the separate `anthropic` provider picks it up automatically.
</Callout>

## Log in

```bash
bitrouter providers login claude-code
```

This reuses your local Claude Code session when one is available, and drives Claude Code's own login flow if needed. The credential is stored under `$XDG_DATA_HOME/bitrouter/oauth-tokens.json` and auto-refreshes — you log in once. To remove it later:

```bash
bitrouter providers logout claude-code      # remove the stored credential
```

<Callout type="warn">
**One auth mode per request.** A request routed to `claude-code:<model>` uses your OAuth subscription; a request routed to the `anthropic` provider uses `ANTHROPIC_API_KEY`. Run `bitrouter providers logout claude-code` to remove the subscription route.
</Callout>

### Multiple accounts

Each credential is keyed by `(provider, label)`. Pass `--label` to keep more than one Claude account side by side:

```bash
bitrouter providers login claude-code --label work
bitrouter providers login claude-code --label personal
```

## Route to it

No `bitrouter.yaml` block is required — the `claude-code` provider is enabled automatically when the stored credential exists. Address Claude models through the explicit subscription provider route:

```bash
bitrouter route claude-code:claude-sonnet-4-6
```

Then [start BitRouter and send a request](/docs/integrations/models#start-bitrouter-and-send-a-request). Use the explicit id `claude-code:claude-sonnet-4-6` to pin the request to your subscription, or use the Claude Code harness flow below so BitRouter detects genuine Claude Code traffic and rewrites bare Claude model names to the subscription provider.

## Run Claude Code through BitRouter (with telemetry)

Use BitRouter as a transparent harness for the Claude Code CLI — with BitRouter in the path purely for side-effects: observability today, optional model rerouting tomorrow. From a freshly-installed `bitrouter`:

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
bitrouter launch -a claude  # interactive; run `bitrouter stop` when you're done
```

Genuine Claude Code traffic — recognised by its `anthropic-beta: claude-code-*` agent-profile marker — is routed to your subscription; anything else falls through to your other configured providers. Telemetry is attributed to an anonymous install id. *(Optional: run `bitrouter cloud login` first to also serve non-Claude-Code models from your BitRouter Cloud account.)*

## Learn more

- [Claude Code](/docs/integrations/claude-code) — point the Claude Code CLI at BitRouter (a harness), distinct from using your plan as a model source above.
- [Models](/docs/concepts/models) — the full `provider/model` id scheme.
- [Model fallback](/docs/features/model-fallback) — fail over from your subscription to a hosted model on overload.
