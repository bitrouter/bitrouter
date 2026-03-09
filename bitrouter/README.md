# bitrouter

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

Command-line entry point for BitRouter.

This crate builds the `bitrouter` binary and exposes the top-level operational
commands used to run or control the service. It wires CLI parsing to the
runtime crate and keeps the executable layer intentionally thin.

## Commands

- `serve` to run the HTTP server in the foreground
- `start`, `stop`, and `restart` to manage the daemon
- `status` to print current runtime information
