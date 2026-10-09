# Updating BitRouter

`bro update` updates the installed binary in place using cargo-dist's
self-update path.

- Default follows **prereleases** (the project ships `1.0.0-alpha.*`); pass
  `--stable` for stable-only once 1.0 exists.
- `--check` reports whether a newer version exists and exits without changing
  anything.
- `--tag <VERSION>` pins to a specific release (also downgrades/rolls back),
  e.g. `bro update --tag 1.0.0-alpha.18`. Named `--tag`, not `--version`,
  because `--version` prints the binary version.
- `-y`/`--yes` skips the confirmation prompt.
- `--restart` is a hidden compatibility spelling; a safe handoff is already
  attempted by default.
- **Homebrew / `cargo install`** installs are not self-updated — the command
  prints the right upgrade command (`brew upgrade bitrouter` /
  `cargo install bro --force`) instead of clobbering a managed binary.
- After a successful self-managed update, an idle daemon launched by
  `bro start` can hand off to the new binary. The daemon must support the
  handoff protocol and pass its SQLite migration preflight. A busy, legacy,
  incompatible, or externally supervised daemon stays running; the update
  reports `daemon: "deferred"` and exits non-zero. Finish active work, then
  use `bro restart` or the owning service manager as appropriate. `--check`
  never restarts a daemon.

`bro status` shows a one-line "↑ <version> available" nudge when a newer
release exists (checked at most once per day). Disable it with
`BITROUTER_NO_UPDATE_CHECK=1`.
