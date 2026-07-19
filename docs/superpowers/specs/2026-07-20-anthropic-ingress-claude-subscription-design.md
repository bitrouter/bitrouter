# Anthropic Ingress to Claude Subscription Design

## Goal

Allow a non-Claude-Code agent, including Terminus 2, to call BitRouter through
the standard Anthropic Messages API while BitRouter routes an explicitly pinned
request to a Claude Pro/Max subscription and emits the OAuth-compatible request
shape expected by the Claude Code upstream.

The work is complete only when both `claude-code:claude-fable-5` and
`claude-code:claude-sonnet-5` pass a real Harbor + Terminus 2 + central EC2
BitRouter daemon + ephemeral EC2 sandbox canary with strict trace, four-bucket
usage, reward/cost join, TrialResult, and AWS cleanup evidence.

## Product boundary

BitRouter exposes the Anthropic Messages API to downstream clients. Downstream
clients are not required to identify as Claude Code and must not synthesize
Claude Code-specific headers. When the resolved target is explicitly qualified
as `claude-code:<model>`, BitRouter is responsible for adapting that normal
Anthropic request to the Claude Code subscription protocol.

An explicit `claude-code:<model>` target is the operator's authorization to use
the subscription. It may originate from a fixed policy tier, a direct
provider-qualified model, or an explicit provider-only preference. A bare
canonical Claude model is not authorization: subscription providers remain
excluded from automatic canonical-provider cascade.

Genuine Claude Code traffic retains its convenience path. The ingress
`ClaudeCodeRouter` may continue detecting `anthropic-beta: claude-code-*` and
rewriting a bare Claude model to `claude-code:<model>`. That detector is an
automatic-routing hint, not an authentication gate on an already explicit
subscription route.

## Request transformation

For every request whose resolved provider is `claude-code`, the subscription
auth applier performs the following outbound transformation:

1. resolve a Claude OAuth credential, preferring the process-local
   `CLAUDE_CODE_OAUTH_TOKEN` when present;
2. set `Authorization: Bearer <oauth-token>`;
3. remove `x-api-key` so the request carries only one upstream auth scheme;
4. set the pinned `anthropic-version`;
5. merge the required `claude-code-*` and OAuth beta values with any downstream
   feature betas, preserving feature flags without duplication;
6. set the Claude Code-compatible `user-agent` and `x-app` values;
7. forward the Anthropic Messages body faithfully.

The applier must no longer reject a request merely because the downstream
client did not send a Claude Code agent-profile beta. Credential absence still
fails closed with `401`. Explicit route resolution, subscription exclusion from
automatic cascade, and normal BitRouter authentication remain unchanged.

## Credential handling

`CLAUDE_CODE_OAUTH_TOKEN` is a long-lived, non-refreshable process credential.
It is captured in memory when the auth applier is constructed and therefore
must be present before the central daemon starts.

The token must never be placed in Git, BitRouter YAML/JSON, Harbor TrialConfig,
controller manifests, command arguments, traces, logs, documentation, or chat
output. Deployment uses a protected owner-only environment file on the central
host. The value is delivered through a non-echoing standard-input path, the
file is written atomically with mode `0600`, and operational checks report only
`present` or `missing`.

## Test strategy

The code change is test-driven:

- a standard Anthropic request without an inbound Claude Code beta succeeds on
  an explicit `claude-code` target when an OAuth credential exists;
- the outbound request contains Bearer auth, no `x-api-key`, the required beta
  values, the pinned Anthropic version, and Claude Code client headers;
- downstream feature betas remain present after the required values are added;
- no credential still returns `401`;
- genuine Claude Code ingress auto-routing remains unchanged;
- a bare canonical Claude request still cannot cascade onto a subscription
  provider;
- environment-token auto-enable and non-refreshing precedence remain covered.

Documentation tests must keep English and Chinese product pages in lockstep.
The reusable benchmark skill must explain that Terminus 2 is compatible only
through an explicit `claude-code:<model>` route on a BitRouter build that owns
this translation, and must retain the credential-secrecy rules above.

## Deployment and EC2 verification

Build one release Linux binary from the verified source commit and deploy it to
the retained central EC2 host under a content-addressed filename. Record only
the source commit and binary SHA-256. Do not replace the binary or configuration
inside an active canary lineage.

The live validation sequence is:

1. prove the dedicated AWS IAM access-key profile and `us-east-2` quota;
2. inject the long-lived OAuth token into the protected central-daemon
   environment without echoing it;
3. run a direct standard Anthropic Messages sentinel for each Claude model,
   deliberately omitting Claude Code-specific inbound headers;
4. run one immutable non-scoring Terminus 2 canary per model through Harbor,
   the central daemon, and an ephemeral EC2 sandbox;
5. keep controller capacity at 4 and use a four-case capacity canary only after
   both one-route canaries are stable;
6. verify exact provider/model traces, TrialResult, four usage buckets,
   terminal subscription settlement, strict cost/reward/session joins, and
   zero EC2 instance/EBS/ENI residue;
7. preserve rejected evidence and use a new immutable run identity after every
   code, binary, configuration, or input change.

No scored Terminal-Bench case is consumed by this validation. A real EC2
Terminus 2 TrialResult is mandatory; a local unit test or direct curl response
alone is insufficient.

## Acceptance criteria

The feature is accepted only when all of the following are true:

- focused Claude provider, routing, policy, and workflow-state tests pass;
- full repository test, Clippy, formatting, registry, and documentation gates
  required by `AGENTS.md` pass;
- the shipped docs describe explicit subscription routing consistently in
  English and Chinese;
- no credential literal or derivative appears in the Git diff or retained
  evidence;
- direct no-beta Anthropic sentinels succeed for Fable 5 and Sonnet 5;
- both real Terminus 2 EC2 canaries are strictly accepted;
- a capacity-4 EC2 canary proves the controller can hold four simultaneous
  sandboxes without retry or cleanup regression;
- every exact run identity has zero live instance, volume, and ENI residue.
