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

## Final candidate comparison (2026-10-17)

Candidate source: `d233944b0bb34f46ee3880dfed1af6939e8c293e`.
[Raw final samples](gateway-final-2026-10-17.json) and
[immutable artifact identities](artifacts-final-2026-10-17.json) retain the
full/minimum/harness checksums, sizes and compiled capabilities. The baseline
executable is unchanged from the first comparison. The same hardware, compiler,
development profile, configuration, warmup, three rotating orders and bounded
workload were used. All three candidate executables were built and copied
before measurement; no Cargo build, test or lint ran during the samples.

| Metric | Baseline full | Candidate full | Minimum gateway |
| --- | ---: | ---: | ---: |
| Executable bytes | 322,416,544 | 317,003,456 | 116,035,872 |
| Startup ms | 63.224 | 65.380 | 61.313 |
| Warmed idle RSS MiB | 61.406 | 64.219 | 42.469 |
| After-load RSS MiB | 211.047 | 208.922 | 182.125 |
| Ordinary requests/s | 4,046.907 | 4,016.302 | 4,072.040 |
| Streaming requests/s | 5,267.430 | 5,170.963 | 5,249.790 |
| Ordinary p95 latency ms | 3.053 | 3.054 | 3.028 |
| Streaming p95 latency ms | 2.046 | 2.089 | 2.059 |
| Normal/build dependency name-version pairs | 435 | 431 | 304 |

The final full-product idle increase is **2.8125 MiB / 4.58%**, below the
previously accepted measured increase. The minimum profile uses **30.84% less**
idle memory than the contemporaneous baseline. Full ordinary and streaming
throughput medians differ by **-0.76%** and **-1.83%**. Baseline ordinary samples
span 3,840–4,085 requests/s and streaming spans 4,902–5,334; candidate full spans
4,002–4,034 and 5,157–5,197 respectively. These small local samples do not
establish a sustained throughput change. Preserve the idle-memory optimization
follow-up and the development-build interpretation limits above.

Dependency counts use separate invocations of
`cargo +1.99.0 tree --locked --offline -p bitrouter -e normal,build --prefix none`,
adding `--no-default-features` only for the minimum candidate. Count distinct
package name/version pairs, including the root package; repeated rendered edges
are excluded. The corresponding rendered entry counts are 1,353 / 1,286 / 799.
These isolated product closures are distinct from the earlier workspace-wide,
all-features baseline in the progress ledger. No dependency reduction is
inferred from the reduced number of workspace packages alone.
