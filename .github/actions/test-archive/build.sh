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
    targets=(--lib --test database_backends --test database_capture --test product_process --test mixed_store_migration)
    ;;
  *) exit 1 ;;
esac
mkdir -p ci-evidence
/usr/bin/time -v -o ci-evidence/build-fixtures.txt \
  cargo build --locked -p "$package" --all-features --bin "$binary" "${fixtures[@]}" --jobs 1
/usr/bin/time -v -o ci-evidence/build-tests.txt \
  cargo nextest archive --locked -p "$package" --all-features "${targets[@]}" --build-jobs 1 \
  --profile "$profile" --archive-file tests.tar.zst
cargo nextest list --locked -p "$package" --all-features "${targets[@]}" --message-format json > ci-evidence/test-inventory.json
printf '%s\n' "$GITHUB_SHA" > tests.sha
printf '%s\n' "$GITHUB_WORKSPACE" > tests.workspace
sha256sum tests.tar.zst > tests.tar.zst.sha256
du -sh target tests.tar.zst > ci-evidence/build-size.txt
