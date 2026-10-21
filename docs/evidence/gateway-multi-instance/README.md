# P1 candidate evidence — 2026-10-21

This evidence accompanies the [acceptance record](../../GATEWAY_MULTI_INSTANCE_ACCEPTANCE.md).
P0 source is `9cbe512d101e777470d113b0086ab9e331b3a638`; measured P1 runtime source is
`2495bf8db118de399a11fc11f82ba18bd6a6455d`. Later documentation-only commits do
not change the measured runtime. The later
[pooled-read follow-up](read-retry-2026-10-21.md) records current shared validation
and why the original local measurements retain their original provenance. [Artifact identities](artifacts-2026-10-21.json)
record source, hashes, features, compiler, build settings and dependency counts.

## Local comparison method

Both sources use Rust 1.97.0 on macOS arm64, development optimization, no debug
information or incremental compilation, and two build jobs. Each executable was
copied outside Cargo outputs before another build. The owned PostgreSQL fixture
was stopped and absent during measurement; no build, test or lint ran alongside
the samples. The default and minimum builds start using fresh local SQLite.

The unchanged [Rust measurement fixture](../../../apps/bitrouter/tests/fixtures/gateway_measurements/README.md)
uses three rotating profile orders, one gateway child at a time, a counted
loopback provider, 16 warmup requests and 4,096 ordinary plus 4,096 SSE requests
per sample at concurrency eight. It validates complete bodies, SSE framing,
content, usage, exact provider counts and owned child cleanup. Each report has
nine samples. The default report compares P0 default with P1 default and keeps
P1 minimum as a supplemental control. The minimum report compares P0 minimum
with P1 minimum; its P1 default samples are supplemental. Cross-profile samples
are not interchangeable baselines.

The tables show the median of three samples and their complete range. Idle RSS
first takes the median of three readings within each sample. Latency values are
medians of per-sample percentiles, not pooled request percentiles. After-load RSS
is one observation per process, not peak memory or a leak/growth measurement.
These development-build results do not establish release performance, provider
speed, time to first token, or multi-replica scaling.

## Initial matched results

Raw [default](local-default-2026-10-21.json) and
[minimum](local-minimum-2026-10-21.json) reports retain all 18 samples and 147,744
provider calls. The [summary](local-summary-2026-10-21.json) includes all metrics,
individual values and supplemental controls.

| Default metric | P0 median [range] | P1 median [range] | Median change |
| --- | ---: | ---: | ---: |
| Startup, ms | 64.533 [59.449–77.384] | 60.635 [60.255–79.197] | -6.04% |
| Idle RSS, MiB | 65.453 [65.266–65.766] | 65.859 [65.750–65.938] | +0.406 MiB / +0.62% |
| After-load RSS, MiB | 192.719 [192.563–213.766] | 203.453 [180.328–214.453] | +10.734 MiB / +5.57% |
| Ordinary requests/s | 4,060.591 [3,997.392–4,105.808] | 4,110.721 [4,098.597–4,143.936] | +1.23% |
| SSE requests/s | 5,252.488 [4,919.226–5,281.119] | 5,193.389 [3,401.550–5,214.658] | -1.13% |
| Ordinary p95, ms | 3.015 [2.955–3.035] | 2.923 [2.855–2.974] | -3.06% |
| SSE p95, ms | 2.055 [2.048–2.299] | 2.088 [2.066–5.995] | +1.59% |

| Minimum metric | P0 median [range] | P1 median [range] | Median change |
| --- | ---: | ---: | ---: |
| Startup, ms | 58.253 [57.274–67.297] | 58.858 [57.058–60.865] | +1.04% |
| Idle RSS, MiB | 42.203 [42.016–42.484] | 43.016 [42.859–43.141] | +0.813 MiB / +1.93% |
| After-load RSS, MiB | 178.438 [174.297–183.219] | 178.656 [173.172–180.000] | +0.219 MiB / +0.12% |
| Ordinary requests/s | 4,084.808 [4,080.992–4,119.159] | 4,096.039 [4,067.001–4,138.508] | +0.27% |
| SSE requests/s | 5,216.725 [5,038.318–5,324.725] | 5,272.085 [5,144.261–5,290.350] | +1.06% |
| Ordinary p95, ms | 3.004 [2.965–3.014] | 2.998 [2.907–3.014] | -0.20% |
| SSE p95, ms | 2.041 [2.006–2.087] | 2.024 [2.021–2.035] | -0.81% |

## Fixed default confirmation

