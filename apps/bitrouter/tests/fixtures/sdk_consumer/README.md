# Packaged gateway / independent SDK acceptance

This is bounded P0 A03/A04/A14 evidence. The integration host launches a supplied
`bro` unpacked from the official **gateway** archive and this separately built
consumer. It never substitutes `CARGO_BIN_EXE_bro`, builds either executable, or
imports the application's server/configuration/database code. The consumer is
its own Cargo workspace with only SDK/AI path dependencies and client necessities;
it performs every service call through the actual SDK.

Produce and unpack the gateway archive, then build the consumer before selecting
the ignored integration test. From the repository root, use the native Rust
target for the machine running the test (shown here for Apple Silicon):

```sh
gateway_target=aarch64-apple-darwin
cargo run --locked -p dist-helper -- package --profile gateway \
  --target "$gateway_target" --cargo-profile dev --out target/gateway-acceptance
mkdir -p target/gateway-acceptance/unpacked
tar -xzf "target/gateway-acceptance/bitrouter-gateway-$gateway_target.tar.gz" \
  -C target/gateway-acceptance/unpacked
export BITROUTER_TEST_GATEWAY_BINARY=/absolute/path/to/unpacked-gateway/bro
export BITROUTER_SDK_CONSUMER_TARGET_DIR=/absolute/path/to/separate-sdk-consumer-target
cargo build --locked --manifest-path apps/bitrouter/tests/fixtures/sdk_consumer/Cargo.toml \
  --target-dir "$BITROUTER_SDK_CONSUMER_TARGET_DIR"
export BITROUTER_TEST_SDK_CONSUMER_BINARY="$BITROUTER_SDK_CONSUMER_TARGET_DIR/debug/bitrouter-packaged-sdk-consumer"
cargo test -p bitrouter --no-default-features --test packaged_gateway \
  packaged_gateway_independent_sdk_consumer -- --ignored --exact --nocapture
```

Use `.exe` for both executable paths on Windows. Both `BITROUTER_TEST_*` values
must name existing absolute file paths; missing or relative values **fail** when
the test is selected. A full/custom executable or an artifact without source
revision metadata fails as well. The host compares `build-info` with the SDK's
remote artifact identity. Packaging/archive provenance is the
responsibility of the packaging command and CI: this fixture does not unpack,
verify signatures, or build archives. The release workflow uses the same command
with `--cargo-profile dist`; `dev` archives are test candidates only.

To inspect the separately resolved consumer graph after building:

```sh
cargo tree --manifest-path apps/bitrouter/tests/fixtures/sdk_consumer/Cargo.toml \
  --locked --edges normal,build
```

Review that closure for application/server/ORM/provider-auth/terminal dependencies;
the standalone successful build, not the tree alone, provides compilation evidence.
The nested workspace has its own committed `Cargo.lock`; use `--locked` so the
independent consumer remains reproducible without workspace feature unification.

The Rust host owns a private HOME, configuration, SQLite database, local control
socket, loopback inference/control listeners, and a Rust HTTP mock provider. It
uses the packaged `key sign` CLI to mint the inference credential and configures
a distinct random control token with only `control:read`. Provider credentials
stay with the server. The foreground server, CLI helpers, and consumer all have
guards that kill and reap only their own children on errors/unwind. Ordinary
shutdown uses the packaged `stop` CLI with the explicit private socket and config,
then requires a successful server exit and closed HTTP listeners. There is no
resident-daemon discovery and no detached daemon.

The consumer runs in a different empty temporary working directory with a cleared
environment and exactly these four service inputs (plus `SystemRoot` on Windows):

- `BITROUTER_SDK_INFERENCE_ENDPOINT`
- `BITROUTER_SDK_INFERENCE_CREDENTIAL`
- `BITROUTER_SDK_CONTROL_ENDPOINT`
- `BITROUTER_SDK_CONTROL_CREDENTIAL`

It receives no configuration/storage/provider paths and does no filesystem or
local IPC access. This demonstrates client independence, not an OS filesystem
sandbox against malicious code running as the same user.

Coverage includes Chat/Messages/Responses nonstream and streaming text, final
usage and terminal completion; native predicate Decisions; models and health;
missing/invalid inference credentials; an unknown-model public error; typed
control capabilities/status and read-only scope; rejection of both swapped
credential domains; and a Chat upstream connection interrupted after partial
content. Interruption must report a stream failure, incomplete stream, or
non-timeout transport failure, never successful completion. Discovery and auth
probes must dispatch nothing. Exactly eight upstream POSTs are expected (six
generation calls, one Decisions call, one interrupted stream), checked by native
path, stream mode, interruption marker, model and provider credential **after
the gateway exits**, so delayed replay cannot evade the count.

All process waits are bounded (consumer: 90 seconds internally / 100 externally;
startup: 30; key setup: 30; identity: 10; stop command: 15; graceful exit: 20).
The ephemeral port reservations must be released immediately before gateway bind;
another local process winning that race causes a failure, not a retry against a
different daemon. This test is portable Rust and has no Unix-only test gate.

This fixture does not complete all A03/A04/A14 gates: full-product packaging,
installation/upgrades, registry/schema bootstrap, OS matrix execution, opaque
history/extension preservation, malformed/oversized peer coverage, explicit
consumer cancellation, and core/harness channel/ACK acceptance remain separate
suites. The interrupted case covers an upstream socket failure, not proof of
remote execution cancellation. No build, test, or commit is performed by the
integration host itself.

## Isolated prehashed manifest fidelity

This separate regression consumes a manifest emitted by the application's real
`HarnessManifestExt::digest` and `HarnessManifestExt::validate` implementation in
`apps/bitrouter/src/storage/checkpoint/validation.rs`. The
`sdk_manifest_fixture` example constructs a typed, read-only custom tool with
deliberately nonalphabetical JSON schema members. It calls production digest
and validation code; it does not duplicate hashing or hardcode a matching hash.

The output records the writer's fully qualified name, `writer_revision` and
application build identity. `tools_json` preserves the exact bytes supplied to
the digest, and `manifest_json` preserves the original validated manifest as a
string. Keeping those fields as strings prevents the consumer from accidentally
normalizing its own expected result. The consumer compares those original tool
bytes and the retained digest after typed bind and restore round trips. It also
requires nonalphabetical schema keys, so a sorted fixture cannot make the check
vacuous. This checks serialization fidelity, not live core admission or execution.

Run these commands from the repository root after the independent workspace's
lockfile has been refreshed for dependency changes:

```sh
mkdir -p target/sdk-manifest-fidelity
cargo run --locked -p bitrouter --no-default-features --features core \
  --example sdk_manifest_fixture \
  > target/sdk-manifest-fidelity/manifest.json
export BITROUTER_TEST_MANIFEST_FIXTURE="$PWD/target/sdk-manifest-fidelity/manifest.json"
cargo test --locked \
  --manifest-path apps/bitrouter/tests/fixtures/sdk_consumer/Cargo.toml \
  --target-dir target/sdk-consumer --test manifest_fidelity -- \
  --ignored --exact server_prehashed_manifest_survives_isolated_bind_and_restore
```

Only the separately built writer enables application `core`. The consumer test
uses its own workspace and SDK/AI dependencies with defaults disabled; it needs
neither the application, SDK `core`, nor application dev-dependency features.
The test is ignored by ordinary `cargo test` because its input must come from
the separately built writer. Select it explicitly as above; a missing fixture
or missing provenance fails the selected test. Running ordinary consumer tests
for packaged gateway acceptance does not establish this regression's result.
