# Benchmark Operations Q&A

Use this symptom-driven reference during preflight, execution, recovery, and publication. Every safe action preserves case identity and evidence rather than routing around a failed gate.

## Index

| Questions | Area |
| --- | --- |
| Q1-Q2 | Fixed method and smoke-versus-benchmark evidence |
| Q3-Q10 | AWS identity, quota, network, central host, and source pins |
| Q11-Q15 | Harbor, Terminus 2, session attribution, and terminal lifecycle |
| Q16-Q20 | Isolation, resume, postprocessing, controls, and trials |
| Q21-Q26 | Provider errors, settlement, cache-aware cost, and reward semantics |
| Q27-Q29 | Concurrency, cleanup, and publication |
| Q30-Q37 | Target-platform builds, persistent operators, Cloud/Claude OAuth, settlement, and protocol shaping |

## Q1. Which parts of the benchmark are fixed, and which are configurable?

**Symptom:** An operator copies an old run wholesale or proposes replacing EC2/Terminus 2 to fit the local environment.

**Cause / diagnostic:** The fixed method and environment inputs were not separated. Terminal-Bench 2.1, Harbor, Terminus 2, a central EC2 BitRouter daemon, per-trial ephemeral EC2 sandboxes, immutable controls, and strict evidence gates are fixed. Identity, account, region, source, models, providers, secrets, tasks, trials, prices, concurrency, and provision/reuse mode are configurable.

**Safe action:** Preserve the fixed rail, collect every configurable value into a new frozen manifest, and start a different explicitly labeled method if a fixed component must change.

## Q2. Why is a local Docker or direct-provider result not a benchmark result?

**Symptom:** A smoke run passes locally and is presented as evidence that a short/full run will pass.

**Cause / diagnostic:** Local tests omit EC2 sandbox creation, bootstrap, private daemon reachability, Harbor's EC2 adapter, AWS quota, and resource cleanup. Direct probes also omit the real Terminus 2 workflow and verifier.

**Safe action:** Use local tests for protocol/fixture smoke evidence, then run a non-evaluation canary and all scored trials through the fixed AWS topology before accepting benchmark evidence.

## Q3. How should different operators supply AWS IAM credentials?

**Symptom:** A command uses the ambient default identity, or instructions assume one person's credential path/profile.

**Cause / diagnostic:** The authentication mechanism was treated as a global default. Operators may use different named IAM access-key profiles, explicit role assumptions, credential processes, or central-host instance profiles.

**Safe action:** Freeze one explicit identity selector per run, prove it with STS, propagate it to the controller and every Harbor subprocess, record only a redacted identity proof, and never hardcode or archive the credential source/value.

## Q4. What if STS fails or returns an unexpected principal?

**Symptom:** `get-caller-identity` errors, resolves another account/role, or succeeds only after removing the explicit selector.

**Cause / diagnostic:** The selected credential is missing, expired, misconfigured, or silently falling back. Removing the selector proves ambiguity rather than fixing it.

**Safe action:** Stop before quota/EC2 calls and before any case is `started`. Repair the declared credential mechanism, repeat STS, and compare the exact account/principal with the private manifest.

## Q5. How is the EC2 quota gate calculated?

**Symptom:** Sandbox launches fail despite the account showing a positive quota.

**Cause / diagnostic:** The operator ignored the central host, sandbox vCPU shape, current running instances, or headroom. The relevant Standard On-Demand vCPU quota is `L-1216C47A` in the target region.

**Safe action:** Immediately before every batch, require available quota for live central-host vCPU plus frozen concurrency times sandbox vCPU plus declared headroom, after subtracting current regional use.

## Q6. What should happen on `VcpuLimitExceeded`?

**Symptom:** EC2 rejects `RunInstances`, sometimes after the controller has consumed a case identity.

**Cause / diagnostic:** Quota was checked too early or `started` was written before the quota/health gates.

