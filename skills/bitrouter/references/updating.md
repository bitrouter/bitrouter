# Updating BitRouter and BRO

Use `bitrouter update` for the service and `bro update` for the local harness. The managed shell installers write independent receipts for the two executables. On a joint release the BRO PowerShell installer installs both products with separate receipts. Use `-BroOnly` to install just BRO. Check `bitrouter --json build-info` or `bro --json build-info` for the product, version, target and source revision.

- `--check` resolves the release inventory and reports availability.
- The default channel includes prereleases. Use `--stable` to select stable releases.
- `--tag TAG` selects the exact tag, for that product. A package-scoped tag applies to that package. Use `-y` to skip confirmation.
- The updater selects the archive for the target compiled into the invoking executable. Current releases support Apple Silicon macOS, ARM64 and x86-64 GNU/Linux, and x86-64 Windows.

Before replacement, the updater verifies the installed binary digest, receipt product, executable, aliases, version, layout and prefix. It stages the selected archive, compares its SHA-256 with the digest in that tag’s `distribution-manifest.json`, and runs the staged binary to verify product, version, target and source revision. The service also checks its profile and features. Older published releases can supply the digest through their legacy plan and sidecar. If an older installed updater requires a sidecar for a current release, run the matching installer once at the existing prefix.

Homebrew and npm installations delegate to the package manager. A Cargo installation without a release receipt delegates to Cargo. A manual archive installation requires a manual update. Reduced service profiles (`gateway`, `gateway-shared` and custom builds) report a source-install command with the exact effective features. For a manual archive replacement, verify the archive checksum and the new executable’s build information. Use the matching gateway or gateway-shared distribution and preserve the current service’s data and capabilities.

If your global npm prefix already has the standalone `bitrouter` package, run `npm uninstall -g bitrouter` before `npm install -g bitrouter-bro`. The current joint package owns both command shims.

After `bitrouter update` installs the replacement, run `bitrouter restart`. Restart closes admission and drains in-flight requests, including streams, before starting the new process. Use the owning service manager for a foreground or shared gateway. Check `bitrouter status` to verify the running version. Start a new BRO invocation after `bro update`.

`bitrouter status` can show a cached release nudge for the full service profile. Set `BITROUTER_NO_UPDATE_CHECK=1` to disable it.

For installation and runtime checks, see [Diagnosing BitRouter](diagnose.md).
