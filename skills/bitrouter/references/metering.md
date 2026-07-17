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
bitrouter workflow-state reliability-report \
  --database-url sqlite://$HOME/.bitrouter/bitrouter.db \
  --config $HOME/.bitrouter/bitrouter.yaml \
  --output artifacts/reliability-report.json
```

This command is read-only with respect to the database. Its JSON output is
stable for the same ordered event log and config, includes route and endpoint
classifications plus an event-log SHA-256, and contains no credential material,
prompt/response content, or tool commands.

## Export an auditable usage snapshot

```bash
bitrouter workflow-state metering-usage \
  --database-url sqlite://$HOME/.bitrouter/bitrouter.db \
  --since 2026-07-14T00:00:00Z \
  --until 2026-07-15T00:00:00Z \
  --output artifacts/cloud-usage.jsonl
```

If a provider price was unavailable during the run, impute all four rates
explicitly:

```bash
bitrouter workflow-state metering-usage \
  --database-url sqlite://$HOME/.bitrouter/bitrouter.db \
  --output artifacts/cloud-usage.jsonl \
  --impute-price 'provider:model=1.0,0.1,1.25,6.0'
```

The legacy `input,output` form is accepted only when the matching records have
no cache usage. Overrides are preserved as `pricing_source: override` evidence.

## Build the strict run bundle

```bash
bitrouter workflow-state bundle \
  --run-label short13-fixed-strong \
  --traces artifacts/traces.jsonl \
  --cloud-usage artifacts/cloud-usage.jsonl \
  --outcomes artifacts/benchmark-outcomes.jsonl \
  --policy-decisions artifacts/policy-decisions.jsonl \
  --output-dir artifacts/bundle
```

Once usage is supplied, bundle creation requires an exact one-to-one
trace/usage request-id join, provider-reported raw usage, consistent normalized
buckets, a computed charge, complete effective rates, and a full pricing hash.
When policy decisions are supplied, trace/decision request ids must also match
exactly. Terminus-2 bundles additionally require complete structured workflow
identity; see `references/harness-terminus-2.md`.

An analytical replay may omit usage and decisions. That mode is useful for
extractor development, but it is not benchmark-grade cost evidence.
