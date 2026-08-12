# Run the Benchmark with Harbor

Harbor owns benchmark execution. Use its installed CLI and the selected
benchmark's versioned files; do not reproduce its scheduler in this skill.

Official references:

- Run jobs: <https://www.harborframework.com/docs/run-jobs>
- Core concepts: <https://www.harborframework.com/docs/core-concepts>
- Agents: <https://www.harborframework.com/docs/agents>
- Jobs and sharing: <https://www.harborframework.com/docs/sharing/jobs>

## Discover the installed contract

Run read-only discovery first:

```bash
harbor --version
harbor run --help
harbor datasets list
```

If the installed command is missing or broken, include the proposed pinned
install/repair in the resolved plan. Do not install or upgrade silently. Record
the effective Harbor version and agent version with the job.

Inspect the dataset/repository documentation and task metadata before forming a
command. A benchmark-owned `job.yaml` or equivalent official config wins over
generic flags: preserve its dataset revision, verifier/judges, agent constraints,
attempts, environment, timeouts, resources, and concurrency unless its rules
explicitly allow an override.

## Form an ad hoc run only when appropriate

Use the forms exposed by the installed `harbor run --help`:

```bash
# Registry dataset
harbor run --dataset ORG/DATASET@VERSION --agent AGENT --model ENTRY_MODEL

# Local task or dataset
harbor run --path /absolute/path --agent AGENT --model ENTRY_MODEL

# Repository-backed dataset, when supported by the installed version
harbor run --repo REPOSITORY --dataset DATASET --agent AGENT --model ENTRY_MODEL
```

Add the confirmed attempt option only when needed. Add
`--n-concurrent NUMBER` only when the benchmark config or user selected an
explicit value. Otherwise omit it and record Harbor's effective default. Do
not confuse trial concurrency with model/provider rate limits or with the
number of attempts.

Add an environment only when the benchmark config or user selects one. The
BitRouter deployment location does not determine Harbor's sandbox environment:
local Docker may call a router on AWS, and Harbor EC2 may call an existing
router elsewhere.

Use the benchmark's documented secret injection for agent and verifier
credentials. Prefer scoped environment/config mechanisms whose values are not
persisted in the job. Inspect the installed behavior; never place raw values in
examples or uploadable manifests.

## Execute and recover

Before the scored job:

1. validate the benchmark-owned config or ad hoc command;
2. run the confirmed bounded agent/protocol smoke;
3. confirm a fresh output path and expected trial count;
4. launch exactly the resolved Harbor job.

Preserve all completed and failed trials. For a compatible incomplete job, use
the installed `harbor job resume -p JOB_DIR` interface. Do not delete failures,
invent case slots, or write a custom controller, scheduler, retry ledger, or
resume mechanism.

For a routing comparison, create separate Harbor jobs. Freeze the same dataset,
agent/version, task selection, attempts, environment, timeouts, and allowed
concurrency settings for each side. Change only the predeclared route/config
factor. Do not require a comparison for a single-score request.

## Preserve the job

Keep the full Harbor job directory, effective redacted config, exact invocation,
Harbor/agent versions, task selection, results, exceptions, trajectories, logs,
and artifacts. Report every attempted trial and the exact score denominator.
Do not reinterpret verifier reward or invent request-level attribution that the
job does not contain.
