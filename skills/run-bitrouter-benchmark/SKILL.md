---
name: run-bitrouter-benchmark
description: Use when a user wants to run, compare, resume, audit, share, or submit a Harbor benchmark through BitRouter, including choosing a Harbor dataset and agent, confirming routed providers and models, or optionally operating BitRouter OSS on AWS.
---

# Run BitRouter Benchmark

## Core rule

Let Harbor own tasks, trials, environments, concurrency, scoring, artifacts,
and resume. BitRouter is the model endpoint. Ask only for choices that cannot be
discovered, then obtain one explicit confirmation of the resolved run before
paid or mutating actions. Detected config and credentials are never permission.
Treat every path, command, port, profile, region, service, package manager, and
repository layout as specific to the current user's environment; discover or
ask for it instead of inheriting assumptions from the skill author's machine.

## 1. Identify the run

Before discovery, ask for every missing choice together in one short message
and stop. Do not infer an unspecified choice from local state:

1. Harbor dataset with version, local path, repository, or job config.
2. Agent: Codex, Claude Code, Terminus 2, or another installed Harbor agent.
3. Routing config: matching official OSS template (default), existing user
   config, or a new custom config.
4. Endpoint: existing/managed BitRouter, or BitRouter OSS to operate on AWS.
5. Intent: private score, routing comparison, or official publication.

If the user chooses a new custom config, ask one follow-up for the entry
preset/model, reachable provider/model targets, fallback, and credential source
names. Do not require a baseline when the user only wants one routed score. Do
not ask AWS questions when neither BitRouter AWS deployment nor a Harbor
AWS/EC2 environment is selected. Treat `any` as permission for that named
choice only; never default a different missing choice. Ask all remaining Stage
1 choices together.

## 2. Inspect before asking again

Inspect without external mutation:

- how Harbor is launched (PATH command, absolute executable, environment
  launcher, or container), then its version, run help, agents, environments,
  and the selected dataset or benchmark-owned job config;
- how BitRouter is accessed and managed: endpoint, executable or service /
  container identity, config path or mount, and source/release provenance;
- current benchmark run and submission rules;
- BitRouter endpoint provenance and health metadata where available;
- selected config source and every provider/model/fallback reachable from its
  entry route, plus broader exposure from inherited defaults or credentials;
- AWS profile or assume-role name only if BitRouter AWS deployment or a Harbor
  AWS/EC2 environment was selected.

Use read-only host discovery first. A command absent from `PATH` is unresolved,
not proof that the software is uninstalled. After discovery, batch all access
methods or paths that remain genuinely unresolved into one conditional prompt.
If installation or repair is needed and no preference was supplied, propose a
pinned method appropriate to the confirmed host/runtime in the Stage 2 plan;
do not add a separate preference round. Never assume this repository is checked
out.

Use the official template from the same stable release/tag/commit as the
selected BitRouter binary. If AWS deployment has no preselected binary, propose
the latest stable OSS release and its matching template as the default. Use
`main`, a development branch, or another worktree only when the user selects
its exact repository, ref, and path. If binary or endpoint provenance is
unavailable, say `unverified`; never invent a template match. If the selected
release has no compatible template, ask for an existing or custom config
instead of silently using starter output.

Classify the result:

- **Official unchanged** — exact template.
- **Official-derived deployment patch** — only listen/auth/database/path
  changes; preserve and review the diff.
- **Derived from official** — provider, model, fallback, preset, tier, or policy
  changed; preserve the diff.
- **Custom** — user-owned or built from scratch.

Load the `bitrouter` skill when available for current BitRouter CLI, config,
provider, and harness facts. Otherwise inspect the selected binary and source;
do not copy stale details from this benchmark skill.

## 3. Confirm one resolved plan

Present one compact plan containing:

- benchmark identity/revision and official-submission status;
- Harbor agent, environment, attempts, benchmark-owned settings, and explicit
  concurrency or the effective Harbor default;
