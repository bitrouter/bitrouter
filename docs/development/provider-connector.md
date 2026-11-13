# Provider connector

`apps/bitrouter/src/connector/` owns observation and explicitly enabled bounded
local execution for one fixed-model pool. Static direct members and vLLM engine
labels are operator declarations. This path works without Cloud, gateway
assembly, login, named remote contexts or optional application features. It
implements neither pairing nor any sales or commercial fulfillment behavior.

```sh
bro connector check --config connector.yaml
bro connector observe --config connector.yaml --human
bro connector serve --config connector.yaml --initialize-state
bro connector status --config connector.yaml
bro connector pause --config connector.yaml
bro connector resume --config connector.yaml
bro connector request --config connector.yaml --file request.json
bro connector request-status --config connector.yaml --request-id pilot-001
bro connector cancel --config connector.yaml --request-id pilot-001
bro connector reconcile --config connector.yaml --request-id pilot-001 --file attestation.json
```

`check` validates the strict standalone file and reports the observation plan
without network I/O. `observe` takes one bounded snapshot. Neither dispatches
inference, even if execution is configured. JSON is the default; `--human`
selects readable reports. The [configuration contract](../specs/configuration/README.md#standalone-provider-connector)
links the schema and example. `main/connector.rs` selects these commands before
gateway configuration, context resolution, provider credentials and daemon
state. `--context local` is accepted; other named contexts are refused.

## Operator startup and bounds

Start in observation mode. Enabling the optional `execution` block is an
operator opt-in to a small bounded pilot, not a capacity estimate or assurance
about primary workload impact. Explicitly set rate/burst, maximum extra
concurrency, input/output token bounds, duration/body limits, pressure ceilings,
lease/update timing and a healthy recovery window using representative workload
measurements. The example is deliberately disabled and its values are
illustrative. The policy closes on pressure, stale/missing metrics, observed
restart evidence, clock regression, manual pause, unknown execution or lost
state authority. Budget refresh cannot erase already sent work.

The local service defaults to `127.0.0.1:4367`; `--listen` may select another
loopback IP/port. Non-loopback listen addresses are rejected in this pilot.
Every HTTP route requires `Authorization: Bearer ...`, resolved from
`execution.auth_bearer_env`. Supply a locally generated secret containing 32–4096
printable ASCII bytes. Control commands use that same environment reference and
accept `--endpoint http://127.0.0.1:4367` to select another loopback origin.
Upstream metrics and inference bearer variables are independent and stay local.
No credential, prompt or response body enters diagnostics or recovery records.
The explicit `request` command writes the requested inference response itself.

Use one persistent local filesystem directory for this pool's authority. Every
process authorized to admit this pool must share it. `exclusive_pool_owner`
records this operator requirement; separate volumes do not coordinate owners.
The initial start requires `--initialize-state`. Subsequent starts omit it;
existing state is never reset by that flag. The SQLite journal and initialized
marker must survive restarts together. Missing, corrupt or incompatible state
blocks startup; do not delete or replace it to clear unknown work. The supported
pilot ownership lock requires a Unix host and a filesystem with reliable local
file locking. Transparent failover and distributed ownership are not provided.

## Local request protocol

The [service API contract](../specs/service-api/README.md#standalone-local-connector)
defines compatibility, identity and terminal semantics for this local profile.

All paths are below `/v1/connector`: `GET /status`, `POST /pause`, `POST /resume`,
`POST /requests`, `GET /requests/{request_id}` and
`POST /requests/{request_id}/cancel`, plus the trusted operator-only
`POST /requests/{request_id}/reconcile`. Status includes `pool_generation`, the
current budget and its reason, retained identity count, observation and separate
reserved/sending/running/unknown occupancy. Pause persists across restarts;
resume removes only manual pause, leaving all health and budget checks active.

A request file has this shape; obtain the actual generation from status:

```json
{
  "request_id": "pilot-001",
  "pool_id": "local-pilot",
  "pool_generation": "COPY_FROM_STATUS",
  "deadline_ms": 10000,
  "request": {
    "model": "fixture-model",
    "messages": [{"role": "user", "content": "Hello"}],
    "max_tokens": 32,
    "stream": false
  }
}
```

This is a narrow local pilot profile, not a general OpenAI API. Unknown fields
at every level are rejected. Messages contain only nonempty text and the roles
system/user/assistant; there are 1–128 messages and the final message is user.
No tools, media, caller templates, URL overrides, multiple choices or alternate
output-limit spellings are accepted. `deadline_ms` is a relative whole-call
limit starting at HTTP ingress, including body reading and tokenization. The
execution record stores the resulting absolute Unix deadline in milliseconds.
Only the first declared member executes this pilot's requests; other members
remain in complete-pool observation. There is no load balancing or fallback.
An aggregate hard running ceiling also bounds the chosen member, but is not a
per-replica throughput model or proof that primary latency is protected.

Before generation, the exact selected member's `/tokenize` endpoint renders and
counts the chat. Its count must equal a nonnegative integer token array's
length; count plus the forced output bound must fit `max_model_len`.
Input counts are never supplied by callers or estimated from characters.
Both tokenizer and generation force `add_generation_prompt: true`,
`continue_final_message: false`, `add_special_tokens: false`, and the same exact
model/messages. These fields follow the
[vLLM TokenizeChatRequest contract](https://docs.vllm.ai/en/v0.15.1/api/vllm/entrypoints/serve/tokenize/protocol/).
The pilot requires an operator-controlled vLLM endpoint with consistent chat
templating and enforcement of `max_tokens`; a server that ignores those controls
is outside this adapter's guarantee. Final usage must match the tokenizer's
input count and stay within the output bound; violations become unknown.

Tokenization has bounded time/body size and sends the selected member's local
inference bearer only to that same origin. Admission is rechecked after this
control request, before durable generation send intent, and after that intent
commits before transport permission. A slow journal write cannot extend an
expired budget lease; a refusal at that boundary records proven-unsent work.
No proxy discovery, redirects, transport retries or target fallback occur.

## Execution evidence and recovery

`controller` atomically combines one token bucket, local reservations, pressure
policy and durable execution transitions. `executor` reuses the selected-model
HTTP send primitive and Chat Completions codec, adding this profile's stricter
bounds and terminal validation. The gateway scheduler lives in
`gateway/scheduling/`; this standalone path retains its own pool-scoped
admission and durable ownership instead of joining the gateway queue.
`journal` retains only request identity/digest, original pool/member binding,
attempt/owner identity, limits, state and bounded evidence.

An equal ID and digest returns the retained record and never replays inference
or stored text. A different digest for an existing ID is refused. IDs remain
retained indefinitely up to `max_records`; reaching that limit stops new IDs.
This pilot deliberately offers no record-pruning/reset operation. Unknown
execution is not released by time or restart. After external engine diagnosis,
the trusted local operator may submit an attestation containing the exact
`attempt_id`, original `pool_generation`, `expected_revision` from the current
record, `expected_state: "unknown"`, `confirmed_not_running: true` and a
1–512 byte printable ASCII `evidence_reference`. Use a diagnostic/ticket
reference, never a prompt, credential or response body. Reconciliation requires
all expected values to match and records the distinct `reconciled` state and
`operator_confirmed_stopped` provenance; it does not claim provider success or
independent proof of termination and never makes the ID executable again.
The loopback owner bearer is trusted management authority; future Cloud request
principals must not inherit this authority.

A non-streaming completion requires one bound choice, a known finish reason,
matching response identity/model, bounded usage and clean body EOF. HTTP status
or headers alone never release occupancy. Streaming forwards bounded valid
OpenAI chat SSE data, requires finish plus usage plus `[DONE]` and clean framed
EOF, and persists completion before forwarding the final `[DONE]`. Truncated or
malformed frames, later data/errors, excess output, failed transports and
non-success HTTP responses remain unknown. Full stream text is not retained or
recovered after disconnect.

Cancellation releases only a reservation with no durable send intent. Once
sending starts, cancel returns a conflict; this pilot has no trustworthy engine
abort operation. Client disconnect or deadline drops upstream I/O and records
unknown while retaining capacity; neither proves GPU execution stopped. Pause
stops new admissions and dispatch but lets sent requests finish. Graceful
shutdown pauses then drains bounded active requests. A crash restores unsent
reservations as cancelled and possible sends as unknown before any fresh
observation can enable a new budget.

## Observation and evidence limits

The observer requires the exact declared model and complete engine-label set for
`vllm:num_requests_running`, `vllm:num_requests_waiting` and
`vllm:kv_cache_usage_perc`. Missing, invalid, stale or incomplete data is unknown,
never zero load. Metrics include primary and extra traffic; extra work is never
subtracted to invent headroom. Because these gauges cannot identify which local
attempts are already reflected, the pilot deliberately adds all local unfinished
work when checking pressure. Reflected work can therefore be counted twice,
tightening admission; this is a conservative upper bound rather than an estimate
of unreflected work. No calibrated safe throughput is inferred from GPU or KV
percentages. By default execution requires recent exporter timestamps; the
explicit `allow_collection_only_metrics` acknowledgement permits only collection
freshness, which cannot establish exporter sample freshness.

The operator must ensure each direct inference/tokenizer endpoint and metrics
URL covers the declared engine set and that this set covers primary traffic.
Observation reports preserve unverified physical membership, production scope,
endpoint binding and model fidelity. Fixture and process tests establish local
protocol/recovery behavior; real GPU mixed-load acceptance must measure primary
TTFT, inter-token/tail latency, errors, extra completions/refusals and unknown
outcomes across bursts, telemetry delay, long requests and restarts. This code
alone is not production protection evidence or readiness to sell.
