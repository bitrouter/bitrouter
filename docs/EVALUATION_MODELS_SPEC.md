# First-class evaluation models through `/v1/evaluate`

Status: **Phases 0–1 passed local and hosted CI. The earlier provider-named
Phase 2 implementation passed deterministic local and hosted checks, but its
format-first packaging, integration with the shared extension host on main,
and credentialed TypeSafe smoke test remain pending. Laya prototype evidence
is retained as history; Laya is not in the current delivery scope. None of
these results establishes production readiness.**

Date: 2026-09-23

Initial baseline: `e2a8c644`

This specification adds Jev and similar typed-decision models as first-class
BitRouter model routes. It does not disguise them as chat models. Clients use
the existing selector vocabulary — slash-delimited canonical model ids and the
`provider:model` pin form — but invoke them through a new evaluation operation:

```text
POST /v1/evaluate
model: typesafe/jev-1.13
```

The public question-and-answer shape uses OpenRouter Decisions as a format
reference, while the endpoint, authentication, provider selection, tracing,
and routing remain BitRouter-owned. OpenRouter is not a provider route in this
increment. The first implementation targets the TypeSafe-hosted,
version-pinned Jev model through a compiled native Rust **System One wire-format
extension**. Provider configuration binds that format to TypeSafe's endpoint,
credentials, model id, and limits. The architecture permits later format,
authentication, transport, and runtime facets without making one universal
provider hook or Jev-specific provider implementation part of the core
contract.

The external facts used by this specification were checked on 2026-09-22:

- TypeSafe serves Jev through `POST /v1/systemone`; requests contain `state`,
  `model`, and typed `questions`, while responses contain keyed `answers` and
  token `usage`: <https://docs.typesafe.ai/api>.
- `jev-latest` is a moving alias. TypeSafe recommends pinning the version after
  calibrating thresholds: <https://docs.typesafe.ai/models>.
- OpenRouter exposes typed decisions through the alpha
  `POST /api/alpha/decisions` operation. Its public request and answer vocabulary
  is `noul`, `choice`, and `score`, and its response carries `id`, `model`,
  `provider`, and token/cost usage:
  <https://openrouter.ai/openapi.json>.
- OpenRouter's AI SDK provider exposes the same operation through
  `evaluationModel()` rather than treating it as Chat Completions:
  <https://github.com/OpenRouterTeam/ai-sdk-provider/blob/main/CHANGELOG.md>.

---

## 1. Decision summary

1. `evaluate` is a distinct inference operation. It is not a
   [`Capability`](../crates/bitrouter-sdk/src/language_model/types.rs) and is
   not encoded as a special generation request.
2. `POST /v1/evaluate` is the only new public inference endpoint in the first
   release. No inbound `/v1/systemone` compatibility endpoint is exposed; the
   direct TypeSafe extension still calls TypeSafe's `/v1/systemone` upstream.
3. The public request and answer vocabulary is `noul`, `choice`, and `score`,
   following the OpenRouter Decisions domain format. BitRouter does not adopt
   OpenRouter's endpoint path or gateway-control fields and does not claim full
   OpenRouter API compatibility in the first release.
4. A canonical model id remains distinct from a provider and its wire model
   id. The first versioned identity is `typesafe/jev-1.13`; its direct TypeSafe
   route sends `jev-1.13.0` upstream.
5. Existing model selector forms remain authoritative:
   `typesafe/jev-1.13` is canonical and
   `typesafe:typesafe/jev-1.13` pins the provider using the existing
   `provider:model` syntax.
6. Model routing becomes operation-aware. A model route that supports only
   `evaluate` cannot receive a Chat Completions, Messages, Responses, or
   Generate Content request. A generation-only route cannot receive an
   evaluation request.
7. Evaluation shares provider configuration, credentials, accounts, headers,
   timeouts, rate limiting, pricing, model identity, reload state, metering,
   and operational evidence with generation. It has its own canonical request,
   response, format adapter, executor, and pipeline types.
8. The first release is non-streaming. It does not add an SSE form of
   `/v1/evaluate`.
9. The first release supports direct TypeSafe service routes and fallback
   across multiple accounts of that same provider/model. Cross-provider
   fallback is outside this increment; no OpenRouter gateway route or
   provider-model equivalence claim is required.
10. Concrete upstream wire-format support is not built into the core or
    registered by the default `bro` host. A custom host compiles and registers
    a native Rust format extension through a revisioned `ExtensionApi` hook.
    Provider routes bind to that format through configuration and registry data.
11. An extension is a package that may register typed facets; it is organized
    by the wire contract it implements, not by a provider name. The first
    implemented facet is `EvaluationFormatAdapter`; no universal
    `ProviderAdapter::execute(json)` is introduced.
12. The first release does not add a WASM runtime, dynamic Rust library loading,
    extension installation, or an extension marketplace.
13. An evaluation answer is data. BitRouter does not treat it as authorization
    to invoke tools, mutate policy, or cause external side effects.

These decisions keep model selection unified without forcing typed decisions
through the current `Prompt -> GenerateResult` contract or forcing unrelated
model operations through one opaque provider callback.

---

## 2. Current baseline and the required boundary

The current SDK supports four bidirectional generation protocols:

- Chat Completions;
- Messages;
- Generate Content; and
- Responses.

`ApiProtocol::Custom` is outbound-only, but its `OutboundAdapter` still renders
a canonical `Prompt` and parses a `GenerateResult`. Registering
`Custom("system_one")` would therefore make Jev configurable without making it
semantically usable. It would either discard the question types and probability
distributions or encode them as provider-specific JSON hidden inside a chat
message.

The current `Capability` enum also describes optional properties of a
generation request: tools, structured output, reasoning, media input, and media
output. `evaluate` changes the request and response contract itself. Treating
it as one more capability would allow invalid chains such as a Chat
Completions request selecting an evaluation-only target.

The required separation is:

| Concept | Example | Meaning |
| --- | --- | --- |
| canonical model | `typesafe/jev-1.13` | Stable BitRouter model identity |
| provider | `typesafe` | Account, credentials, endpoint, limits, and billing source |
| operation | `evaluate` | Canonical request and result semantics |
| format adapter | `system-one/json@1` | Revisioned upstream JSON wire contract, initially verified with TypeSafe |

Provider and model remain shared control-plane concepts. Operation and the
registered format facet determine the typed data-plane contract; host-owned
transport and auth perform the call.

---

## 3. Goals and non-goals

### 3.1 Goals

1. Make version-pinned Jev addressable as a normal canonical model and as an
   explicitly provider-pinned route.
2. Preserve the full typed answer: selected choice or score, all probabilities,
   provider-supplied confidence, score legend, token usage, and actual upstream
   model version.
3. Prevent an evaluation-only model from entering any generation chain, and
   prevent a generation-only model from entering an evaluation chain.
4. Reuse BitRouter's provider account, credential, timeout, header, pricing,
   model-listing, saved/running configuration, and evidence boundaries.
5. Define an operation-neutral registry shape so another typed-decision model
   can be added without adding Jev fields to generic provider or model types.
6. Make format errors and malformed probability results fail explicitly;
   never manufacture confidence or silently normalize an invalid answer.
