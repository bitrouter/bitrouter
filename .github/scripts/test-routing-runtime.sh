#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."

# Extra arguments select a Cargo profile/configuration for local runs.
# Select executables from this Cargo invocation, never a target directory scan.
routing_mode=${1?expected a feature list or heterogeneous}
shift
routing_cargo=(--locked -p bitrouter --no-default-features "$@")
routing_work=$(mktemp -d "${TMPDIR:-/tmp}/bitrouter-routing.XXXXXX")
trap 'rm -rf "$routing_work"' EXIT

build_fixture() {
  cargo build "${routing_cargo[@]}" --features "$1" \
    --example routing_topology_fixture --message-format=json-render-diagnostics \
    > "$routing_work/build.json"
  routing_binary=$(jq -ers '
    [.[] | select(.reason == "compiler-artifact" and
      .target.name == "routing_topology_fixture" and .executable != null) | .executable]
    | if length == 1 then .[0] else error("expected exactly one topology fixture") end
  ' "$routing_work/build.json")
  test -f "$routing_binary"
}

if [[ "$routing_mode" == heterogeneous ]]; then
  build_fixture semantic-routing
  routing_user="$routing_work/user-$(basename "$routing_binary")"
  cp "$routing_binary" "$routing_user"
  build_fixture inference-routing
  routing_serving="$routing_work/serving-$(basename "$routing_binary")"
  cp "$routing_binary" "$routing_serving"
  export BITROUTER_TEST_SEMANTIC_GATEWAY_BINARY="$routing_user"
  export BITROUTER_TEST_INFERENCE_GATEWAY_BINARY="$routing_serving"
  routing_selector=processes::heterogeneous_gateways_keep_model_constraints_deadline_and_retry_ownership
  cargo test "${routing_cargo[@]}" --features semantic-routing --test routing_composition \
    "$routing_selector" -- --ignored --exact --list > "$routing_work/selection.txt"
  cat "$routing_work/selection.txt"
  # libtest permits zero matches; require precisely this campaign before running.
  awk -v expected="$routing_selector: test" '
    /: test$/ { total++; if ($0 == expected) matched++ }
    END { exit !(total == 1 && matched == 1) }
  ' "$routing_work/selection.txt"
  cargo test "${routing_cargo[@]}" --features semantic-routing --test routing_composition \
    "$routing_selector" -- --ignored --exact --nocapture
else
  case "$routing_mode" in
    ''|semantic-routing|inference-routing|semantic-routing,inference-routing) ;;
    *) echo "unsupported routing composition: $routing_mode" >&2; exit 2 ;;
  esac
  build_fixture "$routing_mode"
  export BITROUTER_TEST_EXAMPLES_DIR
  BITROUTER_TEST_EXAMPLES_DIR=$(dirname "$routing_binary")
  export BITROUTER_TEST_PREBUILT_ONLY=1
  cargo test "${routing_cargo[@]}" --features "$routing_mode" \
    --test routing_composition --test artifact_build_metadata -- --test-threads=2
  cargo test "${routing_cargo[@]}" --features "$routing_mode" --test e2e \
    named_router_fallback_keeps_one_settled_identity -- --exact --test-threads=2
fi
