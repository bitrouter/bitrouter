# Updating BitRouter and BRO

Use `bitrouter update` for the service and `bro update` for the local harness. Each product’s cargo-dist installer places its own executable in the configured Cargo bin directory and writes a separate receipt for that product. Run both installers to install both executables. Check `bitrouter --json build-info` or `bro --json build-info` for the product, version, target and source revision.

- `--check` resolves the release inventory and reports availability.
- The default channel includes prereleases. Use `--stable` to select stable releases.
- `--tag TAG` selects the exact tag, including a compatible older split release. A package-scoped tag applies to that package. Use `-y` to skip confirmation.
- macOS release archives support Apple Silicon. The updater selects the archive for the target compiled into the invoking executable, including the GNU or musl ABI on Linux.

Before replacement, the updater verifies the installed binary digest, receipt product, executable, aliases, version, layout and prefix. It stages the selected archive, checks its SHA-256 sidecar and runs the staged binary to verify product, version, target and source revision. The service also checks its profile and features. A release missing the split distribution plan or the executable identity fails before replacement.

M0 installed a combined `bro` and a `bitrouter` alias. To migrate, stop M0, snapshot its data and preserve the old binary, alias and receipt together. Install each desired split product using its own installer. A return to M0 requires the corresponding application and data snapshot.

Homebrew and npm installations delegate to the package manager. A Cargo installation without a release receipt delegates to Cargo. A manual archive installation requires a manual update. Reduced service profiles (`gateway`, `gateway-shared` and custom builds) report a source-install command with the exact effective features. For a manual archive replacement, verify the archive checksum and the new executable’s build information. Use the matching gateway or gateway-shared distribution and preserve the current service’s data and capabilities.

A service update attempts a compatible idle daemon handoff after verifying the installed replacement. The hidden service flag `--restart` remains accepted. A deferred handoff leaves the resident process running and reports the reason. Finish active work before explicit restart or use the owning service manager. BRO updates can run before opening a session store or connecting to a gateway.

`bitrouter status` can show a cached release nudge for the full service profile. Set `BITROUTER_NO_UPDATE_CHECK=1` to disable it.