**Safe action:** Gate immediately before each batch and append `started` only after quota, usage, daemon health, provider sentinel, and residue checks pass. If the identity is already `started`, preserve the terminal failure and never relaunch it; repair the controller for the next lineage.

## Q7. Why can an EC2 sandbox start but fail during bootstrap or SSH?

**Symptom:** The instance is running, yet package installation, Docker bootstrap, or controller SSH times out.

**Cause / diagnostic:** The subnet has no effective egress path, or the sandbox lacks the public IPv4/NAT route declared by the manifest. Route tables, Internet Gateway/NAT, security groups, network ACLs, DNS, and SSH source range may disagree.

**Safe action:** Choose public-IP, NAT, or prebuilt-image behavior explicitly; prove it with a disposable non-evaluation sandbox; restrict SSH to the controller source; and do not spend a scored identity on network diagnosis.

## Q8. Why can the sandbox not reach the central BitRouter daemon?

**Symptom:** Terminus 2 reports connection refused/timeout while central-host health is green locally.

**Cause / diagnostic:** The daemon listens only on loopback, the config uses a public/wrong address, the sandbox security group is not allowed, the port differs, or Harbor network policy omitted the private host.

**Safe action:** Bind the declared private interface, authorize only the sandbox security group/port, add the private daemon host to the agent's allowed hosts, and prove sandbox-to-daemon health before a scored batch.

## Q9. When is reusing a central host safe?

**Symptom:** Reuse saves time but produces port conflicts, old policy behavior, or mixed logs.

**Cause / diagnostic:** A previous process, database, socket, trace, decision log, controller event store, or credential remained active. Reuse was treated as permission to reuse run state.

**Safe action:** Verify instance/source/binary identity, kill only proven stale run processes, require new paths/ports/databases, repeat network and credential gates, and reject reuse if isolation cannot be proven.

## Q10. What if source commits or binary hashes differ from the manifest?

**Symptom:** The repo is at the right branch but the executable or Harbor behavior differs across groups.

**Cause / diagnostic:** A branch moved, a dirty patch was omitted, build flags/toolchain changed, or a binary was hot-swapped.

**Safe action:** Freeze full commits, patch/lock hashes, build command, and serving binary SHA-256. Stop the lineage on mismatch. Permit a separate postprocessing binary only with its own hash, zero case launches, and no routing-state mutation.

## Q11. Why does Harbor reject a generated config mentioning `agent` or `agents`?

**Symptom:** Pydantic reports a missing or extra agent field before launch.

**Cause / diagnostic:** Harbor's multi-case `JobConfig.agents` is plural, while a directly launched one-case `TrialConfig.agent` is singular.

**Safe action:** Generate the correct model shape deliberately and validate it through the pinned Harbor Pydantic class before the controller records `started`; never patch the field name after validation.

## Q12. Why does Terminus 2 ignore the BitRouter endpoint or report a missing API key?

**Symptom:** The agent calls another endpoint, or LiteLLM fails even though `OPENAI_API_KEY` is present.

**Cause / diagnostic:** In affected Harbor versions, `api_base` must be in `agent.kwargs`, and the non-secret daemon key must also be in `agent.kwargs.llm_kwargs.api_key`; environment-only configuration is insufficient.

**Safe action:** Inspect the pinned constructor, render and Pydantic-validate both fields, keep the upstream provider secret on the central daemon, and use only a non-secret local token in the sandbox/agent config.

## Q13. Why do request traces fail to join the Harbor outcome?

**Symptom:** Requests have timing/model data but the artifact reports Low/ambiguous session confidence or unmatched outcomes.

**Cause / diagnostic:** `x-bitrouter-workflow-session` is absent or differs from the Harbor outcome `session_key`. Prompt hashes, provider response IDs, and body metadata are weaker fallbacks.

**Safe action:** Generate one exact trial session value, place it in Terminus 2 `session_id` and the explicit header, capture a canary request, and assert equality with the emitted outcome before enabling parallelism.

## Q14. Why is time-window attribution unsafe with parallel trials?

