# Continuous integration

The PR workflow checks feature configurations at compile time and runs ordinary
behavior tests from an all-feature Linux build. Tests should cover module
contracts, failure paths and boundaries with bounded local fixtures. Small
process and database integration tests remain in PR CI.

## Build once, run separately

`build-linux` compiles the workspace tests and examples with two build jobs and
without debug information. It publishes a nextest archive for the exact GitHub
revision. The module, integration, process-contract and database jobs download
that artifact and run its executables without invoking Cargo or rustc. The
archive includes process fixtures; a missing fixture is an error, not permission
to rebuild during a test deadline.

All artifact consumers use Ubuntu 24.04 and verify both the revision and the
original workspace path. Some tests embed Cargo paths, so arbitrary relocation
is not supported. Dependency caches speed later builds; the uploaded archive
shares the actual executables within one run. See [nextest archives](https://nexte.st/docs/ci-features/archiving/).

Consumers retain JUnit, elapsed time, maximum resident memory and disk usage in
their evidence artifacts. `/usr/bin/time` reports process resource accounting,
not the peak memory of the entire VM or database service containers. Concurrent
tests are limited to two; database and ignored process parents run serially.
Only the listed ignored parents run: internal fault-injection children must not
be selected with a blanket `--run-ignored all`.

The `test (ubuntu-latest)` aggregate remains present for documentation and
registry-only changes. Documentation changes skip compilation; registry changes
run catalog validation and generated-artifact checks. Protocol specifications
under `docs/specs/` still trigger the code checks.

## Platform and feature coverage

Linux runs the common workspace behavior once, with all features. The feature
matrix checks isolated production binaries before test dependencies can unify
features, then checks test targets and dependency boundaries. Public-library
checks retain their independent feature closures.

macOS runs Unix socket permissions, signal draining and terminal behavior.
Windows runs named-pipe behavior and the platform-only file replacement,
PowerShell cleanup, unsupported tenancy and installer contracts. Windows still
needs the library harness to reach its private unit tests, but only the selected
platform tests execute. Pure Keychain output parsing also runs on Linux.

An all-feature binary cannot exercise `cfg(not(feature = ...))` branches or
prove that an absent feature produces no side effects. Those runtime contracts,
isolated SDK consumers, live registry lifecycle and packaged gateway acceptance
remain in **Distribution acceptance**, available manually, weekly and on version
tags. The release workflow retains its existing packaged target matrix.

## Capacity campaigns

**Capacity campaigns** runs manually or weekly on one Linux runner. It contains
the full Choice receipt-history campaigns, fixed 128-session experiment, maximum
Core tree cases, long checkpoint histories and the 100,000-row offline recovery
campaign. Ordinary smaller boundary cases remain in PR CI. Product limits and
per-operation deadlines are unchanged.

The large test modules require an explicit test-build cfg so normal PR builds do
not compile them. To select a capacity case locally:

```sh
RUSTFLAGS='--cfg bitrouter_capacity_tests' cargo nextest run \
  -p bitrouter --all-features --lib --test core_execution \
  --cargo-profile ci-capacity --test-threads 1 \
  -E 'test(tenancy::tests::execution::native::session::choice::headroom::)'
```

The `ci-capacity` profile uses light optimization without release LTO, retaining
debug assertions and overflow checks. External load generators and prolonged
stress testing belong outside the PR workflow.
