# Generic BitRouter Benchmark Skill Specification

## Status

Proposed design for replacing the current Terminal-Bench-specific
`run-bitrouter-benchmark` skill with a generic BitRouter and Harbor workflow.

## Problem

The current skill encodes one experiment: Terminal-Bench 2.1, Terminus 2, a
central EC2 router, custom control and policy rounds, a custom scheduler, and a
large evidence and settlement protocol. That makes the skill difficult to use
for an ordinary Harbor benchmark and encourages agents to build orchestration
that Harbor already provides.

The replacement must support any benchmark that the installed Harbor version
can run, while keeping the human interaction short and preserving explicit
authorization for provider, model, credential, and AWS changes.

## Goals

- Run benchmarks through the installed Harbor CLI, including Harbor's native
  attempt and concurrency controls.
- Support Codex, Claude Code, Terminus 2, and other Harbor agents without making
  one agent the universal default.
- Route the benchmark through either an existing or managed BitRouter endpoint,
  or a BitRouter OSS deployment that the agent operates on AWS.
- Default routing configuration to the official OSS template that matches the
  selected BitRouter release or commit.
- Require the human to confirm the complete provider and model target set even
  when it was discovered from an existing configuration.
- Explain AWS deployment and IAM as an operational method. Do not ship
  Terraform, CloudFormation, deployment scripts, systemd units, or Docker
  Compose assets.
- Preserve Harbor's job directory and optional Harbor Hub upload as the primary
  benchmark evidence, while accurately describing benchmark-specific official
  submission requirements.
- Keep the main skill concise and load detailed references only when selected.

## Non-goals

- Reimplement Harbor scheduling, slots, retries, sandboxes, scoring, or resume.
- Guarantee that every Harbor-runnable dataset has a public leaderboard or
  accepts routed results.
- Build a general AWS deployment framework.
- Infer authorization from environment variables, config files, AWS profiles,
  or detected provider credentials.
- Require a baseline or control run when the user only wants one routed score.
- Claim route-level savings or model attribution from Harbor results alone.

## Considered interaction designs

### A. One large questionnaire

Ask benchmark, agent, endpoint, routing config, every provider and model, AWS,
Harbor environment, attempts, concurrency, upload, and submission questions at
the start. This is complete but asks questions that discovery could answer and
creates a poor first-use experience.

### B. One question at a time

Prompt for each field in sequence. This is easy to implement but creates too
many round trips and makes the skill feel like a deployment wizard.

### C. Two-stage confirmation (selected)

Collect only the choices needed to inspect the environment, then present one
resolved execution summary for explicit confirmation. This minimizes human
interaction without treating detected configuration as permission.

## Human interaction contract

### Stage 1: identify the run

Ask for missing values together in one short prompt:

1. Benchmark input: Harbor dataset name/version, local path, or repository.
2. Agent: Codex, Claude Code, Terminus 2, or another installed Harbor agent.
3. Routing config source: matching official OSS template (default), an existing
   user-owned config, or a new custom config.
4. BitRouter endpoint: existing or managed endpoint, or an OSS deployment on
   AWS.
5. Intent: private score, routing comparison, or official publication.

Do not ask for AWS details when deployment is not selected. Do not ask for a
baseline when the intent does not require a comparison.

If the user selects a new custom config, ask one conditional follow-up for the
entry preset/model, reachable provider/model targets, fallback behavior, and
credential source names. Do not turn the default official-template path into a
from-scratch configuration interview.

### Discovery

Inspect, without mutating external systems:

- the installed Harbor version and relevant `--help` output;
- benchmark metadata and documented submission constraints;
- the selected Harbor agent and environment support;
- the BitRouter binary source, version, release, or commit;
- the selected routing config and all inherited or default providers, models,
  presets, fallbacks, and credential source names;
- AWS profiles or assume-role mechanisms by name only when AWS deployment is
  selected.

Discovery produces candidates, not authorization.

### Stage 2: confirm one resolved plan

Present one compact summary and require explicit confirmation before paid
provider calls or other external mutations. Include:

- benchmark identity and official-submission status;
- Harbor agent, environment, attempts, and explicit `--n-concurrent` value or
  the effective Harbor default;
- BitRouter endpoint and observed or user-attested version/commit provenance;
- routing config classification and exact source ref/path;
- entry model or preset, every resolved provider/model target, fallback target,
  and named credential source;
- AWS account/principal/region and resource plan when deploying;
- expected cost boundary and upload visibility;
- planned installs, service starts, config writes, secret-store writes, and the
  smoke request limit.

The human must confirm the summary once before any paid provider request,
software installation, service start, secret-store write, AWS mutation,
benchmark execution, or upload. Provider and model targets require confirmation
even when all values were already present in the environment or config.

## Routing configuration model

Resolve the config from exactly one of these sources:

1. **Official unchanged.** Use the official template from the same release,
   tag, or commit as the selected BitRouter binary.
