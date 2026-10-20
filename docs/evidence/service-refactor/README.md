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

## Post-revert integration comparison (2026-10-20)

Final runtime source: `a51effdc114bee5d0500d2231dc6b105f0fcee36`, integrating
main `e972d5ebb8215a51dbe88d404616087b940250ca`. Main reverted the Goal
implementation in #987 before the earlier attempted integration was pushed.
Results for the superseded `4cfa4b5` / `c215cb7` pair are not final acceptance
evidence for this PR. The new comparison uses a rebuilt `e972d5e` baseline.

[Raw samples](gateway-postrevert-2026-10-20.json),
[artifact identities](artifacts-postrevert-2026-10-20.json), and
[validation evidence](validation-postrevert-2026-10-20.json) record the exact
source, bytes, feature sets, dependency counts and process checks. The gateway
archive is a separate explicit-target build with its own verified binary hash;
the native minimum build is the executable used for measurement.

Settings match the earlier comparison: Mac16,12 / Apple M4 / 24 GiB, Rust 1.99.0,
unoptimized development builds, incremental compilation and debug symbols off,
and two build jobs. All executables were built and copied before sampling. Three
rotating rounds each use 16 warmups, 4,096 ordinary and 4,096 SSE requests at
concurrency eight, fresh SQLite state, exact body/usage/framing checks and exactly
8,208 upstream calls. Only one benchmark gateway runs at a time; unrelated
existing processes remain untouched. No build, test or lint ran during samples.

| Metric (median unless stated) | Post-revert main full | P0 full | Minimum gateway |
| --- | ---: | ---: | ---: |
| Executable bytes | 317,131,456 | 316,986,944 | 116,019,360 |
| Startup ms | 65.406 | 62.018 | 55.275 |
| Warmed idle RSS MiB | 61.469 | 64.109 | 42.594 |
| After-load RSS MiB | 196.094 | 204.172 | 176.125 |
| Ordinary requests/s | 4,075.186 | 4,144.505 | 4,159.999 |
| Streaming requests/s | 5,447.858 | 5,311.691 | 5,449.623 |
| Ordinary p95 latency ms | 3.003 | 2.983 | 2.956 |
| Streaming p95 latency ms | 1.962 | 2.022 | 1.957 |
| Normal/build dependency name-version pairs | 435 | 431 | 304 |

Full idle RSS increases by **2.640625 MiB / 4.30%**, below the previously accepted
measured increase in both absolute and relative terms. The minimum uses
**30.71% less** idle memory than the matching baseline. Ordinary throughput
medians differ by **+1.70%**, while SSE differs by **-2.50%**. Retain the idle-memory
optimization follow-up and record the SSE difference for follow-up measurement;
these samples do not establish a production-performance change.

No sample is discarded. One baseline startup took **2,037.536 ms**, versus
60.679 and 65.406 ms for the other two; this report does not diagnose its cause
or claim a startup improvement. Ordinary throughput spans 3,999–4,088 requests/s
for the baseline and 4,077–4,149 for P0. SSE spans 5,404–5,483 and 5,296–5,392,
respectively. After-load RSS medians differ by +4.12%, with ranges of
190.578–199.516 MiB and 192.219–212.703 MiB. The three-sample workload does not
establish long-running retained-memory behavior or cover remote provider latency.

The existing proc-macro-error2 future warning and macOS development-linker
compact-unwind warning remain visible in build logs. These builds and their
process acceptance pass; exception-unwinding performance is not measured.
Dependency counts retain the isolated normal/build name-version method above.

## Terminal continuation follow-up (2026-10-20)

The final candidate source is `6e8221738d10242fbc56e28486db84c5bb00c892`.
The preceding PR head exposed the shared terminal-continuation race again on
Ubuntu. Its failed job and log digest are retained in
[validation-terminal-fix-2026-10-20.json](validation-terminal-fix-2026-10-20.json).
The shared helper now subscribes to SIGCONT before SIGSTOP and awaits explicit
continuation before any of the three existing terminal callers reacquires input.
This restores the generic fix removed with the Goals revert, without restoring
Goals. The fixed-key CodeQL finding was inside a settlement test; that fixture
now generates random key bytes without changing its assertions.