One additional default triplet was planned after independent review of the
initial SSE slowdown and after-load RSS variation. It uses the same immutable
executables, runner and workload after all Cargo validation finishes. The
original results remain primary evidence and are not replaced or filtered.
The [repeat report](local-default-repeat-2026-10-21.json) and
[repeat summary](local-default-repeat-summary-2026-10-21.json) retain all nine
additional samples. Together the campaigns cover 27 samples and 221,616 provider
calls. Binary and runner hashes were checked again after the repeat.

| Default repeat metric | P0 median [range] | P1 median [range] | Median change |
| --- | ---: | ---: | ---: |
| Startup, ms | 59.131 [57.544–78.522] | 61.190 [58.292–81.860] | +3.48% |
| Idle RSS, MiB | 65.156 [65.125–65.922] | 65.953 [65.953–66.109] | +0.797 MiB / +1.22% |
| After-load RSS, MiB | 194.297 [190.313–204.109] | 190.656 [187.078–196.281] | -3.641 MiB / -1.87% |
| Ordinary requests/s | 4,043.375 [3,992.326–4,118.715] | 4,100.243 [4,024.980–4,132.547] | +1.41% |
| SSE requests/s | 5,151.228 [5,015.942–5,265.608] | 5,030.987 [4,924.046–5,202.216] | -2.33% |
| Ordinary p95, ms | 2.941 [2.872–3.061] | 2.949 [2.909–2.974] | +0.29% |
| SSE p95, ms | 2.056 [2.041–2.111] | 2.126 [2.093–2.261] | +3.41% |

The initial 3,401.550 SSE requests/s sample with 5.995 ms p95 did not recur.
Its cause is not established, and it remains in the initial report. Default
after-load RSS did not remain elevated in the repeat; the two blocks have
opposite median changes and overlapping ranges. Neither result proves absence
of a leak or unchanged production tails. The smaller default SSE median
decreases (-1.13%, then -2.33%) and p95 increases remain measured differences.

A09 is accepted for this candidate with these measured costs recorded:
default idle increases of 0.406/0.797 MiB, minimum idle increase of 0.813 MiB,
the executable increases below, and the bounded default SSE differences. This
is an engineering disposition for the recorded workload, not a new general
numeric budget or an assertion of no regression. The local profiles still start
without shared infrastructure and keep their dependency/feature closure. Small
footprint and SSE differences remain follow-up optimization items; the single
large SSE slowdown and runner notice below remain diagnostic follow-ups if
they recur. No additional repeat was used to select a preferred result.

## Dependency and executable footprint

The normal/build dependency trees, including features, are identical after
normalizing only checkout paths: 431 distinct name/version pairs for default and
304 for minimum, including the root package. The feature lists are unchanged;
neither local profile enables `multi-instance`.

| Executable | P0 bytes | P1 bytes | Change |
| --- | ---: | ---: | ---: |
| Default/full | 322,162,448 | 323,180,928 | +1,018,480 / +0.316% |
| Minimum gateway | 117,910,960 | 119,061,584 | +1,150,624 / +0.976% |

The recorded idle and executable increases must not be described as zero
overhead. Historical acceptance of a P0 memory increase is not an automatic
allowance for P1 or for after-load RSS. A later performance investigation should
use optimized builds and controlled memory profiles before attributing these
small footprint changes to particular source edits.

## Validation and retained failures

[Initial-candidate validation](validation-2026-10-21.json) records commands, source revisions,
counts and log hashes: that workspace passed 4,939/4,939 in 677.839s, with
147 configured skips; doctests passed 11 with one ignored. Strict workspace
Clippy, warning-denied rustdoc, registry validation, distribution consistency and
format checks passed. The separate shared selection passed 203/203, the fixed
v8 stability campaign 40/40, and v9 maintenance repetitions 3/3.

The workspace retained one nextest `LEAK` notice for the existing
`graceful_shutdown_waits_for_required_drain_recovery_after_server_stops` test.
[Nextest defines this as captured output remaining open after test exit](https://www.nexte.st/docs/features/leaky-tests/),
not a heap-leak measurement. Bounded review found that the test and helper are
unchanged from P0, create no child process and join their successful-path task.
One exact-case check passed in 0.033s without the notice, with zero retries and
unchanged leak settings. Its cause remains unexplained; no source fix or timeout
relaxation is justified by that evidence. The initial notice is not erased.

The [progress record](../../GATEWAY_MULTI_INSTANCE_PROGRESS.md)
retains prior failed candidates and scoped repairs. Failed runs are not counted
as successful final validation. The artifact record also preserves the failed
build after switching a Cargo target directory between P0 and P1; no output
from that build was used. The three affected local package outputs were cleaned,
the build succeeded, and subsequent builds used only the P1 checkout.
