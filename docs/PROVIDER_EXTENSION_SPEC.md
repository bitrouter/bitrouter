# Native provider extensions for evaluation models

Status: **approved architecture; implementation in progress on PR #942.**
The current branch passed 3,596 local nextest tests, strict CI-form Clippy,
format, registry/schema checks, SDK no-config compilation and warning-free
public docs. Hosted CI and credentialed TypeSafe smoke remain outstanding;
this document does not authorize merging or releasing the implementation.

Date: 2026-09-23

Related: [evaluation model spec](EVALUATION_MODELS_SPEC.md),
[shared host extension design](HOST_EXTENSION_DX_SPEC.md), and
[#942](https://github.com/bitrouter/bitrouter/pull/942).

## 1. Decision and scope

BitRouter should support a **compiled, provider-owned extension**. An extension
contributes one provider identity, its executable model/operation claims, and
only the typed operation implementations it actually supports. TypeSafe is the
first such extension. Its System One JSON encoding is a private implementation
detail of that provider extension, not a public `system-one/json@1` format
registration or a configuration binding.

This is Pi-like in *provider ownership and authoring boundary*, not in runtime
installation. The default `bro` executable explicitly links and registers the
reviewed first-party TypeSafe extension at build time. Linking it is not the
same as activating a provider account: an uncredentialed or disabled TypeSafe
route remains unavailable. Third-party extensions may still use the shared
custom-host composition API, but they are not automatically linked into
`bro`. There is no dynamic library loader, WASM runtime, marketplace,
extension installer, or hot loading in this increment.

This proposal retains the public decisions in §4, §5.1–5.2, and the applicable
failure/usage/security rules in §7–9 of the evaluation model spec. In
particular:

- `POST /v1/evaluate` is the only new inbound endpoint. There is no inbound
  `/v1/systemone`; TypeSafe's `/v1/systemone` remains an **outbound** call.
- The public body uses the OpenRouter Decisions-inspired `noul`, `choice`, and
  `score` vocabulary. OpenRouter is a shape reference, not a provider route or
  a compatibility guarantee.
- `typesafe/jev-1.13` is the sole initial, version-pinned canonical model;
  `typesafe:typesafe/jev-1.13` pins the provider. The upstream wire model id is
  `jev-1.13.0`. No `typesafe/jev-latest` or Laya support is planned here.
- Unknown top-level request fields are accepted and ignored in this release.
  No default question-count or question-id-byte cap is imposed before
  server-side measurement. Existing shape validation and upstream limits still
  apply.
- Evaluation remains a separate, non-streaming operation. An evaluation-only
  model cannot enter a generation path, and a generation-only model cannot
  enter `/v1/evaluate`.
- The default `bro` always serves the authenticated `/v1/evaluate` route. With
  no active evaluation provider, a valid request gets the specified
  `evaluation_model_not_found` response (404), not a missing endpoint; the
  model list still contains no inactive Jev route.

**Precedence:** the existing evaluation spec describes the historical
format-first candidate, not the current provider-extension architecture.
This document replaces that spec's
format-first architecture (§1 items 10–11, §2's format-adapter boundary,
§3 goals 9–10, §5.3–5.6, §6's `operations.evaluate.format` binding, §10's
format-oriented Phase 2–4, and affected acceptance/rejected-alternative/review
statements). The public HTTP contract and compatible phase evidence remain;
the new provider-extension acceptance gates below govern the refactor.

## 2. Why provider ownership is the right unit

The in-flight design lets the `system-one/json@1` adapter render and parse
JSON while provider configuration owns TypeSafe's endpoint, credentials,
wire model id, and executable claim. That separation is valuable when two
independent providers have a *verified* identical protocol. It is premature
for the current case: the only demonstrated implementation is direct TypeSafe,
and the author has to coordinate a format crate, central provider metadata,
SDK registration, provider-registry gating, and a separate host crate to make
one provider work. It also makes a third-party provider difficult to add
without editing BitRouter's central registry.

The new unit is `typesafe`, not `system-one`. The extension owns TypeSafe's
upstream API semantics, including request rendering and successful-response
interpretation. The host still owns common HTTP error classification, routing
policy and resource authority. Reuse of a wire codec can be a private
Rust module or ordinary dependency; it does not imply that another provider
is safe to route until its semantics are independently verified.

This is **not** a single universal provider hook. The first author API exposes
one typed `evaluate` facet. A later provider may implement another operation
only when that operation has a canonical request/result contract and a real
consumer. Avoid `execute(operation: String, body: Value) -> Value`, speculative
facet families, or provider branches inside the TypeSafe implementation.

## 3. Ownership and dependency direction

| Owner | Responsibility | Must not own |
| --- | --- | --- |
| `bitrouter-sdk` | Canonical `EvaluateRequest`/`EvaluateResult`, operation identity, a small provider-author registration contract, host-governed outbound-call context, typed error/usage result | TypeSafe or System One knowledge; credential lookup/config-file schema as a prerequisite for authoring a provider |
| `bitrouter-providers` | Current public registry fetch/cache/metadata application and existing provider defaults/protocol machinery | Concrete TypeSafe evaluation code; the executable provider-extension hook; an `ExtensionApi` parameter solely to decide whether registry data is executable |
| Host assembly (`bitrouter` app/library) | Explicitly register reviewed first-party extensions in default `bro`; compose registrations with config and registry, validate executable routes, activate accounts, choose candidates, enforce timeout/retry/cancellation, meter and expose models | TypeSafe's wire schema; a hard-coded TypeSafe execution branch |
| `extensions/typesafe/` | Register the `typesafe` provider, its `evaluate` facet, model-to-wire-id mapping and TypeSafe-specific upstream request/response semantics | Public BitRouter HTTP handler, account selection, global retry policy, billing settlement, another provider's claims |
| Custom host API | Permit a downstream binary to link additional trusted extensions explicitly | A required second BitRouter executable for official TypeSafe support |

The author contract belongs with canonical operation types in `bitrouter-sdk`
for now because statically linked external extensions and the host must share
it. **Placement in the SDK does not make TypeSafe part of the SDK**, and the
provider facet must compile without the SDK's `config_file` feature. Keep the
author-facing module small and stable; host/config-only types stay private to
host assembly. A separate `bitrouter-provider-api` crate is justified only if
real independent consumers need a separately versioned or dependency-light
contract; extracting it now adds a release/versioning edge without removing
the need for a shared contract.

`bitrouter-providers` remains a registry/defaults consumer of SDK types, not
the extension runtime. It can apply descriptive catalog data, but the host
must reconcile that data with the compiled registrations. This removes the
current circular-seeming responsibility where registry application consults
`ExtensionApi` to decide which format-bound provider exists.

## 4. Minimal native author contract

The first facet accepts a typed canonical evaluation request and a resolved
provider-model wire id; it returns constrained HTTP request details, then
parses a successful JSON response into typed provider data. The host executes
the request and classifies standard HTTP failures. Its registration
also declares the provider id and executable model/operation claims. The exact
Rust spelling is implementation work, but these semantics are normative:

1. **One provider identity:** registration id `typesafe`; reject duplicate
   registrations and duplicate claims for the same provider/model/operation.
   Registration errors must invalidate startup even if the caller ignores the
   error, matching the existing `ExtensionApi` behavior.
2. **Typed facet:** only `evaluate` is introduced. A registration without this
   facet cannot claim `evaluate`. No generic JSON escape hatch is part of the
   public author API.
3. **Executable models:** declare `typesafe/jev-1.13` with upstream id
   `jev-1.13.0`, supported question kinds and known upstream limits. Claims
   are namespaced to the registered provider unless a later explicit
   cross-provider routing rule is designed. No alias inference.
4. **Credential requirement:** declare the TypeSafe bearer credential source
   (`TYPESAFE_API_KEY`) and permissible operator overrides. The host resolves
   and protects credentials and injects the selected account's bearer header
   into the outbound call; the extension receives neither the entire config
   nor other accounts' credentials.
   Do not add generic OAuth/provider-auth plug-in machinery without a second
   use case.
5. **Host-governed outbound I/O:** the host supplies an HTTP execution service
   that applies the selected account's base URL/headers, request deadline,
   cancellation, transport-level payload limits and instrumentation. The
   extension can choose TypeSafe's relative path, method, and provider-specific
   non-auth headers/body, but cannot redirect to another origin, select another
   account, override authentication/framing headers, or bypass the host's
   resource limits. It maps TypeSafe's successful response semantics. The host
   sanitizes and classifies HTTP failures, then decides whether another
   eligible account is selected. Add a provider-specific error classifier only
   when a concrete upstream error fixture requires different semantics.
6. **Typed accounting:** the facet returns the provider-reported model version
   and usage when present, plus the raw answer values needed by the canonical
   result. It does not fabricate probability/confidence/usage values or settle
   billing itself. The host records request/route evidence and settles once.
7. **Inactive registration:** default `bro` may contain valid TypeSafe code
   without an activated TypeSafe provider/account; this creates no model route
   or upstream work. Normal absence of the optional TypeSafe credential is not
   a startup warning; administrative inspection can explain why the model is
   unavailable. An explicitly enabled TypeSafe route without a matching
   registration or credential fails with an actionable startup/config error
   rather than silently falling back to a different protocol.

The typed facet may internally use JSON for TypeSafe's wire body. The ban on
an opaque JSON hook applies to the *shared author boundary*, not to provider
wire implementations. A native Rust extension is still trusted in-process
code; a narrow trait is an API boundary, **not** a security sandbox. Updating
the default TypeSafe extension requires rebuilding and releasing `bro`;
downstream custom hosts rebuild for their own extension changes.

## 5. Catalog, configuration, and model listing

Executable support and descriptive catalog data are distinct:

1. **Compiled extension registration is executable authority.** It declares
   provider id, supported canonical model ids, operation facets, wire model ids
   and provider-specific execution semantics. No registry row or user YAML can
   invent an unregistered executable route.
2. **Public registry is optional metadata for an extension-owned provider.**
   It may add display name, documentation URL, pricing and other curated facts
   to a matching identity. An extension must be usable from its own minimal
   declaration plus local account configuration even if the public registry is
   disabled or unreachable. An inactive catalog row must not appear as a
   routable model merely because it exists in `dist/registry`.
3. **Operator configuration controls activation and account values:** secret
   reference/value, base URL if permitted by the extension, account weight,
   local limits, pricing override and enable/disable. It cannot change the
   facet's operation or wire model id. Pricing override provenance remains
   visible to metering rather than silently changing the catalog source.
4. **Conflicts are errors, not precedence guesses.** Duplicate provider
   registrations, an explicitly activated registry/config model whose wire id
   or operation disagrees with the extension, or an unsupported operation fail
   readiness with the provider/model/field named. Inactive unrelated registry
   rows may be ignored with diagnostics. The host must not silently override
   extension declarations with central metadata, or vice versa.
5. **Listing is capability-filtered.** `/v1/models` and route eligibility
   reflect only activated, credentialed, executable provider/model/operation
   combinations. Default `bro` includes the TypeSafe implementation, but does
   not list or route Jev without an active, credentialed TypeSafe account.
   Administrative diagnostics may show an unavailable catalog item and its
   reason.

Migration of the current TypeSafe registry row removes
`operations.evaluate.format: { extension: system-one, adapter: json, revision: 1 }`.
The row can retain curated metadata and the upstream id for consistency
checking, but it no longer binds execution. The provider extension must not
depend on that row to exist. Existing generation-provider registry behavior
is unchanged by this proposal.

## 6. Host composition and package shape

The existing `ExtensionApi` registration phase and shared
`serve_with_extensions` startup remain the composition path. Add a typed
provider registration to that API; do not build a parallel provider registrar
or an evaluation-specific host pipeline. The default `bro serve` registration
closure invokes the official TypeSafe extension, linked as a regular
`apps/bitrouter` dependency; `bro start` launches that same executable's
`serve` path. Common app assembly consumes provider and request-check
registrations together. A downstream custom binary may compose additional
extensions through the same API.

Recommended source shape:

```text
crates/bitrouter-sdk/src/extension/provider/   # typed author contract
extensions/typesafe/provider/                    # one provider crate; System One codec private
apps/bitrouter/                                  # common host assembly and default bro target
```

The current `extensions/system-one/format/` package would be migrated into
the TypeSafe provider crate, not retained as a published format-extension API.
Keep any reusable codec as an internal module until a second independently
verified consumer exists. Avoid a crate per provider *plus* a crate per format
by default.

The default `bro` is **not** required to be extension-free. It includes only
explicitly reviewed first-party integrations, not every extension in the
workspace. This is a composition choice at the app boundary, not a reason to
move TypeSafe code into `bitrouter-sdk` or `bitrouter-providers`. Normal
`bro serve`, `bro start`, reload, status, and stop must all address the same
TypeSafe-capable daemon. A second official `bro-evaluate` binary would split
that lifecycle for no distinct runtime capability; remove the
`bitrouter-evaluation-host` package and its documentation/packaging entries
as part of migration, once equivalent coverage exists in `bro`.

Mount the canonical `/v1/evaluate` endpoint independently of whether an
evaluation pipeline has an active provider. Authenticate first, then return
`evaluation_model_not_found` (404) for a valid selector with no eligible
evaluation route. Do not add an inactive Jev model to `/v1/models`. The
present evaluation handler already models an absent pipeline; assembly must
stop making route mounting contingent on that pipeline. Preserve the single
combined `/v1/models` response rather than competing default/evaluation
handlers.

This makes the official provider extension follow `bro`'s release cadence.
That is acceptable for the first reviewed provider; independent shipping or
third-party runtime installation is a separate future product requirement,
not a reason to maintain an evaluation-only host now.

## 7. Delivery and pass gates

The previously completed Phase 0–1 checks establish the public evaluation
shape and operation-aware rail *for the current branch*. They are not proof
that this new provider boundary works. Run their regressions again after the
refactor. The old format-first Phase 2 deterministic green result is historical
evidence, not acceptance of this proposal.

### Phase A — shared provider contract and assembly

Implement the minimal typed `evaluate` provider facet and registration in
`bitrouter-sdk`; remove its author API's `config_file` dependency. Reconcile
registrations, accounts and registry metadata in common host assembly, not in
`bitrouter-providers` registry merge. Preserve request-check registrations.

**Pass:** compile a tiny external test provider against the author API without
`config_file`; show it works with the registry disabled; reject duplicate ids,
model/operation conflicts and ignored registration errors at startup; prove
unrelated inactive registrations do not create routes. Existing generation and
request-check suites remain green. No TypeSafe-specific symbols are required
by the SDK or provider-registry crate; app composition is the only layer
allowed to name a concrete provider extension. A default `bro` with no
evaluation provider starts cleanly, reports no Jev model, serves
`/v1/evaluate`, and returns the documented 404 after authentication for an
otherwise valid Jev request.
The evaluation pipeline must remain available to a daemon that starts without
an active provider, so a validated reload can activate a route. `/v1/models`
must reflect that same live routing snapshot after activation or deactivation.

### Phase B — TypeSafe provider and `/v1/evaluate`

Move System One rendering/parsing into `extensions/typesafe/provider/`, bind
`typesafe/jev-1.13` to `jev-1.13.0`, and register it in default `bro`.
The host keeps the existing public endpoint, account-selection, retry,
cancellation and settlement behavior. Migrate TypeSafe registry metadata away
from format binding, remove the standalone evaluation host, and update the
shipped `/bitrouter` skill to describe the standard `bro` lifecycle and
credential-gated activation.

**Pass (deterministic):** golden TypeSafe request/response/error fixtures;
`noul`/`choice`/`score` normalization; malformed answer/probability rejection;
provider-reported model and usage preservation; unknown top-level field
behavior; no default question-count/id-byte cap; operation mismatch; canonical
and provider-pinned selection; registry-disabled execution; conflict and
missing-credential diagnostics; mock-HTTP TypeSafe end-to-end; same-provider
multi-account retry/failover and single terminal settlement; deadline,
cancellation and redaction checks. Assert that ordinary `bro serve` and
detached `bro start` route Jev with an active credential, while the same
binary with no TypeSafe credential keeps `/v1/evaluate` available but does
not list or route Jev. Verify reload and management commands against that
same daemon and that no `bro-evaluate` artifact remains. Run the full Rust
tests, Clippy, format, registry validation/build/check and hosted CI matrices
required by the repo. A deterministic green run is **not** a credentialed
smoke pass.

**Pass (credentialed smoke):** in a controlled `bro` instance using a revocable
test TypeSafe credential, send one real `/v1/evaluate` request per question kind
and inspect response, actual provider model, usage, cost/evidence and logs for
secret leakage. Measure server behavior with increasing question counts and
id lengths before proposing default limits. Do not log or commit the key or
raw sensitive answers. Record date, model version and observed limits; a
provider outage or absent credential leaves this gate pending, not passed.

### Later provider extension — separate approval

Only add a second provider when chosen explicitly. Its pass gate includes an
independent upstream conformance fixture, conflicting-model/capability tests,
shared-host composition, and proof that TypeSafe code is not branched on to
serve it. OpenRouter gateway and Laya are not implied follow-ups. No exact
provider-model equivalence claim is required merely because OpenRouter shaped
the public API.

## 8. Rejected shortcuts and review decisions

- **Keep `EvaluationFormatAdapter` as the public provider story:** insufficient
  provider ownership and makes the sole implementation depend on central
  format bindings; private codecs remain fine.
- **Move TypeSafe into `bitrouter-providers`:** conflates registry metadata
  with native provider behavior. Linking a provider extension into `bro` does
  not require putting its implementation in the registry crate.
- **Make every extension a full provider implementation across all APIs:**
  forces unused callbacks and weakens typed operation guarantees.
- **Keep an official evaluation-only host or add one per extension:** duplicates
  the daemon/CLI lifecycle without a distinct runtime capability. The current
  standalone host is removed after default-`bro` parity is verified.
- **Require default `bro` to be extension-free:** useful only with a concrete
  minimal-binary, trust, licensing, or distribution requirement. It is not
  necessary for provider-extension modularity and is not a first-release
  constraint.
- **Add WASM or runtime install now:** different trust, ABI, resource and
  distribution problem. Revisit only with a concrete deployment need for
  independently distributed or untrusted providers; the absence of Pi-style
  hot install is accepted.

The agreed first-release composition is default `bro` with the reviewed
TypeSafe provider extension statically registered, credential-gated activation,
a stable `/v1/evaluate` ingress, and no separate evaluation host. One remaining
implementation review point is whether the host-governed HTTP/context and
scoped-credential boundary suffices for TypeSafe without introducing a generic
auth/transport plug-in. All other items above are the proposed implementation
and test contract, not questions deferred to code review.
