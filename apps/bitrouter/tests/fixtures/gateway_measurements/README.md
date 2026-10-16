# Local gateway measurements

This standalone Rust fixture compares three immutable gateway executables: the
pre-refactor baseline, the full product, and the minimum gateway. It has no
application library dependency. The comparison exercises separate processes
with a fixed loopback mock provider, private configuration, fresh SQLite state
and an isolated environment. It never discovers or stops an installed daemon.

Build each executable with the same compiler, target, optimization and debug
settings, and copy it outside Cargo's output directory before building another
profile or a test target. The report records each executable's SHA-256 and size;
record the source revision, feature selection and compiler alongside it. Use
optimized builds for production performance conclusions. Development builds
only establish a comparison under their recorded build settings.

```sh
cargo build --locked \
  --manifest-path apps/bitrouter/tests/fixtures/gateway_measurements/Cargo.toml
apps/bitrouter/tests/fixtures/gateway_measurements/target/debug/bitrouter-gateway-measurements \
  /absolute/baseline/bro /absolute/full/bro /absolute/gateway/bro \
  /absolute/new-measurement.json
```

`CARGO_TARGET_DIR` may select a separate fixture build directory. The JSON
destination must be absolute and must not already exist. No completed report is
written when a request, readiness check or shutdown fails. SIGINT and SIGTERM
cancel the active sample and drop the owned gateway guard; it kills and reaps
that child before deleting its temporary files. SIGKILL cannot run cleanup.

The workload uses three rotating profile orders, one gateway at a time. Each
sample performs 16 warmup requests, then three idle RSS samples 100 ms apart,
4,096 ordinary requests and 4,096 streaming requests, with concurrency eight.
Every response contains the same short text and known usage. The stream check
requires valid event framing, exact reconstructed content, completion, usage
and terminal `[DONE]`; disconnect or substring presence alone cannot pass.
Inference bodies are bounded to 64 KiB and health bodies to 16 KiB. Each sample
requires exactly 8,208 provider calls, detecting missing dispatch or replay.

Startup time includes process launch, migrations and readiness through private
control plus HTTP health. Idle RSS means the warmed process before the measured
load, in KiB from `ps`; after-load RSS is a separate sample. Latencies include
HTTP transport, complete body consumption and validation. Streaming throughput
uses an immediate fixed response, not a simulated model's token-generation rate
or a time-to-first-token measurement. The fixture does not measure remote
network latency, provider speed, long-lived streams or every optional subsystem.
Report all samples and their variation, rather than selecting the fastest run.

Run parser and body-bound tests normally. The ignored process test requires an
immutable gateway and checks both SIGINT and SIGTERM while its child is alive:

```sh
cargo test --locked \
  --manifest-path apps/bitrouter/tests/fixtures/gateway_measurements/Cargo.toml
BITROUTER_MEASUREMENT_TEST_BINARY=/absolute/gateway/bro \
  cargo test --locked \
    --manifest-path apps/bitrouter/tests/fixtures/gateway_measurements/Cargo.toml \
    --test cancellation -- --ignored --test-threads=1
```

The fixture requires Unix, `ps` and `kill`. CI validates its parser/body tests
and interruption cleanup against the retained minimum artifact on Linux and
macOS. Comparable performance runs remain explicit because they require the
matching baseline artifact and controlled build and host conditions.
