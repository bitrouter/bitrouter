#!/usr/bin/env bash
# https://nexte.st/docs/ci-features/archiving/#manually-creating-your-own-archives
set -euo pipefail
label="${1:?test evidence label required}"
shift
[[ "$label" =~ ^[a-zA-Z0-9_-]+$ ]]
mkdir -p ci-evidence
# A step may invoke several exact parents. Preserve every invocation's evidence.
label="$(basename "$(mktemp -d "ci-evidence/$label-XXXXXX")")"
rm -f target/nextest/ci/junit.xml
status=0
/usr/bin/time -v -o "ci-evidence/$label-resources.txt" \
  cargo-nextest nextest run \
  --cargo-metadata "$PWD/target/nextest/cargo-metadata.json" \
  --binaries-metadata "$PWD/target/nextest/binaries-metadata.json" \
  --workspace-remap "$PWD" --target-dir-remap "$PWD/target" \
  --profile ci --retries 0 --no-tests fail "$@" || status=$?
if [[ -f target/nextest/ci/junit.xml ]]; then
  cp target/nextest/ci/junit.xml "ci-evidence/$label-junit.xml"
fi
du -sh target > "ci-evidence/$label-disk.txt"
exit "$status"