2. **Official-derived deployment patch.** Change only deployment properties
   such as listen address, authentication, database, and file locations. Record
   the exact diff and review security-sensitive changes.
3. **Derived from official.** Any provider, model, fallback, tier, preset, or
   policy change uses this label and records a diff from the official source.
4. **Custom.** Use a user-owned config or build a new config from scratch.

Use the latest stable release by default. Use `main`, a development branch, or
a worktree only after the human explicitly selects its repository, ref, and
path. Never search unrelated local worktrees and call a discovered file an
official template. If the selected release has no compatible official
template, report that the default is unavailable and ask the user to choose an
existing or custom config; do not silently substitute `bitrouter init` output.
If an existing or managed endpoint cannot attest its version, label provenance
as unverified and do not claim an exact release or template match.

Keep three concepts separate in the confirmation:

- the Harbor model string that the agent calls;
- the BitRouter preset, alias, or entry model that receives the call;
- the actual downstream provider/model targets and fallbacks.

`inherit_defaults`, provider registry discovery, and environment credentials
must be included in resolution. Enumerate every target reachable from the
selected entry/policy and separately disclose any broader inherited catalog or
credential exposure; do not dump unrelated registry entries into the prompt.
Detection never grants permission.

## Execution architecture

### BitRouter endpoint

Prefer an existing healthy endpoint. If the user chooses AWS deployment, the
agent acts as the operator using standard AWS CLI and SSH actions described in
the AWS reference. The method covers identity confirmation, least privilege,
network exposure, secret placement, health checks, resource tagging, cost
review, and cleanup. It does not prescribe infrastructure-as-code or ship
deployment assets.

An OSS starter config that binds to loopback or skips inbound authentication
must not be exposed unchanged. A remote deployment requires a private network
or TLS, authentication, restricted security-group sources, and upstream
provider credentials stored only on the router host or an authorized secret
service.

Other deployment environments may apply the same identity, network, secret,
health, tagging, and cleanup principles, but the skill will not contain
provider-specific instructions for them.

### Harbor benchmark

Use the installed CLI as the authority for flags. If a benchmark publishes a
job config for official runs, use that config as the base and do not override
its attempts, agent, environment, verifier, or concurrency unless its rules
permit the change. Otherwise prefer a command shaped like:

```bash
harbor run \
  --dataset <dataset> \
  --agent <agent> \
  --model <bitrouter-entry-model> \
  --n-attempts <attempts>
```

Use `--path` or `--repo` instead of `--dataset` when appropriate. Add an
environment only when the benchmark or user selects one. Add
`--n-concurrent <n>` only when the benchmark config or user selects an explicit
value; otherwise leave it unset and record Harbor's effective default. Do not
replace Harbor's scheduler with custom slots or controllers. Use
`harbor job resume` for incomplete jobs when supported by the installed
version.

For a requested comparison, use separate Harbor jobs with the same frozen
benchmark, agent, attempts, environment, and allowed concurrency settings. Do
not create a cross-job scheduler or slot ledger.

Run an agent-specific non-benchmark smoke task before the scored job because a
routing template validated for one agent protocol is not automatically valid
for Codex, Claude Code, or Terminus 2. The Stage 2 summary names the smoke
payload class, endpoint, maximum requests, expected cost ceiling, and retained
artifacts.

### Evidence and publication

Treat the Harbor job directory as the benchmark record. Preserve a redacted
config manifest, trial outputs, results, logs, installed version, and
invocation. Use documented secret injection, keep secret values out of command
arguments and persistent config where the installed Harbor integration allows,
and inspect the job for sensitive data before upload. Never upload a job that
contains credentials. Use Harbor's upload command only after the user confirms
destination, visibility, and data sensitivity.

A Harbor Hub job is shareable Harbor evidence; it is not automatically an
official leaderboard submission. Before a public run, inspect the selected
benchmark's current rules for agent, model, environment, attempts, network,
dataset revision, upload, and review requirements. Record the rules URL/ref,
retrieval date, dataset revision, and a requirement-by-requirement checklist.
Report a result as official only after those benchmark-specific requirements
are met.

BitRouter's resolved config and routing logs are separate routing evidence.
Harbor results prove benchmark outcomes; they do not by themselves prove which
downstream model served each request. Route-level model, cost, or savings claims
require job-isolated route records (for example a dedicated endpoint/database
or a documented per-job/session correlation) and an explicit join to BitRouter
evidence. Otherwise report the aggregate Harbor score only.

## AWS identity and IAM method

Document three independent identities:

1. **Deployment operator.** Human supplies an AWS profile or assume-role path.
   The agent runs `aws sts get-caller-identity`, shows account and principal,
   presents resources and expected cost, and obtains confirmation before
   mutation.
2. **BitRouter instance role.** Optional. Grant only required access such as
   SSM, secret retrieval, logs, or Bedrock. Do not attach it merely because the
   deployment runs on EC2.
