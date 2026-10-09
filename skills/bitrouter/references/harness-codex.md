# Harness: Codex

Codex has two deliberate BitRouter facets:

- `bro code codex` drives the built-in `codex-acp` adapter inside
  BitRouter's full-screen ACP lifecycle UI.
- `bro codex` launches Codex's own native interface with reversible
  one-shot configuration overrides.

For local ACP sessions, install Node.js 22+ and `npx`. No `agents:` YAML is
required. A ChatGPT Codex subscription can be imported with:

```bash
bro providers login openai-codex --import-existing
bro init --yes --harness codex --after exit
```

## ACP session

```bash
bro code codex
bro run codex "summarize this repo"
bro acp serve codex
```

The pinned `@agentclientprotocol/codex-acp@1.10.0` adapter uses a local Codex
CLI when its version is at least 0.153.3. Older, missing, failing, or
unresponsive CLIs use the adapter's bundled worker. `CODEX_PATH` can select a
worker explicitly; it does not bypass ACP. `--direct` keeps the adapter's own
provider authentication; ordinary sessions route through BitRouter and can
auto-start the local daemon.

With an active `openai-codex` provider, no model pin is needed: the ACP adapter
keeps the Codex CLI's native default and model picker. BitRouter maps its
declared native model names to the Codex subscription at gateway ingress.
Generic API calls still require an explicit subscription route; canonical ids,
provider-qualified routes, presets, and user-defined virtual models retain
their normal routing behavior. A daemon reload updates this mapping too.

`init --model ID` saves the default model. `code codex --model ID` overrides it
for one session. No vendor CLI config file is rewritten.

## Native interface

```bash
bro codex
bro codex --model gpt-6.1-sol
```

The native launcher supplies a `bitrouter` model provider for
`http://localhost:4356/v1` with `wire_api="responses"` through one-shot `-c`
arguments; it does not edit `~/.codex/config.toml`. `BITROUTER_API_KEY` is used
when set, otherwise the launcher supplies the placeholder accepted by the
`skip_auth: true` local default. Everything after `--` is forwarded verbatim,
and a missing local daemon is auto-started unless `--no-start` is set.

Like the ACP facet, the native launcher identifies Codex requests at gateway
ingress. Declared native model names such as `gpt-6.1-sol` remain available for
Codex model metadata and resolve through the active
`openai-codex` subscription provider.
Explicit routes and user-defined virtual models keep their normal semantics.
For its own local daemon, the launcher adds the aggregate MCP gateway only
when it is enabled and has configured MCP upstreams. External daemon targets
keep the configured gateway route because their upstreams are managed remotely.
Codex can still warn and use fallback model metadata
if its own catalog does not recognize the selected model; that warning does not
indicate a failed BitRouter route.

Existing Codex processes must be restarted before changed provider routing
takes effect. Inspect routed traffic with `bro requests`; ACP session
diagnostics live under the BitRouter home in `logs/session-*.log`.

## BRO native tools with a Codex subscription

`bro code` (without an agent ID) and `bro task run` use BRO's native harness.
Use an explicit subscription route and its known output ceiling, for example:

```yaml
inherit_defaults: true
registry:
  enabled: true
providers:
  openai-codex:
    active: true
    models:
      - id: openai/gpt-6.1-sol
        provider_model_id: gpt-6.1-sol
        api_protocol: responses
        token_limits:
          max_output_tokens: 128000
```

```bash
bro providers login openai-codex --import-existing
bro code --model openai-codex:openai/gpt-6.1-sol --max-output-tokens 128000 --workspace PATH -c bitrouter.yaml
bro task run "Inspect this workspace" --read-only --model openai-codex:openai/gpt-6.1-sol --max-output-tokens 128000 --workspace PATH -c bitrouter.yaml
```

The reservation is persisted with the Thread. It bounds admission; it does not
promise a provider-enforced cap or establish a monetary subscription cost.
The registry currently omits runtime token limits, so the example declares the
ceiling explicitly. Use metadata for the selected model; do not lower a model's
declared ceiling to make a smaller reservation pass.
