#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

cat > "$TMP_DIR/traces.jsonl" <<'JSONL'
{"id":"trace-001","harness":"hermes","protocol":"chat_completions","method":"POST","path":"/v1/chat/completions","headers":{"x-bitrouter-workflow-session":"session-a","x-bitrouter-cloud-request-id":"cloud-req-001"},"raw_body":{"model":"openai/bitrouter-hermes-tbench","messages":[{"role":"user","content":"reply ok"}],"tools":[]},"outcome":{"http_status":200,"status":"completed"}}
JSONL

cat > "$TMP_DIR/cloud-usage.jsonl" <<'JSONL'
{"id":"usage-row-1","request_id":"cloud-req-001","provider_id":"bitrouter","model_id":"deepseek-v4-flash","prompt_tokens":100,"completion_tokens":10,"final_charge_micro_usd":42,"status":"succeeded"}
JSONL

cat > "$TMP_DIR/benchmark-outcomes.jsonl" <<'JSONL'
{"session_key":"session-a","task_id":"filter-js-from-html","reward":0.0,"failed_reason":"verifier_failed","finished_at":"2026-07-08T00:00:00Z"}
JSONL

cargo run --manifest-path "$ROOT/Cargo.toml" -p bitrouter -- workflow-state bundle \
  --run-label smoke \
  --traces "$TMP_DIR/traces.jsonl" \
  --cloud-usage "$TMP_DIR/cloud-usage.jsonl" \
  --outcomes "$TMP_DIR/benchmark-outcomes.jsonl" \
  --output-dir "$TMP_DIR/out"

test -f "$TMP_DIR/out/traces.jsonl"
test -f "$TMP_DIR/out/cloud-usage.jsonl"
test -f "$TMP_DIR/out/benchmark-outcomes.jsonl"
test -f "$TMP_DIR/out/run-artifact.json"
test -f "$TMP_DIR/out/shadow-policy.json"

jq -e '.reward_join.matched_trace_count == 1' "$TMP_DIR/out/run-artifact.json" >/dev/null
jq -e '.semantic_inadequacy_candidates[0].task_id == "filter-js-from-html"' "$TMP_DIR/out/run-artifact.json" >/dev/null
jq -e '.total == 1' "$TMP_DIR/out/shadow-policy.json" >/dev/null
