# Continuous integration

PR CI builds workspace tests on Linux and archives their executables with nextest.
The archive consumers run gateway behavior, process, capacity and database tests
against the same revision. BRO tests run with `cargo nextest run -p bitrouter-bro`.
Use [nextest archives](https://nexte.st/docs/ci-features/archiving/) when reproducing
an archived test locally.

## Platform and feature coverage

The feature checks compile the service and client configurations independently
so workspace dev-dependencies cannot mask a missing runtime dependency.
The all-feature Linux build covers common behavior. Feature-specific jobs
exercise behavior that this build cannot select, including the gRPC-only OTLP
exporter. The database jobs use dedicated PostgreSQL and MySQL services.
macOS and Windows exercise platform-specific control, shutdown and file operations.

Documentation-only changes keep the required CI status without building the
workspace. Registry changes run catalog and generated-artifact validation.
Changes to compatibility-sensitive files under `docs/specs/` trigger code checks.

## Distribution

Distribution acceptance exercises reduced-feature builds, independent SDK
consumers and installed product behavior. Release jobs build and verify the
actual artifacts for the supported targets. See [distribution profiles](DISTRIBUTION.md)
and [shared gateway process validation](SHARED_GATEWAY_DISTRIBUTION.md).
