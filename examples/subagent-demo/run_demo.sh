#!/usr/bin/env bash
# Agent-driven subagent budgets — end-to-end demo.
#
# The top-level opencode agent (routed through the local BitRouter daemon) calls
# the router-owned `spawn_subagent` tool. BitRouter mints a capped brvk_, spawns
# a headless `opencode acp` worker pinned to that key+model, meters it, and fails
# it closed at the budget. The parent receives a structured result it can review.
#
# Prereqs:
#   - `bitrouter` and `opencode` on PATH
#   - bitrouter.demo.yaml `providers:` filled in for the chosen model, WITH pricing
#   - opencode.parent.json `apiKey` set to a parent brvk_ (or a skip_auth key)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"

echo "==> starting the daemon (skip_auth:false, spawn_subagent enabled)"
bitrouter start --config "$HERE/bitrouter.demo.yaml"
sleep 2

echo "==> driving the top-level agent through the local daemon"
echo "    It should call spawn_subagent(model, budget, task) mid-conversation."
OPENCODE_CONFIG="$HERE/opencode.parent.json" \
  opencode run "Delegate implementing a hello() function (writing 'hello from subagent' to the ABSOLUTE path /tmp/demo_out/hello.txt) to a cheap subagent: call spawn_subagent with model 'bitrouter/z-ai/glm-5.1', budget_micro_usd 500000, and that task. Then summarize the structured result it returns (final_message, files_touched, spend_micro_usd, capped)."

echo
echo "==> worker spend (the subagent's brvk_ should show spend, capped at its budget)"
bitrouter cloud usage

echo
echo "Finale: re-run with a tiny budget (e.g. budget_micro_usd 1) to see the worker"
echo "FAIL CLOSED. The cap is checked pre-request against SETTLED spend, so the first"
echo "inference runs; once its spend settles past the cap, the NEXT call is denied"
echo "(Forbidden) and the result shows capped:true. If spend stays 0, the upstream"
echo "isn't reporting streaming usage — switch models (spec §8)."
