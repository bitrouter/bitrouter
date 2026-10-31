# Continuous integration

The PR workflow checks feature configurations at compile time and runs ordinary
behavior tests from an all-feature Linux build. Tests should cover module
contracts, failure paths and boundaries with bounded local fixtures. Small
process and database integration tests remain in PR CI.

## Build once, run separately

`build-linux` compiles the workspace tests and examples with two build jobs and
without debug information. It publishes a nextest archive for the exact GitHub
revision. The module, integration, process-contract, bounded-capacity and database
jobs download that artifact and run its executables without invoking Cargo. The
archive includes process fixtures; a missing fixture is an error, not permission
to rebuild during a test deadline.

The coding-judge integration test intentionally compiles tiny temporary Rust
programs to verify executed pass/fail evidence. Rustc remains available for that
bounded fixture; workspace and example rebuilds are forbidden.

All artifact consumers use Ubuntu 24.04 and verify both the revision and the
original workspace path. Some tests embed Cargo paths, so arbitrary relocation
is not supported. Dependency caches speed later builds; the uploaded archive
shares the actual executables within one run. See [nextest archives](https://nexte.st/docs/ci-features/archiving/).

Consumers retain JUnit, elapsed time, maximum resident memory and disk usage in
their evidence artifacts, including retained gateway child logs on failure.
Evidence names include the run attempt so retries preserve earlier failures.
A rebuild replaces the revision's archive; retrying only a consumer reuses the
existing build. See [artifact replacement](https://github.com/actions/upload-artifact#overwriting-an-artifact).
`/usr/bin/time` reports process resource accounting,
not the peak memory of the entire VM or database service containers. Module
tests use four slots on the standard four-CPU Linux runner; integration tests use
two, and database, ignored process parents and each capacity lane run serially.
Builds stay at two jobs because their memory demand is much higher than test execution.
Only the listed ignored parents run: internal fault-injection children must not
be selected with a blanket `--run-ignored all`.

The `test (ubuntu-latest)` aggregate remains present for documentation and
registry-only changes. Documentation changes skip compilation; registry changes
run catalog validation and generated-artifact checks. Protocol specifications
under `docs/specs/` still trigger the code checks.

## Platform and feature coverage

Linux runs the common workspace behavior once, with all features. One feature
job checks the 17 production configurations and their dependency boundaries in
sequence, without dev-dependencies unifying extra features. Test targets and
examples compile once with all features; their reduced-feature compatibility
and the observation benchmark are checked by Distribution acceptance.
Public-library checks retain their independent feature closures.

One focused PR job also runs the gRPC-only telemetry delivery contract. The
all-feature archive chooses HTTP at compile time and excludes this test, so
this target requires a separate `--no-default-features --features otel-grpc`
build. It verifies spans, metrics, authentication metadata and shutdown flushing
against a local collector, with one test thread and no retries. The same action
runs in Distribution acceptance's existing `otel-grpc` profile. Both paths
require exactly one selected test and preserve JUnit and resource evidence.

macOS runs Unix socket permissions, signal draining and terminal behavior.
Windows runs named-pipe behavior and the platform-only file replacement,
PowerShell cleanup, unsupported tenancy and installer contracts. It also checks
native update commands and executable names, copied CLI aliases, config identity,
SQLite path normalization and atomic policy publication/rollback. Windows still
needs the library harness to reach its private unit tests, but only the selected
platform tests execute. The platform jobs require the complete selected inventory
(16 macOS cases and 20 Windows cases) before running the same built executables,
and upload the inventory and JUnit evidence. Pure Keychain output parsing also
runs on Linux.

An all-feature binary cannot exercise `cfg(not(feature = ...))` branches or
prove that an absent feature produces no side effects. Those runtime contracts,
isolated SDK consumers, live registry lifecycle and packaged gateway acceptance
remain in **Distribution acceptance**, available manually and weekly. Version
tags also run the feature/consumer contracts, while the release workflow alone
builds and verifies the packaged target matrix against its actual dist artifacts.

## Capacity campaigns

PR CI also runs ten bounded capacity parents from the same all-feature archive:

- Five maximum-tree, tool-batch and artifact-quota contracts.
- Three native checkpoint capacity, long-loop and cold-recovery contracts.
- The fixed 128-session experiment and the 100,000-row offline recovery contract.

Three Linux lanes run one parent at a time, with a 25-minute job limit, no retries
and resource/JUnit evidence. Internal concurrency, product limits, operation
counts and per-operation deadlines are unchanged. These cases use `#[ignore]`
to avoid duplicate execution in the ordinary lanes, and exact selectors prevent
internal fault-injection children from running independently. A missing match
fails the job. They remain available in the manual/weekly **Capacity campaigns**
workflow as well; its explicit filter includes ignored parents.

The two full Choice receipt-history campaigns still require an explicit test-build
cfg and the lightly optimized `ci-capacity` profile. Their earlier optimized
runtime is not evidence that running their growing snapshots in the ordinary
unoptimized archive would fit the PR budget. To select a Choice case locally:

```sh
RUSTFLAGS='--cfg bitrouter_capacity_tests' cargo nextest run \
  -p bitrouter --all-features --lib --test core_execution \
  --cargo-profile ci-capacity --test-threads 1 \
  -E 'test(tenancy::tests::execution::native::session::choice::headroom::)'
```

The `ci-capacity` profile uses light optimization without release LTO, retaining
debug assertions and overflow checks. External load generators and prolonged
stress testing belong outside the PR workflow.

## Measured PR baseline

[PR #1009's final run](https://github.com/bitrouter/bitrouter/actions/runs/41133691229)
passed all 21 jobs on October 31, 2026. It used 172.4 runner-minutes; the Linux
critical path was 40.4 minutes (23.8 minutes building/publishing, then at most
14.7 minutes restoring/running tests, plus initial scheduling). The full run
lasted 171.7 minutes because macOS started 162 minutes after creation; that
platform job itself took only 9.6 minutes. Queue delay must be reported separately
from execution, and runner-minutes are not CPU-minutes.

Before restoration, successful unoptimized runs put the ten capacity parents at
13–323 seconds each. Those measurements justify testing archive reuse; the
restored lanes' evidence measures their actual elapsed time and maximum resident
memory on the current runner. Choice's old release-profile timing is excluded
from this comparison. Shared archive download/extraction and extra compilation
are still real costs, so compare the whole workflow, not only test execution.
