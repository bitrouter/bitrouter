#!/bin/sh
set -eu
review_logs=/Users/kelsen/.codex/archives/bro-refactor/2026-10-16-review-fixes
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0
export CARGO_INCREMENTAL=0
cargo clippy --workspace --all-targets --all-features -- -D warnings > "$review_logs/clippy.log" 2>&1
cargo fmt -- --check > "$review_logs/fmt-check.log" 2>&1
cargo test --workspace --all-features --doc > "$review_logs/doctests.log" 2>&1
cargo run -p dist-helper -- check > "$review_logs/dist-check.log" 2>&1
cargo check -p bitrouter-sdk --no-default-features > "$review_logs/sdk-default-check.log" 2>&1
cargo check -p bitrouter-orchestrator --no-default-features > "$review_logs/orchestrator-native-check.log" 2>&1
git diff --check > "$review_logs/diff-check.log" 2>&1
