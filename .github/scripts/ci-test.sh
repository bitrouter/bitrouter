#!/usr/bin/env bash
# Run the selected suite from the restored nextest archive.
set -euo pipefail
exec cargo-nextest nextest run \
  --cargo-metadata "$PWD/target/nextest/cargo-metadata.json" \
  --binaries-metadata "$PWD/target/nextest/binaries-metadata.json" \
  --workspace-remap "$PWD" --target-dir-remap "$PWD/target" \
  --profile ci --retries 0 --no-fail-fast --no-tests fail "$@"