**Symptom:** Multiple outcomes can plausibly own the same request because trial windows overlap.

**Cause / diagnostic:** The assembler used timestamps as identity rather than a high-confidence session/request key.

**Safe action:** Keep concurrency 1 until explicit sessions are proven. Use time only to bound scans; use stable request IDs and exact workflow sessions for final set membership and reward joins.

## Q15. What should happen when the Terminus 2 tmux session disappears?

**Symptom:** A model call returns, then `send-keys` fails because the pane/session ended.

**Cause / diagnostic:** The session may exit during the model wait, creating a time-of-check/time-of-use race. Recreating a pane would lose cwd, environment, and process state.

**Safe action:** Pin Harbor behavior that raises typed `TerminalSessionEnded`, stops agent interaction, and still runs the verifier when the environment is reachable. Do not recreate the pane; treat environment disconnect as runtime-invalid.

## Q16. Why must paths, databases, sockets, and ports be new?

**Symptom:** Counts exceed expected values, policy appears pre-trained, traces truncate, or a daemon binds the wrong state.

**Cause / diagnostic:** A partial/failed run was resumed in place or control and policy state overlapped.

**Safe action:** Allocate new run/group paths and ports, use a dedicated control database and one fresh shared r1-r3 policy database, archive the old attempt unchanged, and start a new lineage after implementation/config fixes.

## Q17. Which cases may a crash-resume entrypoint launch?

**Symptom:** A controller restart sees no final result and proposes rerunning that case.

**Cause / diagnostic:** Missing result was mistaken for `not_started`; the case may already have a `started` event, Harbor directory, process, EC2 resource, or provider request.

**Safe action:** Resume only identities proven `not_started` across manifest, append-only events, Harbor outputs, processes, EC2 tags, and request IDs. Never relaunch `started`, `terminal_valid`, or `terminal_invalid` identities.

## Q18. Can postprocessing repair an artifact without rerunning cases?

**Symptom:** Serving completed but settlement arrived late or the original bundler had a deterministic assembly bug.

**Cause / diagnostic:** Runtime evidence may be intact while postprocessing is incomplete; rerunning would violate identity immutability.

**Safe action:** Use a separately pinned/checksummed recovery binary that launches zero cases, selects the original stable request IDs, preserves original files, records every mutation/query, and does not change the serving database or routing state.

## Q19. What if the control catalog has no match or only a partial match?

**Symptom:** Policy work is ready, but exact control case/trial identities are absent, failed, or use another harness/model/sandbox key.

**Cause / diagnostic:** Control reuse requires exact key equality; policy code revision alone neither matches nor invalidates a control.

**Safe action:** Reuse accepted matching identities, preserve terminal failures, and launch only truly absent identities once after all gates pass. Never rerun accepted/failed controls or substitute a near-match silently.

## Q20. Are five public trials retries of one case?

**Symptom:** The runner uses Harbor retries to reach a five-trial target or discards failed samples.

**Cause / diagnostic:** Predeclared trials are independent identities; retries are failure-driven extra attempts with different statistical/cost meaning.

**Safe action:** Declare every trial identity before launch, keep mechanism retries at zero, preserve every failure, and label/report any protocol-permitted replacement attempt and its cost separately.

## Q21. How should provider `401` or `403` errors be handled?

**Symptom:** Strong/economy sentinels or benchmark requests return authentication/authorization errors.

**Cause / diagnostic:** Credential expiry, wrong secret source, missing entitlement, protocol/base-URL mismatch, or accidental credential forwarding is more likely than model incapability.

**Safe action:** Stop the group, inspect redacted provider logs and secret-source metadata, refresh/repair credentials outside configs, repeat non-evaluation sentinels, and start a new lineage if any scored identity was consumed.

## Q22. How should `429`, `5xx`, and timeout failures be interpreted?

**Symptom:** Weak calls fail under concurrency, then strong fallback completes the task.

