# Gemini protocol retirement progress

Status: implementation complete with local and hosted CI checks passing; replacement and release validation remain open.

Baseline: PR #962 at `529f2fdeb7dc1f6bd3cef2ae243b22106b66ef16`.
Branch: `codex/retire-gemini-protocol`. Spec approved in `3c211ffc`.
Updated: 2026-10-07, America/New_York.

## Implemented behavior

- Three built-in codecs remain: Chat Completions, Responses, Messages. Native Gemini ingress, codec, transport, typed request snapshot and executable protocol variant are removed.
- Metered `google` uses `https://generativelanguage.googleapis.com/v1beta/openai`, bearer auth and unchanged `GEMINI_API_KEY`. Declared Google model compatibility scopes extensions; a generic Chat envelope does not grant Google replay authority.
- Tool-call signatures survive ordinary/SSE parsing, collection, downstream output and subsequent ingress. A stateless replay proof binds exact call/signature bytes to the selected static key, endpoint, model, provider and account label. It travels only between BitRouter and its client and is removed before Google dispatch.
- Unsupported schema constraints, Google cache resources, message-level continuity and unclassified Google options are refused before effects. Continuity failures after HTTP success carry available provider usage into SDK settlement; streamed content is never retried after visible commitment.
- Native-only Zen Gemini entries are removed based on the [official endpoint table](https://opencode.ai/docs/zen/#endpoints); other Zen models remain.
- `google-ai` private Antigravity protocol/auth/import/refresh and Vertex Express are retired. The bundled Gemini CLI ACP/runtime entry is removed because its routing requires the retired native gateway. Independent own-auth `agy` launch is retained.
- Active retired configuration gets bounded location-specific migration errors. Catalog/cache parsing filters retired providers/models before decoding current protocol vocabulary. Saved credentials and historical protocol provenance remain readable; no automatic billing/credential migration occurs.
- Internal docs, shipped skill guidance and generated artifacts are synchronized. Historical AI-refactor batch evidence remains labeled as baseline evidence.

## Acceptance ledger

| Gate | Current evidence and remaining work |
| --- | --- |
| GR-01 | Local pass: three-protocol ordinary/SSE, assembled routing/structured-output matrix, negative native route tests with zero executor/upstream calls. |
| GR-02 | Fixture tests pass for opaque bytes, tool-call placement, fragmented arguments, parallel same-function calls and late/signature-only updates. Selected call and later gateway request are tested. SDK routing also passes two signed tool cycles with a parallel same-function turn, in both ordinary and streaming modes; unrelated fallback targets are excluded. |
| GR-03 | No metered Gemini credential available to this process. User requested live testing at the end. Two real tool cycles, parallel turn, thinking model, latency/errors/usage/cost remain a release gate. |
| GR-04 | Target-scoped thinking controls, cache refusal, schema exclusions, actual auth and replay guards covered locally. Real image/schema/reasoning/usage coverage pending with GR-03. |
| GR-05 | Actual `google-antigravity==0.1.20` completed a tool-using task through the assembled `bro` gateway and a fixture Chat upstream: two streamed calls, custom tool executed once, returned nonce verified. Signed-response fixture fails: SDK drops the BitRouter replay sidecar and replaces function details with empty name/arguments after a metadata-only delta. Google signed-tool interoperability remains blocked; generic SDK execution is proven locally only. |
| GR-06 | Pre-dispatch provider/protocol guards and cache filtering implemented; dedicated config/cache/activation fixtures pass, including mixed migrated/native model defaults. No credential store migration is performed. |
| GR-07 | Local pass: historical identifiers decode as provenance, saved OAuth fixtures remain byte-identical, source-bound disk caches remain readable after quarantine, retired calls fail. These fixtures do not certify external legacy raw formats or production recovery. |
| GR-08 | Local pass: native execution modules/registrations removed; the remaining diagnostic enum is historical decoding only. No renamed first-party native transport remains. |
| GR-09 | Local pass: 3,659 workspace tests, six doctests (one existing ignored example), Clippy with warnings denied, formatting, registry validate/build/check, config schema and pinned public API guards. The public dependency inventory remains 16, with no OpenTelemetry type exposed. The final malformed-signed-argument hardening passes the full suite; all 410 AI unit tests also pass after removing panic-based error handling from restored shared fixtures. Hosted CI: all 22 jobs passed at implementation commit `ad5f7a41`, including Linux/macOS/Windows tests, feature isolation and public API checks. External consumers remain blocked as below. |

## External release blockers

`bitrouter-cloud` checkout inspected at `/Users/kelsen/Documents/Code/bitrouter-cloud`.
It still depends on released SDK alpha.30 and has direct removed-variant/module consumers:

- `src/v1/routing.rs`: native protocol mapping and test.
- `src/routing_preset/canonical_params.rs`: native response-format fixture.
- `src/openapi/operations.rs`: imports native codec schemas and advertises native operations.
- `src/openapi/info.rs`: native endpoint documentation.

Cloud requires the parent #962 AI API migration plus this retirement migration and its own tests before updating its dependencies. This branch does not claim that delivery or deployment.

`bitrouter-docs` product API and provider migration guidance, English/Chinese authoring and committed catalog refresh must be delivered against the new public catalog before the breaking release. No external repository is edited by this implementation branch. Plugin manifests were searched; they do not name the removed protocol/provider/agent and need no manifest edit.

## Evidence classes

Local fixtures, actual SDK execution against a fixture provider, credentialed Google inference, hosted CI and production proof are recorded separately. The parent PR's test totals are not validation of this branch. Hosted CI success applies to the exact implementation commit recorded below. No live-provider or production success is claimed.

## Actual SDK fixture evidence

Python 3.12, `google-antigravity==0.1.20`, actual packaged local harness, foreground `bro` in an isolated temporary home with loopback ports and `inherit_defaults: false`.
Unsigned fixture: two `/v1/chat/completions` requests with `stream: true`; `lookup_nonce(query="review-fixture")` executed once; final text `verified nonce: fixture-4356`.
Signed fixture: one upstream call, followed by gateway HTTP 400 before a second upstream attempt. The valid base64 Google signature survived, but the SDK omitted `extra_content.bitrouter.google_replay_proof` and replayed an empty function name with `{}` arguments. This is a real SDK/harness client limitation, not credentialed Google inference. Do not advertise this SDK version as a Google thinking-tool client or bypass replay validation.

Full-content telemetry now redacts continuity values while durable canonical records keep their original values. A span-capture fixture exercises that boundary.

## Reproduction commands

Install `google-antigravity==0.1.20` in a separate Python >=3.10 environment, then run:

```sh
python scripts/validate_gemini_retirement_sdk.py --bro target/debug/bro
python scripts/validate_gemini_retirement_sdk.py --bro target/debug/bro --signed
```

The unsigned run must complete a tool task; the signed run verifies the known client limitation and pre-upstream rejection. Its successful exit means that the limitation was reproduced, not that Google tool interoperability passed. Both runs use fixture credentials and loopback upstreams.

The first full workspace build exhausted local disk while linking. Only this worktree's generated incremental cache was removed. The completed local verification used `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`, two build jobs and two test threads. These are build-storage settings; no test behavior or checks were disabled.

Google reasoning uses the canonical declared `reasoning_effort` path (`minimal` through `high`) plus scoped `include_thoughts`. Raw `thinking_level`/`thinking_budget`, `none`/`xhigh`/`max`, seeds and penalties remain excluded until selected-model equivalence is demonstrated. Static-auth validation rejects duplicate bearer headers and conflicting Google key headers/query credentials.

The initial model for the full text/image/function/schema/thinking validation is `gemini-3.8-flash`, the model used in Google's current Chat compatibility examples. Its route declares those capabilities and the common `minimal`/`low`/`medium`/`high` effort set. This is documentation/fixture evidence; the live gates are still pending. Older entries retain their existing narrower capability declarations.

## Hosted CI follow-up

The [initial hosted CI run](https://github.com/bitrouter/bitrouter/actions/runs/37570017412) at `5066839e` found an ungated optional file-store import in the new retirement integration fixture. The credential portion is now guarded by `file-store`; historical protocol decoding/rejection remains covered without file storage. Local AI tests pass with no default features and with `pkce`, `hosted-login`, and `file-store` individually, plus Clippy with all features/tests and warnings denied.

The [corrected hosted CI run](https://github.com/bitrouter/bitrouter/actions/runs/37570703221) completed successfully at `ad5f7a412d13d92679f47aed0c13008bc950d9f4`: all 22 jobs passed, including workspace/application tests on Linux, macOS and Windows, Clippy on all three platforms, feature isolation, doctests, documentation, MSRV, registry/schema, plugin and pinned public API checks. Subsequent documentation-only evidence updates do not change the tested implementation; any CI on those commits is a separate run.
