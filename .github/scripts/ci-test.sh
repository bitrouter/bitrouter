#!/usr/bin/env bash
# Run a suite from the verified download, reusing one extracted tree at a time.
set -euo pipefail

run_database() {
  local backend="${1:?database backend required}" url
  case "$backend" in
    postgres) url='postgres://fixture:fixture-password@127.0.0.1:5432/bitrouter_test' ;;
    mysql) url='mysql://fixture:fixture-password@127.0.0.1:3306/bitrouter_test' ;;
    *) echo "Unknown database backend: $backend" >&2; return 1 ;;
  esac
  BITROUTER_TEST_MIGRATION_DATABASE_URL="$url" \
    db_test "kind(lib) & test(/^db::migration::baseline::server_tests::${backend}_/)"
  export BITROUTER_TEST_DATABASE_URL="$url"
  db_test "kind(lib) & test(/::db_server_(both|${backend})_[^:]+$/)"
}

db_test() {
  bash "$0" service --test-threads 1 --run-ignored only -E "$1"
}

if [[ "${1:-}" == database ]]; then
  run_database "${2:-}"
  exit
fi

suite="${1:?suite required}"
shift
workspace="${GITHUB_WORKSPACE:?workspace required}"
restore=ci-tests
profile=ci
suffix=
if [[ "$RUNNER_OS" == Windows ]]; then suffix=.exe; fi

# Match CARGO_TARGET_DIR in test-archive/build.sh. Test binaries embed this
# absolute path through Cargo's CARGO_BIN_EXE_* variables at compile time.
suite_target() {
  case "$1" in
    service|bro) printf '%s\n' target ;;
    service-minimal|service-default|service-shared|service-grpc) printf 'target/%s\n' "$1" ;;
    *) echo "Unknown CI suite: $1" >&2; return 1 ;;
  esac
}

target="$(suite_target "$suite")"
case "$suite" in
  service-minimal|service-default|service-shared) profile="ci-$suite" ;;
esac
previous=
if [[ -f "$restore/suite" ]]; then
  previous="$(cat "$restore/suite")"
fi
if [[ "$previous" != "$suite" || ! -f "$target/nextest/binaries-metadata.json" ]]; then
  if [[ -n "$previous" ]]; then
    previous_target="$(suite_target "$previous")"
    rm -rf -- "$previous_target"
  fi
  rm -rf -- "$restore/target"
  mkdir -p "$restore"
  cargo-nextest nextest list --list-type binaries-only \
    --archive-file "ci-artifacts/$suite.tar.zst" --extract-to "$workspace/$restore" \
    --workspace-remap "$workspace" >/dev/null
  # Nextest can create its store in the workspace target while listing.
  rm -rf -- "$target"
  mkdir -p "$(dirname "$target")"
  mv "$restore/target" "$target"
  printf '%s\n' "$suite" > "$restore/suite"
fi

export BITROUTER_TEST_PREBUILT_ONLY=1
export BITROUTER_TEST_EXAMPLES_DIR="$workspace/$target/debug/examples"
case "$suite" in
  bro)
    export BITROUTER_TEST_BRO="$workspace/$target/debug/bro$suffix"
    test -f "$BITROUTER_TEST_EXAMPLES_DIR/bro_acp_fixture$suffix"
    "$BITROUTER_TEST_BRO" build-info | jq -e --arg sha "$GITHUB_SHA" '
      .product == "bitrouter-bro" and .source_revision == $sha'
    ;;
  service-grpc) ;; # This suite builds and runs library tests only.
  *)
    export BITROUTER_TEST_GATEWAY_BINARY="$workspace/$target/debug/bitrouter$suffix"
    "$BITROUTER_TEST_GATEWAY_BINARY" build-info | jq -e --arg sha "$GITHUB_SHA" '
      .product == "bitrouter" and .source_revision == $sha'
    ;;
esac

report="target/nextest/$profile/junit.xml"
rm -f -- "$report"
status=0
cargo-nextest nextest run \
  --cargo-metadata "$workspace/$target/nextest/cargo-metadata.json" \
  --binaries-metadata "$workspace/$target/nextest/binaries-metadata.json" \
  --workspace-remap "$workspace" --target-dir-remap "$workspace/$target" \
  --profile "$profile" --retries 0 --no-fail-fast --no-tests fail "$@" || status=$?
if [[ -f "$report" ]]; then
  mkdir -p ci-test-results
  report_directory="$(mktemp -d "ci-test-results/$suite.XXXXXX")"
  cp "$report" "$report_directory/junit.xml"
fi
exit "$status"
