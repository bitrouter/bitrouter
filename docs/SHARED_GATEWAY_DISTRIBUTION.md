# Shared gateway distribution and process validation

`dist-helper package --profile gateway-shared` builds the service executable `bitrouter` with
`--no-default-features --features multi-instance`. Its effective application
features are exactly `["db-postgres", "multi-instance"]`. The process configuration selects shared authority at startup.

The archive root and filename are `bitrouter-gateway-shared-<target>` with a
`.tar.gz` extension, or `.zip` on Windows. The release's
`distribution-manifest.json` records the archive SHA-256. The archive's
`manifest.json` records the embedded identity, compilation profile and binary
digest. The profile spelling is `gateway-shared` in JSON, human output and
archive names.

The `gateway` profile disables optional application features. Additional
capabilities combined with `multi-instance` are classified as `custom`.
Set `gateway_state.mode: shared` to select shared PostgreSQL authority.

## CI and release path

`distribution-acceptance.yml` calls the reusable `gateway-artifacts.yml` with
the `dev` compilation profile for scheduled and manual acceptance runs.
`release.yml` calls it with the distribution profile for packaging and shared
changes in pull requests and for tags that include the service. The reusable
workflow builds both gateway distributions on all four supported targets. It
verifies the archive against its internal sidecar and checks the executable's
`build-info` against its embedded manifest. Cargo-dist generates the default
service and BRO installers.

The reusable workflow also owns a Linux PostgreSQL 17 service job. It downloads
the already packaged x86_64 GNU shared archive and runs the ignored
`gateway::shared::runtime::process_tests::` campaigns with that exact executable.
This includes the existing multi-process bootstrap, authority/authentication,
operator, admission/duplicate and drain campaigns selected by that module.
New tests in the same module join this selection. No workspace debug executable is
substituted. The PostgreSQL user belongs to the disposable CI service and has
CREATEDB for UUID-owned blank-database fixtures. Two test threads retain each
campaign's real concurrent replicas; this is not a mock SQL or CLI-help gate.

The CI feature-isolation matrix also selects `multi-instance` explicitly. Its
PostgreSQL dependency check recognizes the implied driver, while exclusions for
UI, harness, MCP, telemetry and MySQL dependencies continue to apply.

## Reproduce on Linux x86_64

Use Rust 1.97.0 or newer, cargo-nextest, jq, tar, sha256sum and a dedicated
PostgreSQL 17 test database. Set `BITROUTER_TEST_DATABASE_URL` to a role with
CREATEDB and permission to create the fixture schemas/tables. These campaigns
write fixture records; never point them at a deployment or developer database
containing valuable data. Blank-database tests create/drop only their own
successfully created UUID databases; other process fixtures use unique
namespaces in the provided dedicated test database.

From the repository root, choose a fresh artifact output directory (the helper
refuses to overwrite an existing one):

```sh
export BITROUTER_TEST_DATABASE_URL='postgres://fixture:fixture-password@127.0.0.1:5432/bitrouter_shared_test'
export BITROUTER_BUILD_REVISION="$(git rev-parse HEAD)"
export CARGO_TARGET_DIR="$PWD/target/shared-distribution-tests"
export CARGO_BUILD_JOBS=2
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0

cargo run --locked -p dist-helper -- package --profile gateway-shared \
  --target x86_64-unknown-linux-gnu --cargo-profile dev \
  --out target/shared-package
(cd target/shared-package && sha256sum -c *.sha256)
mkdir target/shared-package/unpacked
tar -xzf target/shared-package/bitrouter-gateway-shared-x86_64-unknown-linux-gnu.tar.gz \
  -C target/shared-package/unpacked
export BITROUTER_TEST_GATEWAY_BINARY="$PWD/target/shared-package/unpacked/bitrouter-gateway-shared-x86_64-unknown-linux-gnu/bitrouter"
"$BITROUTER_TEST_GATEWAY_BINARY" --json build-info > target/shared-package/build-info.json
jq -e --slurpfile actual target/shared-package/build-info.json \
  --arg revision "$BITROUTER_BUILD_REVISION" \
  '.identity == $actual[0] and .identity.profile == "gateway-shared" and .identity.features == ["db-postgres", "multi-instance"] and .identity.source_revision == $revision' \
  target/shared-package/unpacked/bitrouter-gateway-shared-x86_64-unknown-linux-gnu/manifest.json

cargo test --locked -p dist-helper distribution::tests
cargo test --locked -p bitrouter-ai --all-features --test artifact_contracts
cargo test --locked -p bitrouter --no-default-features --features multi-instance \
  --test artifact_build_metadata
cargo nextest run --locked -p bitrouter --no-default-features --features multi-instance \
  --lib --run-ignored only --test-threads 2 \
  -E 'test(gateway::shared::runtime::process_tests::)'
```

Use `--cargo-profile dist` for release-equivalent archives. The helper builds in
its own temporary target directory and pairs Cargo's selected executable with
that invocation's OUT_DIR metadata. The test compilation directory is separate
from the unpacked candidate, so later test builds cannot replace the binary
under test. The source revision describes the base commit; use a clean checkout
when reproducing release evidence and retain any dirty-source hashes separately.

## Updates

Verify the replacement archive and its `build-info`, then roll the foreground
service through its owning service manager. Drain in-flight requests during
shutdown. The shared archive uses its own profile and manual installation
path. See [gateway updates](DISTRIBUTION.md#updates-and-profile-transitions) and
[shared gateway configuration](../skills/bitrouter/references/shared-gateway.md).
