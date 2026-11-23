#!/usr/bin/env bash
set -euo pipefail
product="${1:?product required}"
fixtures=(--examples)
targets=()
profile="ci-$product"
case "$product" in
  service) package=bitrouter; binary=bitrouter ;;
  bro) package=bitrouter-bro; binary=bro ;;
  service-process)
    package=bitrouter
    binary=bitrouter
    fixtures=()
    targets=(--lib --test product_process)
    ;;
  *) exit 1 ;;
esac
cargo build --locked -p "$package" --all-features --bin "$binary" "${fixtures[@]}" --jobs 1
cargo nextest archive --locked -p "$package" --all-features "${targets[@]}" --build-jobs 1 \
  --profile "$profile" --archive-file tests.tar.zst
printf '%s\n' "$GITHUB_SHA" > tests.sha
printf '%s\n' "$GITHUB_WORKSPACE" > tests.workspace
sha256sum tests.tar.zst > tests.tar.zst.sha256
