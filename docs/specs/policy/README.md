# Model-routing policy

Status: **accepted, `policy/1.0.0-beta.1`**.
This is the canonical contract for
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

`spec_version` is required and exactly `policy/1.0.0-beta.1`. Readers reject
missing versions, unsupported revisions and versions from other namespaces. Unknown fields at
document, policy and target levels are errors. Optional fields use only the
defaults declared by the public types. Null is not interchangeable with omission
except for optional nullable fields represented by `Option`. Duplicate keys at
every nesting level MUST be rejected from raw JSON/YAML before a map or generic
JSON value can discard them.

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
Targets cannot refer recursively to another BitRouter router.

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

The optional `routes` map associates a typed `RouteClass` with a tier name.
Each `RouteClass` combines a `TaskFamily`, a `NextStep` and a `Risk` and
serializes as one case-sensitive key, `<task-family>|<next-step>|<risk>`.
For example, `code:debugging|verify|guarded` is one key.

Families are `code:generation`,
`code:debugging`, `code:review`, `code:sql_database`, `code:frontend_ui`,
`code:devops_config`, `code:repository_analysis`, `agent:multi_step_planning`,
`agent:workflow_execution`, `agent:web_research`, `agent:memory_operations`,
`agent:general`, and `unknown`. Steps are `orchestrate`, `implement`,
`mechanical`, `verify`, `finalize`, and `unknown`. Risks are `normal` and `guarded`. Unknown labels, missing or extra segments
and repeated keys are rejected. The generated schema constrains keys with
`propertyNames`.

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
management. A route binds the full immutable `ResourceRef` and literal selected
name. `auto` and `auto:cost` are independent names; selection requires
an exact name match. Updating the managed head does not retarget an old route/configuration. Configuration
activation is explicit and uses its existing authority revision/CAS. Drafting,
evaluation or compilation does not activate a policy.

Files, defaults, HTTP, SDK, shared publication, reads and exports use the same
typed `policy::Document`. `Definition::Policy.document` and source-bearing reads
use that type; reported targets use `policy::Target`. Store normalized source
and separate management metadata, then build a private model-selection view.
Provider secrets are excluded.
Qualification covers the complete reachable closure at import, configuration
creation, activation, restart and execution reopening, including hidden and
unselected dependencies. Experimental content fails with `capability_mismatch`.

| Identity | Meaning |
| --- | --- |
| `source_digest` | `sha256:` plus lowercase SHA-256 of compact UTF-8 JSON of normalized typed source; recursively sort object keys by UTF-8 bytes, retain array order, use typed omission/default rules and JSON string escaping without ASCII-only conversion |
| Compiled cache identity | Private compiler identity, source digest, literal selected name, classifier contract and exact authorized route/dependency identities; never a resource revision |
| Policy revision | Full immutable ResourceRef: owner kind/ID, resource kind/ID and revision ID |
| Configuration revision | Immutable configuration selecting the resource/name; activation authority revision and expected-head CAS remain separate |
| `classifier_digest` | Installed classification contract; never a resource revision or assessment cohort |

The source digest excludes paths and management metadata. Equivalent YAML/JSON
and object order have one digest.

Publication takes explicit CAS parameters, never an artifact `parent_digest`.
Rollback reactivates current-contract source/revision and preserves concurrency
rejection and atomic file replacement; it does not load old snapshots. Source
grants no publication permission. Active is the selected running instance's
default view; disk is explicitly selected and cannot replace a failed active or
remote read. Authorized source reads return normalized source, literal selected
name and exact resource status; the source can be resubmitted directly.

A [model-policy Eval subject](../eval/README.md#subjects-and-ownership) binds
evidence to an exact revision. After evaluating that revision, an authorized
client may submit and freeze facts, author new source, create a new resource
revision and explicitly activate a new configuration with CAS.

## Conformance

Conforming implementations test source/schema admission, wrong namespaces and
unsupported beta revisions, unknown field rejection, invalid
references/classes/classifier digests, normalized source round trips, exact
revision activation, actual model dispatch and context preservation on default,
class, tool and bypass paths. Management tests cover authorization, replay/CAS,
restart, revocation and immutable old configurations. Shared publication uses
the same admission rules as tenant submission.

Conformance cases include raw duplicate-key refusal, equivalent YAML/JSON
source digests, source read/resubmission, literal selected-name failure, hidden
experimental closures and Policy-free experimental routes in minimum and
all-feature builds. Eval facts for same-name different-revision policies remain
distinct; generic and cross-endpoint profiles cannot impersonate them. These
cases are required for conformance. Current-format recovery remains required;
old stores are refused without conversion or inference replay under the
[store contract](../service-api/README.md#resource-and-evidence-migration-and-rollback).