**Cause / diagnostic:** Rate limits, upstream availability, or client timeout are provider reliability signals, not direct semantic evidence. Request IDs and provider timing distinguish transient failure from model output inadequacy.

**Safe action:** Preserve the failed request/charge, write reliability evidence, respect frozen circuit/backpressure rules, and do not award semantic success to the failed route. Change concurrency only in a new canary/lineage.

## Q23. What if a timed-out request remains `pending`?

**Symptom:** The local client stopped waiting but the provider may complete and charge later.

**Cause / diagnostic:** Client timeout is not authoritative settlement. The upstream receipt may still transition to `computed` or `not_charged` under the same request ID.

**Safe action:** Poll the pinned receipt/reconciliation interface within the frozen grace budget. Accept only authoritative terminal status; if it remains `pending` or becomes `unknown`, reject cost evidence and do not apply feedback.

## Q24. Why require all four usage buckets even when cache writes are zero?

**Symptom:** An exporter provides prompt/output totals but omits cache fields, or a report treats absence as zero.

**Cause / diagnostic:** Cached input can materially change cost, and an observed zero differs from a missing field. The required fields are `uncached_input_tokens`, `cache_read_tokens`, `cache_write_tokens`, and `output_tokens`.

**Safe action:** Require numeric authoritative values for every field on every request, retain observed zeros, report reasoning/fixed charges separately, and reject rows whose cache split cannot be reconstructed.

## Q25. Which price applies when a cache-read or cache-write rate is missing?

**Symptom:** A row cannot be priced, or code borrows a rate from another route.

**Cause / diagnostic:** Price precedence was not frozen. Explicit route-specific cache rates must win; only declared product behavior may fall back to that same route's valid base input rate.

**Safe action:** Use explicit cache price first, documented same-route base input second, otherwise mark unknown. Never use another model's price, an invalid base, zero, or a historical average.

## Q26. Why separate provider reliability from semantic reward?

**Symptom:** A successful task gives positive evidence to a weak request that timed out before strong fallback solved it, or a timeout permanently marks the model incapable.

**Cause / diagnostic:** Transport outcome and verifier task quality were written into one ledger without request-level eligibility.

**Safe action:** Write timeout/429/5xx to a provider/model/credential/region/protocol reliability key. Write semantic evidence only for successfully completed, attributable, authoritatively settled requests tied to verifier reward.

## Q27. Can concurrency be raised from 3 to 4, 6, or 8 during a run?

**Symptom:** Extra regional quota becomes available and the operator wants to speed up r2/r3 mid-lineage.

**Cause / diagnostic:** Concurrency changes provider pressure and runtime conditions, breaking comparability. Prior canaries in another environment are not authorization for this one.

**Safe action:** Keep the lineage's frozen value. Test 4, then 6, then 8 gradually in separate non-evaluation identities, require strict settlement/joins/cleanup at each step, and use a successful value only for a new lineage.

## Q28. Why does cleanup fail when Harbor says `delete: true`?

**Symptom:** Harbor exits but stopped instances, detached volumes, or network interfaces remain.

**Cause / diagnostic:** Adapter cleanup can be interrupted, tag propagation may differ, or broad queries miss attached resources. Harbor logs are not independent AWS evidence.

**Safe action:** Query instances, volumes, and interfaces by exact run tags plus tracked IDs/attachments, delete only proven run resources, preserve the authenticated empty result, and reject the group until residue is zero.

## Q29. What changes between an internal run and a public reproduction?

**Symptom:** A one-trial tuning result is prepared for publication as a stable model benchmark, with mixed subscription/list-price costs or an over-broad archive.

**Cause / diagnostic:** Internal mechanism evidence may use short13/~20 tasks and one trial per case; public reproduction uses all 89 tasks and five predeclared trials, declared held-out semantics, provenance, and uncertainty. Actual charge and notional list-price cost are different series, and raw archives may contain secrets.