7. Retain the distinction between a requested canonical selector, the selected
   provider, the provider wire model id, and the actual model version reported
   by that provider.
8. Keep the first implementation additive. Existing generation APIs and
   provider configurations must retain their behavior.
9. Organize concrete extensions by upstream wire contract, independently of
   provider identity and typed operation facets. A future extension may register
   multiple closely related typed facets without gaining an opaque
   execute-anything hook.
10. Keep endpoint selection, credentials, HTTP execution, deadlines, usage,
    metering, and candidate legality host-owned when a native format adapter is
    invoked.

### 3.2 Non-goals

- Making Jev compatible with `/v1/chat/completions`, `/v1/messages`,
  `/v1/responses`, or Gemini Generate Content.
- Adding a natural-language wrapper that asks Jev to emit prose.
- Adding an inbound `/v1/systemone` TypeSafe SDK compatibility endpoint in the
  first release.
- Embedding a local model runtime in `bro` or shipping a Laya provider in this
  increment.
- Using evaluation answers as a BitRouter route selector, request checker,
  guardrail, or tool authorization policy in this increment.
- Calibrating application-specific confidence thresholds.
- Claiming that models or provider routes with similar request shapes are
  behaviorally interchangeable.
- Adding batch jobs, streaming, fine-tuning, training, or model-weight
  distribution.
- Listing the moving `jev-latest` alias in the initial registry or
  automatically placing floating and version-pinned aliases in the same
  fallback chain.
- Adding an OpenRouter gateway provider route. OpenRouter supplies the public
  data-format reference for this increment, not an upstream integration.
- Defining generic embedding, reranking, image, audio, realtime, local-runtime,
  authentication, discovery, or transport hooks before a concrete operation
  needs them.
- Loading Rust dynamic libraries or claiming that a compiled native hook is a
  user-installable extension.
- Implementing the proposed WASM component runtime, WIT contracts, catalog,
  lockfile, installer, or permission system in this increment.

---

## 4. Public HTTP contract

### 4.1 Endpoint and authentication

```text
POST /v1/evaluate
Authorization: Bearer <BitRouter credential>
Content-Type: application/json
```

The endpoint uses the same inbound authentication and caller identity policy as
the existing inference endpoints. An upstream provider key is never accepted in
the request body and never returned in an error or metadata object.

The first release accepts JSON only and never upgrades to an SSE stream. An
unrecognized `stream` field has no effect and is not forwarded upstream.

`/v1/evaluate` is BitRouter's provider-neutral inbound operation: clients use
its canonical `provider/model` selector and BitRouter owns routing, metering,
and the response contract. TypeSafe's `/v1/systemone` is the direct provider's
outbound wire endpoint used by the native extension. A separate inbound
`/v1/systemone` endpoint would serve a different purpose: compatibility for
clients such as the TypeSafe SDK that already send System One requests to that
path. The first release does not add this inbound endpoint or claim TypeSafe
SDK compatibility. A later compatibility proposal would need to address bare
Jev model names and SDK model-listing behavior, not just the POST path, while
reusing the same canonical evaluation pipeline.

### 4.2 Request

```json
{
  "model": "typesafe/jev-1.13",
  "state": {
    "ticket": "My card was charged twice.",
    "account_tier": "pro"
  },
  "questions": {
    "department": {
      "type": "choice",
      "instructions": "Which team should handle this ticket?",
      "criteria": {
        "billing": "Payments, invoices, refunds, or duplicate charges",
        "technical": "Product errors or integration failures",
        "sales": "Pricing, upgrades, or new accounts"
      }
    },
    "urgent": {
      "type": "noul",
      "instructions": "Does this require urgent handling?",
      "criteria": {
        "true": "Delay could cause additional financial harm",
        "false": "Normal queueing is acceptable"
      }
    }
  }
}
```

Top-level fields:

| Field | Required | Contract |
| --- | --- | --- |
| `model` | yes | Any evaluation-capable canonical or provider-pinned selector |
| `state` | yes | String, object, or array; no binary or media parts |
| `questions` | yes | Non-empty object keyed by caller-selected question id |

Unknown top-level fields are accepted and ignored in the first release. They
are not forwarded to the provider, do not affect routing or tracing, and do
not change the response contract. This includes OpenRouter's request-level
`provider`, `user`, `session_id`, and `trace` controls; `extraBody` is not a
BitRouter gateway-control field. Provider selection uses BitRouter's selector
and routing configuration; caller/session/trace identity uses BitRouter's
shared request context. After implementation, server-side tests must verify
this behavior, including that unknown fields cannot override known fields or
trigger provider effects.

`state`, `instructions`, and individual criteria preserve JSON structure. The
canonical contract does not stringify objects or arrays before an adapter sees
them. Every adapter must either preserve that structure or reject the request
before making an upstream call.

### 4.3 Question types

The canonical API uses a tagged union.

#### Noul

```json
{
  "type": "noul",
  "instructions": "Did the build succeed?",
  "criteria": {
    "true": "The process exited with code 0",
    "false": "The process exited with any non-zero code"
  }
}
```

- `instructions` is required.
- `criteria` is optional.
- When present, `criteria` may contain only `true` and `false`.
- The answer field is `noul`: the probability of `true`, not a thresholded
  boolean.

#### Choice

```json
{
  "type": "choice",
  "instructions": "Choose exactly one destination.",
  "criteria": {
    "billing": "Payment-related requests",
    "technical": "Product and integration failures"
  }
}
```

- `instructions` and `criteria` are required.
- `criteria` must contain at least two uniquely named options.
- An option's description may be a string, object, array, or `null`.
- The first TypeSafe binding permits at most 255 options, matching the upstream
  limit. A stricter provider limit is enforced before dispatch.

#### Score

```json
{
  "type": "score",
  "instructions": "Rate the urgency from low to high.",
  "criteria": ["low", "medium", "high"]
}
```

- `instructions` and `criteria` are required.
- `criteria` is ordered and must contain between 2 and 10 levels for the first
  TypeSafe binding.
- Each level may be a string, object, or array.

The public endpoint does not accept `type: "boolean"`. `noul` is the selected
OpenRouter Decisions spelling for a binary-probability question. Internally the
SDK may describe its semantics as a boolean probability, but the serialized
request and answer remain `noul`.

### 4.4 Validation and resource bounds

Validation happens before provider resolution where possible, and before the
first upstream call in every case:

1. The request body uses the existing bounded HTTP-body mechanism; no
   unbounded parser is added.
2. `model`, question ids, choice keys, and object keys must be non-empty UTF-8
   strings.
3. `questions` must be non-empty. The first release adds no default numeric
   question-count limit; the existing HTTP-body bound and documented provider
   limits still apply.
4. A question id must be non-empty and unique as a JSON object key. The first
   release adds no default UTF-8 byte-length limit for it.
5. Numbers inside structured state or instructions must be finite JSON
   numbers. Binary values and data URLs receive no special interpretation.
6. Provider-specific token limits remain provider facts. If BitRouter lacks an
   exact tokenizer, it must not claim a successful local token preflight; the
   bounded byte limit still applies and an upstream size rejection remains a
   validation error.
7. Invalid local structure produces no upstream attempt and no billable usage.

