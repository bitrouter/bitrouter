# Registry v1 conformance

Status: **normative beta, `1.0.0-beta.4`; wire version `1`.**

These observable requirements accompany the [Registry v1 protocol](README.md).
Each case applies to the producer, reader, synchronizer, or publication role
defined by that contract. Unsupported optional interfaces and billing features
remain subject to the protocol's scoped compatibility rules. A retained record
does not by itself establish execution or pricing support.

Conformance cases define expected behavior, not a report of completed work.
Implementations should record results against a precise protocol revision and
source/artifact snapshot in their own tests and acceptance evidence.

## Conformance cases

Tests below are semantic scenarios, not checks that a particular commercial
model remains listed forever. Each synthetic fixture is small and makes one
failure observable. Negative cases must assert both the diagnostic and absence
of unintended catalog/routing/credential effects.

| ID | Scenario | Required observable result |
| --- | --- | --- |
| C01 | Missing/wrong/nonintegral version; unsupported major | Reject new artifact; retain previous document |
| C02 | Unsupported beta; newer stable v1 optional data | Reject unknown beta; accept compatible stable data |
| C03 | Unknown field at every nested level through sync/build/cache | Retained unchanged; known routes continue working |
| C04 | Unknown type/modality/capability/protocol/operation mixed with known entries | Load catalog; no invented support; known interfaces remain usable |
| C05 | Unknown provider auth/status, unknown price kind, scoped `requires` | Disable only prescribed scope; no auth/price fallback |
| C06 | Malformed known field, duplicate keys/IDs, null required field | Reject invalid candidate; do not silently convert to an unknown extension |
| C07 | `openai`/`anthropic` in protocol positions versus identity positions | Retain unsupported interface without invocation; provider/model identity unchanged; no vendor-to-protocol inference |
| C08 | Old curated version expires but provider still lists it | Curated entry removed; provider offer/metadata unchanged; explicit canonical and native requests still resolve |
| C09 | New provider-native model absent from curated IDs and prices | Retained; explicitly addressable if interface/auth allow; not an automatic candidate |
| C10 | Same native ID at two providers; two native aliases for one canonical model | Distinct inventory offers; deterministic routing and no cross-provider identity inference |
| C11 | New unusable release, family exclusions, explicit curation tombstone | No eviction of healthy older version; no rediscovery of tombstoned entry |
| C12 | Grace boundary, repeated sync, re-entry, discovery failure | UTC expiry; no deadline sliding; cancellation works; failures do not fabricate releases |
| C13 | Provider withdraws last eligible offer | Only its inventory changes; publication removes unavailable curated recommendation; no provider suspension |
| C14 | JSON Pointer nested mapping, escaped slash/tilde, empty object key, root array | Exact reads/writes, fixed-index arrays, no expression evaluation |
| C15 | One source copied to native/canonical targets with fixed prefix | Native ID untouched; `canonical_id` assigned only after independent identity/binding verification |
| C16 | Missing versus null versus wrong type, conflicting destinations, unsupported transform | Preserve/default/clear/fail exactly as specified; no silent identity or capability defaults |
| C17 | Per-token string price scaled by 1,000,000; unknown or invalid unit/value | Exact checked conversion when declared; otherwise unknown/error, never free |
| C18 | Public feed, bearer key, custom header/version, empty/missing key | Correct headers; no credential inheritance, network request on missing key, or secret in logs |
| C19 | Redirect, 401/403, timeout/429/5xx, oversize/malformed body | Bounded execution; no auth downgrade or partial inventory publication |
| C20 | Multiple pages, empty middle page, duplicate ID/cursor, page cap, missing cursor | Complete traversal or whole-feed failure; never retire from first-page-only data |
| C21 | Complete missing runs, failed run between, repeated/stale observation sequence, account scope, empty listing | Withdrawal only with both count/time evidence; no double-count; account/empty guards preserved |
| C22 | Manual override, separate pricing feed, interface reorder, unknown extension, reappearance | Ownership respected; prices join by protocol/operation/profile triple; no accidental reactivation or extension loss |
| C23 | Same inputs built across a wall-clock expiry boundary; one-document generation | Same explicit `as_of` yields identical output bytes with one producer configuration; canonical joins derive from inventory; agents/runtimes unchanged |
| C24 | Truncated/invalid document or cache, unsupported version, offline refresh | Whole-document admission; last good document retained; explicit expiries still enforced |
| C25 | Model supports generation and Decisions with different tariffs | Matching operation selected; Decisions never charged generation rate |
| C26 | Future media/embedding model alongside current chat model | Old reader loads both; skips unsupported media operation and meters; chat still works |
| C27 | Different modalities/capabilities on two interfaces | No capability union across interfaces; incompatible request rejected before dispatch |
| C28 | Native selector with reserved characters, local overrides, subscription offer | Listing/dispatch round-trip; no credential escalation or implicit subscription enrollment |
| C29 | Failed required feed A, successful B, discovery failure | B can be proposed coherently; A preserved; final CI failure visible |
| C30 | Fork PR changes URL/auth/feed instructions | Offline validation only; no privileged credentialed request or agent execution |
| C31 | Trusted dry-run/write and real scheduled sync | Same plan; one valid complete document; explicit secret prerequisites and sanitized failures |
| C32 | One native LLM has both generation and decision interfaces | One native/canonical identity; both roles usable only through their verified interfaces; no support inferred from type labels |
| C33 | OpenAI Decisions and two System One profiles, including a `/decisions` path alias | Select the declared codec; reject unsupported/lossy conversion; preserve question IDs/types, typed choices, refusal and confidence semantics |
| C34 | Token, answered-question, and completed-request decision tariffs; partial answers, retries, cached/repeated inputs | Exact registered counter and applicable tariff; no question/request confusion, inferred multiplication, missing charge, or cross-interface rate fallback |
| C35 | Malformed, oversized, deeply nested, or unsupported catalog refresh in a running host | Bounded normal error with scope/recovery hint; process survives, last good document and independent routes still work |
| C36 | Cold start has no usable cache and an incompatible bundled catalog | Empty registry, actionable compatibility error; control surfaces/local routes survive; registry-dependent requests fail normally |
| C37 | Verified, unknown, delisted, renamed, and conflicting OpenRouter identities | Current known `canonical_id` required; history retained but cannot bypass current audit; uncertain offers remain native-addressable; conflicts fail reviewably |
| C38 | User-defined router explicitly selects non-curated canonical/native offers with no quality scores | Routes work when execution gates pass; default automatic pool unchanged; score-dependent policy reports missing evidence |
| C39 | Unknown protocol profile/question type/meter among known interfaces; source/cache round-trip | Preserve raw values; disable only unsupported scope; unknown question semantics grant no support and unknown meters make price unavailable |
| C40 | Full-list audit of curated IDs and all provider canonical IDs, including unchanged/non-curated/inactive rows | Every reference is checked against exact `data[].id`; neither `canonical_slug` nor curation filters substitute; missing known bindings and unrelated-model bindings fail |
| C41 | One or multiple invalid canonical IDs while other provider updates succeed | Report all offending paths/IDs; global audit fails; no source/artifact publication or sync PR update; existing inventory/published document preserved |
| C42 | OpenRouter timeout/auth failure, empty/malformed/duplicate-ID/truncated response, unexpected pagination, or old cache | Audit unavailable and final CI failure; no historical/cache fallback, credential downgrade, or absence-driven deletion |
| C43 | Historical ID no longer listed, with and without public references | Referenced ID blocks publication; unreferenced ledger history is reported and retained; reviewed native-only resolution does not withdraw provider offer |
| C44 | Agentic edit, manual edit, rebase, or later publication after a passing audit | Changed candidate requires a new audit; later run refetches; recorded evidence and output-file SHA-256 identify exact validated output; offline checks never claim live success |
| C45 | OpenRouter audit includes image/decision/embedding/audio models | Request uses exactly `output_modalities=all`, omits narrowing/pagination parameters, and validates non-text IDs; missing/default-text parameter fails configuration |
| C46 | A(10), B(11), replay A(10), same-sequence conflict, backward time, reload, failed global audit | Stale/replay causes no mutation; conflict/time rollback fails normally; checkpoint and offer changes commit together only after validation |
| C47 | 3 decisions at USD 0.20/1000; token subsets; multiple meters/attempts; unknown usage | Exact USD 0.0006 decision charge; disjoint token counters and all applicable charges summed without per-item rounding; unknown total unavailable |
| C48 | Known/unknown access policies, missing HTTP base, reasoning levels/default, endpoint profile and cached/reasoning subsets | Resolve explicit contracts without legacy implementation knowledge; unknown execution prerequisites disable affected scope; no guessed credential, counter, or multiplier |
| C49 | Withdrawn offer returns with ID-only listing; explicit mapped status; manual status override | Feed-owned status resolves from mapping then status default on every presence; reactivates deterministically; manual status and other missing metadata preserved |
| C50 | Any configured feed is failed, unsupported, or missing a secret | Final run fails for each; successful independent updates may be proposed only after global audit; an unknown optional flag cannot downgrade requiredness |
