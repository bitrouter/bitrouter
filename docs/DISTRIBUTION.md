# Distribution profiles and packaging

The Cargo package `bitrouter` installs the `bitrouter` gateway and operator CLI.
The package `bitrouter-bro` installs the `bro` local harness. Each has its own
shell and PowerShell installer, npm package, Homebrew formula and install
receipt. The default service, minimal gateway, PostgreSQL shared gateway and BRO
are available for the five targets below.

| Distribution | Executable | Owner and update path |
| --- | --- | --- |
| BitRouter default | `bitrouter` | Service operator, matching product installer or package manager |
| Minimal gateway archive | `bitrouter` | Service operator, checksum-verified manual archive |
| Shared gateway archive | `bitrouter` | Service operator, checksum-verified manual archive |
| BRO | `bro` | Workspace user, matching BRO installer or package manager |

Installing BRO on a client requires the gateway origin and a scoped client
credential. Configure provider secrets and the service database on the gateway
host. For source installs, use `cargo install --locked --path apps/bitrouter`
or `cargo install --locked --path apps/bro` from the checkout. Each product's
`build-info` command identifies the selected executable.

## Gateway archives

The release workflow builds `bitrouter-gateway-<target>.tar.gz` on Unix and
`bitrouter-gateway-<target>.zip` on Windows, together with `<archive>.sha256`.
The two product installers and manual archives support the following targets.

Official macOS distributions and CI support Apple Silicon (`arm64`) only.
The same platform policy applies to both products.

| Target | Archive |
| --- | --- |
| `aarch64-apple-darwin` | `.tar.gz` |
| `aarch64-unknown-linux-gnu` | `.tar.gz` |
| `x86_64-unknown-linux-gnu` | `.tar.gz` |
| `x86_64-unknown-linux-musl` | `.tar.gz` |
| `x86_64-pc-windows-msvc` | `.zip` |

Download the archive and checksum from the same release. Verify the checksum
before extraction with `sha256sum -c <archive>.sha256` on Linux or
`shasum -a 256 -c <archive>.sha256` on macOS. On Windows, compare
`Get-FileHash <archive> -Algorithm SHA256` with the downloaded checksum.
Extract the archive into a dedicated version directory. It contains `bitrouter`
(`bitrouter.exe` on Windows), `manifest.json`, the README and license.

Run the extracted executable's `build-info` command. Its profile must be
`gateway`, with an empty feature list, and its target and version must match the
chosen release. The manifest records the compiled identity and binary hash.
Run this executable directly with `serve --config <path>` or install it into the
service's executable directory. Provide the service configuration and provider
credentials on the server, and connect clients through the supported HTTP APIs
or [the service SDK](../crates/bitrouter-sdk/README.md). An SDK consumer needs no
server configuration files, provider credentials or database access.

The minimum archive includes SQLite, routing, authentication, metering and model
protocol conversion. The reduced service omits MCP, request-local
server tools, OTel exporters and PostgreSQL/MySQL drivers. Configuration cannot
enable an omitted capability. Effective unsupported activation fails explicitly;
dormant saved settings remain readable.

## Updates and profile transitions

Keep the installed service profile when updating. Operators can replace a manual
archive with a verified archive for the same target and profile. The service
updater prints an exact-feature Cargo install command for a manual profile.
Use the product's installer receipt for a matching installed product, or the
owning package manager. Check `bitrouter update --help` and `bro update --help`
on the two installations for their update options.

In the M0 installer layout `bitrouter` was a link to `bro`. Stop the combined
daemon, preserve its executable, link, receipt, configuration, database and
external keys as one rollback set. Move the previous installation aside before
installing either new product. The installer verifies ownership before replacing
an existing executable. Select a separate installation prefix if a previous
manual installation needs to remain on disk.

For an M0 SQLite installation, run the stopped-store migration described in the
[CLI reference](CLI.md#split-a-stopped-m0-sqlite-store). Keep the complete M0
snapshot and stop admission before reconciling any new usage when rolling back.

Stage and inspect the replacement before restarting the service. A changed
profile is an explicit operator decision; `status` distinguishes installed and
resident artifact identities, and automatic handoff refuses a profile mismatch.
Explicit transitions retain preflight, active-work, ownership and recovery
requirements. Never replace a running service merely because two binaries have
the same version. See [CLI identity](CLI.md#executable-identity) and
[updating](../skills/bitrouter/references/updating.md).

### Recovering an interrupted product install

Retry the same product's guarded installer at the original prefix. When its
executable still runs, its native `update` command can also finish an interrupted
transaction. `--check` leaves the receipt and locks intact. New writers hold OS
leases while changing the binary and receipt. They can reuse a marked lock
after the OS releases the lease. The persistent `.lease` files can remain in
place between installs. Git Bash uses directory locks and requires a quiescent
manual recovery after a hard stop.

The product receipt is `<config>/<package>/<package>-receipt.json`. Its staged
successor is `<config>/<package>/.install-lock/receipt`. The guards compare both
receipts' product and prefix bindings with the destination and select the one
whose SHA-256 matches the executable. On Windows, if the running image was
relocated before the replacement arrived, rerun the guarded PowerShell installer.
It restores `.install-lock/previous.exe` when its digest matches the previous
published receipt.

For an unmarked lock or conflicting evidence:

1. Stop the product's updaters and installers. For a service rollback, also stop
   the service and follow the data rollback procedure above. Copy the executable,
   product receipt, `<bin>/.<package>-install-lock` and
   `<config>/<package>/.install-lock` to a fresh backup directory.
2. Hash the executable and inspect the published receipt, staged `receipt` and
   any older `previous-receipt.json` in the receipt lock. A usable receipt must
   name the same product, official source, exact destination layout and prefix,
   single expected executable, empty aliases and matching `binary_sha256`.
   Verify the version and product using the executable's `--json build-info`.
3. If exactly one distinct receipt matches, copy it to a temporary file beside
   the published receipt and rename it into place. Otherwise restore the last
   independently verified binary **and** receipt together from a backup for this
   product and prefix. If that pair is unavailable, keep the evidence and use a
   separate fresh installation for further diagnosis.
4. Once the pair verifies, move the two quiescent lock directories to the backup
   and rerun the same product's guarded installer. Keep the peer product's files
   and receipts in place.

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
cargo run --locked -p dist-helper -- package --profile gateway-shared \
  --target aarch64-apple-darwin --out target/shared-candidate
```

The default `--cargo-profile dist` uses the release distribution settings.
`--cargo-profile dev` is available for faster integration tests; those archives
are intended for integration tests. Cargo-dist owns default service and BRO
installers and uses each product's own package name, binary and feature defaults.

The packager requires a source revision from the Git checkout, packaged VCS
metadata or explicit `BITROUTER_BUILD_REVISION`. CI supplies the checked-out
revision. The artifact identity describes that source revision and compiled
capabilities; archive and binary hashes identify the exact bytes. A local dirty
checkout is not a reproducible release merely because its base revision is known.

The reusable `gateway-artifacts.yml` workflow builds the configured targets, verifies
archive checksums, extracts each candidate and runs the
[independent SDK acceptance](../apps/bitrouter/tests/fixtures/sdk_consumer/README.md).
PR CI uses development candidates. Release builds use distribution candidates
and require the packaged SDK checks before upload. Release planning and archive
validation cover both cargo-dist products. See the
[shared gateway process checks](SHARED_GATEWAY_DISTRIBUTION.md).
