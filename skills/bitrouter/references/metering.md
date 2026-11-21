# Cache-aware metering

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
`cache_write`, and `output_tokens.text` fields. Registry v1 amounts stay exact
decimal rationals through calculation; evidence retains numerator/denominator
and scope, while integer micro-USD is a rounded operational estimate. Rates
are frozen with the selected protocol, operation and endpoint. Unknown tariff
kinds, meters, eligibility, usage origins or unproven retry totals produce
unavailable pricing rather than a zero or a price borrowed from another wire.

## What is persisted

Each new metering row retains:

- the provider's raw usage object
- usage origin (`provider_reported`, `estimated`, or `unknown`)
- normalized uncached/cache-read/cache-write/output/reasoning buckets
- effective rates and whether they came from config or an explicit override
- a deterministic SHA-256 pricing version
- charge status, final charge, and any unknown reason


## Inspect request usage

Use `bitrouter requests` to inspect settled gateway rows and available
charge evidence. Filter by time, model or provider. On an authorized
tenant service, retrieve execution receipts with the SDK. Keep missing usage
and uncertain charges explicit when joining results. See [Eval workflow](workflow-optimization.md)
for submitting evidence against a pinned configuration.
