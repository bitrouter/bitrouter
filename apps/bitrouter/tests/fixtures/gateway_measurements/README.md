# Local gateway measurements

This standalone Rust fixture compares three immutable gateway executables: the
pre-refactor baseline, the full product, and the minimum gateway. It has no
application library dependency. The comparison exercises separate processes
with a fixed loopback mock provider, private configuration, fresh SQLite state
and an isolated environment. It never discovers or stops an installed daemon.

Build each executable with the same compiler, target, optimization and debug
settings, and copy it outside Cargo's output directory before building another
profile or a test target. The report records each executable's SHA-256 and size
and rechecks both before and after every sample, outside its timed workload;
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
written when a request, readiness check, required RSS observation, binary
identity check or shutdown fails. SIGINT and SIGTERM
cancel the active sample and drop the owned gateway guard; it kills and reaps
that child before deleting its temporary files. SIGKILL cannot run cleanup.

The workload uses three rotating profile orders, one gateway at a time. Each
sample performs 16 warmup requests, then three warmed idle process observations
with a one-second quiet wait before each,
4,096 ordinary requests and 4,096 streaming requests, with concurrency eight.
Every response contains the same short text and known usage. The stream check
requires valid event framing, exact reconstructed content, completion, usage
and terminal `[DONE]`; disconnect or substring presence alone cannot pass.
Inference bodies are bounded to 64 KiB and health bodies to 16 KiB. Each sample
requires exactly 8,208 provider calls, detecting missing dispatch or replay.
These counts are fixed workload checkpoints, not product or directory capacity
ceilings. Each measured mode counts
`successful`, `refused` (any HTTP non-2xx response), and `failed` (transport,
timeout, malformed or incomplete successful response). The fixture never follows
redirects; readiness checks also stay on the owned listener.
An explicit SSE error event or JSON error envelope invalidates a response even
if it also contains otherwise successful content, usage or `[DONE]`.
On the first failure it stops issuing new requests and drains only those already
issued, each under its
request timeout. The error includes exact attempted/successful/refused/failed
counts and `not_started` out of the planned 4,096. Any refusal or failure
invalidates the run; no latency summary or
completed JSON report is published for that mode. Successful reports therefore
have 4,096 successes and zero refused/failed per mode. Warmup still stops at its
first failure and reports its request ordinal. Interruption is an incomplete
run, never a zero-success performance result.

Startup time includes process launch, migrations and readiness through private
control plus HTTP health. Idle RSS means the warmed process before the measured
load, in KiB from `ps`; after-load RSS is a separate sample. Only the owned
gateway PID is observed: neither the fixture's wiremock/client process nor
gateway descendants are included. Higher after-load RSS is an observation,
not evidence of a leak. Latencies include
HTTP transport, complete body consumption and validation. Both measured modes
report total-latency `latency_p50_ms`, `latency_p95_ms` and `latency_p99_ms`.
Percentiles select the zero-based sorted sample at
`floor(count * percentile / 100)`, without interpolation.
Streaming throughput uses an immediate fixed response. `ttft_p50_ms` /
`ttft_p95_ms` measure request start to arrival of the first **complete SSE frame
with nonempty content**,
before complete-body validation. Headers, comments, role-only deltas and usage
do not count. A partial frame or split UTF-8 character must finish first; the
entire stream must still validate. Ordinary-mode TTFT is `null` (not applicable).
The immediate mock can coalesce every event into one network chunk, so this is
local streaming response latency, not provider token-generation speed. There
is no long-stream or paced-token workload in this fixture.

The v2 report preserves the v1 RSS and latency field names, but changes the
sampling schedule and adds total-latency P99, process windows, TTFT and request
accounting. Readers
must recognize `bitrouter.gateway-local-measurements.v2`; do not pool v1 and v2
samples as an unchanged experiment. Rerun both frozen artifacts with the same
fixture revision for a comparison.

`warmed_idle`, each mode's `process`, and `after_load` hold timestamped point
observations (RSS, cumulative process CPU time, thread count and open FD count)
plus a CPU interval derived from their first and last observations. Each mode
has observations bracketing its load, and samples started during load at roughly
one-second intervals; `during_load_samples` can be zero for a short mode. These
are point samples, not peak or time-averaged thread/FD/RSS values. A final
after-load window contains two separate observations with a one-second quiet
wait between them. Diagnostic subprocesses are polled concurrently with the
request workload, so waiting for `ps` or `lsof` does not stop response reads.
CPU windows include observation overhead and any tail after requests finish;
their `elapsed_ms` is distinct from timed request throughput. Every observation
records its duration to expose slow or intrusive collectors.

CPU percentage is cumulative user+system CPU delta divided by monotonic wall
time, with 100% meaning one fully occupied core (it may exceed 100%). It excludes
child-process CPU. Linux reads process ticks and thread count from `/proc/PID/stat`
and obtains tick frequency from `getconf CLK_TCK`; FDs are numeric directory
entries in `/proc/PID/fd`. macOS uses `ps -o time=` (cumulative CPU, not the
decaying `%cpu` value), `ps -M` thread rows, and `lsof -Fpf` numeric descriptors;
`cwd`, `txt` and mapped-file entries are not descriptors. Source time precision
is reported as `resolution_ms`; a zero delta means no measurable tick change,
not proof of zero CPU use. Counts are not atomic with the CPU or RSS reading.

CPU/thread/FD observations are explicitly `available` with value/source, or
`unavailable` with source/reason. Missing tools, permission errors, timeouts,
diagnostic warnings and unrecognized output never turn into numeric zero.
The CPU window is unavailable if an endpoint is unavailable or the counter
decreases. RSS and a live owned child remain mandatory. Unsupported Unix
variants report the optional metrics unavailable rather than assuming Linux
or macOS semantics. No native FFI, extra Rust dependencies or Python are added;
Tokio's existing `sync` feature supplies the workload-completion notification.

The fixture does not measure remote
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

The fixture requires Unix, `ps` and `kill`; Linux CPU observation also uses
`getconf` and macOS FD observation uses the system `lsof`. Parser tests cover
both platforms even when run on one, and a small self-process smoke test checks
the host's collector without starting a gateway. CI validates parser/body tests
and interruption cleanup against the retained minimum artifact on Linux and
macOS. Comparable performance runs remain explicit because they require the
matching baseline artifact and controlled build and host conditions.
