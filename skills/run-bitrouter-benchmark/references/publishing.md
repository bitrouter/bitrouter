# Evidence, Sharing, and Official Submission

## Keep two evidence layers distinct

### Harbor benchmark evidence

The Harbor job directory records the benchmark input, agent/model config,
trials, verifier outputs, rewards, exceptions, trajectories, logs, and
artifacts that the installed integration produced. It supports claims about
that exact job's observed benchmark outcomes.

It does not by itself prove which downstream provider/model BitRouter selected,
the router's actual cash cost, or causal savings against another route.

### BitRouter routing evidence

The resolved BitRouter config and router-authored request/route/usage records
support route claims only when they are isolated to the job and explicitly
joined to its requests or trials. Isolation can be a dedicated endpoint or
database, or a documented per-job/session correlation that excludes unrelated
traffic.

Without that join, report the aggregate Harbor score and omit per-route model,
cost, and savings claims. Distinguish provider cash evidence from notional cost
computed with a price table.

## Inspect before upload

Harbor Hub can store and share jobs. Current documentation:
<https://www.harborframework.com/docs/sharing/jobs>

Before upload:

1. preserve the original local job;
2. inspect config, commands, logs, trajectories, and artifacts for credentials,
   private prompts/code, personal data, and internal endpoints;
3. do not upload any credential; resolve sensitive-content policy with the
   user rather than deleting evidence needed by the benchmark;
4. confirm destination, public/private visibility, organization/user shares,
   and data sensitivity;
5. use the installed `harbor upload` or run-time `--upload` interface.

A Hub URL proves that Harbor accepted and serves that job. It is not universal
leaderboard admission or maintainer review.

## Check official submission separately

Before an official run, record:

- benchmark rules URL and version/ref;
- retrieval date;
- exact dataset/task revision;
- required job config, agent/model/version, attempts, environment, network,
  verifier/judges, resources, and upload visibility;
- required submission command, repository/portal, CI, and human review;
- a requirement-by-requirement pass/fail checklist.

Prefer the benchmark-owned job config. If routing through an external endpoint
is forbidden, stop. A private adaptation is allowed only when the benchmark
policy permits it and the user confirms it will be reported as non-official.

Some benchmarks use a public Harbor Hub job plus a separate submission and
review. For example, Harbor Index documents this flow in its repository:
<https://github.com/harbor-framework/harbor-index#submitting-to-the-leaderboard>

Call the result official only after the benchmark's submission is accepted by
its actual process. Until then use precise states such as `local job complete`,
`Hub upload complete`, or `submission pending`.

## Report contract

Report:

- benchmark/dataset revision, task selection, agent and model entry, attempts,
  environment, concurrency source/value, Harbor and BitRouter provenance;
- successful, failed, and incomplete trial counts plus the score denominator;
- config classification and resolved reachable provider/model/fallback set;
- smoke outcome, route-evidence availability, cost-evidence type, and limits;
- job path and checksum/inventory as appropriate;
- Hub URL/visibility and official-submission state when applicable;
- any deviation from benchmark rules, without presenting it as official.