Bounded server-side tests measure practical question-count and question-id
limits after the endpoint and extension work. A future numeric default must
be justified by those measurements; omitting one here does not remove the
existing HTTP-body bound or known provider-specific constraints.

Phase 2 local mock measurement: one request with 512 Noul questions, including
a 1,024-byte ASCII question id, passed through `/v1/evaluate` and returned
matching answers. A body exceeding 16 MiB was rejected before upstream
dispatch. This demonstrates only BitRouter's local behavior, not TypeSafe's
practical upstream token or question-count ceiling.

### 4.5 Response

```json
{
  "id": "eval_...",
  "model": "jev-1.13.0",
  "provider": "typesafe",
  "answers": {
    "department": {
      "type": "choice",
      "choice": "billing",
      "probabilities": {
        "billing": 0.88,
        "technical": 0.1,
        "sales": 0.02
      },
      "confidence": 0.81
    },
    "urgent": {
      "type": "noul",
      "noul": 0.94
    }
  },
  "usage": {
    "input_tokens": 318,
    "output_tokens": 34,
    "cost": 0.000013356
  }
}
```

Response rules:

- `id` is the stable BitRouter evaluation request id.
- `model` is the actual model/version reported by the successful provider when
  available, otherwise its configured provider model id. A moving alias must
  not erase a provider-reported concrete version.
- `provider` is the selected BitRouter provider id, not a caller-controlled
  routing preference.
- `usage.input_tokens` and `usage.output_tokens` report provider-returned counts.
  A provider may charge zero for output tokens while still reporting non-zero
  output usage; zero price must not be represented as zero usage.
- `usage.cost` is BitRouter's settled cost for the successful attempt, using the
  same evidence hierarchy as generation. It is omitted rather than fabricated
  when no charge or configured-price evidence exists.
- Provider-supplied `confidence` is preserved. BitRouter never derives a
  replacement confidence from probabilities when the provider omits it.
- Provider-specific metadata is not copied wholesale. Adapters expose only
  typed, bounded, non-secret facts approved by the response contract.
- Requested selector, resolved canonical model, provider model id, selected
  account, attempts, and fallback facts remain in BitRouter's request/receipt
  evidence even though the OpenRouter-shaped response body does not add a
  separate `providerMetadata` object.

The first release uses the OpenRouter Decisions question, answer, identity,
provider, and usage layout as a reference at `POST /v1/evaluate`. It does not
claim byte-for-byte OpenRouter API compatibility: the endpoint differs,
OpenRouter gateway-control fields are ignored rather than implemented, and
BitRouter owns provider selection and trace context.

### 4.6 Answer invariants

An adapter must reject a successful upstream HTTP response unless all of these
hold:

1. The answer keys exactly match the request's question keys.
2. Each answer type matches its question type after format translation.
3. Every probability is finite and lies in `[0, 1]`.
4. Choice probability keys exactly match the declared choice keys.
5. A returned `choice` names one of the declared options.
6. Score probability and legend keys represent every declared level exactly
   once.
7. Probability sums must be consistent with 1 under the direct provider's
   documented or conformance-tested output precision. There is no universal
   `1e-6` tolerance: before Phase 2 passes, TypeSafe fixtures and a live
   response must justify a specific, documented acceptance rule. Tests must
   accept valid rounding drift and reject distributions outside that rule.
   BitRouter does not renormalize values.
8. Noul probability lies in `[0, 1]`.
9. `confidence`, when present, lies in `[0, 1]`.
10. Usage counters are non-negative integers.

A violation is `upstream_invalid_response`. It is never returned as a partial
answer and never repaired silently.

### 4.7 Errors

The endpoint uses BitRouter's existing JSON error envelope and request-id
header. It adds stable evaluation-specific codes:

| HTTP | Code | Meaning |
| --- | --- | --- |
| 400 | `invalid_evaluation_request` | Locally invalid required or recognized field; unknown top-level fields are ignored |
| 404 | `evaluation_model_not_found` | Selector has no evaluation route |
| 409 | `model_operation_mismatch` | Known model exists, but not for `evaluate` |
| 422 | `provider_rejected_evaluation` | Upstream rejected valid canonical content |
| 429 | `upstream_rate_limited` | Evaluation provider rate limit exhausted |
| 502 | `upstream_authentication_failed` | Configured upstream credential was rejected |
| 502 | `upstream_invalid_response` | Malformed or semantically invalid answer |
| 503 | `upstream_unavailable` | All eligible attempts unavailable |
| 504 | `upstream_timeout` | Deadline elapsed |

Errors never include `state`, instructions, criteria, upstream credentials, or
an unbounded upstream body. Upstream error bodies are bounded and sanitized by
the same standard as existing generation transports.

---

## 5. Canonical SDK contract

### 5.1 Operation identity

The shared routing/configuration layer gains a stable operation enum:

```rust
pub enum InferenceOperation {
    Generate,
    Evaluate,
}
```

This enum is not an umbrella request type. The first implementation must not
replace every generation call with `InferenceRequest::Generate(Prompt)` merely
for theoretical uniformity. Generation and evaluation retain typed pipelines;
they share model and provider resolution through operation-aware configuration.

### 5.2 Evaluation types

The SDK adds an `evaluation` module beside `language_model`, conceptually:

```rust
pub struct EvaluationRequest {
    pub model: String,
    pub state: StructuredValue,
    pub questions: BTreeMap<String, EvaluationQuestion>,
}

pub enum EvaluationQuestion {
    Noul {
        instructions: StructuredValue,
        criteria: Option<BooleanCriteria>,
    },
    Choice {
        instructions: StructuredValue,
        criteria: BTreeMap<String, Option<StructuredValue>>,
    },
    Score {
        instructions: StructuredValue,
        criteria: Vec<StructuredValue>,
    },
}

pub struct EvaluationResult {
    pub model: String,
    pub answers: BTreeMap<String, EvaluationAnswer>,
    pub usage: Usage,
    pub provider_model: Option<String>,
}
```

`StructuredValue` is a validated JSON value whose root is a string, object, or
array. Descendants may contain ordinary JSON scalars, which preserves structured
records such as numeric order totals and boolean flags. The implementation may
use a private serde representation, but no value receives an implicit binary,
media, configuration, or executable interpretation. Choice descriptions use
`Option<StructuredValue>` so their explicitly permitted top-level `null` stays
distinct from a missing option.

The canonical answer union preserves:

- Noul/binary probability;
- selected choice, full distribution, and optional confidence;
- interpolated score, full distribution, legend, and optional confidence; and
- provider-reported usage and provider model version.

The Rust type may use an internal semantic helper named `BooleanProbability`,
but its public serde tag and answer field are `noul`, matching the selected HTTP
contract.

### 5.3 Wire-format extensions and typed facets

A wire-format extension is a compiled Rust package and registration unit, not
one universal execution callback or one package per provider. One package may
register closely related typed facets through `ExtensionApi`; the host continues
to own operation dispatch and invokes only the facet selected by validated
provider/model configuration. The package is organized around an independently
versioned upstream wire contract. TypeSafe is the first binding to the System
One JSON contract, not the owner of that format identity.