3. **Harbor controller identity.** Required only when Harbor itself uses the EC2
   environment. Its permissions are separate from the router deployment.

Harbor trial sandboxes receive no IAM role by default. `iam:PassRole` is needed
only when the human explicitly authorizes attaching a role. Never ask for raw
AWS access keys in chat, log secret values, or infer authority from a profile's
existence.

## Agent references

Provide Harbor-native configuration for:

- **Codex:** OpenAI Responses-compatible BitRouter endpoint, normally with a
  `/v1` base URL.
- **Claude Code:** Anthropic Messages-compatible BitRouter endpoint, normally
  without `/v1` in the base URL.
- **Terminus 2:** its installed Harbor agent configuration, normally using an
  OpenAI-compatible BitRouter endpoint; document an Anthropic-route variant
  only when the selected model requires it.

Examples must derive exact agent names and flags from the installed Harbor
version. They must not embed secrets.

Do not duplicate drift-prone BitRouter CLI flags, listen ports, environment
variable names, or config syntax already owned by the `bitrouter` skill. Load
that skill when available or verify the selected binary's current CLI and
config schema directly.

## Skill layout

Replace the existing content with:

```text
skills/run-bitrouter-benchmark/
├── SKILL.md
├── agents/openai.yaml
└── references/
    ├── agents.md
    ├── aws.md
    ├── harbor.md
    └── publishing.md
```

- `SKILL.md`: two-stage interaction, config source decision, workflow, and hard
  boundaries. Keep it below 200 lines.
- `agents.md`: Codex, Claude Code, Terminus 2, protocol, and smoke-test guidance.
- `aws.md`: operational deployment and IAM method only.
- `harbor.md`: generic CLI discovery, run, concurrency, resume, and evidence.
- `publishing.md`: Harbor Hub versus benchmark-specific official submission and
  route-claim limits.
- `agents/openai.yaml`: skill UI metadata.

Delete the old experiment-specific references. Add no scripts or assets.

Update `skills/README.md` so it describes the generic workflow rather than the
old Terminal-Bench experiment.

## Adversarial acceptance scenarios

The skill must handle these cases without improvising around the contract:

1. A user says "use whatever providers are already configured." The agent must
   still enumerate and obtain confirmation for every reachable
   provider/model/fallback and disclose broader inherited exposure.
2. A stale local clone lacks the official template while public `main` has one.
   The agent must resolve the template from the selected binary's canonical
   release/ref, not conclude from stale local state.
3. A development worktree contains a better-looking template. The agent must
   ignore it unless the user selected that exact repo/ref/path.
4. `inherit_defaults` adds targets not written in the template. The resolved
   confirmation must include them or stop.
5. A user chooses Claude Code with a template validated only for Codex. The
   agent must run the Claude-specific smoke task before the benchmark.
6. A user requests an AWS deployment but supplies only a profile name. The
   agent must prove STS identity, show resource and cost scope, and confirm
   before mutation; it must not request raw keys.
7. Harbor uses Docker locally while BitRouter runs on AWS. The agent must not
   request Harbor EC2 permissions.
8. Harbor uses EC2 sandboxes. The agent must separate controller permissions
   from the router instance role and omit sandbox roles unless authorized.
9. A Hub upload succeeds. The agent must not call it an official leaderboard
   result without benchmark-specific acceptance.
10. A user asks for a routed score only. The agent must not invent a baseline,
    custom scheduler, slot ledger, or route-attribution framework.
11. A user asks for model-level savings from a shared router. The agent must
    decline the claim or switch to job-isolated routing evidence.
12. A benchmark forbids external network endpoints. The agent must stop. It may
    run a private non-official adaptation only when the benchmark policy permits
    it and the user explicitly authorizes that documented deviation.

## Verification

- Capture baseline failures from the existing skill on representative generic
  benchmark prompts before editing it. Keep baseline evaluation offline with
  fixtures or hypothetical prompts; do not make provider calls, mutate AWS, or
  launch benchmarks.
- Re-run the same scenarios after the rewrite and verify the two-stage prompt,
  explicit provider/model confirmation, pure Harbor execution, and evidence
  boundaries.
- Validate the skill with `skills-ref validate` and the repository's skill
  checks.
- Verify every referenced BitRouter command, config field, template path, and
  Harbor command against the selected source or official documentation.
- Confirm `SKILL.md` is below 200 lines, every reference is linked directly from
  it, no old experiment-specific assumption remains, and no secret appears in
  examples.

## Authoritative sources

Implementation must re-check current interfaces rather than copy these examples
blindly:

- BitRouter repository source and matching release/ref for templates and config.
- Harbor documentation for [running jobs](https://www.harborframework.com/docs/run-jobs),
  [agents](https://www.harborframework.com/docs/agents), and
  [job sharing](https://www.harborframework.com/docs/sharing/jobs).
- The selected benchmark's own versioned run and submission instructions; for
  example, Harbor Index uses a public Hub job plus a separate reviewed
  leaderboard submission.
