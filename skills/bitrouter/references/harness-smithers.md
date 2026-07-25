# Smithers reward-supervised routing

Smithers supplies the workflow execution and terminal eval reward. BitRouter
supplies request routing, metering, adequacy learning, and deterministic policy
materialization. The outer supervisor should isolate the router config, policy
lock, database, traces, and decisions for every optimization run.

## Request identity

An in-process Smithers agent must attach the following headers from Smithers'
stable `taskContext`; do not derive identity from prompt text or array position.

```text
x-bitrouter-harness: smithers
x-bitrouter-protocol: chat
x-bitrouter-workflow-session: <Smithers runId>
x-bitrouter-benchmark-run-id: <optimizer run id>
x-bitrouter-trial-id: <Smithers runId>
x-bitrouter-parent-session-id: <Smithers runId>
x-bitrouter-agent-session-id: <runId>:<nodeId>:<iteration>:<attempt>
x-smithers-workflow-id: <stable workflow id>
x-smithers-node-id: <stable node id>
```

This yields a `smithers|...|<workflow id>|<node id>|...` workflow-state key.
Blank workflow or node headers are ignored.

## One-variable training

During a training episode only, add:

```text
x-bitrouter-exploration-target: <exact request key>
```

The target cannot turn exploration on. It only permits an already-eligible
trial or learned exploration lock when the request's derived key matches
exactly. Finalization, tool-safety, adequacy-pin, and reliability guards still
win. Sending the same target on every request in one Smithers run therefore
allows only one routing variable to change.

## Terminal reward

Convert every Smithers eval result to newline-delimited JSON:

```json
{"session_key":"<runId>","task_id":"<caseId>","reward":1.0}
```

Use `1.0` when the case passed and `0.0` when it failed. Export provider-reported
usage, then apply the strict trace/decision/usage/outcome join:

```bash
bitrouter workflow-state metering-usage \
  --database-url sqlite://./bitrouter.db \
  --output artifacts/cloud-usage.jsonl

bitrouter workflow-state apply-reward-feedback \
  --database-url sqlite://./bitrouter.db \
  --traces artifacts/traces.jsonl \
  --cloud-usage artifacts/cloud-usage.jsonl \
  --outcomes artifacts/outcomes.jsonl \
  --policy-decisions artifacts/policy-decisions.jsonl
```

Reject an experiment when any trace, usage row, decision, or outcome is
unmatched. Positive terminal reward adds semantic-success evidence only to the
decision's named-policy ledger key. Negative reward pins that key and clears its
positive lock state.

## Frozen candidate

Materialize a deployment/holdout artifact without unlocking or changing the
active policy:

```bash
bitrouter policy evolve --config bitrouter.yaml \
  --output artifacts/policy-lock.candidate.yaml --freeze
```

`--freeze` materializes qualified routes, then disables both adequacy learning
and exploration. Routing-only guards such as the process-local per-session
downgrade budget remain active, so holdout cannot mutate the evidence ledger.

Run holdout against a fresh router using the exported lock. Export the same
unchanged evidence twice and compare both file bytes and semantic digests before
accepting the candidate.
