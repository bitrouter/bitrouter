# Distribution profiles and packaging

The installable package remains `bitrouter`, and the executable remains `bro`.
Default Cargo, shell, PowerShell, npm and Homebrew installations contain the full
product. Gateway archives are separate release assets; choosing one does not
require building a custom application. Both profiles always include the gateway
and its management CLI. See [the feature plan](BITROUTER_SERVICE_REFACTOR_PLAN.md#feature-dependency-plan)
for optional capabilities and the full default selection.

## Gateway archives

The release workflow builds `bitrouter-gateway-<target>.tar.gz` on Unix and
`bitrouter-gateway-<target>.zip` on Windows, together with `<archive>.sha256`.
The target set matches the full-product release configuration:

| Target | Archive |
| --- | --- |
| `aarch64-apple-darwin` | `.tar.gz` |
| `x86_64-apple-darwin` | `.tar.gz` |
| `aarch64-unknown-linux-gnu` | `.tar.gz` |
| `x86_64-unknown-linux-gnu` | `.tar.gz` |
| `x86_64-unknown-linux-musl` | `.tar.gz` |
| `x86_64-pc-windows-msvc` | `.zip` |

Download the archive and checksum from the same release. Verify the checksum
before extraction with `sha256sum -c <archive>.sha256` on Linux or
`shasum -a 256 -c <archive>.sha256` on macOS. On Windows, compare
`Get-FileHash <archive> -Algorithm SHA256` with the downloaded checksum.
Extract the archive into a dedicated version directory. It contains `bro`
(`bro.exe` on Windows), `manifest.json`, the README and license.

Run the extracted executable's `build-info` command. Its profile must be
`gateway`, with an empty feature list, and its target and version must match the
chosen release. The manifest records the compiled identity and binary hash.
Run this executable directly with `serve --config <path>` or install it into the
service's executable directory. Provide the service configuration and provider
credentials on the server, and connect clients through the supported HTTP APIs
or [the service SDK](../crates/bitrouter-sdk/README.md). An SDK consumer needs no
server configuration files, provider credentials or database access.

The minimum archive includes SQLite, routing, authentication, metering and model
protocol conversion. It omits core, harness, terminal UI, MCP, request-local
server tools, OTel exporters and PostgreSQL/MySQL drivers. Configuration cannot
enable an omitted capability. Effective unsupported activation fails explicitly;
dormant saved settings remain readable.

## Updates and profile transitions

Keep the installed profile when updating. Full-product installations retain their
existing updater and package-manager paths. Gateway archives can be replaced
with a verified gateway archive for the new release and the same target. The
current gateway updater delegates to an exact-feature Cargo installation command
instead of using the full-product installer. It does not automatically download
these archives. Custom builds receive instructions preserving their effective
feature set.

Stage and inspect the replacement before restarting the service. A changed
profile is an explicit operator decision; `status` distinguishes installed and
resident artifact identities, and automatic handoff refuses a profile mismatch.
Explicit transitions retain preflight, active-work, ownership and recovery
requirements. Never replace a running service merely because two binaries have
the same version. See [CLI identity](CLI.md#executable-identity) and
[updating](../skills/bitrouter/references/updating.md).

## Reproducing candidate archives

`dist-helper` builds the selected application profile, obtains its executable and
build-script metadata from Cargo's JSON messages, and packages those exact files.
Each invocation owns a fresh temporary Cargo target directory until publication
finishes; concurrent builds cannot replace its selected executable or metadata.
It does not execute a cross-compiled binary to infer identity. Archive names are
profile-specific and existing outputs are not overwritten.

```sh
cargo run --locked -p dist-helper -- package --profile gateway \
  --target aarch64-apple-darwin --out target/gateway-candidate
cargo run --locked -p dist-helper -- package --profile full \
  --target aarch64-apple-darwin --out target/full-candidate
```

The default `--cargo-profile dist` uses the release distribution settings.
`--cargo-profile dev` is available for faster integration tests; those archives
are test candidates, not release/performance evidence. `--profile full` produces
a separate `bitrouter-full-<target>` candidate and does not replace cargo-dist's
existing full-product installer archives.

The packager requires a source revision from the Git checkout, packaged VCS
metadata or explicit `BITROUTER_BUILD_REVISION`. CI supplies the checked-out
revision. The artifact identity describes that source revision and compiled
capabilities; archive and binary hashes identify the exact bytes. A local dirty
checkout is not a reproducible release merely because its base revision is known.

The reusable `gateway-artifacts.yml` workflow builds the six targets, verifies
archive checksums, extracts each candidate and runs the
[independent SDK acceptance](../apps/bitrouter/tests/fixtures/sdk_consumer/README.md).
PR CI uses development candidates; the cargo-dist local-artifacts hook uses
distribution candidates and requires acceptance before upload. Only the normal
release workflow publishes assets. Existing full-product installers remain
cargo-dist-owned; do not hand-edit its generated `release.yml`.
