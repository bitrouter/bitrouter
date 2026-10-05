# Cache-aware metering and benchmark evidence

BitRouter records provider usage in four non-overlapping billing buckets:

- uncached input tokens
- cache-read input tokens
- cache-write input tokens
- output tokens, with reasoning tokens retained as a separate output subtype

The recorded charge is computed only when every non-zero bucket has a known
rate. A missing rate is different from an explicit zero price: missing data
produces `charge_status: unknown`, never a fabricated zero-dollar charge.

## Configure prices

Hand-written provider models use micro-USD per token:

```yaml
providers:
  custom:
    api_base: https://provider.example/v1
    api_key: ${CUSTOM_API_KEY}
    api_protocol: openai
    models:
      - id: example-model
        pricing:
          input_micro_usd_per_token: 1.0
          cache_read_micro_usd_per_token: 0.1
          cache_write_micro_usd_per_token: 1.25
          output_micro_usd_per_token: 6.0
          context_tiers:
            - above_input_tokens: 128000
              input_micro_usd_per_token: 2.0
              cache_read_micro_usd_per_token: 0.2
              cache_write_micro_usd_per_token: 2.5
              output_micro_usd_per_token: 9.0
```

An omitted context-tier rate inherits the base rate. Registry models use the
registry's per-million-token `input_tokens.no_cache`, `cache_read`,
`cache_write`, and `output_tokens.text` fields; BitRouter converts those into
the same internal rate representation.

## What is persisted

Each new metering row retains:

- the provider's raw usage object
- usage origin (`provider_reported`, `estimated`, or `unknown`)
- normalized uncached/cache-read/cache-write/output/reasoning buckets
- effective rates and whether they came from config or an explicit override
- a deterministic SHA-256 pricing version
- charge status, final charge, and any unknown reason

Historical rows created before this evidence schema are marked
`legacy_unknown`. They do not become zero-cost requests during export.

Provider transport reliability observations are stored separately from task
reward and metering rows. Export their deterministically replayed circuit state
with the same frozen config used by the daemon:

```bash
bro workflow-state reliability-report \
  --database-url sqlite://$HOME/.bitrouter/bitrouter.db \
  --config $HOME/.bitrouter/bitrouter.yaml \
  --output artifacts/reliability-report.json
```

This command is read-only with respect to the database. Its JSON output is
stable for the same ordered event log and config, includes route and endpoint
classifications plus an event-log SHA-256, and contains no credential material,
prompt/response content, or tool commands.

## Reconcile request-scoped receipts

Hosted BitRouter Cloud rows can be reconciled without exporting a static API
key into the environment. Point the command at the owner-only credential file
used by `bro cloud login`, provided that file contains a static API key:

```bash
bro workflow-state reconcile-metering \
  --database-url sqlite://$HOME/.bitrouter/bitrouter.db \
  --credentials-file "$XDG_DATA_HOME/bitrouter/account-credentials.json" \
  --request-id req-123 \
  --price 'bitrouter:model=1.0,0.1,1.25,6.0'
```

For API-key environments, omit `--credentials-file` and provide the variable
named by `--api-key-env` (default `BITROUTER_API_KEY`). A non-empty environment
value takes precedence over the file. OAuth credentials are rejected and never
refreshed for settlement. Never place either credential in command arguments,
logs, or benchmark artifacts.

## Export an auditable usage snapshot

```bash
bro workflow-state metering-usage \
  --database-url sqlite://$HOME/.bitrouter/bitrouter.db \
  --since 2026-07-14T00:00:00Z \
  --until 2026-07-15T00:00:00Z \
  --output artifacts/cloud-usage.jsonl
```

If a provider price was unavailable during the run, impute all four rates
explicitly:

```bash
bro workflow-state metering-usage \
  --database-url sqlite://$HOME/.bitrouter/bitrouter.db \
  --output artifacts/cloud-usage.jsonl \
  --impute-price 'provider:model=1.0,0.1,1.25,6.0'
```

The legacy `input,output` form is accepted only when the matching records have
no cache usage. Overrides are preserved as `pricing_source: override` evidence.

## Build the strict run bundle

```bash
bro workflow-state bundle \
  --run-label short13-fixed-strong \
  --traces artifacts/traces.jsonl \
  --cloud-usage artifacts/cloud-usage.jsonl \
  --policy-decisions artifacts/policy-decisions.jsonl \
  --output-dir artifacts/bundle
```

This example leaves terminal outcomes to the Eval Exchange. Add
`--outcomes artifacts/benchmark-outcomes.jsonl` only for genuinely
request-scoped outcomes whose request-ID set exactly matches the traces. Task-
or episode-scoped outcomes need a separate scope join and explicit evaluator
decision credit; do not broadcast them into request-scoped rows.

For any non-empty trace set, bundle creation requires an exact one-to-one
trace/usage request-id join, provider-reported raw usage, consistent normalized
buckets, a computed charge, complete effective rates, and a full pricing hash.
When policy decisions or outcomes are supplied, their persisted `request_id`
sets must each match the trace set exactly and one-to-one. Timestamp overlap,
session IDs, and trial IDs are useful benchmark diagnostics only; none can
replace the strict request-ID join. Terminus-2 identity is likewise diagnostic
adapter evidence, not a source-specific bundle requirement.

The benchmark bundle gate validates archival completeness. Reward-feedback
admission additionally requires the exact request-ID joins, a completed
request, and authoritative computed settlement; diagnostic identity fields do
not participate in learning.

The bundle writes `routing-baselines.json` in addition to the run artifact,
trace, usage, outcome, decision, and shadow-policy files. Eligible policy
decisions carry a versioned `route_measurement`: the complete semantic
tier/model/effort candidate set, pre-guard logging action, and integer-ppm
logging probabilities from one immutable policy snapshot. Effective
`selected_*` fields can differ after safety or progress guards.

Routing baselines are deterministic measurement controls. They are isolated by
candidate-set digest, expose no raw request IDs, and include both every
always-tier allocation and an exact share-matched content-blind allocation.
They make no counterfactual quality claim; decisions without measurement are
reported as excluded rather than silently synthesized.

The in-memory analytical `build*` APIs may omit usage and decisions for
extractor development. The `workflow-state bundle` command is benchmark-grade
and fails closed when traces exist without usage evidence.

## Rank a candidate policy by settled baseline cost

```bash
bro workflow-state policy-oracle \
  --traces artifacts/traces.jsonl \
  --cloud-usage artifacts/cloud-usage.jsonl \
  --policy-lock policy-lock.yaml \
  --policy auto \
  --effective-cost-factor 0.24 \
  --target-savings 0.30 \
  --target-savings 0.40 \
  --output artifacts/policy-oracle.json
```

The oracle requires an exact trace/usage request-ID set. Its effective cost
factor must include expected token, retry, and turn inflation. The JSON output
ranks eligible requests by baseline charge, identifies the highest-cost routes
still left on the default tier, and reports whether each savings target is
attainable under the candidate lock. Treat it as a cost-only upper bound;
quality and trajectory effects still require live eval evidence.
