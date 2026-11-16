# Model-routing policy

Status: **beta, `policy/1.0.0-beta.1`**. This is the canonical contract for
portable model-only policy source, authored as JSON or YAML and submitted to
the service through JSON. Consumers are policy authors, SDK clients, gateway
publication, tenant resource management and the serving compiler. MUST, MUST NOT,
SHOULD and MAY express normative requirements.

The public types in [`bitrouter-ai::policy`](../../../crates/bitrouter-ai/src/policy.rs)
and the generated [schema](policy.schema.json) describe the wire fields. The
service additionally validates references and execution semantics. Schema
acceptance alone does not authorize or qualify a policy for execution.
Regenerate the schema with `cargo run -p bitrouter-ai --features schema
--example policy_schema`; the contract test checks the committed output.

## Identity and compatibility

`spec_version` is required and exactly `policy/1.0.0-beta.1`. There is no
unversioned, numeric-vx or namespace substitution fallback. Unknown fields at
document, policy and target levels are errors. Optional fields use only the
defaults declared by the public types. Null is not interchangeable with omission
except for optional nullable fields represented by `Option`.

This contract replaces the service's former direct acceptance of
`lockfileVersion` documents. Old context actions, compiler lineage, certificates,
guards, adequacy settings and optimization state are not Policy source fields.
They cannot be renamed into this contract. Unsupported beta revisions require
an updated reader or explicitly authored supported source; no automatic downgrade
or compatibility converter is provided.

Changing a policy's model or rule produces a new resource revision, not a new
contract revision. Tenant and installation identity, immutable revision,
authorization, activation and expected-head CAS belong to the service envelope.
The document is portable and contains no session, credential, tenant or path
authority. Model selectors resolve only against the explicitly authorized route
source. A document cannot introduce an upstream credential or ambient provider.

## Admission

The serialized normalized source is at most 1 MiB. It contains 1–256 named
policies. Policy names are at most 256 UTF-8 bytes, use ASCII letters, digits,
dot, underscore and hyphen in one or two colon-separated segments, and neither
segment starts with a dot. Each policy has 1–256 tiers; tier names are nonempty
and at most 256 bytes. Each model selector is nonblank and at most 512 bytes.
Targets cannot refer recursively to another BitRouter router or preset.

`default_tier` is required and names an existing tier. Every explicit route,
tool fallback and tool-safe entry names an existing tier. Duplicate model/effort
targets under different tiers are rejected. There are at most 4,096 explicit
classification routes and 256 tool-safe entries per policy.

For example:

```yaml
spec_version: policy/1.0.0-beta.1
policies:
  auto:
    tiers:
      economy: {model: 'example:small'}
      strong: {model: 'example:large', effort: high}
    default_tier: economy
    tool_use_tier: strong
    tool_safe_tiers: [strong]
```

These selectors illustrate format; an installation must explicitly provision
their models, protocols and credentials before a bound route can be compiled.

## Model selection

Every tier preserves the complete admitted request context. A policy may select
a generation model and optional reasoning effort. It MUST NOT compact, summarize,
replace or route context, execute tools, own a harness session, or create an
agent. Preservation also applies to no-match and bypass paths. Normal inference
protocol conversion and provider fallback remain gateway responsibilities.

Explicit classification routes are optional. Their keys have exactly the form
`<task-family>|<next-step>|<risk>`. Families are `code:generation`,
`code:debugging`, `code:review`, `code:sql_database`, `code:frontend_ui`,
`code:devops_config`, `code:repository_analysis`, `agent:multi_step_planning`,
`agent:workflow_execution`, `agent:web_research`, `agent:memory_operations`,
`agent:general`, and `unknown`. Steps are `orchestrate`, `implement`,
`mechanical`, `verify`, `finalize`, and `unknown`. Risks are `normal`, `context`
and `guarded`; these are classification labels and grant no context operations.

A policy with routes requires `classifier_digest`, exactly matching the serving
classifier's rubric/configuration contract digest. A different digest is rejected
before mutation; it is not replaced with the current binary's digest. A policy
without routes omits this field. This digest is distinct from an assessment
cohort's backend/threshold identity. Classifier contract changes require an
explicit source update. Runtime classification can abstain and is not a claim
of deterministic model output.
Clients obtain the installed digest from the protected
`GET /control/v1/tenancy/policy-capabilities` endpoint, bound to the addressed
installation. Discovery does not grant publication or activation authority.

Lookup uses the exact class, then the same step/risk with unknown family, then
`default_tier`. Tool-bearing requests selecting an unsafe tier use
`tool_use_tier` when configured; that fallback must also appear in
`tool_safe_tiers`. Without a tool fallback this guard is disabled. An explicit
caller effort remains a constraint; a policy target's effort applies only where
the inference contract permits the override. Absence preserves caller effort.

## Management and evaluation

Policy submission creates an immutable revision through existing resource
management. A route binds the exact revision and selected policy name. Updating
the managed head does not retarget an old route/configuration. Configuration
activation is explicit and uses its existing authority revision/CAS. Drafting,
evaluation or compilation does not activate a policy.

Authorized source reads return the normalized document and selected name with
the exact resource status. Provider secrets and generic resource definitions
are not disclosed. Experimental artifacts without a complete lossless source
representation fail with `capability_mismatch`.

The compiler may produce private routing IR, but must retain every meaningful
source distinction and validate that representation on reload. Internal adapter
certificates attest submitted rules only: zero evidence counts, no quality or
promotion claims. They do not grant installation-operator authority.

Existing execution/evaluation evidence identifies the immutable policy resource
and the compiled policy digest. That digest is a runtime-IR identity, not a
public-source digest, Policy format version or evidence of learned quality.
Evaluation cannot silently publish or activate a replacement.

## Conformance

Conforming implementations test source/schema admission, wrong namespaces and
unsupported beta revisions, legacy and experimental field rejection, invalid
references/classes/classifier digests, normalized source round trips, exact
revision activation, actual model dispatch and context preservation on default,
class, tool and bypass paths. Management tests cover authorization, replay/CAS,
restart, revocation and immutable old configurations. Shared publication uses
the same admission rules as tenant submission.
