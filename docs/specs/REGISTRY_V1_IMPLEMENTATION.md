# Registry v1 delivery and conformance plan

Status: **design only; implementation not started.** Contract revision:
[`1.0.0-beta.1`](REGISTRY_V1_SPEC.md). Date: 2026-10-22.

This work stacks on [#990](https://github.com/bitrouter/bitrouter/pull/990),
`codex/gateway-multi-instance` at
`337123edd7a3992f3202a6a43a8753f7d0dec8d4`. The design branch is
`codex/registry-v1-spec`. Recheck the parent's head before implementation;
this document does not imply #990 has merged.

## 1. Baseline and ownership

These paths are verified against the parent, rather than the older pre-refactor
layout. The v1 migration must respect the parent's service/AI ownership split.

| Owner/path | Current limitation | Required change |
| --- | --- | --- |
| `helpers/dist-helper/src/registry.rs` | Closed source enums, unversioned artifacts, vendor protocol strings | Open v1 document model, schema validation, normalized interfaces, versioned deterministic pair |
| Same file: `apply_model_discovery_plan` | Canonical expiry removes provider rows and may suspend providers | Curation-only retention; separate provider reconciliation |
| Same file: `canonical_resolver`, `v1_models_plan_for_provider`, `models_dev_plan_for_provider` | New offers join only against current curated IDs; no price means no import | Exact native inventory identity and explicit independent canonical bindings; absent price retained |
| Same file: `fetch_v1_models`, `v1_auth_headers` | Fixed list shape, no feed credentials, no paging/reconciliation | Declarative per-feed request/mapping/auth/pagination and staged transactions |
| `crates/bitrouter-ai/src/catalog/types.rs` | Closed registry values; legacy source/runtime protocol translation | Shared v1 metadata contracts with raw unknown retention; no network or app configuration here |
| `crates/bitrouter-ai/src/types.rs` | Custom protocols currently imply generation | Registry activation requires registered protocol/operation compatibility; do not grant unknown interfaces execution by parsing |
| `apps/bitrouter/src/gateway/catalog/{fetch,decode,store,cache,bundled}.rs` | Historical decoding and private cached projection | One version/revision admission contract, coherent pair publication, raw metadata retained |
| `apps/bitrouter/src/providers/registry/apply.rs` | Canonical-dependent metadata joins and provider-wide protocol maps | Self-contained offer projection, per-interface facts, local-config precedence retained |
| `apps/bitrouter/src/gateway/routing/` and automatic selector callers | Model alias and operation routing integration | Curated automatic candidates separate from explicit/provider inventory; collision-free native selectors |
| `apps/bitrouter/src/config/mod.rs`, `apps/bitrouter/build.rs` | Unversioned default URL and build-time embedded catalogs | v1 artifact URL, exact beta pin, package directory/cache migration |
| `helpers/dist-helper/src/chart.rs` | Existing envelope/model fields | Read v1; distinguish curated versus inventory views without dropping unknown entries |
| `.github/workflows/registry-sync.yml` | Single deterministic stage, no provider feed env bindings | Explicit repository-secret env bindings, isolated outcomes, failure summaries and coherent publication |
| `registry/README.md`, `docs/README.md`, `skills/bitrouter/` | Documentation of current lifecycle and defaults | Update when behavior ships; this design only adds links/status notices |

The design does not modify `registry/`, runtime code, workflows, or packaged
artifacts yet. Existing validation commands still validate the old contract.
Their success today would not establish v1 conformance.

## 2. Implementation sequence

### P1 — Wire contract and compatibility boundary

- Add `registry/registry.yaml`, open source/wire record types, and structural
  schemas under `docs/specs/registry-v1/`. Schemas must accept unknown optional
  fields and open string vocabularies while rejecting malformed known fields.
- Implement `version`, `spec_revision`, snapshot-pair admission and scoped
  `requires`. Stable v1 future revisions and unsupported beta revisions have
  different acceptance rules.
- Keep raw JSON/YAML data alongside typed projections. Round-trip unknown
  provider/model/interface/price fields through source sync, dist build, and
  disk cache. Avoid `#[serde(other)]` if it discards the original string.
- Decode unsupported records without activating them. Do not put an unknown
  protocol into an implicit default generation adapter.
- Add the synthetic old-reader/new-data cases before replacing current catalog
  types. Keep private cache changes distinct from the public wire version.

Exit: C01–C06 pass against standalone fixtures; no runtime default switches yet.

### P2 — Data migration and independent inventories

- Convert provider/model sources to the spec. Expand old protocol/rate-limit
  globs using their actual existing resolution order, then materialize interface
  facts. Rewrite `openai` to `chat_completions` and `anthropic` to `messages` only
  in protocol positions; provider names and canonical namespaces stay intact.
- Replace `api_protocol`, `protocol_endpoints`, and `pricing_by_protocol` with
  ordered interfaces. Preserve effective endpoint overrides, compatibility
  settings, reasoning effort, rate limits, and Decisions tariff applicability.
- Introduce provider-native primary keys, optional canonical bindings, explicit
  offer status, `billing: usage`, and scoped source ownership. Existing manually
  authored records begin manually owned until reviewed feed ownership is assigned.
- Move copied model facts needed for explicit routing into provider interfaces.
  Removing a curated record must not erase limits, capabilities, or a canonical
  binding needed by an older still-active offer.
- Split canonical rolling plans from provider update plans. Translate editorial
  expiry and real upstream retirement separately; review ambiguous old dates.
  Existing `retired_models` become curation-only evidence-backed tombstones.
- Generate `dist/registry/v1` and packaged v1 artifacts. Agent/runtime artifacts
  stay in their existing location and schema. Produce a migration report of
  every old offer's new identity/interface/price and unresolved facts.

Exit: C07–C13 and C23 pass; inventory count differences are individually
explained in the migration report, not frozen in permanent provider-specific tests.

### P3 — Feed execution and reconciliation

- Replace the one-feed `auto_sync` object with `feeds` and explicit write domains.
  Prefer each provider's own membership feed. Existing `models_dev` usage may
  remain pricing/metadata enrichment, or a documented provisional merge source;
  it cannot silently assert authoritative lifecycle.
- Implement exact URL GET, public headers/query, `none`/`bearer`/`header` auth,
  env-name validation, secret-safe diagnostics, and bounded requests. No redirect
  forwarding, anonymous retry, or reuse of inference credentials.
- Implement dotted/JSON Pointer reads, source-to-target mapping, fan-out,
  explicit defaults, whitelisted transforms, null/absence semantics, filtered
  state interpretation, and fixed-index destination arrays.
- Fetch all cursor pages before computing changes. Stage normalized rows,
  ownership-aware updates, and missing-offer evidence. Withdraw only with the
  required complete public snapshot evidence; preserve inactive history.
- Preserve manual/unknown data. Match existing interfaces by protocol/operation
  before merging separately owned pricing. Reject ambiguous merges.
- Implement idempotent run observation IDs, atomic provider writes, dry-run
  parity, and sanitized machine-readable reports. `models_dev` and reviewed
  agentic output use the same update/ownership validator.

Exit: C14–C22 pass with a local synthetic HTTP server and synthetic credentials.
No live provider key is required for CI fixtures.

### P4 — Routing, listing, caches, and packaging

- Update catalog fetch/decode, bundled embedding, cache serialization, provider
  application, onboarding, chart generation, and every `RegistryData` consumer.
  There must be no hidden unversioned or old-cache entry point around admission.
- Separate explicit inventory routes from automatic curated candidates. Join
  only verified canonical bindings and check protocol/operation/capability,
  expiry, auth, account policy, health, and required-price constraints.
- Preserve explicit local provider configuration and the existing subscription
  opt-in boundary; automatic curation does not authorize personal subscriptions.
- Implement the reserved native selector form in spec section 4.3 consistently
  in listing and resolution. A native ID with slashes, colons, percent signs,
  or non-ASCII text must round-trip byte-for-byte to upstream dispatch.
- Adopt a versioned cache namespace keyed by source URL and public major/revision;
  retain the last complete valid pair after refresh failures. Known expiry still
  applies when working offline; stale data is not fresh availability evidence.
- Point beta clients at an immutable commit/tag ending in `dist/registry/v1`.
  After stable freeze, the default can follow the stable v1 stream at that path.
  The legacy unversioned location can remain a frozen historical snapshot; no
  dual reader or continuing v0 synchronization is required.

Exit: C24–C28 pass, including the official packaged application path.

### P5 — Trusted scheduled sync and stable freeze

- Inventory real feed documentation, response shape, page semantics, coverage,
  price units, and environment names. Keep a checked-in feed configuration
  manifest describing which repository secret names the workflow must bind;
  it contains names only, never values.
- Bind each key only to the deterministic sync step that needs it. Keep the
  agentic stage's credentials separate. Validate contributor PRs offline; trusted
  scheduled/manual jobs alone perform credentialed provider requests.
- Continue independent successful provider work after feed failures. A final
  always-run outcome step fails the scheduled run when required feeds failed;
  report/build/PR steps still propose the coherent successful portion. Treat
  every configured feed as required in this beta; optional feeds need an
  explicit later contract, not a swallowed error.
- Update the registry README, skill/default configuration documentation, source
  schemas, package include paths, and consumers that download published artifacts.
  No source facts are declared implemented until the corresponding code lands.
- Freeze `1.0.0` only after the conformance campaign and one trusted scheduled
  sync have documented outcomes. Missing repository keys are a deployment
  prerequisite and must be reported; do not invent them or commit placeholders
  as credentials. Incompatible design changes before freeze increment beta.

Exit: C29–C31 pass; the acceptance record identifies the tested source revision,
artifact snapshot, configured/skipped providers, and remaining live coverage.

## 3. Conformance matrix

Tests below are semantic scenarios, not checks that a particular commercial
model remains listed forever. Each synthetic fixture is small and makes one
failure observable. Negative cases must assert both the diagnostic and absence
of unintended catalog/routing/credential effects.

| ID | Scenario | Required observable result |
| --- | --- | --- |
| C01 | Missing/wrong/nonintegral version; unsupported major | Reject new artifact; retain previous pair |
| C02 | Unsupported beta; newer stable v1 optional data | Reject unknown beta; accept compatible stable data |
| C03 | Unknown field at every nested level through sync/build/cache | Retained unchanged; known routes continue working |
| C04 | Unknown type/modality/capability/protocol/operation mixed with known entries | Load catalog; no invented support; known interfaces remain usable |
| C05 | Unknown provider auth/status, unknown price kind, scoped `requires` | Disable only prescribed scope; no auth/price fallback |
| C06 | Malformed known field, duplicate keys/IDs, null required field | Reject invalid candidate; do not silently convert to an unknown extension |
| C07 | `openai`/`anthropic` in protocol positions versus identity positions | Protocol aliases rejected in v1; provider/model identity unchanged |
| C08 | Old curated version expires but provider still lists it | Curated entry removed; provider offer/metadata unchanged; explicit canonical and native requests still resolve |
| C09 | New provider-native model absent from curated IDs and prices | Retained; explicitly addressable if interface/auth allow; not an automatic candidate |
| C10 | Same native ID at two providers; two native aliases for one canonical model | Distinct inventory offers; deterministic routing and no cross-provider identity inference |
| C11 | New unusable release, family exclusions, explicit curation tombstone | No eviction of healthy older version; no rediscovery of tombstoned entry |
| C12 | Grace boundary, repeated sync, re-entry, discovery failure | UTC expiry; no deadline sliding; cancellation works; failures do not fabricate releases |
| C13 | Provider withdraws last eligible offer | Only its inventory changes; publication removes unavailable curated recommendation; no provider suspension |
| C14 | Custom nested mapping, escaped dot/backslash, JSON Pointer, root array | Exact reads/writes, fixed-index arrays, no expression evaluation |
| C15 | One source copied to native/canonical targets with fixed prefix | Native ID untouched; canonical binding independent of curated list |
| C16 | Missing versus null versus wrong type, conflicting destinations, unsupported transform | Preserve/default/clear/fail exactly as specified; no silent identity or capability defaults |
| C17 | Per-token string price scaled by 1,000,000; unknown or invalid unit/value | Exact checked conversion when declared; otherwise unknown/error, never free |
| C18 | Public feed, bearer key, custom header/version, empty/missing key | Correct headers; no credential inheritance, network request on missing key, or secret in logs |
| C19 | Redirect, 401/403, timeout/429/5xx, oversize/malformed body | Bounded execution; no auth downgrade or partial inventory publication |
| C20 | Multiple pages, empty middle page, duplicate ID/cursor, page cap, missing cursor | Complete traversal or whole-feed failure; never retire from first-page-only data |
| C21 | Complete missing runs, failed run between, repeated observation ID, account scope, empty listing | Withdrawal only with both count/time evidence; no double-count; account/empty guards preserved |
| C22 | Manual override, separate pricing feed, interface reorder, unknown extension, reappearance | Ownership respected; prices join by pair; no accidental reactivation or extension loss |
| C23 | Same inputs built across a wall-clock expiry boundary; paired generation | Same explicit `as_of` yields identical bytes/snapshot; reverse references valid; agents/runtimes unchanged |
| C24 | Mismatched fetched snapshots, invalid cache, unsupported version, offline refresh | No mixed pair; last good pair retained; explicit expiries still enforced |
| C25 | Model supports generation and Decisions with different tariffs | Matching operation selected; Decisions never charged generation rate |
| C26 | Future media/embedding model alongside current chat model | Old reader loads both; skips unsupported media operation and meters; chat still works |
| C27 | Different modalities/capabilities on two interfaces | No capability union across interfaces; incompatible request rejected before dispatch |
| C28 | Native selector with reserved characters, local overrides, subscription offer | Listing/dispatch round-trip; no credential escalation or implicit subscription enrollment |
| C29 | Failed required feed A, successful B, discovery failure | B can be proposed coherently; A preserved; final CI failure visible |
| C30 | Fork PR changes URL/auth/feed instructions | Offline validation only; no privileged credentialed request or agent execution |
| C31 | Trusted dry-run/write and real scheduled sync | Same plan; valid paired artifacts; explicit secret prerequisites and sanitized failures |

## 4. Validation and review record

For this design change, validate Markdown links, JSON examples, example joins,
and the diff for scope/consistency. These checks do not prove an unimplemented
importer or router works. No Rust test run is required for prose/examples only.

For implementation, run focused contract/importer/routing tests during each
stage, followed by the repository-required Rust tests, Clippy, and formatting.
Run `cargo run -p dist-helper -- registry validate`, `registry build`, and
`cargo run -p dist-helper -- check` on migrated sources. Do not add tests that
freeze current provider/model names or catalog counts. Keep an acceptance ledger
mapping every C-ID to source revision and observed results.

Pre-freeze review must specifically challenge:

- Does any provider inventory path still use the rolling curated list as its
  identity vocabulary or deletion list?
- Does ignoring a future field accidentally authorize a new action or omit a
  billable unit? Is its requirement scoped correctly?
- Can missing metadata, a failed page, or a credential-scoped response look
  like authoritative absence?
- Can source/default expansion or cache serialization discard unknown data?
- Can media/Decisions metadata accidentally become a generation route through
  the current custom-protocol fallback?
- Does a missing secret stop a provider safely while still making scheduled
  synchronization failure visible?

These are implementation acceptance criteria, not unresolved design options.