Local terminal acceptance passed **50/50**, with six existing explicit skips and
zero retries. The two affected PTY cases now perform three suspension cycles and
compare the stopped terminal's complete termios against its original state.
All six settlement tests and strict all-feature library/test Clippy passed.
Independent review approved the implementation. Fresh hosted checks on the
follow-up PR head remain required; the earlier head's macOS/Windows and packaging
results do not replace that gate.

[gateway-terminal-fix-2026-10-20.json](gateway-terminal-fix-2026-10-20.json) retains
all nine new samples and artifact hashes. The full and minimum executables were
rebuilt from the candidate above; the baseline executable remains post-revert
main `e972d5e`. The same Rust 1.99 development profile, isolated local provider,
fresh SQLite, 4,096 ordinary plus 4,096 SSE requests, concurrency eight and three
rotating sample orders apply. No build, test or lint ran during measurement.

| Median | Baseline full | Candidate full | Minimum gateway |
| --- | ---: | ---: | ---: |
| Startup ms | 65.966 | 66.460 | 62.061 |
| Idle RSS MiB | 61.531 | 63.688 | 42.000 |
| After-load RSS MiB | 201.922 | 197.375 | 176.391 |
| Ordinary requests/s | 4,147.676 | 4,180.606 | 4,174.293 |
| SSE requests/s | 5,306.651 | 5,353.048 | 5,146.833 |
| Ordinary p95 ms | 2.981 | 2.878 | 2.922 |
| SSE p95 ms | 1.993 | 2.001 | 2.102 |

Full idle overhead is **2.15625 MiB / 3.50%**, below the accepted earlier small
increase; the memory optimization follow-up remains open. Full ordinary/SSE
throughput medians differ by **+0.79% / +0.87%**. Minimum SSE samples span
4,844–5,457 requests/s (median -3.01% from baseline); all samples are retained.
These small local samples do not prove either an improvement or a regression in
production throughput. Earlier performance records and their limitations remain
available above rather than being overwritten.

## Terminal input custody follow-up (2026-10-20)

Runtime source `6218709fdb24f8c4a1e1136f6651c9f00fa3e6a7` supersedes the preceding candidate.
The next Ubuntu run exposed an external-editor return/input race. The application
now owns its terminal reader and confirms its termination before transferring
custody. Stop is asynchronous and bounded; a stuck reader rejects the handoff
and never blocks a destructor. Editors retain custody across SIGCONT, and each
interactive client stops input before restoring shell modes.

The final selected run passed **53 tests, zero retries**: 51 terminal tests and
two reader lifecycle tests. Six terminal cases remain explicitly skipped; the
other 3,528 library tests were filtered out of this scoped command. A later
test-only assertion correction passed both reader unit tests and strict
all-feature library/test Clippy. Independent review approved the lifecycle fix.
The initial local candidate’s synchronization and shutdown defects, their fixes,
and command/log identities are retained in
[validation-reader-fix-2026-10-20.json](validation-reader-fix-2026-10-20.json).

The prior hosted macOS job compiled for 23m52s and continued passing tests until
the 45-minute job limit canceled it near test 3869/4693. Its total budget is now
75 minutes; individual test deadlines and retry settings remain unchanged.
Fresh hosted checks on the follow-up head remain required.

Both full/minimum binaries were rebuilt from the source above. The baseline
remains immutable post-revert main `e972d5e`. Two campaigns use the same
Rust 1.99 development profile, counted provider, fresh SQLite, 4,096 ordinary plus
4,096 SSE requests at concurrency eight and three rotating orders. No build,
test or lint ran during either campaign. All **18 samples** are retained in
[initial measurements](gateway-reader-fix-2026-10-20.json) and
[repeat measurements](gateway-reader-repeat-2026-10-20.json).