- endpoint provenance and config classification/ref/path;
- Harbor model string, BitRouter entry preset/model, every reachable downstream
  provider/model/fallback, and credential source names (never values), plus any
  broader inherited catalog or credential exposure as a separate disclosure;
- bounded agent-specific smoke payload, maximum requests, expected cost, the
  whole benchmark's provider-spend estimate or ceiling, and retained artifacts;
- installs, service/config/secret-store changes, plus AWS account/principal,
  region, resources, cost, and—when Harbor uses EC2—the resolved controller-to-
  sandbox address/SSH path, bootstrap egress, proposed temporary rule
  specifications and stopping condition, and cleanup only when applicable;
- output path and upload destination, visibility, and data sensitivity.

Require one explicit confirmation before installation, service start,
secret-store write, paid provider request, AWS mutation, benchmark launch, or
upload. Reconfirm provider/model targets even when they came from existing
config. Read [agents.md](references/agents.md) for agent protocol and smoke
checks; read [aws.md](references/aws.md) when operating BitRouter or a Harbor
EC2 environment on AWS.

## 4. Prepare the route

Validate the selected BitRouter config through the selected binary. A remote
OSS endpoint must use private reachability or TLS, inbound authentication, and
restricted network sources; never expose a loopback/unauthenticated starter
unchanged. Keep upstream credentials at the router or its authorized secret
service. Give Harbor only the BitRouter endpoint and inbound credential.

Run the confirmed bounded smoke through the selected Harbor agent protocol.
Protocol success for one agent does not validate another. Stop on wrong
endpoint, model, fallback, auth, or route behavior.

## 5. Run Harbor

Read [harbor.md](references/harbor.md). Prefer the benchmark's pinned job config
for official runs. Otherwise derive the command from the installed
`harbor run --help` using the selected dataset/path/repo, agent, model, attempts,
and environment. Set `--n-concurrent` only when the benchmark or user chose a
value; otherwise leave it unset and record Harbor's effective default.

Do not create slots, controllers, schedulers, or retry ledgers. Use
`harbor job resume` for an incomplete compatible job. For a requested
comparison, run separate Harbor jobs with the same frozen benchmark, agent,
attempts, environment, and allowed concurrency settings.

## 6. Report or publish

Preserve the Harbor job directory, redacted effective config, invocation,
version, results, failures, logs, trajectories, and artifacts. Report exactly
the attempted dataset/tasks and score denominator. Inspect artifacts for
credentials and sensitive content before any upload.

Read [publishing.md](references/publishing.md). A Harbor Hub job is shareable
evidence, not automatic leaderboard acceptance. Claim an official result only
after satisfying the benchmark's versioned checklist. Harbor evidence proves
benchmark outcomes; downstream route/model/cost claims additionally require
job-isolated BitRouter evidence and an explicit join.

## Hard boundaries

- Do not treat discovered providers, models, credentials, or AWS profiles as
  authorization.
- Do not silently substitute a template, agent, model, fallback, benchmark
  setting, or concurrency value.
- Do not put raw secrets in chat, command arguments, persistent configs, or
  uploadable artifacts when documented secret injection is available.
- Do not override an official job config unless its rules allow the change.
- Stop when benchmark policy forbids the routed endpoint. Run a private
  adaptation only when policy permits it and the user confirms the deviation.
- Do not call an upload an official submission or infer per-route economics
  from an aggregate Harbor score.

## References

| Read | When |
|---|---|
| [agents.md](references/agents.md) | Configure or smoke Codex, Claude Code, Terminus 2, or another Harbor agent |
| [aws.md](references/aws.md) | Operate BitRouter OSS or a Harbor EC2 environment on AWS, or select IAM boundaries |
| [harbor.md](references/harbor.md) | Discover/run/resume Harbor jobs and set attempts or concurrency |
| [publishing.md](references/publishing.md) | Preserve evidence, upload to Hub, submit officially, or make route claims |
