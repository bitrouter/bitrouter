# Configuration Contract

This reference lists every value an operator must discover, choose, and freeze before a BitRouter benchmark. It is a review contract, not a machine-readable schema and not an infrastructure template.

## Contents

- [Configuration principles](#configuration-principles)
- [Run identity](#run-identity)
- [AWS IAM identity](#aws-iam-identity)
- [Central host mode](#central-host-mode)
- [EC2 and network inputs](#ec2-and-network-inputs)
- [BitRouter source and daemon](#bitrouter-source-and-daemon)
- [Models, providers, and secrets](#models-providers-and-secrets)
- [Harbor and Terminus 2](#harbor-and-terminus-2)
- [Tasks, trials, and controller](#tasks-trials-and-controller)
- [Prices and settlement](#prices-and-settlement)
- [Paths, ports, and tags](#paths-ports-and-tags)
- [Stop limits and final review](#stop-limits-and-final-review)

## Configuration principles

Freeze configuration before any scored case is marked `started`.

- Record explicit values, not ambient defaults.
- Record a secret source name or protected location, never its value.
- Keep an operator-only manifest and a redacted publication manifest.
- Hash the task manifest, configs, binaries, and policy-state snapshots.
- Validate inputs through the exact pinned software that will run the benchmark.
- Treat a changed value as a new lineage unless the methodology explicitly identifies it as postprocessing-only.

Do not reuse a historical manifest unchanged. Its source revisions, credentials, prices, resource identifiers, and paths belong to a different environment.

## Run identity

Record:

| Field | Required value |
| --- | --- |
| Benchmark | `terminal-bench/terminal-bench-2-1` and its resolved dataset revision |
| Run class | Non-evaluation canary, mechanism, replicated, or public reproduction |
| Lineage and round | New immutable names for control, r1, r2, and r3 as applicable |
| Task manifest | Ordered task names and a content hash |
| Trial manifest | Explicit trial identities per task and a content hash |
| Tuning/held-out role | Tuning, held-out, diagnostic, or non-evaluation |
| Control key | Canonical fields plus catalog lookup result |
| Start window | Planned UTC window and maximum allowed duration |
| Owner and reviewer | Operator and independent evidence reviewer |

Names must be unique without embedding credentials, account identifiers, or model secrets.

## AWS IAM identity

Choose one explicit identity mechanism for the whole run. Supported examples include:

- a named AWS CLI profile backed by operator-provided IAM access keys;
- a named profile using `credential_process` or explicit role assumption;
- an EC2 instance profile attached to the central host;
- another AWS-supported method that can be selected deterministically and propagated to every subprocess.

Record the mechanism type and its selector, not credential contents. The selector may be a profile name, role ARN, instance-profile ARN, or environment contract. Do not assume the `default` profile or silently fall back to another source.

Before any quota or EC2 call, prove the explicit identity with STS. Compare the returned account and principal with the private manifest, write only a redacted identity proof to public artifacts, and stop on mismatch. The controller and each Harbor process must use the same explicit identity selection.

Grant the dedicated AWS IAM principal only the permissions required to:

- read caller identity and regional Service Quotas;
- create, tag, describe, stop, and terminate the selected EC2 instances;
- create, inspect, attach, detach, and delete the required volumes and network interfaces;
- inspect the selected images, subnets, security groups, key pair, and instance profiles.

Root credentials are unnecessary. If credentials must be copied to a central host, use a protected channel and restrictive file permissions, remove them during decommissioning, and never place them in an AMI, user data, shell history, repository, config bundle, log, or archive.

## Central host mode

Choose exactly one mode: provision a central host or reuse a central host.

### Provision a central host

Freeze the image, instance type, volume, subnet, security groups, SSH path, instance profile or credential transfer method, bootstrap procedure, and teardown/retention decision. Tag the host separately from ephemeral sandboxes.

Provisioning is not complete until the host proves:

- the expected AWS identity;
- the pinned BitRouter, Harbor, and Terminus 2 revisions;
- the expected binary and configuration hashes;
- private reachability from a canary sandbox;
- sufficient disk, memory, ports, and file permissions;
- protected secret sources and sanitized logging.

### Reuse a central host

Freeze the instance identity and re-run every proof above. Also prove that no stale benchmark process, port listener, database, socket, trace file, controller event log, or output directory can overlap the new lineage.

Reuse saves provisioning time. It does not permit weaker evidence or reuse of run-scoped state.

## EC2 and network inputs

Record:

- AWS account selector and region;
- central and sandbox AMI IDs and architecture;
- central and sandbox instance types and vCPU counts;
- root-volume type and size;
- subnet and availability-zone strategy;
- central, sandbox, and controller security groups;
- SSH key source and authorized source range;
- whether sandboxes use public IPv4, NAT, or a prebuilt image for bootstrap;
- private daemon address, ports, and allowed source security group;
- resource tags for project, role, lineage, round, case, and trial;
- observed Standard On-Demand vCPU quota and headroom.

Use Service Quotas code `L-1216C47A`. Freeze the calculation:

```text
required vCPU = central-host vCPU
              + max-parallel-sandboxes * sandbox vCPU
              + declared headroom
```

If a central host is retained and already consumes quota, count its actual live vCPU rather than adding it twice. Check both the quota ceiling and current tagged/non-tagged consumption before every batch.

Restrict sandbox-to-daemon ingress to the sandbox security group and benchmark ports. If the subnet has an Internet Gateway but no NAT route, decide explicitly whether ephemeral sandboxes receive public IPs for bootstrap and controller SSH. Keep model traffic on the private daemon address.

## BitRouter source and daemon

Freeze:

- repository URL and full source commit;
- dirty-tree status and patch hash when the source is not pristine;
- toolchain and dependency lockfile hash;
- exact build command and feature set;
- serving binary SHA-256;
- postprocessing binary SHA-256 when it differs from the serving binary;
- daemon configuration hash and redacted copy;
- control and policy listen addresses;
- control database path and a separate policy database path;
- trace, policy-decision, daemon-log, and settlement-log paths per group;
- health endpoint, startup timeout, graceful shutdown timeout, and settlement grace period;
- workflow-state and metering commands available at that source revision.

Build on the central host or transfer a checksummed binary. Never hot-swap the serving binary inside a lineage. A newer postprocessing binary may repair evidence only when it launches zero cases, does not mutate routing state, is separately pinned, and produces an audit trail.

## Models, providers, and secrets

Record every route as provider, model, API protocol, base URL class, credential class, and intended tier:

| Tier | Role | Selection guidance |
| --- | --- | --- |
| Strong | Fixed control and escalation | Frontier-quality route used by the immutable baseline |
| Balanced | Default cost/quality tradeoff | Mid-price route selected for general work |
| Economy | Aggressive savings | Lowest-price eligible route with task/capability safeguards |

Do not infer tiers from marketing names. Freeze a price snapshot and capability hypothesis, then validate tool use, context length, protocol behavior, and reliability through canaries.

For each credential, record only:

- the provider and credential class;
- the secret source name or protected path;
- the renewal/expiry check;
- which process reads it;
- the public redaction rule.

Provider API keys, subscription OAuth material, AWS keys, and SSH keys stay out of Harbor configs and evidence archives. Sandboxes receive only the private daemon URL and, when local daemon authentication is disabled, a non-secret local token required by the client library.

For a Claude Pro/Max subscription, freeze an explicit
`claude-code:<model>` target and the protected source of
`CLAUDE_CODE_OAUTH_TOKEN`. The central daemon must receive that variable before
provider construction. Terminus 2 and other non-Claude-Code harnesses send
normal client requests and must not receive the OAuth token or synthesize
Claude Code identity headers; BitRouter owns the outbound OAuth-compatible
translation. A bare canonical Claude model is not equivalent to this explicit
opt-in and must remain outside subscription auto-cascade.

## Harbor and Terminus 2

Freeze:

- Harbor repository and full commit;
- any applied patch hash and installation command;
- Python version and dependency lock/freeze hash;
- Terminal-Bench dataset name and resolved revision;
- Terminus 2 agent name, package version, reasoning configuration, context limits, and timeouts;
- entry model name and API protocol exposed by BitRouter;
- `api_base` placement and `llm_kwargs` passed by the pinned Harbor version;
- the non-secret daemon key field required by the Terminus 2 LiteLLM client;
- the exact workflow-session propagation mechanism;
- EC2 environment fields and Pydantic validation command.

Record whether a multi-trial Harbor `JobConfig` or a one-case `TrialConfig` will be used. The two shapes differ; do not translate between them by string editing after validation.

## Tasks, trials, and controller

Freeze:

- exact ordered task list and dataset revision;
- trial identities, random seeds when exposed, and attempts per task;
- control versus policy group membership;
- `max_parallel_sandboxes` for the entire lineage;
- Harbor retry count, normally zero for mechanism runs;
- task, agent-setup, verifier, settlement, and group timeouts;
- pre-batch quota, health, and sentinel gates;
- append-only case event store and atomic `started` rule;
- resume rule allowing only `not_started` cases;
- predeclared spend and severe-quality stop limits;
- provider-rate-limit and latency thresholds for canaries.

The manifest must distinguish a predeclared trial from a retry and must state how any public-protocol replacement attempt will be labeled and reported.

## Prices and settlement

Freeze one price record per provider/model/credential class and effective period:

- uncached-input price;
- cache-read price;
- cache-write price;
- output price;
- reasoning-token price when billed separately;
- fixed request, subscription, credit, or platform charges when relevant;
- currency, tax treatment, unit, source URL, source date, and retrieval timestamp;
- whether the result is an actual charge or a notional list-price comparison.

Explicit cache prices take precedence. If the selected BitRouter revision supports same-route fallback for a missing cache rate, document that behavior: only the same route's valid base input price may be used. A missing or invalid base price remains unknown.

Freeze settlement states accepted by the run. Cost-bearing evidence must resolve to authoritative `computed` or authoritative `not_charged`. `pending`, `unknown`, inferred zero, or model-estimated usage is not accepted.

## Paths, ports, and tags

Allocate new run-scoped values for:

- controller state and append-only case events;
- control database and policy database;
- per-group daemon logs, traces, decisions, usage, outcomes, and artifacts;
- sockets and PID files;
- control and policy ports;
- Harbor jobs directory and job name;
- secret-free manifest and private operator manifest;
- archive, checksum manifest, and secret-scan result.

Use the same immutable run tag on controller events, sandbox resources, artifacts, and cleanup queries. Use a separate role tag for the retained central host so sandbox cleanup cannot terminate it accidentally.

## Stop limits and final review

Before launch, have the operator and reviewer sign off on:

- identity and permission proof;
- source commits, patches, and hashes;
- task/trial/control manifests;
- model routes, protocols, and credential sources;
- quota calculation and concurrency;
- prices and actual/notional classification;
- retry, timeout, settlement, feedback, and cleanup rules;
- maximum spend and severe-quality stop conditions;
- new paths, ports, tags, and secret redaction.

After sign-off, make the manifest immutable. If an input must change, stop before starting another case and create a new lineage manifest.
