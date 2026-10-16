# Service refactor measurement evidence

[Raw gateway samples](gateway-local-2026-10-16.json) were captured on
2026-10-16 using the standalone
[measurement fixture](../../../apps/bitrouter/tests/fixtures/gateway_measurements/README.md).
The host was macOS arm64, Apple M4 (Mac16,12), 10 logical CPUs and 24 GiB RAM.
All three gateways use Rust/Cargo 1.99.0 development builds with incremental
compilation and debug symbols disabled. They are unoptimized executables.

The baseline is source `9956a427bd5d4316b3bf2359f88e293b158781d5`.
Candidate executables are the immutable full/minimum artifacts described in the
[policy-profile evidence](../../BITROUTER_SERVICE_REFACTOR_PROGRESS.md#gateway-policy-and-judge-accounting-across-profiles-2026-10-16):
stage base `5f2e88e` plus the explicit serialization-feature repair committed in
`91e0c29`. Product sources match that repair; later `2bb583a` adds tests only.
These are stage artifacts, not the final committed release candidates.
Paths in the JSON identify local capture files; SHA-256 and byte sizes identify
their contents independently of those paths. Each hash matches its earlier
artifact record.

Each profile ran three times in rotating order. Every sample uses a fresh
private database, 16 warmup requests, 4,096 ordinary requests and 4,096 immediate
SSE responses at concurrency eight. Each host made exactly 8,208 mock-provider
calls. Requests validate exact text, completion and usage; streams also require
complete framing and terminal DONE. Only one gateway ran at a time. No gateway
build or lint ran concurrently with this measurement.

The following values are medians across the three samples. Idle RSS is the
median of each sample's three readings after warmup, before measured load.

| Metric | Baseline full | Candidate full | Minimum gateway |
| --- | ---: | ---: | ---: |
| Executable bytes | 322,416,544 | 316,998,896 | 116,029,456 |
| Startup ms | 63.969 | 63.888 | 60.142 |
| Warmed idle RSS MiB | 61.234 | 64.328 | 41.875 |
| After-load RSS MiB | 201.594 | 196.516 | 181.500 |
| Ordinary requests/s | 4,154.188 | 4,128.221 | 4,255.874 |
| Streaming requests/s | 5,325.867 | 5,403.538 | 5,487.168 |
| Ordinary p95 latency ms | 2.898 | 2.968 | 2.897 |
| Streaming p95 latency ms | 2.030 | 1.987 | 1.937 |

Full-product idle RSS increased by 3.094 MiB (5.05%); minimum idle RSS decreased
by 31.62%. Full ordinary throughput differed by -0.63%, and full streaming by
+1.46%. Keep the memory increase visible: this run does not establish its cause
or absence of a full-product memory regression. The acceptance decision below
records the subsequent approval of this measured overhead.

Ranges matter. Baseline streaming throughput ranged from 4,806.110 to
5,514.002 requests/s; candidate full ranged from 5,358.288 to 5,413.167.
Minimum after-load RSS ranged from 169.219 to 195.906 MiB. These local samples
do not justify a general throughput improvement claim, a long-running memory
claim or production latency predictions. They exclude remote network/provider
delay and do not activate every optional runtime or telemetry exporter.

The earlier 512-request samples were exploratory and are not the comparison
reported here. This report provides comparable A16 evidence; final candidate
validation and normal dependency counts remain separate gates.

## Accepted overhead and follow-up (2026-10-17)

The recorded full-product idle RSS increase from **61.234 to 64.328 MiB**
(**3.094 MiB / 5.05%**) is accepted for P0. Eliminating this measured increase is
not a prerequisite for the implementation PR. Keep it visible in the final
comparison and retain a follow-up to identify and reduce the additional idle
memory while preserving the supported full-product behavior.

This decision covers the development-build measurement and workload above; it
does not establish a general percentage allowance for other metrics, profiles
or later regressions, and it is not a production-performance result. Re-measure
the final candidate under comparable conditions and assess any additional
regression separately. A16 remains open until that comparison and the remaining
size, normal-dependency and request/stream evidence are reconciled.
