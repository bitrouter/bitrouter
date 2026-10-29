# Updating BitRouter

`bro update` preserves the installed distribution profile. Full-profile
installations can update in place using cargo-dist's self-update path when
the install receipt belongs to the running executable. A missing, unreadable,
or unrelated receipt delegates to a manual update; `--tag` never bypasses
receipt ownership checks.

The canonical executable must also be named `bro` (`bro.exe` on Windows).
Symlink aliases resolving to that binary work; renamed copies delegate to a
manual path even when they share its installation directory.

- Default follows **prereleases** (the project ships `1.0.0-alpha.*`); pass
  `--stable` for stable-only once 1.0 exists.
- `--check` reports whether a newer version exists on a supported self-update
  channel, or delegates to the appropriate manual path, without changing anything.
- `--tag <VERSION>` pins to a specific release (also downgrades/rolls back),
  e.g. `bro update --tag 1.0.0-alpha.18`. Named `--tag`, not `--version`,
  because `--version` prints the binary version.
- `-y`/`--yes` skips the confirmation prompt.
- `--restart` is a hidden compatibility spelling; a safe handoff is already
  attempted by default.
- **Full-profile Homebrew / `cargo install` / npm** installations delegate to
  `brew upgrade bitrouter`, `cargo install bitrouter --force`, or
  `npm i -g bitrouter@latest`.
- **Gateway / gateway-shared / custom profiles** currently have no matching release
  installer/receipt channel. Every install method, including `--check`, returns
  `status: "delegated"` with a `manual_reason` and a source-install command:
  `cargo install bitrouter --force --no-default-features`, followed by
  `--features <sorted-effective-features>` when the feature set is nonempty.
  Use the exact command printed for that binary; a generic full installer or
  package-manager upgrade would change its capabilities. An empty feature set
  is the gateway profile; it keeps `--no-default-features` and omits `--features`.
  The `gateway-shared` profile retains exactly `db-postgres,multi-instance`;
  its source command includes both normalized feature names. Enabling only
  `core` is a custom profile. Cargo installs into its configured bin directory, so select that binary explicitly if the current
  installation is elsewhere.
- A gateway release archive is also a manual update path: download the archive
  and checksum for the same release and target, verify and extract it, then
  inspect `bro build-info` before switching the service executable. Require
  `profile: "gateway"` and an empty feature list. `bro update` does not download
  these archives automatically. Keep active-work and explicit restart checks;
  never substitute a full-product archive for a missing gateway asset.
- Shared gateway archives are named `bitrouter-gateway-shared-<target>.tar.gz`
  (`.zip` on Windows), with a matching `.sha256` file. Require
  `profile: "gateway-shared"` and exactly `["db-postgres", "multi-instance"]`
  in both `manifest.json` and `bro --json build-info`. Use the owning service
  manager for shared rollout/drain; this profile does not gain a local-daemon
  handoff or a release-installer receipt. `bro update` keeps delegating to the
  exact-feature source command rather than installing the full distribution.
- For these source installs, a semver `--tag` (optionally prefixed with `v`)
  becomes an exact Cargo `--version` requirement. Other tags select the official
  Git repository with a shell-quoted `--tag`. The manual reason names the
  required shell: a POSIX-compatible shell on Unix, or **PowerShell 7.3 or newer**
  on Windows. Windows commands use literal PowerShell quoting and a local
  `Standard` native-argument-passing setting to preserve embedded quotes without
  changing the caller's setting. Without a tag (including with `--stable`),
  Cargo's registry version selection applies; the release installer's prerelease
  channel is unavailable for gateway/gateway-shared/custom profiles.
- JSON results include `current_artifact`, the identity of the invoking
  executable. `target_artifact`, when present, describes the independently
  verified installed executable. `target_version` retains its existing meaning
  as the available or installed version; a manual result does not claim that
  an update is available or completed.
- A legacy rollback can complete installation even when the older executable
  lacks `build-info`. An unreadable, malformed, or mismatched artifact identity
  keeps `status: "updated"` and the installer's `target_version`, omits
  `target_artifact`, and reports `target_verification_error` in JSON and human
  output. Automatic handoff is disabled. If a daemon was awaiting handoff, it
  stays running with `daemon: "deferred"`, an explanatory `daemon_reason`, and
  a non-zero exit code. With no resident daemon, daemon fields remain absent
  and the completed installation retains exit code zero despite the explicit
  verification diagnostic.
- After a successful self-managed update, an idle daemon launched by
  `bro start` can hand off to the new binary. The daemon must support the
  handoff protocol and pass its SQLite migration preflight. A busy, legacy,
  incompatible, or externally supervised daemon stays running; the update
  reports `daemon: "deferred"` and exits non-zero. Finish active work, then
  use `bro restart` or the owning service manager as appropriate. `--check`
  never restarts a daemon.
- Every resident built with `core`, including a core-only service with an
  external harness, conservatively defers automatic and cross-profile handoff
  for its managed runtime's lifetime. HTTP idleness does not prove that channel
  work or checkpoint acknowledgements have finished. Finish managed work and
  use same-capability explicit restart or the owning service manager; gateway
  and harness-only builds retain their tracked idle-handoff checks.

`bro status` shows a one-line "↑ <version> available" nudge for a supported
full-profile release channel (checked at most once per day). Cached checks
must match the profile, target and effective features. Legacy caches without
artifact identity are treated as unknown, and gateway/gateway-shared/custom installations
never reuse or advertise full-profile release checks. Disable the nudge with
`BITROUTER_NO_UPDATE_CHECK=1`.
