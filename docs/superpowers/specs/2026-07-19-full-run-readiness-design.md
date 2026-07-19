# Terminal-Bench Full-Run Readiness Design

## Goal

Prepare PR #717 and the production benchmark environment for one internal,
single-trial Terminal-Bench 2.1 full run using Harbor, Terminus 2, a central
EC2 BitRouter daemon, and ephemeral EC2 sandboxes, with no serving-binary or
configuration hot update after the scored lineage starts.

The preparation is successful only when the merged PR branch builds and its
exact binary passes independent, non-scoring end-to-end canaries for Claude
subscription, Codex subscription, and BitRouter Cloud routes, and each route
produces independently auditable trace and metering evidence.

## Constraints

- Merge the latest `main` into the PR #717 head; do not rebase or rewrite the
  existing PR history.
- Preserve the user's dirty original PR worktree. Perform integration in a new
  clean worktree and update the PR head only after verification.
- Do not rerun the completed short benchmark solely because `main` changed.
  Instead, run source-level regression tests plus non-evaluation provider and
  real-agent canaries.
- Use Terminal-Bench 2.1, Harbor, Terminus 2, one retained central EC2 daemon,
  and one ephemeral EC2 sandbox per canary trial.
- Use one explicit named AWS IAM access-key profile for all AWS/controller/
  Harbor operations. Never rely on the ambient default identity.
- Keep all provider credentials on the central daemon. Harbor and Terminus 2
  receive only the private daemon endpoint and a non-secret local key.
- Never print, commit, archive, or place an OAuth token, API key, AWS key, or
  SSH private key in a process argument.
- Treat this as an internal one-trial mechanism run, not a public five-trial
  Terminal-Bench score.

## Integration architecture

Create a clean branch from `origin/codex/c0-c1-policy-router`, merge the exact
latest `origin/main` commit, and resolve conflicts compositionally. The known
content conflicts are additive:

1. retain both the PR's workflow-state reliability CLI test and main's Cloud
   API short-flag test;
2. retain `UsageOrigin` from PR #717 and `ChatStreamOptions` from main in the
   Chat Completions adapter;
3. retain `UsageOrigin` from PR #717 and `FinishReason` from main in settlement.

Generated registry/schema artifacts are rebuilt from the merged sources rather
than resolved by selecting either side. The merged release binary and all
runtime configs receive hashes before any canary starts.

## Provider matrix

The frozen preflight matrix contains four routes:

| Route | Provider target | Model | Credential source |
|---|---|---|---|
| Claude strong | `claude-code` | `claude-fable-5` | process-only `CLAUDE_CODE_OAUTH_TOKEN` |
| Claude balanced | `claude-code` | `claude-sonnet-5` | the same process-only token |
| Codex strong | `openai-codex` | `gpt-5.6-sol` | protected Codex OAuth store |
| Cloud comparison | `bitrouter` | `moonshotai/kimi-k2.7-code` | protected BitRouter Cloud credential |

The Claude environment token is captured by the daemon's subscription auth
applier at process construction, remains in memory, and is non-refreshable.
The central service therefore must start with the variable already present;
injecting it after startup is not a valid repair.

## Canary data flow

Each route uses a unique non-scoring run ID, port, control socket, database,
trace file, and output directory. For every route:

1. validate the frozen BitRouter config with the pinned binary;
2. start the daemon with only protected central-host secret sources;
3. run one direct sentinel through the real inbound protocol;
4. launch one predeclared Harbor TrialConfig through an ephemeral EC2 sandbox
   and Terminus 2;
5. propagate one exact workflow session in the Terminus 2 session and headers;
6. stop the daemon without replacing its binary or config;
7. export usage and outcomes and assemble strict evidence;
8. independently query AWS instances, volumes, and interfaces until exact-tag
   residue is zero.

The controller capacity for all validations after 2026-07-20 is frozen at 4.
A one-case route canary still launches and must observe exactly one sandbox;
multi-case validation must use enough predeclared non-evaluation cases to
exercise a peak of 4. Canaries do not consume a scored full-run trial identity.

The harness/provider compatibility gate runs before credentials or resource
allocation. Terminus 2 cannot use the `claude-code` subscription provider:
that route requires a genuine Claude Code client marker, and BitRouter must not
synthesize it. A Terminus 2 full run may include Claude only through the
separate `anthropic` API-key provider; otherwise both Claude subscription routes
remain excluded from the scored manifest.

## Evidence and acceptance

For each route, acceptance requires:

- one complete TrialResult with verifier reward and no unexplained exception;
- unique request IDs and an exact high-confidence workflow-session join;
- real request traces identifying the intended provider and model;
- numeric `uncached_input_tokens`, `cache_read_tokens`,
  `cache_write_tokens`, and `output_tokens` on every usage row;
- an authoritative observed zero when a provider does not report a cache
  category, never a missing value silently converted to zero;
- authoritative terminal settlement or an explicit subscription/notional
  classification;
- exact trace/usage/decision/outcome membership with no unmatched rows;
- zero retries and no duplicate `started` event;
- sandbox peak at most one and final EC2/EBS/ENI residue zero.

Direct sentinels prove provider/protocol readiness; only the real Terminus 2
EC2 trial proves end-to-end benchmark readiness. Both pieces are retained.

## Failure handling

- Authentication `401`/`403`: stop before any scored identity, inspect only
  redacted secret-source metadata, repair the central daemon credential, and
  start a new canary identity.
- Provider `429`/`5xx`/timeout: preserve reliability and settlement evidence;
  do not classify it as semantic model inadequacy or raise concurrency.
- Missing cache buckets or pending/unknown settlement: reject the route's
  readiness evidence and fix the adapter/exporter before the full run.
- Session mismatch, missing TrialResult, or runtime exception: reject the
  canary; do not infer attribution from overlapping timestamps.
- Resource residue: terminate/delete only exact-tag resources and reject the
  canary until the authenticated query is empty.

Any code repair is made on the integrated PR branch, tested locally, rebuilt,
and redeployed as a new frozen canary tuple. The scored full run never starts
on a branch that still requires runtime patching.

## Documentation update

If preparation exposes a reusable operational failure, update the
documentation-only `skills/run-bitrouter-benchmark/` package in the same PR
branch. Put stable procedure in configuration/operations and symptom-specific
diagnosis in Q&A. Do not add account IDs, private paths, IP addresses, run IDs,
or credential shapes that would make the skill operator-specific.

## Final readiness decision

Issue `GO` only when the merged PR head, serving binary hash, Harbor revision,
Terminus 2 configuration, provider/model matrix, AWS identity selector, ports,
paths, prices, task/trial manifest, concurrency, retry rule, and stop limits are
frozen and all four end-to-end canaries are strictly accepted. Otherwise issue
`NO-GO`, retain the evidence, repair on a new canary identity, and keep the
scored full run unstarted.
