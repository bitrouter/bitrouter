# Operations Runbook

This reference describes the fixed Terminal-Bench 2.1 + Harbor + Terminus 2 + AWS EC2 lifecycle. Replace shell variables with values from the approved manifest, verify every command against the pinned source revisions, and stop at the first failed gate.

## Contents

- [Lifecycle at a glance](#lifecycle-at-a-glance)
- [1. Freeze and authenticate](#1-freeze-and-authenticate)
- [2. Gate quota before case consumption](#2-gate-quota-before-case-consumption)
- [3. Prepare the central host](#3-prepare-the-central-host)
- [4. Build and start BitRouter](#4-build-and-start-bitrouter)
- [5. Validate Harbor and Terminus 2](#5-validate-harbor-and-terminus-2)
- [6. Run non-evaluation canaries](#6-run-non-evaluation-canaries)
- [7. Resolve or create the immutable control](#7-resolve-or-create-the-immutable-control)
- [8. Run policy rounds](#8-run-policy-rounds)
- [9. Reconcile and assemble evidence](#9-reconcile-and-assemble-evidence)
- [10. Resume safely](#10-resume-safely)
- [11. Clean up AWS resources](#11-clean-up-aws-resources)
- [12. Archive and scale concurrency](#12-archive-and-scale-concurrency)

## Lifecycle at a glance

```text
freeze manifest
  -> prove explicit AWS identity
  -> quota/network/source/config preflight
  -> provision or verify central host
  -> direct strong and weak sentinels
  -> one real non-evaluation Terminus 2 canary
  -> resolve immutable control
  -> launch only missing control identities, once
  -> r1 -> feedback -> r2 -> feedback -> r3
  -> settle and strictly accept each group
  -> exact-tag cleanup after every group
  -> sanitize, checksum, archive, and report
```

Run no scored case merely to test installation, credentials, networking, concurrency, or cleanup. Use probes and predeclared non-evaluation canaries for those purposes.

## 1. Freeze and authenticate

Create a new run root and write the approved private manifest plus a redacted review copy. Verify that no run label, port, database, socket, trace, decision, log, Harbor output, or controller event path already exists.

Select one AWS credential mechanism. For a named profile, a non-printing identity preflight can use:

```bash
aws --profile "$AWS_PROFILE" sts get-caller-identity \
  --output json >"$PRIVATE_IDENTITY_PROOF"
```

For an instance profile or explicit assumed role, omit `--profile` and use the manifest's deterministic selector. In every mode:

1. compare account and principal with the private manifest;
2. produce a redacted proof for the archive;
3. pass the same selector to all controller calls;
4. propagate the same environment/profile to every Harbor subprocess;
5. stop before any quota or EC2 call on mismatch or fallback.

Do not print credentials or include them in process arguments that will be archived. Check secret-file permissions without displaying contents.

## 2. Gate quota before case consumption

Query Standard On-Demand vCPU quota code `L-1216C47A` in the selected region. A named-profile example is:

```bash
aws --profile "$AWS_PROFILE" service-quotas get-service-quota \
  --region "$AWS_REGION" \
  --service-code ec2 \
  --quota-code L-1216C47A \
  --query 'Quota.Value' \
  --output text
```

Calculate:

```text
required vCPU = live central-host vCPU
              + max_parallel_sandboxes * sandbox vCPU
              + declared headroom
```

Also inspect current running/pending instances because quota availability is the ceiling minus current use. Immediately before every canary or benchmark batch:

1. prove identity again if the credential could have rotated;
2. query quota and current usage;
3. query live sandboxes for the exact run tag;
4. check daemon health and provider sentinels;
5. only then atomically append `started` and call Harbor.

If the gate fails, leave the case `not_started`. A rejected `RunInstances` call must not accidentally consume a control identity.

## 3. Prepare the central host

### Provisioning mode

Create the central instance with the frozen AMI, instance type, volume, subnet, security groups, role or credential channel, and tags. Restrict SSH to the operator/controller source. Restrict daemon ingress to the sandbox security group and declared ports.

Bootstrap through a reproducible command log that contains no secrets. Install the pinned toolchain, BitRouter source, Harbor source, Terminal-Bench dependencies, and Terminus 2 dependencies. Store secrets only after the image/bootstrap stage so they cannot enter an AMI or user data.

### Reuse mode

Verify the existing instance ID, source revisions, binary hashes, Python environment, credential mechanism, network, disk, and clock. Reject reuse if any run-scoped process or file overlaps the new manifest.

For both modes, prove:

- central-host private and optional public addresses match the manifest;
- the host can create/tag/delete a disposable sandbox;
- the sandbox can bootstrap packages using its declared public-IP/NAT/image path;
- the sandbox reaches BitRouter only through the private address;
- the central host has no unexpected listener on selected ports;
- controller, daemon, Harbor, and archive paths are writable by the intended user only.

Long-running commands must be owned by a persistent central supervisor (for example, a named tmux pane or systemd unit), not by the operator's SSH connection. This is especially important when SSH uses Tailscale or `ProxyJump`: a jump timeout is not a benchmark terminal state. SSH should poll retained output, exit status, markers, processes, and exact AWS tags.

## 4. Build and start BitRouter

Checkout the full frozen commit and confirm the worktree/patch state. Build with the recorded command, compute the serving binary hash, and compare it with `binary SHA-256` in the manifest.

Also prove target compatibility: record central-host OS/architecture, use the pinned target or central toolchain (including an absolute Cargo path when non-interactive SSH omits it from `PATH`), and require the serving artifact to be a matching Linux ELF before execution. A hash of a macOS artifact is not a valid Linux build proof.

Validate control and policy configs through that binary:

```bash
"$BITROUTER_BIN" config validate --config "$CONTROL_CONFIG"
"$BITROUTER_BIN" config validate --config "$POLICY_CONFIG"
```

Use a dedicated control database and a separate new policy database. The policy database persists across r1-r3; per-group traces, decisions, logs, and process lifetimes do not.

Before starting a group, set new output paths and the harness identifier expected by the pinned BitRouter revision. Start the daemon, wait on its health endpoint with a bounded timeout, and record start time, PID, config hash, binary hash, database, port, and output paths in an append-only group manifest.

When a route uses the Claude Code subscription, inject
`CLAUDE_CODE_OAUTH_TOKEN` into an owner-only central environment source before
the daemon starts. Deliver the value through a non-echoing protected channel;
never put it in an argument, config, Harbor environment, controller manifest,
log, or archive. Presence checks report only `present` or `missing`. The direct
provider sentinel must use a standard Anthropic request with an explicit
`claude-code:<model>` target and deliberately omit `anthropic-beta`, proving
that BitRouter—not the benchmark harness—constructs the upstream Claude Code
OAuth request.

The accepted bridge also prepends the current Claude Agent SDK identity block
on the central outbound hop while retaining every downstream system
instruction. The sandbox must not originate that identity. A direct sentinel
that receives a generic upstream 429 should be compared against the official
Claude Code CLI: if the CLI succeeds with the same token/model, verify the
serving binary includes this body transform before blaming account quota.

Never hot-swap the serving binary, config, provider credential class, or policy database within a lineage.

## 5. Validate Harbor and Terminus 2

Inspect the selected Harbor models before generating configs. In the validated shape:

- a multi-case `JobConfig` uses the plural `agents` field;
- a directly launched one-case `TrialConfig` uses the singular `agent` field.

The distinction is `JobConfig.agents` versus `TrialConfig.agent`. Validate generated YAML/JSON through Harbor's own Pydantic model before marking a case `started`.

For Terminus 2, the agent configuration must express the same non-secret data as:

```yaml
agent:
  name: terminus-2
  model_name: "$ENTRY_MODEL"
  extra_allowed_hosts:
    - "$DAEMON_PRIVATE_HOST"
  env:
    OPENAI_API_KEY: bitrouter-local
  kwargs:
    api_base: "http://$DAEMON_PRIVATE_HOST:$DAEMON_PORT/v1"
    parser_name: json
    session_id: "$EXACT_TRIAL_SESSION"
    llm_kwargs:
      api_key: bitrouter-local
    llm_call_kwargs:
      extra_headers:
        x-bitrouter-workflow-session: "$EXACT_TRIAL_SESSION"
```

Render variables before Pydantic validation. `api_base` belongs in `agent.kwargs` for the pinned Harbor implementation. Keep the non-secret local key in `agent.kwargs.llm_kwargs.api_key`; `agent.env.OPENAI_API_KEY` alone is insufficient in affected Harbor versions.

For a Claude subscription route, replace the entry model with
`anthropic/<claude-model>` and replace `OPENAI_API_KEY` with the same non-secret
local value under `ANTHROPIC_API_KEY`. This makes Terminus 2's complete
downstream hop use Anthropic Messages. Keep `api_base`, `llm_kwargs.api_key`,
and the immutable workflow headers; the fixed daemon tier resolves the request
to `claude-code:<claude-model>`.

Set `x-bitrouter-workflow-session` to the exact value later emitted as the Harbor outcome `session_key`. Confirm the header on a captured canary request. Do not use prompt hashes, body metadata, response IDs, or overlapping time windows as the primary parallel attribution key.

Set the EC2 environment to ephemeral deletion and include exact run, role, case, and trial tags. Explicitly choose public-IP/NAT bootstrap behavior. Keep the daemon host in `extra_allowed_hosts` and do not broaden network policy unnecessarily.

Pin a Harbor revision that treats a vanished tmux pane/session as typed `TerminalSessionEnded`, ends agent interaction, and still runs the verifier when the environment remains accessible. Do not recreate a pane automatically: recreation loses working directory, environment variables, and process state. An environment transport failure remains runtime-invalid.

## 6. Run non-evaluation canaries

Run these gates without consuming the scored manifest:

1. a harness/provider compatibility check before credentials or resource allocation;
2. one direct strong-route request through BitRouter;
3. one direct balanced/economy request through BitRouter;
4. one real Terminus 2 trial in an ephemeral EC2 sandbox;
5. one session-end fixture if the Harbor revision changed;
6. one cleanup cycle proving zero sandbox residue.

Each canary/preflight uses a fresh immutable non-evaluation identity and owner-only evidence directory. Preserve config validation, daemon log, sanitized provider response, trace, and an accepted/rejected marker even when the request fails. Validate the real Harbor `TrialConfig`, controller manifest, and daemon config before creating any scored run root.

The canary passes only when:

- Harbor emits a complete TrialResult and verifier reward;
- every request has one trace, decision when policy applies, settlement row, and High-confidence session;
- the outcome session key equals the explicit header;
- all four usage buckets are numeric, including observed zeros;
- charge status is authoritative;
- the sandbox is gone and no run-tagged volume or network interface remains.

Keep concurrency 1 until explicit sessions are proven. Test higher concurrency only in separate non-evaluation identities.

For an explicitly routed `claude-code:<model>` canary, Terminus 2 is a valid
downstream harness on builds that implement the Anthropic-to-subscription
bridge. Keep its normal ingress configuration unchanged: do not inject the
OAuth token, a Claude Code agent-profile beta, or a Claude Code identity prompt
into the sandbox; the central daemon owns all three upstream adaptations.
Reject the canary if a bare canonical Claude model reaches
the subscription or if any secret appears outside the central daemon.

## 7. Resolve or create the immutable control

Build the canonical control key from the frozen manifest and query the append-only control catalog.

### Matching accepted control exists

Verify artifact checksum, task/trial manifest hash, harness/model/protocol key, sandbox shape, raw usage, and pricing provenance. Record its artifact ID in every policy group. Launch zero control cases.

### No matching control exists

Compute the missing case/trial identities by exact key. Re-run all preflight gates. Launch each missing identity once through the same Terminus 2 and EC2 topology used by policy groups.

Append `started` atomically immediately before Harbor launch. After launch, the identity is permanently consumed. Store a complete accepted outcome or terminal failure in the catalog; never remove a poor result or rerun it.

Control uses a clean dedicated database, routes every request to the frozen strong model, disables policy learning, and receives no reward feedback.

## 8. Run policy rounds

Create a new policy database and use this exact sequence:

```text
r1 -> feedback -> r2 -> feedback -> r3
```

For each round:

1. create new daemon/process/log/trace/decision/outcome/artifact paths;
2. keep the same policy database and frozen concurrency;
3. send a non-scoring strong-route sentinel before the batch;
4. gate identity, quota, current usage, daemon health, provider health, and sandbox residue before each batch;
5. launch only the batch's `not_started` cases;
6. wait for every started process to reach a terminal state;
7. stop the daemon gracefully and allow the declared settlement grace;
8. reconcile usage and build the strict evidence bundle;
9. audit exact-tag AWS cleanup;
10. accept or reject the group independently.

Apply reward feedback once after accepted r1 and once after accepted r2. Snapshot policy state before and after feedback. Do not apply feedback after r3 for this evaluation lineage, after a rejected group, to a control, or to held-out evidence.

If an intermediate accepted round loses quality or costs more, continue through the predeclared r3 unless a severe stop limit was crossed. If a round fails evidence integrity, stop the lineage before feedback or the next round.

## 9. Reconcile and assemble evidence

Inspect `bitrouter workflow-state --help` at the pinned source revision. The established command family includes:

```bash
"$BITROUTER_BIN" workflow-state harbor-outcomes \
  --harbor-run-dir "$HARBOR_RUN_DIR" \
  --output "$OUTCOMES_FILE"

"$BITROUTER_BIN" workflow-state metering-usage \
  --database-url "$DATABASE_URL" \
  --since "$SCAN_START" \
  --until "$SCAN_END" \
  --output "$USAGE_FILE"

"$BITROUTER_BIN" workflow-state bundle \
  --run-label "$RUN_LABEL" \
  --traces "$TRACE_FILE" \
  --cloud-usage "$USAGE_FILE" \
  --outcomes "$OUTCOMES_FILE" \
  --policy-decisions "$DECISION_FILE" \
  --output-dir "$ARTIFACT_DIR"
```

Use the exact options supported by the pinned binary. A broad time window may locate candidates, but final membership is the manifest's exact request ID set. Require set equality and uniqueness; reject missing, duplicate, or extra usage.

For timeout or client-disconnect paths, use the pinned reconciliation interface to query authoritative receipts with the same stable request ID. Poll only within the frozen grace/attempt budget. Accept `computed` or authoritative `not_charged`; retain `pending` or `unknown` as a rejection, never as zero.

Immediately before freezing a Cloud lineage, fetch the authenticated `/v1/models` entry for every selected model and save only its public model and pricing fields as the price snapshot. Do not assume the OSS registry revision is current. Reconstruct every computed receipt with that exact snapshot; an `authoritative_charge_mismatch` is a fail-closed pricing-drift signal, not permission to estimate. A corrected postprocess may reopen the same receipt rows and rebuild missing artifacts only when it launches zero Harbor cases and preserves the original rejection evidence.

When Cloud inference authenticates from `account-credentials.json`, prefer the same owner-only credential store for receipt reconciliation if the pinned binary supports `--credentials-file`. This keeps OAuth refresh inside the process and avoids exporting the bearer. Otherwise declare the exact API-key environment source in the private manifest. A present credential file is not sufficient: a non-evaluation inference sentinel must prove that its refresh token is still accepted.

Build and validate the bundle before reward mutates policy state. Independently reconstruct it from the database/request IDs and compare totals. A postprocess-only recovery may use a separately pinned binary, but it must launch zero cases, preserve original artifacts, avoid routing-state mutation, and record its own hash and actions.

For accepted policy groups, apply feedback through the pinned `workflow-state apply-reward-feedback` interface and archive the result plus policy-state snapshots.

## 10. Resume safely

Use an append-only case state machine:

| State | Meaning | May launch? |
| --- | --- | --- |
| `not_started` | Harbor was never called for this identity | Yes, after all current gates pass |
| `started` | Identity was atomically consumed; process may need reconciliation | No |
| `terminal_valid` | Complete TrialResult and runtime evidence exist | No |
| `terminal_invalid` | Started, but runtime or TrialResult is incomplete | No |

A crash-resume entrypoint must derive candidates from the manifest and event log, then assert that every candidate is `not_started` and absent from Harbor outputs. Wait for all processes from an interrupted batch to settle before classifying them.

Do not infer "not started" from a missing final result alone. Inspect the event log, process record, Harbor job directory, EC2 tags, and provider request IDs. Preserve old attempts and use a new lineage after code/config fixes when the group is rejected.

Postprocessing recovery is different: it may reconcile or rebuild artifacts for already-started cases while launching no Harbor trial.

## 11. Clean up AWS resources

Set Harbor environment deletion true, then independently verify cleanup. Query by exact run tags and tracked resource IDs, not by name fragments or age.

A named-profile instance query can use:

```bash
aws --profile "$AWS_PROFILE" ec2 describe-instances \
  --region "$AWS_REGION" \
  --filters \
    "Name=tag:bitrouter-benchmark-run-id,Values=$RUN_ID" \
    "Name=tag:bitrouter-role,Values=sandbox" \
    "Name=instance-state-name,Values=pending,running,stopping,stopped" \
  --query 'Reservations[].Instances[].InstanceId' \
  --output json
```

Run equivalent exact-tag/ID checks for EBS volumes and elastic network interfaces. Follow attachments from tracked instances because tag propagation can differ by resource type. Terminate/delete only resources proven to belong to the run; never clean up by a broad project tag alone.

Acceptance requires:

- no live or stopped run-scoped sandbox instances;
- no available/in-use run-scoped volume outside the retained central host contract;
- no run-scoped network interface outside the retained central host contract;
- sandbox-count monitor peak at or below frozen concurrency and a final tail of zero.

Save the authenticated cleanup query, timestamp, redacted identity proof, and empty result. A cleanup failure rejects the group even when quality and cost data are complete.

## 12. Archive and scale concurrency

Archive accepted and rejected groups. Include manifests, sanitized configs, controller events, Harbor results/logs, daemon logs, traces, decisions, usage, outcomes, reconciliation records, evidence bundles, feedback records, policy snapshots, cleanup proofs, summaries, checksums, and a secret-scan result.

Do not archive live credentials, prompt/provider secrets, SSH keys, credential-store databases, or shell history. Keep request IDs, session IDs, timestamps, raw usage, decisions, and rewards needed for reproducibility unless a public privacy policy requires a documented transformation.

Raise concurrency with new non-evaluation canaries, not inside a scored lineage. A cautious sequence is 1 for session validation, then separately frozen 3, 4, 6, and 8 candidates. At each step require complete TrialResults, authoritative settlement, strict joins, acceptable provider latency/error behavior, observed peak equal to the declared value, and zero AWS residue. A successful canary authorizes only a future new lineage at that fixed value.

Once an operator freezes a value for a new lineage (for example 4 after a successful staged decision), write it into every manifest and controller-capacity field before launch and keep it unchanged across control and r1-r3. A one-case preflight may observe a peak of one even when its controller capacity is four; multi-case capacity validation must use enough predeclared non-evaluation cases to exercise the declared peak.
