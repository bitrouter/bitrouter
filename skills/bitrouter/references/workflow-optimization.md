# Agent workflow optimization

Use this flow when the user wants to reduce an agent workflow's model cost
while retaining a user-defined quality outcome. The online router never sees
task answers or benchmark labels. It sees ordinary request traces; the offline
optimizer joins those decisions to the workflow's observable eval result and
authoritative Cloud settlement.

## Onboard

Interactive:

```bash
bitrouter init
```

Choose workflow optimization, provide the exact workflow argv and an
observable success contract, then choose a qualitative preference. Do not
promise fixed quality-loss or latency percentages: latency is observe-only in
this release and the user's eval data determines the actual trade-off.

Headless:

```bash
bitrouter init --yes --write-config --use-detected --harness codex \
  --after exit --optimize \
  --optimize-workflow-command ./run-agent-eval \
  --optimize-workflow-arg --suite \
  --optimize-workflow-arg smoke.jsonl \
  --optimize-success 'The eval command exits successfully and reports its required checks.'
```

This creates three user-owned, version-controlled artifacts without
overwriting existing files:

- `bitrouter.optimize.yaml`: workflow, route ladder, evaluator, and preference;
- `bitrouter.optimize.lock.yaml`: exact adapter/model/digests and latest run;
- `bitrouter.eval.md`: observable success contract.

The default evaluator is BitRouter's embedded generic agentic eval protocol,
run in a fresh ACP session. BitRouter prefers the detected local agent's own
subscription and pins its adapter/model. Pass `--evaluator-via-cloud` to
`bitrouter optimize setup` only when the user explicitly wants Cloud judging.
ORI is not required.

## Evolve one route at a time

```bash
bitrouter optimize run
bitrouter optimize review
bitrouter optimize publish
```

`run` launches the same exact argv once for the active baseline and once for a
candidate that changes one observed route key. It does not retry. The workflow
command may itself run a multi-case eval suite; its output is judged against
the success contract. BitRouter—not the judge—provides settled cost, latency,
policy lineage, and request attribution.

`review` reports baseline/candidate quality, settled cost delta, observed
latency, the route-key change, and content digests. `run` never changes the
active policy. `publish` is a separate explicit action and rejects stale or
mismatched lineage. Run the loop again after publication to optimize another
eligible route key.

Profiles:

- `quality-first`: no observed quality regression;
- `balanced`: prioritize frequently used route keys and require manual review;
- `savings-first`: prioritize greatest settled cost and require manual review;
- `custom`: explicit pass-rate and quality-loss PPM gates in source control.

## Failure interpretation

- Non-zero workflow exit: valid quality evidence for the evaluator.
- Timeout: terminal run; never publishable.
- No request/decision/settlement join: infrastructure ambiguity; fail closed.
- Candidate did not exercise the changed route: experiment invalid; fail
  closed.
- Workflow argv or referenced file changed between variants: source drift; fail
  closed.
- `publishable: false`: inspect `review`; do not force publication.

Raw workflow output and model replies remain under the BitRouter home. Commit
only the intent, lock, success contract, and policy lock. Never put API keys in
commands, configs, reports, or repository files.
