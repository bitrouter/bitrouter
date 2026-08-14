# Agent workflow optimization

Use this flow when the user wants to reduce an agent workflow's model cost
while retaining a user-defined quality outcome. The online router never sees
task answers or benchmark labels. It sees ordinary request traces; the offline
optimizer joins those decisions to the workflow's observable eval result and
daemon-authored normalized metering.

## Onboard

Interactive:

```bash
bitrouter init
```

Choose workflow optimization, confirm a discovered workflow (or provide exact
argv), provide an observable success contract, then choose a qualitative
preference. Setup proposes repository-owned eval/benchmark package scripts and
executable entrypoints; it adopts one unambiguous candidate automatically and
offers a numbered choice when several are plausible. Ordinary unit-test
scripts and symlink escapes are not guessed. Do not
promise fixed quality-loss or latency percentages: latency is observe-only in
this release and the user's eval data determines the actual trade-off.

Headless:

```bash
bitrouter init --yes --write-config --use-detected --harness codex \
  --after exit --optimize \
  --optimize-workflow-command ./run-agent-eval \
  --optimize-workflow-arg --suite \
  --optimize-workflow-arg smoke.jsonl \
  --optimize-workflow-input .venv \
  --optimize-strong openai-codex:gpt-5.6-sol \
  --optimize-strong-effort high \
  --optimize-economy openai-codex:gpt-5.6-sol \
  --optimize-economy-effort low \
  --optimize-success 'The eval command exits successfully and reports its required checks.'
```

This creates three user-owned, version-controlled artifacts without
overwriting existing files:

- `bitrouter.optimize.yaml`: workflow, route ladder, evaluator, and preference;
- `bitrouter.optimize.lock.yaml`: exact adapter/model/digests and latest run;
- `bitrouter.eval.md`: observable success contract.

The default evaluator is BitRouter's embedded generic agentic eval protocol,
run in a fresh ACP session. BitRouter prefers the detected local agent's own
subscription and pins its adapter integrity, runtime executable digest, and
model. The installed adapter/runtime is a trusted executor, not an OS sandbox.
Pass `--evaluator-via-cloud` to
`bitrouter optimize setup` only when the user explicitly wants Cloud judging.
ORI is not required.

The default route ladder is `openai-codex:gpt-5.6-sol` (Codex subscription)
to `bitrouter:deepseek/deepseek-v4-flash-0731` (BitRouter Cloud OAuth). Both
routes execute through the private daemon. The intent pins the subscription's
normalized API-equivalent price schedule; this is showback for comparison, not
a claim of marginal cash spend.

Strong/economy targets may also use the same supported model at different
effort levels. Pass `--strong-effort` and `--economy-effort` with one of
`none|minimal|low|medium|high|xhigh|max`; setup validates the exact
provider/model matrix before writing the lineage. When both tiers name the same
model, both flags are required and must be distinct. Explicit policy effort
owns the request and overrides caller effort, while a scalar legacy target
preserves caller effort. The daemon translates the canonical value to each
provider's native request shape; it never bypasses BitRouter or launches a
direct model call for an effort variant.

## Evolve one route at a time

```bash
bitrouter optimize run --human
bitrouter optimize review --human
bitrouter optimize publish # confirms the first frozen -> adaptive publication on a TTY
# Headless/CI first publication: bitrouter optimize publish --enable-adaptive
# Optional: restore a prior policy while keeping the optimization lock aligned.
bitrouter optimize rollback sha256:<digest>
```

`run` launches the same exact argv once for the active baseline and once for a
candidate that changes one observed route key. It does not retry. The workflow
command may itself run a multi-case eval suite; its output is judged against
the success contract. BitRouter—not the judge—provides normalized cost, latency,
policy lineage, and request attribution.

`review` reports baseline/candidate quality, normalized showback cost delta,
observed latency, the route-key change, and content digests. `run` never changes the
active policy. `publish` is a separate explicit action and rejects stale or
mismatched lineage. Run the loop again after publication to optimize another
eligible route key.

The stable public model is `bitrouter/auto`. Internal policy and preset keys
remain `auto`, and the generic `@auto` preset form still resolves to the same
policy — document and send `bitrouter/auto`.

The whole `bitrouter/` namespace is reserved and resolved before any provider
lookup, so an unrecognised slug is a `400`, not a `404`. Sending
`bitrouter/auto` before a policy is bound reports the missing binding and names
`bitrouter optimize setup`; it does not fall back to a default route.

Profiles:

- `quality-first`: no observed quality regression;
- `balanced`: prioritize frequently used route keys and require manual review;
- `savings-first`: prioritize greatest normalized cost and require manual
  review;

These profiles control search order. This version still requires one explicit
agentic pass and lower normalized showback cost; it does not claim a
statistical percentage quality-loss budget.

If the workflow uses ignored/generated inputs (for example `node_modules`,
`.venv`, or fixtures), declare each one with repeated `--workflow-input` during
setup or `--optimize-workflow-input` during onboarding. BitRouter freezes the
same manifest into two detached Git worktrees. Controlled execution is
currently Unix-only.

The controlled two-tier experiment does not yet preserve a signed
`progress_guard`. Setup checks every active policy before writing any
optimization file and asks the user to use an unguarded `bitrouter/auto` lineage
rather than silently changing guard semantics.

## Failure interpretation

- Non-zero workflow exit: valid quality evidence for the evaluator.
- Timeout: terminal run; never publishable.
- No request/decision/metering-price join: infrastructure ambiguity; fail closed.
- Candidate did not exercise the changed route: experiment invalid; fail
  closed.
- Workflow argv or referenced file changed between variants: source drift; fail
  closed.
- `publishable: false`: inspect `review`; do not force publication.
- Intent/contract changed: run `bitrouter optimize resolve`, then start a new run.

Raw workflow output and model replies remain under the BitRouter home. Commit
only the intent, lock, success contract, and policy lock. Provider auth stays
inside the daemon's native credential stores (including Cloud OAuth); never put
credentials in commands, configs, reports, or repository files. Known secrets
are redacted best-effort, so eval
commands must not intentionally print credentials.
