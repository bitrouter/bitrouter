# Updating BitRouter and BRO

Use `bitrouter update` for the service and `bro update` for the local harness. By default, each product’s cargo-dist installer places its own executable in the configured Cargo bin directory and writes its own receipt. Run both installers to install both products. An unmanaged installer run writes no receipt. Check `bitrouter --json build-info` or `bro --json build-info` for the product, version, target and source revision.

- `--check` resolves the release inventory and reports availability.
- The default channel includes prereleases. Use `--stable` to select stable releases.
- `--tag TAG` selects the exact tag, for that product. A package-scoped tag applies to that package. Use `-y` to skip confirmation.
- macOS release archives support Apple Silicon. The updater selects the archive for the target compiled into the invoking executable, including the GNU or musl ABI on Linux.

Before replacement, the updater verifies the installed binary digest, receipt product, executable, aliases, version, layout and prefix. It stages the selected archive, checks its SHA-256 sidecar and runs the staged binary to verify product, version, target and source revision. The service also checks its profile and features. A release missing the distribution plan or the executable identity fails before replacement.

Homebrew and npm installations delegate to the package manager. A Cargo installation without a release receipt delegates to Cargo. A manual archive installation requires a manual update. Reduced service profiles (`gateway`, `gateway-shared` and custom builds) report a source-install command with the exact effective features. For a manual archive replacement, verify the archive checksum and the new executable’s build information. Use the matching gateway or gateway-shared distribution and preserve the current service’s data and capabilities.

After `bitrouter update` installs the replacement, run `bitrouter restart`. Restart closes admission and drains in-flight requests, including streams, before starting the new process. Use the owning service manager for a foreground or shared gateway. Check `bitrouter status` to verify the running version. Start a new BRO invocation after `bro update`.

`bitrouter status` can show a cached release nudge for the full service profile. Set `BITROUTER_NO_UPDATE_CHECK=1` to disable it.