One resolved provider/model route is composed rather than owned wholesale by an
extension:

```text
provider route = operation + format facet + transport + auth + registry metadata
```

A provider using an existing operation, registered format, transport, and auth
scheme needs registry data only. A new wire dialect for an existing operation
adds a format facet. A new semantic operation requires a core endpoint and
typed contract before any extension can implement it. Non-standard
authentication or transport uses its own trusted facet rather than expanding
the format adapter.
Sharing a format facet across providers requires separate request/response
conformance evidence for each binding. Similar question types, endpoint names,
or model behavior are insufficient. Provider-specific limits, endpoint, auth,
pricing, and model mapping remain provider/model configuration. If a second
provider's wire behavior differs, use a separate format identity or revision;
do not branch on provider id inside the shared adapter.

The architectural facet families are:

| Facet | Purpose | Current increment |
| --- | --- | --- |
| evaluation format | Canonical evaluation request/result to one upstream JSON dialect | Implemented |
| generation format | Canonical prompt/result to one generation dialect | Existing generation adapters remain unchanged |
| authentication/signing | Credential acquisition or request signing | Reuse existing `AuthApplier`; no new generic hook |
| transport | Non-standard HTTP framing, vendor SDK, or non-HTTP execution | Existing trusted escape hatches remain unchanged |
| discovery | Provider-specific model discovery | Not introduced here |
| local runtime | In-process or external model execution | Out of scope |

Only the evaluation-format registration is added in this increment. Future
operations add their own typed facet only when their public request/result and
host guarantees are defined. The implementation must not predeclare unused
embedding, reranking, media, realtime, discovery, or runtime hooks.

This design deliberately rejects a universal
`ProviderAdapter::execute(operation, json) -> json`. Such an interface would
erase pre-dispatch validation, operation mismatch checks, typed errors, usage
evidence, and the host's ability to bound extension authority.

### 5.4 Native evaluation format registration

Generation's `OutboundAdapter` cannot be reused because it is permanently typed
to `Prompt` and `GenerateResult`. The native extension contract adds a separate
typed facet, conceptually:

```rust
pub struct EvaluationFormatDescriptor {
    pub extension_id: String,
    pub adapter_id: String,
    pub revision: u32,
}

pub trait EvaluationFormatAdapter: Send + Sync {
    fn descriptor(&self) -> EvaluationFormatDescriptor;
    fn render_request(
        &self,
        request: &EvaluationRequest,
        target: &EvaluationRoutingTarget,
    ) -> Result<serde_json::Value>;
    fn parse_response(
        &self,
        body: serde_json::Value,
        request: &EvaluationRequest,
    ) -> Result<EvaluationResult>;
}

impl ExtensionApi {
    pub fn register_evaluation_format(
        &mut self,
        adapter: Arc<dyn EvaluationFormatAdapter>,
    ) -> Result<()>;
}
```

Evaluation is non-streaming, so the trait has no stream encoder or decoder.
The adapter returns or consumes bounded JSON only. It does not receive an API
key, OAuth store, arbitrary URL, `reqwest::Client`, database handle, full config,
policy runtime, or unrestricted pipeline context. Endpoint construction,
authentication/signing, HTTP execution, timeouts, retries, cancellation,
settlement, and metering remain host concerns.

Adapter ids are open strings scoped by a stable format-extension/package
identity; revisions are exact compatibility boundaries. The initial identity
is `system-one/json@1` (`extension_id = "system-one"`, `adapter_id = "json"`,
`revision = 1`). This is the TypeSafe-verified System One JSON dialect, not a
claim that every future provider using that name is wire-compatible. Duplicate
registrations, missing registrations, and configured revision mismatches fail
before database assembly and before any provider call. A valid registered
adapter with no configured provider binding remains inactive.

PR #923 has since established the shared compiled-extension registration and
custom-host lifecycle on main:
<https://github.com/bitrouter/bitrouter/pull/923>. The implementation branches
must add the evaluation facet to that `ExtensionApi` and host composition,
without retaining a parallel evaluation-only registrar or replacing the
request-check contract. The concrete package belongs under
`extensions/system-one/format/` (the `extensions/<extension>/<package>/`
convention). Before this unmerged package is released, give its Cargo package
the format-owned name `bitrouter-system-one-format`; this is a rename of the
existing crate, not an additional crate. Shared typed host contracts stay in
`crates/`, and a custom host, if needed, stays in `apps/`. Moving the
implementation does not make it dynamically installable. A custom host may
register more than one format and must not be created anew for every provider
binding.

The default `bro` host does not register the System One evaluation format. A
custom host explicitly links the extension crate and calls
`register_evaluation_format`. This is a compile-time native extension, not a
runtime-installable plugin and not a stable Rust dynamic-library ABI.

### 5.5 System One JSON format and TypeSafe binding

The first concrete native extension registers the `system-one/json@1`
evaluation-format facet. Because the public contract has adopted OpenRouter
Decisions' `noul` spelling, the format adapter preserves the question and
answer semantics rather than translating Boolean terminology:

| OpenRouter-shaped `/v1/evaluate` | TypeSafe `/v1/systemone` |
| --- | --- |
| `type: "noul"` | `type: "noul"` |
| `noul` | `noul` |
| canonical model id | configured `provider_model_id` |
| `input_tokens` / `output_tokens` | `input_tokens` / `output_tokens` |

Choice and score instructions, criteria, distributions, confidence, and legend
retain their meaning and structure. The adapter does not threshold a Noul
probability, choose an option independently, recalculate a score, or synthesize
confidence. The initial fixtures establish this contract only against TypeSafe.
TypeSafe-observed probability rounding must be verified as a wire-format rule
before it is generalized; otherwise it stays a TypeSafe binding constraint or
requires a distinct format revision.

OpenRouter's Decisions shape is a reference for the canonical public contract,
not a configured upstream route or a second format facet in this increment.
Adding an OpenRouter route later would require its own wire-contract review and
provider binding; direct TypeSafe support does not imply gateway support.

### 5.6 Pipeline boundary

The evaluation pipeline owns:

1. inbound authentication and caller identity;
2. canonical request validation;
3. operation-aware model resolution;
4. provider/account target resolution;
5. one bounded non-streaming upstream execution at a time;
6. response validation;
7. usage, cost, attempt, and terminal-outcome evidence; and
8. rendering the canonical HTTP response.

Generation-only prompt transforms, tool loops, response-format checks,
continuation handling, reasoning-effort filters, and generation hooks do not run
for evaluation requests.

The first release may share implementation helpers for authentication,
provider headers, deadline enforcement, error sanitization, and settlement. It
must not make the evaluation pipeline call the generation pipeline internally.

---

## 6. Configuration and registry

### 6.1 Backward-compatible configuration shape

Existing providers without an `operations` map retain their current implicit
`generate` operation and current `api_protocol` behavior.

Evaluation providers add an operation entry:

```yaml
providers:
  typesafe:
    api_base: https://api.typesafe.ai
    api_key: ${TYPESAFE_API_KEY}
    active: true
    operations:
      evaluate:
        endpoint: /v1/systemone
        format:
          extension: system-one
          adapter: json
          revision: 1
    models:
      - id: typesafe/jev-1.13
        provider_model_id: jev-1.13.0
        operations:
          evaluate:
            question_types: [noul, choice, score]
            max_choice_options: 255
            max_score_levels: 10
        pricing:
          input_micro_usd_per_token: 0.042
          output_micro_usd_per_token: 0
```

