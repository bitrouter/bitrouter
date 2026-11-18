#!/usr/bin/env bash
# https://nexte.st/docs/ci-features/archiving/#manually-creating-your-own-archives
set -euo pipefail
label="${1:?test evidence label required}"
shift
[[ "$label" =~ ^[a-zA-Z0-9_-]+$ ]]
mkdir -p ci-evidence
# A step may invoke several exact parents. Preserve every invocation's evidence.
label="$(basename "$(mktemp -d "ci-evidence/$label-XXXXXX")")"
metadata=(--cargo-metadata "${CI_CARGO_METADATA:-$PWD/target/nextest/cargo-metadata.json}"
  --binaries-metadata "${CI_BINARIES_METADATA:-$PWD/target/nextest/binaries-metadata.json}"
  --workspace-remap "$PWD" --target-dir-remap "$PWD/target")
if [[ ${1:-} == --exact-tests ]]; then
  owner=${2:?nextest binary filter required}
  mode=${3:?ordinary or ignored required}
  shift 3
  (( $# > 0 ))
  expected=$(jq -cn --args '$ARGS.positional' "$@")
  case "$mode" in
    ordinary) flags=(--run-ignored default); ignored=false ;;
    ignored) flags=(--run-ignored only); ignored=true ;;
    *) exit 2 ;;
  esac
  union=''
  for name do
    [[ "$name" =~ ^[a-zA-Z0-9_:]+$ ]]
    union+="${union:+ | }test(=$name)"
  done
  filter="($owner) & ($union)"
  selection="ci-evidence/$label/selection.json"
  cargo-nextest nextest list "${metadata[@]}" --profile ci "${flags[@]}" \
    -E "$filter" --message-format json > "$selection"
  jq -e --argjson expected "$expected" --argjson ignored "$ignored" '
    [."rust-suites"[].testcases | to_entries[] | select(.value."filter-match".status == "matches")] as $found
    | ([$found[].key] | sort) == ($expected | sort) and all($found[]; .value.ignored == $ignored)
  ' "$selection" || { echo "::error::Missing or misclassified $mode tests: $expected"; exit 1; }
  set -- --test-threads 1 "${flags[@]}" -E "$filter"
fi
rm -f target/nextest/ci/junit.xml
status=0
timing=-v
if [[ $(uname -s) == Darwin ]]; then timing=-l; fi
/usr/bin/time "$timing" -o "ci-evidence/$label-resources.txt" \
  cargo-nextest nextest run \
  "${metadata[@]}" \
  --profile ci --retries 0 --no-fail-fast --no-tests fail "$@" || status=$?
if [[ -f target/nextest/ci/junit.xml ]]; then
  cp target/nextest/ci/junit.xml "ci-evidence/$label-junit.xml"
fi
if (( status != 0 )); then
  # Process fixtures retain final child logs outside the checkout on failure.
  # Keep that evidence with JUnit before the hosted runner is discarded.
  shopt -s nullglob
  logs=("${TMPDIR:-/tmp}"/bitrouter-process-cleanup-failure-*.log)
  if (( ${#logs[@]} > 0 )); then
    cp "${logs[@]}" "ci-evidence/$label/"
  fi
fi
du -sh target > "ci-evidence/$label-disk.txt"
exit "$status"
