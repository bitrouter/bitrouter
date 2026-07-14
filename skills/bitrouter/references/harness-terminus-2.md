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

The ingress capture preserves the provider-neutral request id and promotes
`X-Session-ID` or body `session_id` into structured workflow identity. It
attaches:

- `x-bitrouter-parent-session-id`
- `x-bitrouter-agent-session-id`
- `x-bitrouter-agent-role`
- `x-bitrouter-context-epoch`
- `x-bitrouter-context-transition`
- `x-bitrouter-session-fingerprint`

The fingerprint is a SHA-256 digest of benchmark run, trial, parent session,
and context epoch. It is attribution metadata and does not enter the routing
key.

## Compaction-aware identity

Terminus-2 performs summarization with three subagents and encodes their
identity in Harbor's generated session ids:

- `<root>-summarization-<N>-summary`
- `<root>-summarization-<N>-questions`
- `<root>-summarization-<N>-answers`
- `<root>-cont-<N>` for the resumed main agent

BitRouter retains the complete value as the agent session id, extracts `<root>`
as the parent, and groups all four requests into context epoch `N`. Summary
starts compaction, questions and answers continue it, and the resumed main
request records `main_resume`. This suffix evidence takes precedence over
prompt inference. Interleaved trials remain isolated by benchmark run, trial,
and root parent session.

For benchmark-grade bundles, the driver or gateway in front of BitRouter must
also attach immutable `x-bitrouter-benchmark-run-id` and
`x-bitrouter-trial-id` headers. A bundle with decisions is rejected if any
Terminus-2 request has an unknown role, incomplete identity, duplicate request
id, or a trace/decision identity mismatch. Ordinary traffic remains fail-open:
an unrecognized Terminus role stays on the strong tier and is not explored on a
cheaper model.

## Benchmark checklist

1. Use one immutable output directory per run.
2. Keep the task list, model, parser, retry count, and attempt count fixed.
3. Verify every request has trace, policy-decision, provider-reported usage,
   charge evidence, run id, trial id, parent session, role, epoch, and
   fingerprint.
4. Treat infrastructure errors separately from verifier failures.
5. Export metering and build the strict evidence bundle as described in
   `references/metering.md`.

## Gotchas

- Prefer Chat Completions for current Terminus-2 session correlation. Harbor's
  Responses path does not carry the same `session_id` fields.
- Do not use prompt hashes as benchmark identity. They are only a low-confidence
  fallback for ordinary traffic.
- Do not interpret a cache hit as a routing signal yet. Cache-aware settlement
  is recorded now; using expected cache reuse in model selection is a separate
  policy feature.
