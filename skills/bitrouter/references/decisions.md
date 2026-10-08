# Native Decisions API

Decisions and System One now share the Classifier representation. See
[Classifier APIs](classifier.md) for both protocols and their conversion gates.

`POST http://127.0.0.1:4356/v1/decisions` accepts native evidence and typed
predicate, choice and score questions. It returns native answers, including
refusals, with provider-reported usage. Choose a model whose provider metadata
explicitly lists `decisions`; discovery or generic OpenAI compatibility alone
does not establish support.

Use the API-key `openai` provider with `OPENAI_API_KEY` for the upstream hop.
ChatGPT subscription authentication through `openai-codex` does not advertise
Decisions. With `server.skip_auth: false`, authenticate to the local gateway
using the usual BitRouter virtual key; upstream OpenAI credentials and downstream
BitRouter credentials serve different hops.

```bash
curl http://127.0.0.1:4356/v1/decisions \
  -H "Authorization: Bearer $BITROUTER_VIRTUAL_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "<configured-decisions-model>",
    "input": "The submitted change includes a failing regression test.",
    "questions": [{
      "type": "predicate", "name": "has_regression_test",
      "instructions": "Does the evidence describe a regression test?"
    }]
  }'
```

The placeholder selects an explicitly configured native target. There is no new
CLI command, listen port or default model. The `bro route` preview continues to
describe generation routing; confirm the actual native hop in settled requests.

Native input supports text and inline image parts. Image URLs must be inline
base64 data URLs. Questions retain their order and typed values, including
boolean choices. `safety_identifier` is forwarded on the native wire and excluded
from request-check text and telemetry content. Streaming, conversation
continuation and generation tool options are unsupported.

The router filters by operation before selecting a wire. Generation uses the
existing generation protocols; Decisions requires an explicitly advertised
Decisions target. A provider pin cannot bypass this check. Generation's outbound
preference remains the inbound protocol when supported, otherwise the first
compatible configured protocol.

## Independent tariffs and cost evidence

Runtime models accept `pricing_by_protocol`; each entry is a complete independent
tariff, expressed in micro-USD per token like ordinary `pricing`. For example,
this synthetic global tariff sets an input rate of $0.10 per million tokens:

```yaml
models:
  - id: native-test
    api_protocol: [chat_completions, responses, decisions]
    pricing_by_protocol:
      decisions:
        endpoint_profile: openai_global
        input_micro_usd_per_token: 0.10
        cache_read_micro_usd_per_token: 0
        cache_write_micro_usd_per_token: 0
        output_micro_usd_per_token: 0
```

Exact protocol tariffs win. Generation can fall back to ordinary `pricing`;
Decisions requires its own entry. Missing override buckets stay missing. Rates
and context tiers are captured after all route mutations and shared by metering,
evaluation and stream usage selection. Changing tariffs, configured endpoint
profiles or `server.require_known_pricing` requires a daemon restart.

`server.require_known_pricing` defaults to `false`. Setting it to `true` rejects
routes without guaranteed price coverage before dispatch. Decisions is currently
rejected under that requirement because upstream cache billing is unverified.
Ordinary spend/rate/auth policy checks still apply when this option is false.

For Decisions, nonzero cache-read or cache-write counters retain their raw and
canonical usage but make the estimated cost unavailable. A zero-cache response
may be priced from its independent tariff. Regional processing requires a
matching configured endpoint and tariff; a cross-profile endpoint override makes
cost unavailable. No premium is inferred for custom endpoints.

`bro requests` and usage exports retain frozen protocol/profile/tariff evidence.
Unavailable costs remain unknown. A completed malformed native response fails
delivery, retains usable usage, settles once and cannot retry. Pre-completion
transport/status fallback follows the existing policy. Old rows without frozen
protocol evidence retain their legacy status.

Published tariffs may declare `endpoint_profile: openai_global`, `openai_us`, or
`openai_europe`. Such prices keep their declared processing scope when you change
`api_base` or `protocol_endpoints`; inherited global catalog prices become
unavailable on a regional host. Provide that region's complete native tariff and
matching profile explicitly. An omitted profile binds custom/legacy tariffs to
the configured endpoint. This metadata change also requires restart.
