# bitrouter

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

Command-line entry point for BitRouter.

This crate builds the `bitrouter` binary and exposes the top-level operational
commands used to run or control the service. It wires CLI parsing to the
runtime crate and keeps the executable layer intentionally thin.

## Commands

- `init` to run the interactive setup wizard
- `start` to run the HTTP server in the foreground
- `start -d`, `stop`, and `restart` to manage the background daemon
- `status` to print current runtime information

## First-Run Behavior

When `bitrouter` is launched with no subcommand and no providers are configured,
the setup wizard runs automatically before starting the TUI. This guides new
users through provider selection, API key entry, and configuration file
generation. After setup, the runtime reloads and the TUI launches with the
new configuration.

If the user cancels the wizard, the TUI launches in its empty state.
