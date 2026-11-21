# Request evidence and model Policy evaluation

Gateway admission and settlement own request identity and usage. Optional
[trajectory capture](../CLI.md#bitrouter-trajectory) records structural request
evidence. BRO retains its own thread history, approvals and tool effects in
its local home.

## Evaluation and policy publication

External evaluators run inference against a specific configuration and submit
subjects and results. The gateway validates evidence and authority, retains
evidence and can freeze it into a snapshot. An HTTP success or reported usage
does not establish task quality.

Subjects, results and frozen snapshots carry `spec_version: eval/1.0.0-beta.1`.
Tenant model-policy evidence binds the installation, tenant, configuration
revision, exact Policy resource and selected name. Generic tenant and gateway
subjects have separate scopes. Preserve the identities returned by the
gateway when attributing outcomes, including unavailable usage.

An operator uses frozen evidence to author [model Policy](../specs/policy/README.md)
and publishes it with compare-and-swap. A tenant configuration operation
selects the exact immutable revision to activate. See [Eval CLI](../CLI.md#bitrouter-eval)
and [tenant service ownership](tenant-service.md#model-only-policy-boundary).
