# Updating BitRouter

`bitrouter update` updates the installed binary in place using cargo-dist's
self-update path.

- Default follows **prereleases** (the project ships `1.0.0-alpha.*`); pass
  `--stable` for stable-only once 1.0 exists.
- `--check` reports whether a newer version exists and exits without changing
  anything.
- `--tag <VERSION>` pins to a specific release (also downgrades/rolls back),
  e.g. `bitrouter update --tag 1.0.0-alpha.18`. Named `--tag`, not `--version`,
  because `--version` prints the binary version.
- `-y`/`--yes` skips the confirmation prompt.
- `--restart` restarts a running daemon after a successful update.
- **Homebrew / `cargo install`** installs are not self-updated — the command
  prints the right upgrade command (`brew upgrade bitrouter` /
  `cargo install bitrouter --force`) instead of clobbering a managed binary.
- After a successful update, if a daemon is running, the new binary is only
  served after a restart. The command prompts for `bitrouter restart`, or pass
  `--restart` to do it automatically.

`bitrouter status` shows a one-line "↑ <version> available" nudge when a newer
release exists (checked at most once per day). Disable it with
`BITROUTER_NO_UPDATE_CHECK=1`.