**Safe action:** Match the claim to the run class, report tuning versus held-out behavior, separate actual/notional economics, publish every round/failure and registry provenance, sanitize credentials/keys/private paths without deleting request/session/usage evidence, checksum the release, and secret-scan both upload and download.

## Q30. Why does a binary with the expected SHA fail with `Exec format error` on EC2?

**Symptom:** The serving binary hash matches the operator's manifest, but the central Linux host refuses to execute it.

**Cause / diagnostic:** SHA-256 proves byte identity, not target compatibility. The artifact was built on macOS or for another architecture and then copied to an `x86_64` Linux central host. Non-interactive SSH may also omit `~/.cargo/bin` from `PATH`, hiding the actual central Rust toolchain.

**Safe action:** Record `uname -m`, build the exact commit on the target host or with a pinned matching cross-toolchain, invoke the recorded absolute Cargo path, and require both the new SHA-256 and an ELF/architecture check before any provider preflight or run identity is created. Never reuse the incompatible artifact's path or hash.

## Q31. Why must a benchmark process not live inside the operator's SSH session?

**Symptom:** A direct SSH command works for short probes but the Tailscale/ProxyJump connection closes during a model call or Harbor trial, leaving the operator unsure whether the remote process survived.

**Cause / diagnostic:** The control connection became the job supervisor. Jump-host banner latency and transport resets are independent of Harbor, the daemon, and EC2 lifecycle; a child started in a new process session may even outlive a killed parent.

**Safe action:** Run provider canaries and benchmark groups under a named persistent central supervisor such as tmux or systemd. Freeze the command before launch, retain output and exit status, and let SSH perform read-only polling. After any disconnect, first audit the exact PID, port, run root, event log, and EC2 tags; never launch a second attempt merely because the local SSH client lost contact.

## Q32. Why must failed preflights keep their logs?

**Symptom:** A direct provider sentinel returns non-2xx, but a temporary-directory cleanup deletes the daemon log and response body before diagnosis.

**Cause / diagnostic:** The preflight was treated as disposable even though it is the evidence that protects scored identities from installation, protocol, credential, and entitlement failures.

**Safe action:** Give every preflight a fresh immutable non-evaluation identity and owner-only directory. Preserve config validation, daemon log, sanitized response, trace, and exactly one `PREFLIGHT_ACCEPTED` or `PREFLIGHT_REJECTED` marker. Refuse in-place reruns; after a fix, advance the preflight identity while leaving scored case identities untouched.

## Q33. Why can BitRouter Cloud routing return `invalid_grant` even though a credential file exists?

**Symptom:** The built-in Cloud provider finds `account-credentials.json`, attempts OAuth refresh, and returns a 401/502 chain ending in `invalid_grant`; receipt reconciliation may separately ask for `BITROUTER_API_KEY`.

**Cause / diagnostic:** Presence and mode `0600` do not prove that an OAuth refresh token is still accepted. A copied or rotated credential can be stale. Older reconciliation CLIs accepted only an exported inference key even though the provider itself could consume the protected OAuth store.

**Safe action:** Preserve the rejected preflight, run a new interactive `bitrouter cloud login` device flow on the central credential store, and never print/copy the resulting bearer. Use a pinned reconciliation interface that accepts the same protected credential file (or a declared API-key environment source), then advance to a new preflight identity and require both real inference and authoritative receipt settlement before a scored launch.

## Q34. Why can a Cloud receipt be computed but reconciliation still fail with `authoritative_charge_mismatch`?

**Symptom:** The receipt contains the same request ID, model, provider, and four token buckets as the local row, yet strict reconciliation stores it as `unknown` and rejects the artifact.

**Cause / diagnostic:** First recompute the charge from the serialized frozen rates with decimal arithmetic and the provider's documented final-rounding rule. If that result differs from the receipt, the price snapshot is stale: Cloud prices can change independently of the OSS registry revision. If decimal recomputation matches the receipt but the pinned binary differs, the defect is local charge arithmetic. In particular, output and reasoning tokens commonly share one output rate; multiplying them as separate binary floating-point buckets can move an exact half-micro-USD boundary below the value the provider rounds.