The exact Rust storage types may differ, but the serialized contract is fixed:

- provider-level `operations.<operation>.endpoint` supplies a host-validated
  relative endpoint for that operation;
- `operations.<operation>.format` names the exact extension, typed adapter
  facet, and revision the host must have registered; it does not name the
  provider, whose identity remains the enclosing provider key;
- model-level `operations` positively declares which operations that concrete
  route supports;
- operation-specific constraints live under that operation rather than in the
  generation `Capability` list;
- a provider that offers both generation and evaluation may declare both;
- absence of model-level `operations` preserves today's implicit generation
  behavior for backward compatibility; and
- once a model explicitly declares `operations`, undeclared operations are
  unsupported rather than unknown.

`api_protocol` at the existing provider/model locations continues to describe
generation. It is not reinterpreted globally, which avoids changing existing
configs during this additive increment.

The format adapter never receives or chooses `api_base` or `endpoint`. The host
joins the configured base with the validated relative endpoint, renders the
body through the registered facet, applies authentication/signing after body
serialization, and performs the HTTP call.

### 6.2 Native extension availability

Registry/catalog presence and executable support are separate facts:

- a future provider route may declare that it requires
  `system-one/json@1`; the Phase 0 canonical catalog entry alone creates
  no provider route;
- the default `bro` host, which does not register that facet, must not report the
  route as routable;
- administration diagnostics may show the catalog model as unavailable with a
  bounded `missing_extension` reason;
- a configured active provider whose required adapter is absent or has the
  wrong revision fails startup before database assembly; and
- a custom host registration alone does not configure, activate, or bind a
  provider/model route.

Generated provider/model documentation must display the required native
extension/custom-host condition rather than presenting a catalog entry as
stock-`bro` support.

### 6.3 Registry identity

The Phase 0 catalog lists only the versioned canonical model
`typesafe/jev-1.13`, with no provider route. Phase 2 adds the TypeSafe route
mapping to `jev-1.13.0`. The initial catalog does not list
the moving `typesafe/jev-latest` alias. A later alias decision must consider
visible moving-version metadata, the actual provider version in each response,
and the risk of reusing confidence thresholds calibrated for a pinned version.

### 6.4 Model listing

`ModelInfo` gains an `operations` set. When the matching native extension is
registered and the route is configured, `GET /v1/models` adds the operation as
an additive field:

```json
{
  "id": "typesafe/jev-1.13",
  "object": "model",
  "providers": ["typesafe"],
  "operations": ["evaluate"]
}
```

The public inference `GET /v1/models` continues to list only routable
provider/model routes and therefore omits this model when no registered provider
route remains. Administrative/local model inspection may separately show:

```json
{
  "id": "typesafe/jev-1.13",
  "provider": "typesafe",
  "operation": "evaluate",
  "available": false,
  "reason": "missing_extension",
  "extension": "system-one/json@1"
}
```

Existing generation models report `operations: ["generate"]`. A future model
that genuinely supports both may report both. Existing clients that ignore the
new operation field continue to work.

The first release does not rely on provider model discovery for evaluation.
TypeSafe's model listing currently exposes aliases while versioned IDs may be
accepted without being listed, so discovery alone cannot establish the pinned
canonical route used by this spec.

### 6.5 Selector and route rules

For `POST /v1/evaluate`:

1. Resolve the raw selector using the existing canonical/provider-pinned model
   vocabulary.
2. Do not resolve `bitrouter/<router>` named generation routers in the first
   release. They are generation-policy bindings, not evaluation virtual models.
3. Retain only concrete provider/model routes that positively declare
   `evaluate`.
4. Apply the existing provider pin, account ordering, active state, credential,
   timeout, and header rules.
5. If the model exists only for another operation, return
   `model_operation_mismatch`; do not collapse it into model-not-found.
6. Do not construct cross-provider evaluation fallback in this increment. If
   more than one provider remains for an unpinned selector, fail validation
   rather than silently choosing or cascading. A future cross-provider policy
   is a separate design decision, not a prerequisite for the initial direct
   TypeSafe route.

Multiple accounts for the same TypeSafe provider and exact model are eligible
for normal account ordering and retry because they call the same service and
wire model.

---

## 7. Failure, retry, and cancellation semantics

Evaluation requests are read-only with respect to BitRouter, but an upstream
attempt can still incur latency and cost. Every attempt is recorded separately.

### 7.1 Retryable failures

Within the eligible same-provider/model account chain, the next account may be
tried after:

- connection failure;
- timeout before a valid complete response;
- HTTP 408;
- HTTP 429, respecting bounded `Retry-After` when present;
- HTTP 529;
- other HTTP 5xx responses; or
- a bounded malformed upstream response.

Malformed responses are retryable only because the next target is the same
provider service and exact model behind another configured account. The final
error retains the fact that an earlier upstream response was invalid.

### 7.2 Non-retryable failures

Local validation errors, inbound authorization failures, unsupported
operations, and other upstream 4xx validation/authentication failures stop the
request. A provider authentication failure must not be reclassified as a model
failure.

### 7.3 Cancellation and shutdown

Client cancellation stops waiting for work and cancels outstanding HTTP I/O as
the transport permits. It does not claim that a remote evaluation did not run
or incur cost. Once an attempt begins, its terminal evidence distinguishes:

- completed;
- failed;
- timed out;
- client disconnected/cancelled; and
- unknown remote completion.

Graceful daemon shutdown stops accepting new evaluations and drains or records
the outcome of admitted evaluations under the same truthfulness standard as
generation settlement. It must not abandon a started attempt without terminal
or unknown-completion evidence.

---

## 8. Usage, pricing, and observability

Evaluation reuses the canonical token `Usage` fields:

- input tokens map to `prompt_tokens` internally;
- output tokens map to `completion_tokens` internally;
- cache/reasoning fields remain zero unless an evaluation provider explicitly
  reports compatible facts; and
- the bounded raw usage object may be retained for settlement audit under the
  existing redaction policy.

Pricing remains provider/model-route data. Jev may report output tokens while
pricing them at zero. Metering calculates cost from the configured rates rather
than inferring cost from the presence or absence of a counter.

Evaluation spans and request records include:

- operation `evaluate`;
- request id;
- requested and resolved canonical model;
- provider and non-secret account label;
- provider model/version;
- evaluation format adapter id/revision and host transport;
- question count and question-type counts;
- bounded serialized input size;
- input/output usage and calculated cost;
- attempt index, latency, HTTP outcome, and terminal state; and
- whether fallback was attempted.

They exclude state, instructions, criteria, answers, probability maps,
credentials, and raw error bodies by default. A future opt-in content logging
feature requires a separate privacy design; it is not implied by existing
tracing levels.

---

## 9. Security and policy boundary

1. Evaluation output is untrusted model output even when its confidence is
   high.
2. BitRouter validates shape and probabilities, not factual correctness or
   calibration.
