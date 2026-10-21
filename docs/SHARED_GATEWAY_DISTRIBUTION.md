# Shared gateway distribution and process validation

`dist-helper package --profile gateway-shared` builds the official `bro` with
`--no-default-features --features multi-instance`. Its effective application
features are exactly `["db-postgres", "multi-instance"]`. No Cargo feature,
dependency, or default changes are required to select this distribution.

The archive root and filename are `bitrouter-gateway-shared-<target>` with a
`.tar.gz` extension, or `.zip` on Windows. The adjacent `.sha256` checks
the downloaded bytes against the published checksum; `manifest.json` records
the embedded identity, compilation profile, and binary digest. The profile
spelling is `gateway-shared` in JSON, human output and archive names.

The existing `gateway` profile still disables all optional application features;
`full` still uses the manifest defaults. Additional capabilities combined with
`multi-instance` are `custom`. Shared capability alone does not change startup
mode: configuration must explicitly select shared authority. Local SQLite and
the dependencies of existing local selections remain unchanged.

## CI and release path

`.github/workflows/ci.yml` calls the reusable `gateway-artifacts.yml` with the
`dev` compilation profile on PRs and the existing main/v1 pushes.
`dist-workspace.toml` registers that same workflow as a cargo-dist local-artifacts
job. The generated `release.yml` already calls it, waits for completion, and
collects its `artifacts-*` outputs for the release path. The reusable workflow
builds both gateway distributions on all six existing target/runners, verifies
the archive checksum and the executable's `build-info` against the manifest,
and keeps their archive and CI artifact names distinct. Full installers and
their existing jobs are unchanged.

The reusable workflow also owns a Linux PostgreSQL 17 service job. It downloads
the already packaged x86_64 GNU shared archive and runs the ignored
`gateway::shared::runtime::process_tests::` campaigns with that exact executable.
This includes the existing multi-process bootstrap, authority/authentication,
operator, admission/duplicate and drain campaigns selected by that module.
New tests in the same module join this selection. No workspace debug `bro` is
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
export BITROUTER_TEST_GATEWAY_BINARY="$PWD/target/shared-package/unpacked/bitrouter-gateway-shared-x86_64-unknown-linux-gnu/bro"
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

## Update boundary and remaining evidence

Shared archives are manually installed and rolled by the owning service manager.
`bro update` delegates to a source command preserving the exact normalized
feature set; it does not acquire the full distribution's installer receipt or
reuse its update-cache nudge. Status/build-info contracts preserve the distinct
profile. This does not turn `bro status` or local daemon restart into shared
operator APIs.

Archive production and the CI selection are required distribution evidence, not
complete P1 acceptance. Results must come from executed CI/local campaigns;
cross-platform archive checks do not imply PostgreSQL process coverage on every
target. Runtime, recovery, delivery and other milestone gates retain their own
requirements. This change schedules no release and publishes nothing by itself.
