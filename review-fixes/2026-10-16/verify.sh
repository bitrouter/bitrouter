#!/bin/sh
set -eu
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0
export CARGO_INCREMENTAL=0
export BRO_GOAL_FIXTURE_KEEP_FAILURES=1
cargo nextest run -p bitrouter -p bitrouter-sdk --lib --all-features --no-fail-fast --test-threads 2 -E 'test(goal_runtime::model_control::tests::) | test(goal_code::tests::) | test(goal_permission_material::) | test(app_routing_decision_requires_) | test(routing_cancellation_settles_)'
cargo nextest run -p bitrouter --lib --test goal_tui_pty --all-features --no-fail-fast --test-threads 2 -E 'test(acp_pre_prompt_recovery_loads_only_the_original_stopped_session) | binary(goal_tui_pty)'
cargo nextest run --workspace --all-features --no-fail-fast --test-threads 2
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt -- --check
cargo test --workspace --all-features --doc
cargo run -p dist-helper -- check
cargo check -p bitrouter-sdk --no-default-features
cargo check -p bitrouter-orchestrator --no-default-features
