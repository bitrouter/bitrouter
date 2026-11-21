# Harness: Harbor Terminus-2

Wire Harbor's neutral Terminus-2 reference agent through BitRouter. Terminus-2
uses one interactive `tmux` tool and emits bounded JSON or XML actions, making it
useful for comparing models without a first-party coding-agent scaffold.

Official reference: <https://www.harborframework.com/docs/agents/terminus-2>

## Prerequisites

- BitRouter is running and the selected upstream model is routable.
- Harbor is installed (`uv tool install harbor`).
- The task runs in a sandbox. Terminus-2 is autonomous and should not be pointed
  at an unrestricted host environment.

## Point Terminus-2 at BitRouter

Terminus-2's `api_base` is an agent option. Use the OpenAI Chat Completions
shape and configure the selected model or named gateway router.

```python
from harbor.models.agent_name import AgentName
from harbor.models.trial.config import AgentConfig

agent_config = AgentConfig(
    name=AgentName.TERMINUS_2,
    model_name="openai/bitrouter/coding",
    kwargs={
        "api_base": "http://127.0.0.1:4356/v1",
        "parser_name": "json",
        "enable_summarize": True,
    },
)
```

For the local `server.skip_auth: true` default, set `OPENAI_API_KEY=unused` in
the Harbor process. For an authenticated local daemon, set it to the BitRouter
virtual key. For Cloud, use `https://api.bitrouter.ai/v1` and a `brk_*` key.

The model name has a LiteLLM provider prefix. `openai/bitrouter/coding` sends the
BitRouter router `bitrouter/coding`; replace it with the router or model exposed by your
BitRouter configuration.

For an explicit Claude subscription route, use LiteLLM's Anthropic provider so
the complete downstream hop is Anthropic Messages rather than Chat
Completions:

```yaml
agent:
  name: terminus-2
  model_name: anthropic/claude-sonnet-5
  env:
    ANTHROPIC_API_KEY: bitrouter-local
  kwargs:
    api_base: http://CENTRAL_PRIVATE_HOST:4356
    llm_kwargs:
      api_key: bitrouter-local
```

Map the daemon's fixed policy tier to
`claude-code:claude-sonnet-5`. The local key is only the non-secret downstream
credential; `CLAUDE_CODE_OAUTH_TOKEN` remains on the central daemon, where
BitRouter constructs the Claude Code-compatible upstream headers and identity
system block while preserving Terminus 2's own system instructions.

Do not append `/v1` to the Anthropic `api_base`: LiteLLM's Anthropic handler
adds `/v1/messages` itself. A base ending in `/v1` becomes
`/v1/v1/messages`, fails with 404 before reaching BitRouter, and produces no
route trace. The OpenAI provider configuration above still requires `/v1`.
Keep session and trial correlation in Harbor.

Run a Harbor task with `terminus-2` after applying the equivalent agent config:

```bash
harbor run \
  --agent terminus-2 \
  --model openai/bitrouter/coding \
  --path /path/to/tasks \
  --task-name hello-world
```

## Inspect gateway requests

Use `bitrouter requests` to confirm which model and provider served the
trial. Keep trial identifiers and task outcomes in Harbor. Use the gateway's
request IDs to correlate each outcome with usage when you submit [Eval
evidence](workflow-optimization.md).