**Safe action:** For stale prices, fetch the authenticated Cloud `/v1/models` record immediately before freezing a lineage, retain only public model/pricing fields in an owner-only snapshot, and reconcile with those exact rates. For arithmetic divergence, add an exact failing receipt case, combine token classes that share one rate before multiplication while preserving each class's trust cap and audit fields, then deploy a new immutable postprocessing binary. In both cases, treat the receipt as the charge source and frozen rates as an integrity cross-check; archive the rejected staging and use a zero-launch postprocess recovery when the case and TrialResult are complete. Never loosen equality, add an epsilon, rewrite SQLite, or rerun a valid trial to hide the mismatch.

## Q35. Can Terminus 2 use a Claude Code subscription as its model provider?

**Symptom:** A fixed route points Terminus 2 at `claude-code:<model>`, but an older BitRouter build rejects the request for a missing Claude Code agent-profile beta.

**Cause / diagnostic:** Terminus 2 is a normal downstream client and should not originate Claude Code credentials, identity headers, or the Claude Agent SDK identity system block. Current BitRouter treats the explicit `claude-code:<model>` target as the operator's subscription-use boundary and constructs the OAuth-compatible Claude Code request on the upstream side, preserving the client's original system instructions. The older rejection—or a generic upstream 429 when the official CLI succeeds with the same token/model—means the serving binary predates the complete bridge, or the request did not resolve to the explicit provider-qualified target. A bare canonical Claude model still cannot auto-cascade onto a personal subscription.

**Safe action:** Pin and verify a bridge-capable BitRouter source/binary, set the long-lived `CLAUDE_CODE_OAUTH_TOKEN` only in the protected central daemon environment before startup, and use an explicit `claude-code:<model>` target. Keep the token, Claude Code agent-profile header, and injected identity block out of Harbor, Terminus 2, sandbox, manifest, and artifact configuration. First run a no-beta standard Anthropic sentinel, then a real Terminus 2 EC2 canary; accept only with the intended provider/model trace, four-bucket settlement, strict cost/reward/session joins, TrialResult, and zero AWS residue.

## Q36. Why does a Terminus 2 Anthropic canary return 404 without adding a BitRouter trace?

**Symptom:** LiteLLM raises `NotFoundError`, its masked URL ends in `/v1/v1/messages`, and the daemon trace count remains at the direct sentinel only.

**Cause / diagnostic:** The Anthropic handler appends `/v1/messages` to `api_base`. Reusing the OpenAI-style `http://host:port/v1` base therefore duplicates the version segment and misses BitRouter's `/v1/messages` route before routing, metering, or settlement can begin.

**Safe action:** For `model_name: anthropic/<model>`, set `api_base` to `http://host:port` with no `/v1` suffix. Keep `/v1` for OpenAI-provider Terminus 2 configurations. Validate the frozen TrialConfig, run a fresh immutable non-evaluation identity, and preserve the failed identity rather than retrying it.

## Q37. Why does Anthropic reject `extra_body` after the Terminus request reaches BitRouter?

**Symptom:** Route traces appear, but every model call fails with `invalid_request_error: extra_body: Extra inputs are not permitted`; the traced inbound body contains `extra_body.session_id`.

**Cause / diagnostic:** Terminus 2 passes its session into Harbor's LiteLLM wrapper. Some Anthropic-handler versions serialize the provider-extension container instead of merging it, but Anthropic Messages does not accept `extra_body` or a body-level `session_id`. The same session is already present in the immutable BitRouter workflow headers.

**Safe action:** Use a bridge-capable BitRouter build that merges non-conflicting `extra_body` extensions into the outbound body, gives explicit top-level fields precedence, and drops `session_id` only after request/session tracing has captured the headers. Do not patch the scored artifact or retry the consumed identity; rebuild, freeze a new binary and run identity, and repeat the non-evaluation canary.
