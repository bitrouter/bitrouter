#!/usr/bin/env bash
# Historical obligations need a genuine old runtime producer and a current reader.
set -euo pipefail
producer_revision=79d46f4eb78533e7f8179b976c5def07da18bd67
producer_root="$(git rev-parse --show-toplevel)"
producer_archive_target="${CARGO_TARGET_DIR:-$producer_root/target}"
[[ "$producer_archive_target" = /* ]]
# Equal-version workspace crates can reuse artifact names across checkouts.
# Keep the historical dependency graph out of the current reader's build cache.
producer_target="$producer_archive_target/pre-m0/$producer_revision"
producer_source="$(mktemp -d "${TMPDIR:-/tmp}/bitrouter-pre-m0.XXXXXXXX")"
if ! git cat-file -e "$producer_revision^{commit}" 2>/dev/null; then
  git fetch --no-tags origin "$producer_revision"
fi
git worktree add --detach "$producer_source" "$producer_revision"
cleanup_producer_source() {
  git -C "$producer_root" worktree remove --force "$producer_source"
}
trap cleanup_producer_source EXIT
# The adapter exports genuine terminal history and deactivates the old policy
# through its management API. No predecessor runtime source is changed.
git -C "$producer_source" apply "$producer_root/.github/fixtures/pre-m0-terminal-export.patch"
[[ "$(git -C "$producer_source" diff --name-only)" = apps/bitrouter/src/tenancy/tests/execution/native/session/experiment/paid/revision_budget.rs ]]
BITROUTER_BUILD_REVISION="$producer_revision" CARGO_TARGET_DIR="$producer_target" \
  cargo test --manifest-path "$producer_source/Cargo.toml" --locked -p bitrouter \
    --lib --all-features --no-run --message-format=json,json-render-diagnostics \
    > "$producer_source/artifacts.jsonl"
producer_binary="$(jq -sr '
  [.[] | select(.reason == "compiler-artifact"
    and .target.name == "bitrouter" and .target.kind == ["lib"]
    and .profile.test == true and .executable != null)]
  | if length == 1 then .[0].executable else error("expected one predecessor libtest") end
' "$producer_source/artifacts.jsonl")"
[[ "$producer_binary" = /* && -x "$producer_binary" ]]
mkdir -p "$producer_archive_target/debug/examples"
cp "$producer_binary" "$producer_archive_target/debug/examples/pre_m0_policy_producer"
chmod 755 "$producer_archive_target/debug/examples/pre_m0_policy_producer"
printf '%s\n' "$producer_revision" > "$producer_archive_target/debug/examples/pre_m0_policy_producer.revision"