3. The evaluation pipeline cannot directly invoke a tool, alter credentials,
   change the request's selected operation, publish a policy, or authorize an
   external side effect.
4. Applications that apply thresholds own those thresholds and their fallback
   behavior. A model upgrade does not inherit a threshold merely because the
   model name is similar.
5. Provider/model candidate legality remains host-owned. A returned choice
   named like a model or tool has no authority unless an independent caller
   validates it against its allowed set.
6. Static and inbound-passthrough provider headers use the existing validation
   and redaction rules. Evaluation adds no arbitrary caller-selected upstream
   URL or header.
7. Structured state may contain hostile instructions. Adapters transmit it as
   data and never interpret it as BitRouter configuration.
8. A native Rust extension is trusted in-process code. Its narrow trait reduces
   coupling and accidental authority but is not a sandbox and does not prevent
   ambient filesystem, network, environment, or memory access by malicious code.
9. The official extension source, custom-host composition, dependency lock, and
   binary provenance are part of the trust boundary. Revision matching is
   compatibility validation, not a security sandbox.
10. The absence of credential and transport parameters from
    `EvaluationFormatAdapter` is an architectural boundary for honest adapters;
    it does not make arbitrary linked Rust code untrusted-safe.

---

## 10. Delivery phases

### Shared completion rule for implementation phases

Phases 0–2 are accepted incrementally; Phase 3 is a deferred template for
future additions. Each implemented phase must pass its own tests and keep all
earlier phase tests green. Passing a unit test or compiling a mock
adapter does not imply that a public route or the real TypeSafe service works.
Before submitting source changes for any phase, run the workspace all-feature
test suite, strict Clippy, formatting, rustdoc with warnings denied, and
doctests. `cargo nextest` does not execute doctests, so doctests are a separate
check. The reproducible local commands are:

```sh
cargo nextest run --workspace --all-features
cargo clippy --workspace --all-features --tests -- -D warnings
cargo fmt --all -- --check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo test --doc --workspace --all-features
git diff --check
```

If `cargo-nextest` is unavailable, use `cargo test --workspace --all-features`
for the test-suite step. When configuration schema or registry data changes,
also run `cargo run -p dist-helper -- registry validate`,
`cargo run -p dist-helper -- registry build`, and
`cargo run -p dist-helper -- check`, and commit the regenerated artifacts.
The relevant Linux, macOS, Windows, MSRV, feature-isolation, dist, and
documentation CI jobs must pass; a new integration test must be added to
Linux's explicitly enumerated test shard so CI actually exercises it there.

Phase completion evidence records the commit, test command and result, CI
status, and any deliberately skipped external test. A green local run is not a
green hosted CI run, and neither alone establishes production readiness.

### Phase 0 — canonical contract and operation-aware catalog

- Add `InferenceOperation::{Generate, Evaluate}`.
- Add canonical evaluation request/question/answer/result types and validation.
- Add provider/model operation configuration and schema generation.
- Add `ModelInfo.operations` and operation-aware model listing.
- Reject generation/evaluation operation mismatches before dispatch.
- Add the exact TypeSafe Jev version as canonical registry metadata only.
  Do not add an active provider route or claim it works until Phase 2 passes.

**Pass gate:** Canonical type tests cover all three question/answer variants,
mixed questions, structured JSON and permitted `null` descriptions, invalid
shapes, and preservation of structured JSON through serialization.
Configuration regression tests prove that
legacy generation-only configs, selectors, and model listings retain their
behavior. Operation-resolution tests reject an evaluation-only model on every
generation ingress and reject a generation-only model for the evaluation
operation before dispatch. A catalog-only Jev entry is not advertised as
routable without its extension. Schema/registry validation, build, and
freshness checks pass. No live provider call is required or claimed.

### Phase 1 — evaluation execution rail

- Add the revisioned native `ExtensionApi::register_evaluation_format` hook and
  typed `EvaluationFormatAdapter` contract.
- Add custom-host assembly that accepts registered evaluation-format facets;
  do not add a Rust dynamic-library loader or register a concrete adapter in the
  default `bro` host.
- Add a non-streaming evaluation executor and pipeline.
- Reuse bounded HTTP, authentication, provider headers, deadlines, usage,
  pricing, and settlement helpers without calling the generation pipeline.
- Add cancellation and graceful-shutdown evidence.

**Pass gate:** A test-only registered adapter and mock HTTP upstream exercise
the complete internal evaluation pipeline without TypeSafe credentials.
Registration tests cover valid binding, duplicate ids, missing adapters,
revision mismatches, and inactive unbound registrations; invalid bindings
fail before database assembly or an upstream call. Mock-upstream tests prove
that endpoint selection, authentication, headers, deadlines, retries, response
validation, metering, cancellation, and shutdown remain host-owned. Started
attempts end with completed, failed, cancelled, or unknown-remote evidence;
tests do not infer that client cancellation prevented remote execution. The
default `bro` host remains free of a concrete evaluation adapter. This phase
does not claim a working public `/v1/evaluate` endpoint.

### Phase 2 — System One format extension, TypeSafe binding, and public endpoint

- Add the TypeSafe provider route, metadata, and `TYPESAFE_API_KEY` credential
  declaration without compiling the format implementation into the core or
  default host.
- Move the existing native Rust format crate into
  `extensions/system-one/format/`, rename its unreleased Cargo package to
  `bitrouter-system-one-format`, and register `system-one/json@1` in an
  explicit custom-host composition. The package owns the System One JSON
  render/parse contract, not TypeSafe account or transport configuration.
- Integrate its registration with main's shared `ExtensionApi` and custom-host
  lifecycle from PR #923; do not ship a second evaluation-only registration
  surface or a new host per provider.
- Implement `/v1/evaluate`.
- Do not register an inbound `/v1/systemone` route or advertise TypeSafe SDK
  compatibility. The extension's outbound provider call remains
  `/v1/systemone`.
- Implement the OpenRouter Decisions-shaped Noul, Choice, Score, identity,
  provider, usage/cost, confidence, legend, and actual provider-version
  contract.
- Add local conformance fixtures plus a separately gated real-provider smoke
  test that does not run without an explicit credential.
- Add bounded server-side tests that show unknown top-level fields are ignored
  without changing routing or provider requests. Measure practical question
  count and question-id limits before proposing numeric defaults.
- Update `skills/bitrouter/` and all generated config/registry artifacts in the
  same change so the shipped skill distinguishes stock `bro` from the custom
  host and does not advertise an unavailable route as built in.

**Pass gate — deterministic:** The format extension's conformance fixtures
cover Noul, Choice, Score, mixed requests, structured state, choice
descriptions including `null`, provider model-id mapping, actual returned
version, usage/cost, and malformed upstream answers. A full assembled-app HTTP
test sends `/v1/evaluate` requests through a custom host to a mock TypeSafe
`/v1/systemone` server and verifies auth, routing, response, metering, and
error behavior. Tests prove the descriptor is format-owned, TypeSafe binds to
it through provider config, and no second provider binding is advertised in
the first release. They also prove the canonical and provider-pinned selectors
work, the moving alias is not listed, generation/evaluation mismatches fail
before dispatch, and no inbound `/v1/systemone` route exists. Server tests show
that unknown top-level fields, including gateway-style controls, have no routing,
trace, or upstream-body effect. Requests above the previously proposed 256
questions or 128-byte question-id length are not rejected by those discarded
defaults when otherwise valid and within the existing body bound; oversized
bodies fail before upstream dispatch. Bounded measurements record practical
question-count and id-length behavior without claiming a universal maximum.
The TypeSafe-evidenced probability-precision rule in section 4.6 is tested with
both valid rounding drift and invalid sums; its presence in the first format
crate does not certify another provider's wire behavior. The custom-host test
target is included in all applicable CI matrices, the shared extension host
remains functional for existing request-check extensions, and the shipped skill
and generated artifacts match the implementation.

