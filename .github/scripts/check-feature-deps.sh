#!/usr/bin/env bash
# Enforce the BitRouter feature rule: every Cargo feature must produce a
# measurable change in the dependency tree. A feature whose `cargo tree`
# is identical to the baseline is "noise" — it should be removed or rolled
# into another feature.
#
# Usage: .github/scripts/check-feature-deps.sh
#
# For each (crate, feature) pair listed below, the script:
#   1. Captures `cargo tree -p <crate> --no-default-features --edges normal`
#      with the feature's *baseline* enabled.
#   2. Captures the same with the feature additionally enabled.
#   3. Diffs the two outputs and fails if the diff is empty.
#
# Allow-list (skipped):
#   - Database driver selectors that are mutually exclusive (sqlite/postgres/
#     mysql) — each driver replaces the previous, but adding a second driver
#     does add deps so they are tested normally.
#   - Chain selectors (tempo/solana) — same situation; both add real deps.

set -euo pipefail

# Each entry: "crate|baseline_features|feature_to_test"
# An empty `baseline_features` means `--no-default-features` only.
CHECKS=(
    # bitrouter binary — every feature stacked on top of `tempo`
    # (the binary requires at least one chain backend; `tempo` is the cheapest).
    "bitrouter|tempo|cli"
    "bitrouter|tempo|tui"
    "bitrouter|tempo|sqlite"
    "bitrouter|tempo|postgres"
    "bitrouter|tempo|mysql"
    "bitrouter|tempo|mcp"
    # `bitrouter:rest` is intentionally a source-only toggle (delegates to
    # `bitrouter-providers/rest`, which is also source-only). Skipped.
    "bitrouter|tempo|solana"

    # bitrouter-api — features over an empty baseline.
    "bitrouter-api||accounts"
    "bitrouter-api||observe"
    "bitrouter-api||guardrails"
    "bitrouter-api||payments-tempo"
    "bitrouter-api||payments-solana"

    # bitrouter-config — `payments-solana` is a source-only toggle that
    # gates additional config struct definitions; it pulls no extra deps
    # by design and is therefore not checked here.

    # bitrouter-accounts — driver selectors.
    "bitrouter-accounts||sqlite"
    "bitrouter-accounts||postgres"
    "bitrouter-accounts||mysql"

    # bitrouter-observe — driver selectors.
    "bitrouter-observe||sqlite"
    "bitrouter-observe||postgres"
    "bitrouter-observe||mysql"
)

failures=0

cargo_tree() {
    local crate="$1"
    local features="$2"
    local args=(-p "$crate" --no-default-features --edges normal)
    if [ -n "$features" ]; then
        args+=(--features "$features")
    fi
    cargo tree "${args[@]}" 2>/dev/null
}

for entry in "${CHECKS[@]}"; do
    IFS='|' read -r crate baseline feature <<<"$entry"

    if [ -n "$baseline" ]; then
        with="${baseline},${feature}"
    else
        with="$feature"
    fi

    label="${crate}: +${feature}"
    if [ -n "$baseline" ]; then
        label="${label} (baseline: ${baseline})"
    fi

    echo "::group::${label}"

    base_tree="$(cargo_tree "$crate" "$baseline")"
    with_tree="$(cargo_tree "$crate" "$with")"

    if [ "$base_tree" = "$with_tree" ]; then
        echo "::error::feature '${feature}' on '${crate}' produces no dep-tree delta over baseline '${baseline:-<none>}'"
        echo "        every Cargo feature must measurably change the dep tree"
        failures=$((failures + 1))
    else
        added="$(diff <(echo "$base_tree") <(echo "$with_tree") | grep -c '^>' || true)"
        echo "ok: ${added} new tree line(s)"
    fi

    echo "::endgroup::"
done

if [ "$failures" -gt 0 ]; then
    echo
    echo "::error::${failures} feature(s) failed the dep-tree regression check"
    exit 1
fi

echo
echo "All ${#CHECKS[@]} feature checks passed."