| Median | Initial baseline | Initial full | Initial minimum | Repeat baseline | Repeat full | Repeat minimum |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Startup ms | 87.141 | 87.215 | 77.712 | 89.644 | 133.078 | 80.608 |
| Idle RSS MiB | 61.391 | 63.656 | 42.172 | 61.563 | 63.641 | 41.922 |
| After-load RSS MiB | 209.969 | 208.094 | 183.953 | 223.313 | 211.219 | 185.500 |
| Ordinary requests/s | 2980.969 | 2969.349 | 2932.924 | 2974.869 | 2891.691 | 2939.453 |
| SSE requests/s | 3195.728 | 3030.966 | 3235.623 | 3052.352 | 3026.638 | 3123.444 |

Full idle overhead is **3.69% / 3.38%** across the two campaigns, within the
accepted small increase; the memory optimization follow-up remains open.
Ordinary/SSE differences are **-0.39% / -5.16%** initially and **-2.80% / -0.84%**
in the repeat. The repeat was prompted by the initial SSE result and the common
absolute-throughput change; neither campaign is excluded. Startup also varied,
including a repeat full median of 133.078ms versus 89.644ms baseline. Post-campaign
host load observations are recorded, but do not establish the cause of variation.
These samples do not support a production throughput or startup improvement claim.

## Cryptographic source follow-up (2026-10-20)

Runtime source `bbee2e7c955444bc26a76cf9f9fdf9195d767818` removes fixed test keys,
preserves shared-key equivalence across compared/restarted fixtures, and gives
each immutable control-auth snapshot a private random HMAC comparison key.
Production correlation/continuation key creation uses the same random source and
persisted 32-byte format directly. A tuple test fixture now has named independent
fields; health streak logic is unchanged. No CodeQL finding was dismissed and
no check was suppressed.

The preceding main CI passed all 57 jobs at `b25909a`, but a separate CodeQL check
reported 45 hard-coded-cryptographic-value alerts. Fresh hosted CI and CodeQL on
the new head remain required. The selected local all-feature suite passed
**385 tests with zero retries**, with 3,146 other tests filtered out. Independent
review caught the initial metadata-equivalence fixture regression; the fix
retains its original equality assertions. Strict all-feature library/test Clippy
on Rust 1.97 and formatting passed. Exact commands, initial failure, source/log
hashes and compiled identities are in
[validation-codeql-fix-2026-10-20.json](validation-codeql-fix-2026-10-20.json).

Both full/minimum executables were rebuilt from the runtime source above. The
same immutable post-revert main baseline, Rust 1.99 development profile, counted
local provider, fresh SQLite, 4,096 ordinary plus 4,096 SSE requests, concurrency
eight and three rotating orders apply. No build, test or lint ran during either
measurement campaign. All **18 samples** are retained in the
[initial](gateway-codeql-fix-2026-10-20.json) and
[repeat](gateway-codeql-repeat-2026-10-20.json) records.

| Median | Initial baseline | Initial full | Initial minimum | Repeat baseline | Repeat full | Repeat minimum |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Startup ms | 80.287 | 76.722 | 62.444 | 59.582 | 61.289 | 57.252 |
| Idle RSS MiB | 61.516 | 64.359 | 42.766 | 61.406 | 63.875 | 42.219 |
| After-load RSS MiB | 208.984 | 212.078 | 182.484 | 202.141 | 197.281 | 178.500 |
| Ordinary requests/s | 2713.926 | 2794.113 | 2683.015 | 3329.571 | 3359.511 | 3520.883 |
| SSE requests/s | 2912.013 | 2643.073 | 2874.047 | 3930.794 | 4144.536 | 4189.513 |

Full idle RSS overhead is **4.62% / 4.02%**, within the accepted small increase;
the memory optimization follow-up remains open. Ordinary/SSE median differences
are **+2.95% / -9.24%** initially and **+0.90% / +5.44%** in the repeat. The first
SSE result prompted the repeat; neither campaign is excluded. The changing sign
and absolute throughput do not establish a stable regression or improvement.
Host-load observations are retained without assigning a cause. These local
development samples do not constitute a production-performance claim.