**Pass gate — real provider:** A separately gated smoke test uses an explicit
TypeSafe credential, synthetic non-sensitive input, and a bounded request
budget. It verifies a successful mixed-question request, the actual reported
model version, answer keys/types, finite probabilities, and token usage. It
does not assert a fixed choice, probability, latency, or cost for a changing
remote service. Record when and against which version it passed; redact the
credential and input from logs. Do not put this secret-dependent test in
ordinary public PR CI. Without a passing real-provider smoke test, report
"deterministic integration passed" rather than "Phase 2 passed".

**Historical Phase 2 evidence (2026-09-22, before format-first repackaging):**
implementation commit `d2cdc6f6`;
local `cargo nextest run --workspace --all-features` passed 3,555 tests with
23 intentional skips. Strict Clippy, formatting, rustdoc, doctests, registry
validation/build/freshness, and diff whitespace checks passed. [PR #936
hosted CI](https://github.com/bitrouter/bitrouter/pull/936) passed the Linux,
macOS, and Windows test and Clippy jobs, plus the applicable MSRV,
feature-isolation, dist, documentation, and repository checks. The separate
credentialed TypeSafe smoke test was not run. The old provider-named crate and
parallel host path also predate integration with main's shared extension API.
These results are regression evidence, not a pass for the revised Phase 2 or
production proof.

### Phase 3 — deferred additional format or provider

No second provider or format is committed for the first release. In particular,
Laya is not a Phase 3 deliverable or a release gate. OpenRouter remains a
reference for the public contract, not a planned upstream route. A later
proposal for either requires its own scope and wire-contract review.

An added provider may bind an existing format extension only after independent
fixtures and an assembled-app test demonstrate exact request and response wire
compatibility, including probability precision and error behavior. If it is not
compatible, add a new format identity or revision under
`extensions/<format>/<package>/` rather than branching on provider id inside a
nominally shared adapter. Each provider still requires canonical-to-wire model
mapping, authentication, advertised question types, provider-specific limits,
actual version, usage/cost evidence, and an explicit decision on cross-provider
fallback. An external provider needs a separately gated real-provider smoke;
a local process also needs readiness, shutdown, and failure tests. Passing
TypeSafe's suite alone does not pass another binding.

**Pass gate when Phase 3 is scheduled:** Format conformance and provider-binding
tests independently cover the requirements above, followed by a full
assembled-app mock-upstream test, applicable real-provider/process tests, and
the shared local and hosted checks. No Phase 3 pass is asserted while no new
format or provider is selected.

**Historical prototype evidence, not current-scope acceptance:** The earlier
Laya-specific implementation at commit `46df8402` passed local format,
fake-process, assembled-app, and opt-in real-checkpoint tests, as well as the
applicable hosted checks in [PR #938](https://github.com/bitrouter/bitrouter/pull/938).
That evidence remains available for a future proposal; it does not add Laya to
the registry, satisfy the revised format-first Phase 2, or clear the pending
TypeSafe smoke test.

### Phase 4 — optional future WASM decision gate

This phase is not committed implementation. Reconsider a WASM runner for pure
format facets only after all of the following are true:

1. at least two independently implemented evaluation formats exercise the same
   canonical contract;
2. operators need to install or update provider format support without
   rebuilding a custom host;
3. an out-of-tree release/publisher lifecycle exists;
4. the native facet has remained pure render/parse and has not acquired
   credentials, arbitrary networking, database, or pipeline authority;
5. exact native/WASM fixture equivalence and binary-size/latency/resource
   measurements are available; and
6. versioned WIT, import allowlists, signatures, lockfiles, rollback, and honest
   in-process isolation language have been separately accepted.

Authentication/signing, complex transport, vendor SDK, and local-runtime facets
do not automatically move to WASM. A separate process may provide a stronger
and more appropriate boundary for those capabilities.

**Decision gate, not an implementation pass:** Phase 4 reaches a go/no-go
decision only when the six items above have documented evidence. A no-go or
deferral is a valid outcome. If WASM is approved, its runtime, packaging,
permissions, compatibility, resource limits, and rollback tests require a
separate implementation specification; native-format test success is not a
WASM-runtime pass.

---

## 11. Acceptance ledger

| ID | Acceptance requirement |
| --- | --- |
| EV01 | `/v1/evaluate` accepts valid Noul, Choice, Score, mixed-question, string-state, and structured-state requests using the OpenRouter Decisions question/answer spelling. |
| EV02 | TypeSafe System One preserves Noul without thresholding; Choice and Score preserve distributions, legend, and provider confidence. |
| EV03 | Answer ids and types must exactly match the request; missing, extra, NaN/out-of-range, wrong-key, and distributions outside the provider-evidenced precision rule fail as `upstream_invalid_response`. Valid rounding drift is accepted without renormalization. |
| EV04 | `typesafe/jev-1.13` resolves to the TypeSafe route; `typesafe:typesafe/jev-1.13` pins it; unknown selectors fail without an upstream call. |
| EV05 | Sending an evaluation-only model to every generation endpoint fails with `model_operation_mismatch` before dispatch. Sending a generation-only model to `/v1/evaluate` does the same. |
| EV06 | Existing configs without `operations` retain identical generation model lists, routing, protocol preference, and serialized config behavior. |
| EV07 | Public model listing exposes only routable registered-extension routes; administrative inspection may show a catalog route as unavailable with `missing_extension` without presenting it as stock-`bro` support. |
| EV08 | Provider-returned input/output tokens and the configured zero output price produce truthful usage and cost. Non-zero output usage must not become zero because output is free. |
| EV09 | The OpenRouter-shaped response returns `id`, actual provider model/version, provider id, answers, and snake_case token/cost usage; internal request evidence separately preserves requested selector, canonical model, selected account, and provider wire id. |
| EV10 | State, instructions, criteria, answers, distributions, credentials, and raw error bodies do not appear in default logs, traces, model listings, or human errors. |
| EV11 | Local validation, the existing HTTP-body bound, and known provider-specific option and score limits prevent upstream dispatch when violated; this release adds no default question-count or question-id byte-length cap. |
| EV12 | 408/429/529/5xx/timeout behavior follows the bounded same-provider/account retry contract; other 4xx failures do not fall through. |
| EV13 | Client cancellation and daemon shutdown never claim remote non-execution; admitted attempts end with completed, failed, cancelled, or unknown-remote evidence. |
| EV14 | No automatic cross-provider evaluation fallback is constructed in this increment; an ambiguous unpinned multi-provider route fails validation. |
| EV15 | Evaluation does not run generation prompt transforms, tool loops, continuation handling, reasoning filters, or generation hooks. |
| EV16 | Each implemented phase retains earlier tests and passes the shared all-feature tests, strict Clippy, formatting, rustdoc, doctest, generated-artifact, and applicable cross-platform CI gates in section 10. |
| EV17 | The shipped `/bitrouter` skill documents `/v1/evaluate`, selector syntax, non-streaming behavior, operation mismatch, the lack of inbound `/v1/systemone` compatibility, and the fact that direct TypeSafe support requires the matching custom-host native extension. |
| EV18 | The default `bro` host contains no concrete System One evaluation-format registration; a custom host explicitly links and registers the native extension. |
| EV19 | Duplicate adapter ids, missing configured adapters, and revision mismatches fail before database assembly and before any upstream call; an unconfigured valid registration remains inactive. |
| EV20 | `EvaluationFormatAdapter` receives bounded typed evaluation data and JSON only; endpoint choice, credentials, HTTP execution, timeout, retries, cancellation, settlement, and metering remain host-owned. |
| EV21 | No universal `ProviderAdapter::execute(json)`, Rust dynamic-library loader, WASM runtime, extension installer, lockfile, or marketplace is introduced. |
| EV22 | The concrete extension is organized by a versioned wire format, not by provider; this increment implements only the evaluation-format facet and adds no unused hooks for hypothetical operations. |
| EV23 | Unknown top-level `/v1/evaluate` fields are accepted but ignored, do not override known fields or change provider requests, and are covered by server-side tests; post-implementation measurements inform any later numeric question-count or id-length caps. |
| EV24 | No inbound `/v1/systemone` route is registered; an evaluation through `/v1/evaluate` still calls TypeSafe's outbound `/v1/systemone` endpoint. |
| EV25 | Phase 2 is not reported complete until a separately gated real-TypeSafe smoke test passes with an explicit credential and recorded model version; ordinary public PR CI remains credential-free. |
| EV26 | Phase 3 has no first-release provider target. Any later format or provider binding passes its own conformance, assembled-app, and applicable real-provider/process gates rather than inheriting TypeSafe's result; Phase 4 is a documented go/no-go decision, not a WASM implementation pass. |
| EV27 | The TypeSafe route binds to `system-one/json@1` through provider configuration; the format crate lives under `extensions/system-one/format/` and registers through main's shared `ExtensionApi` and custom-host lifecycle without a parallel registrar. |

Real-provider smoke evidence proves only that the configured test account could
reach one provider/model version at that time. It does not replace fixture-based
protocol coverage, calibration studies, hosted CI, or production readiness.

---

## 12. Rejected alternatives

### 12.1 Encode questions inside Chat Completions

Rejected because it loses the native operation contract, weakens probability
validation, and falsely advertises Jev as a text-generation model.

### 12.2 Add `Capability::Evaluation`

Rejected because capability filtering assumes one shared `Prompt` contract.
Evaluation has different canonical input and output types.

### 12.3 Reuse `OutboundAdapter` with provider JSON in supplemental fields

Rejected because it would make the core `Prompt` a carrier for opaque
provider-specific requests and make `GenerateResult` incapable of representing
typed answers honestly.

### 12.4 Generalize every inference API in the first change

Rejected for the first implementation. Replacing all generation APIs with a
single `InferenceRequest`/`InferenceResult` enum would touch stable streaming,
tool, continuation, and protocol behavior without being necessary to ship a
correct evaluation sibling pipeline.

### 12.5 Treat every Jev carrier as an automatic fallback

Rejected for this increment because a shared brand or alias does not establish
the same behavior, limits, metadata, or calibration. There is no OpenRouter
gateway route in scope, and a future cross-provider fallback policy must be
designed separately from explicit provider-pinned access.

### 12.6 Make TypeSafe's `/v1/systemone` the only public endpoint

Rejected because the product capability is typed evaluation, not one vendor's
brand. This release exposes only `/v1/evaluate`. A later inbound
TypeSafe-compatible endpoint would be a thin adapter over the same canonical
evaluation pipeline, not a separate inference implementation.

### 12.7 Use Vercel's `boolean` evaluation vocabulary

Rejected by product decision. The public `/v1/evaluate` request and response use
the OpenRouter Decisions `noul`, `choice`, and `score` spelling. This does not
adopt OpenRouter's endpoint path or gateway routing controls.

### 12.8 Define one universal provider adapter

Rejected because `execute(operation, json) -> json` would erase typed operation
validation, let provider code invent unsupported semantics, weaken metering and
error guarantees, and make extension authority impossible to bound. Provider
extensions are packages of typed facets, not universal callbacks.

### 12.9 Compile the System One adapter into core or register it in default `bro`

Rejected for this increment. Concrete wire-format code lives in a separate
native Rust extension crate and is linked by an explicit custom host. This keeps
the core and stock host free of the concrete adapter, at the cost of requiring a
custom build rather than runtime installation.

### 12.10 Load native Rust dynamic libraries

Rejected because Rust has no stable plugin ABI and an in-process dynamic library
would have the daemon's ambient authority without a meaningful sandbox. Native
extensions remain lockstep compiled hooks.

### 12.11 Require WASM format extensions now

Rejected because one official format adapter does not yet justify the runtime,
WIT, package, installer, signature, resource-control, and compatibility surface.
The future decision gate in Phase 4 preserves the option for pure format facets
without committing authentication, transport, or local-runtime facets to WASM.

### 12.12 Create one evaluation-format extension per provider

Rejected because a provider owns credentials, endpoint, model mapping, limits,
and pricing, while the adapter owns a versioned upstream wire contract. A
provider-named format package obscures reuse and encourages provider branches
inside render/parse. Format reuse still requires independent conformance for
each provider; it is never inferred from shared terminology or model behavior.

---

## 13. Review decisions

The core product direction is already decided by this specification:

- new `/v1/evaluate` operation;
- existing canonical/provider-pinned model selection;
- OpenRouter Decisions-shaped `noul`, `choice`, and `score` request/response;
- separate typed evaluation pipeline;
- format-owned native extensions and provider-configured bindings rather than
  one extension per provider or one universal provider adapter;
- compiled native Rust registration through `ExtensionApi`, with no concrete
  adapter in the default `bro` host;
- TypeSafe Jev as the first provider/model route, bound to the
  `system-one/json@1` native format extension in a custom host;
- Laya outside the current delivery scope; Phase 3 is deferred until a new
  format or provider binding is explicitly selected;
- unknown top-level fields accepted but ignored, with server-side verification
  after implementation;
- no default question-count or question-id byte-length cap before measurements;
- only the pinned `typesafe/jev-1.13` model in the initial registry;
- OpenRouter used as a format reference, not an upstream provider route;
- no WASM runtime or runtime-installable provider extension in this increment;
  and
- `/v1/evaluate` as the sole new public endpoint, with inbound
  `/v1/systemone` compatibility deferred.

The direct TypeSafe extension still calls the provider's `/v1/systemone`
endpoint outbound. This is not an inbound BitRouter route or a claim that the
TypeSafe SDK can use BitRouter by changing only its base URL.
