#!/usr/bin/env bash
set -euo pipefail

suite="${1:?suite required}"
feature_flags=(--locked --all-features)
archive_flags=(--profile ci-service)
build_targets=(--bin bitrouter --examples)
package=bitrouter

case "$suite" in
  service)
    case "$RUNNER_OS" in
      macOS) archive_flags+=(--lib --test mcp_config_pty); build_targets=(--example onboarding_fixture) ;;
      Windows) archive_flags+=(--lib --test invocation); build_targets=() ;;
    esac
    ;;
  service-minimal) feature_flags=(--locked --no-default-features) ;;
  service-default) feature_flags=(--locked) ;;
  service-shared) feature_flags=(--locked --no-default-features --features multi-instance,http2) ;;
  service-grpc)
    feature_flags=(--locked --no-default-features --features otel-grpc)
    archive_flags=(--profile ci --lib)
    build_targets=()
    ;;
  bro)
    package=bitrouter-bro
    archive_flags=(--profile ci-bro)
    build_targets=(--example bro_acp_fixture)
    ;;
  bro-companion)
    package=bitrouter-bro
    build_targets=(--bin bro)
    ;;
  *) echo "Unknown CI suite: $suite" >&2; exit 1 ;;
esac

# Match the target paths used when restoring the independent feature archives.
case "$suite" in
  service|bro|bro-companion) export CARGO_TARGET_DIR=target ;;
  *) export CARGO_TARGET_DIR="target/$suite" ;;
esac
# Cargo also builds the executable for the selected integration tests.
# Only build it separately where the Linux gateway suite verifies its production closure.
if [[ -n "${build_targets[*]:-}" ]]; then
  cargo build -p "$package" "${feature_flags[@]}" "${build_targets[@]}"
fi

if [[ "$suite" == bro-companion ]]; then
  mkdir -p ci-companion
  cp target/debug/bro ci-companion/bro
  (cd ci-companion && sha256sum bro > checksums.sha256)
  exit
fi

mkdir -p ci-artifacts
cargo nextest archive -p "$package" "${feature_flags[@]}" "${archive_flags[@]}" \
  --archive-file "ci-artifacts/$suite.tar.zst"
