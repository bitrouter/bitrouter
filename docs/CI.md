# Continuous integration

The aggregate PR check is **test (ubuntu-latest)**. It always runs and requires
repository hygiene, formatting, plugin validation, and every check selected
for the change. A failed or cancelled dependency fails the aggregate check.

CI selects checks from the diff between the event base and the revision under
test. BRO-local changes build and test BRO on Linux, macOS, and Windows.
Gateway changes build and test the gateway on those platforms and build the
Linux BRO executable for the real gateway-to-BRO process test. Changes to
BRO's gateway client, endpoint wiring, or the two-product test select both full
product suites. Shared changes, including workspace dependencies and files in
`docs/specs/`, also run the shared-library tests and public API checks.
Registry source and public snapshot changes run catalog and generated-artifact
validation. Package-local snapshots also select the product that embeds them.
Documentation-only changes still report the aggregate status.

## Platform and feature coverage

Each product has one build job and one test job definition with native runner
matrices. Build jobs upload revision- and platform-specific nextest archives
through GitHub Actions. Test jobs verify the revision and checksums, then restore
each suite at its original Cargo target path so tests can launch subprocesses
using paths embedded at compile time. BRO's Linux suite requires the workspace
sandbox. macOS and Windows exercise native process and filesystem behavior.

The gateway's Linux job tests the all-feature build, uses PostgreSQL and MySQL
for the database regressions, and exercises the real BRO and independent AP SDK
process boundaries. Separate Linux archives cover the minimal, default, shared,
and gRPC-only gateway configurations. The gateway build checks production
feature closures independently of dev-dependencies. Use
[nextest archives](https://nexte.st/docs/ci-features/archiving/) to reproduce
an archived suite locally.

## Distribution

Distribution acceptance exercises reduced-feature builds, independent SDK
consumers and installed product behavior. Release jobs build and verify the
actual artifacts for the supported targets. See [distribution profiles](DISTRIBUTION.md)
and [shared gateway process validation](SHARED_GATEWAY_DISTRIBUTION.md).
