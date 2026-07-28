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
shape so Harbor includes its generated `session_id` in the request body and the
`X-Session-ID` header:

```python
from harbor.models.agent_name import AgentName
from harbor.models.trial.config import AgentConfig

agent_config = AgentConfig(
    name=AgentName.TERMINUS_2,
    model_name="openai/@coding",
    kwargs={
        "api_base": "http://127.0.0.1:4356/v1",
        "parser_name": "json",
        "enable_summarize": True,
        "session_id": "stable-trial-session-id",
    },
)
```

For the local `server.skip_auth: true` default, set `OPENAI_API_KEY=unused` in
the Harbor process. For an authenticated local daemon, set it to the BitRouter
virtual key. For Cloud, use `https://api.bitrouter.ai/v1` and a `brk_*` key.

The model name has a LiteLLM provider prefix. `openai/@coding` sends the
BitRouter preset `@coding`; replace it with the preset or model exposed by your
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
Current bridge-capable BitRouter builds also unwrap Terminus/LiteLLM's
`extra_body` and remove its body-level `session_id`. Harbor's native
`X-Session-ID` and body `session_id` are sufficient for adapter diagnostics;
they are not routing inputs or strict reward identifiers.

Run a Harbor task with `terminus-2` after applying the equivalent agent config:

```bash
harbor run \
  --agent terminus-2 \
  --model openai/@coding \
  --path /path/to/tasks \
  --task-name hello-world
```

## Enable workflow evidence

Set these on the BitRouter daemon before starting it:

```bash
export BITROUTER_WORKFLOW_TRACE_HARNESS=terminus_2
export BITROUTER_WORKFLOW_TRACE_JSONL="$PWD/artifacts/traces.jsonl"
export BITROUTER_POLICY_DECISION_JSONL="$PWD/artifacts/policy-decisions.jsonl"
bitrouter start --config ./bitrouter.yaml
```

The ingress capture persists a provider-neutral request ID and can promote
`X-Session-ID` or body `session_id` into diagnostic workflow identity. It does
not require callers to add `x-bitrouter-*` workflow, role, trial, or
fingerprint headers. Native evidence and the official Terminus prompt contract
help the Terminus adapter explain a trace; they never affect the policy key,
tier, or exploration eligibility.

## Compaction-aware identity

Terminus-2 performs summarization with three subagents and encodes their
identity in Harbor's generated session ids:

- `<root>-summarization-<N>-summary`
- `<root>-summarization-<N>-questions`
- `<root>-summarization-<N>-answers`
- `<root>-cont-<N>` for the resumed main agent

BitRouter retains the complete value as diagnostic session evidence, extracts
`<root>` as a diagnostic parent, and can group the related requests into
context epoch `N`. Summary starts compaction, questions and answers continue
it, and the resumed main request records `main_resume`. This suffix evidence
takes precedence over prompt inference for diagnostics only. Interleaved trials
remain manageable through their harness records, but neither trial nor role is
a policy input.

Benchmark bundles and reward feedback are source-neutral. They require unique,
persisted request IDs with exact trace/usage/decision/outcome joins and
authoritative settlement evidence. Session, role, prompt, run, and trial data
may be retained to diagnose a benchmark, but are not bundle acceptance gates or
learning-admission keys. Unknown Terminus roles fall back to generic diagnostic
evidence; they do not force a tier or suppress exploration.

## Benchmark checklist

1. Use one immutable output directory per run.
2. Keep the task list, model, parser, retry count, and attempt count fixed.
3. Verify every request has a persisted request ID, trace, policy decision,
   authoritative usage/charge evidence, and an exactly matching outcome when
   outcomes are supplied. Retain run, trial, parent-session, role, and epoch
   fields as diagnostic context when available.
4. Treat infrastructure errors separately from verifier failures.
5. Export metering and build the strict evidence bundle as described in
   `references/metering.md`.

## Gotchas

- Prefer Chat Completions for general current Terminus-2 session diagnostics.
  For an explicit `claude-code:<model>` route, use the Anthropic configuration
  above; do not add BitRouter-private workflow headers or
  `llm_call_kwargs.extra_headers`.
- A Claude Pro/Max subscription is valid only through an explicit
  `claude-code:<model>` BitRouter route. Terminus-2 keeps its normal downstream
  request shape; the central daemon owns `CLAUDE_CODE_OAUTH_TOKEN` and adds the
  upstream OAuth/Claude-Code headers and agent identity system block. Never copy
  any of them into Harbor or the sandbox. Bare Claude models do not
  auto-cascade onto the subscription.
- Do not use prompt hashes as benchmark identity. They are only a low-confidence
  fallback for ordinary traffic.
- Do not interpret a cache hit as a routing signal yet. Cache-aware settlement
  is recorded now; using expected cache reuse in model selection is a separate
  policy feature.
