# Registry v1 conformance

Status: **normative beta, `1.0.0-beta.7`; wire version `1`.**

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
| C21 | One complete authoritative public absence; repeated absence; incomplete, account-scoped, merge-only or disallowed empty listing | Only the complete snapshot proposes immediate withdrawal of owned status; retain native record and other fields; invalid/insufficient evidence preserves inventory; repeated identical content changes no timestamps |
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
| C41 | Ordinary sync without repair proposals has one or multiple invalid canonical IDs while other provider updates succeed | Report all offending paths/IDs; global audit fails; no source/artifact publication or sync PR update; existing inventory/published document preserved |
| C42 | OpenRouter timeout/auth failure, empty/malformed/duplicate-ID/truncated response, unexpected pagination, or old cache | Audit unavailable and final CI failure; no historical/cache fallback, credential downgrade, or absence-driven deletion |
| C43 | Historical ID no longer listed, with and without public references | Referenced ID blocks publication; unreferenced ledger history is reported and retained; reviewed native-only resolution does not withdraw provider offer |
| C44 | Agentic edit, manual edit, rebase, or later publication after a passing audit | Changed candidate requires a new audit; later run refetches; recorded evidence and output-file SHA-256 identify exact validated output; offline checks never claim live success |
| C45 | OpenRouter audit includes image/decision/embedding/audio models | Request uses exactly `output_modalities=all`, omits narrowing/pagination parameters, and validates non-text IDs; missing/default-text parameter fails configuration |
| C46 | Concurrent syncs; changed accepted source or proposal head; interruption and retry; failed global audit | Serialize fetch-through-write; changed heads discard candidate and require fresh fetch/recompute; retry has no checkpoint to resume; conditional atomic content write only after validation/audit |
| C47 | 3 decisions at USD 0.20/1000; token subsets; multiple meters/attempts; unknown usage | Exact USD 0.0006 decision charge; disjoint token counters and all applicable charges summed without per-item rounding; unknown total unavailable |
| C48 | Known/unknown access policies, missing HTTP base, reasoning levels/default, endpoint profile and cached/reasoning subsets | Resolve explicit contracts without legacy implementation knowledge; unknown execution prerequisites disable affected scope; no guessed credential, counter, or multiplier |
| C49 | Withdrawn offer returns with ID-only listing; explicit mapped status; manual status override | Feed-owned status resolves from mapping then status default on every presence; reactivates deterministically; manual status and other missing metadata preserved |
| C50 | Any configured feed is failed, unsupported, or missing a secret | Final run fails for each; successful independent updates may be proposed only after global audit; an unknown optional flag cannot downgrade requiredness |

## BitRouter first-party consumption cases

These additional cases apply to the first-party profile in
[section 11](README.md#11-bitrouter-first-party-consumption-profile), not to
independent readers' choice of refresh policy. They remain observable design
requirements, not evidence that the current CLI implements the profile.

| ID | Scenario | Required observable result |
| --- | --- | --- |
| C51 | Stable build, beta-only build, data update on pinned revision, explicit snapshot/custom URL | Exact stable v1 URL or declared revision-specific beta URL; beta stream accepts only the pinned revision while data rolls; new beta/stable publication preserves old paths; explicit URL always wins and a commit URL freezes content |
| C52 | Fresh/stale/invalid cache; eligible/incompatible bundle; no local data | Validate and use selected-source cache before bundle; stale cache still serves within execution/expiry gates; network never delays readiness; no usable local data means empty registry and working local/control surfaces |
| C53 | First launch, 24-hour boundary, long-running idle host, failed check, identical successful body | Background first check when due; periodic refresh without restart/request; failure retries after 1 hour without changing `checked_at`; success, including unchanged bytes, schedules 24 hours from the successful check |
| C54 | `enabled: false`, `auto_refresh: false`, re-enable, pending response after configuration change | Disabled registry loads/fetches nothing and contributes no routes; disabled automatic refresh uses eligible local data only; re-enable reapplies selection/schedule; late responses cannot restore obsolete configuration |
| C55 | URL/ref/query change, legacy or unbound cache, custom-source failure | Cache is bound to exact source and major; never borrow another source's cache, bundle, validators, or routing data; local routes continue working |
| C56 | Interrupted cache write, unknown metadata, future/missing `checked_at`, lower `as_of`, expired evidence | Atomic complete cache retains unknowns; failed persistence keeps old disk data and reports memory-only success; invalid check time makes refresh due; same-source rollback rejected; expiry enforced without timestamp renewal |
| C57 | Redirect, non-200, timeout, malformed/unsupported/oversized response; explicit loopback HTTP mirror and HTTP non-loopback URL | Bounded normal failure and same-source fallback; only literal loopback custom HTTP is allowed, without proxies; no alternate URL, credential inheritance, partial view, or host termination; valid updates affect subsequent routing and preserve local overrides |
| C58 | First stable publication; software upgrade with implicit versus explicit source; future beta/major at stable URL | Beta pin does not move; stable-supporting software changes only implicit default and uses stable-source cache/bundle; explicit URL remains; unsupported revisions fail recoverably; compatible stable 1.x updates normally and v1 stream is not replaced by prerelease/v2 |

## Shared input resource and preservation case

This case applies to all readers under the public-document profile in section 2.

| ID | Scenario | Required observable result |
| --- | --- | --- |
| C59 | Numeric exponent/precision bomb, exact u64 boundaries, equivalent integer spellings, nested implementation-private keys and opaque null metadata | Reject excess before schema/arithmetic; process remains alive and accepts the next valid refresh; exact integers have consistent interpretation; valid unknown object identity and numeric precision survive all rewrites |

## Current publisher and OpenRouter adapter scope

| ID | Scenario | Required observable result |
| --- | --- | --- |
| C60 | Full listing includes chat/embedding `:batch` variants alongside supported models; manual batch references | Keep the full identity audit intact; exclude batch variants from imported inventory, new ledger bindings, and curation without rewriting IDs; reject batch canonical references or OpenRouter native batch entries at publication |
| C61 | Text model advertises chat, another endpoint, empty endpoints, missing/null endpoints, or malformed endpoint metadata; one evidence field is absent while the other explicitly denies support | Only explicit chat support creates its interface; either unsupported endpoints or unsupported modalities retract owned interfaces independently; merely missing evidence preserves existing interfaces without authorizing new ones; malformed metadata fails the complete feed |
| C62 | Explicit models.dev provider selection, reviewed replacement of a native feed with unavailable credentials, contradictory provider/model IDs, deprecated/unknown statuses and future model types | Import only the selected provider; release prior native status ownership to manual control during migration and preserve existing statuses, bindings, interfaces and tariffs; new offers remain staging with empty interfaces and no canonical/pricing inference; exclude deprecated rows without withdrawal; reject inconsistent identities or unknown lifecycle; no automatic fallback after a native failure |
| C63 | Native resource names with an explicit nonempty literal prefix, unmatched/empty suffixes, chained transforms and canonical-target stripping | Permit exactly one checked native-ID wrapper removal; reject every unmatched/empty identity or forbidden target/chain atomically; preserve suffix spelling and existing binding authority |
| C64 | Missing terminal cursor with/without explicit end policy, wrong cursor types, unknown policy or conflicting pagination options | Strict by default; only the explicit cursor-only policy accepts omission; unknown policies fail before requests; has-more evidence cannot be weakened |
| C65 | Explicit identity repair proposals, absent IDs beside similar current IDs, incomplete snapshots and existing binding conflicts | Ordinary sync rejects missing IDs; explicit proposals list every unbinding or recommendation removal, retain native data and ledger history, pass the complete audit, and enter a draft PR before human review; no alias inference, incomplete evidence or conflict repair |

## Content-driven publication and beta 7 migration

These requirements accompany [section 9.3](README.md#93-content-comparison-and-timestamps)
and [migration](README.md#34-beta-6-to-beta-7-migration). They define expected
behavior, not completed implementation or acceptance evidence.

| ID | Scenario | Required observable result |
| --- | --- | --- |
| C66 | Identical normalized content, reordered object keys, unmapped upstream changes, subsequent run without prior logs/cache | Fresh checks and identity audit; identical source/artifact bytes and times; no new catalog commit or external sync state dependency |
| C67 | One availability-owned assertion changes; separate pricing/manual field changes; unchanged curation and identity binding | Only changed assertions receive responsible evidence times; public change advances as_of; unrelated evidence and ledger times remain unchanged |
| C68 | Changed expiry/provenance/unknown timestamp-like field, interface preference or opaque array order; equivalent normalized numeric values | Semantic changes count, equivalent values do not; exact comparison preserves unknowns and uses only defined evidence-time exclusions |
| C69 | Same desired unmerged proposal across fresh runs; partial return to accepted content; new accepted ownership/configuration; missing proposal | Keep still-needed identical proposal times/bytes and first valid grace deadline; restored assertions reuse accepted evidence; revalidate current scope; no proposal means recompute from accepted source without external recovery state |
| C70 | Curation expiry with identical feeds; repeated grace checks before/after merge; source-only or encoding-only edit; identical client download | Fresh sync evaluation applies deadlines without sliding; deterministic stored-as_of build; no evidence renewal from passage of time or source-only edits; client advances only local cache check time |
| C71 | Failed candidate/audit, failed independent feed, dry-run, interrupted write, clock rollback | No invalid source/artifact mutation; independent changes may proceed only under existing gates; no stale proposal used as successful feed evidence; dry-run writes nothing; backward time fails normally |
| C72 | Beta 6 source has checkpoint, counters/delay controls or last-seen fields under their original paths; unknown same-named extension elsewhere | Reject obsolete exact source paths before fetch with migration guidance; preserve unrelated unknowns; explicit migration retains ownership/content/evidence and discards operational history |
| C73 | Beta 6 reader/new beta 7 data; beta 7-only reader/old beta 6 data; migration and stream publication | Unsupported revisions fail normally; no silent reader opt-in or path replacement; revision change advances document as_of without mass evidence restamp; schema shape alone grants no revision support |
| C74 | Snapshot excludes a still-active model through editorial filter versus documented unavailable lifecycle; manual status conflict | Editorial filtering cannot prove withdrawal; lifecycle filtering requires authoritative evidence; manual status is never overwritten, conflicts fail normally |
| C75 | Accepted offer is active, pending proposal withdraws it, fresh complete feed again yields accepted active content | Retain accepted bytes/timestamps and retire the obsolete proposal or block its stale head; do not leave it mergeable; preserve unrelated human edits and conditional-write/audit gates |
| C76 | Pending assertion B was observed under configuration F0; accepted feed changes to F1 while accepted assertion A stays unchanged; fresh F1 produces B | Reverify under F1 and use current evidence for B; timestamp-excluding equality to old B cannot restore its ineligible evidence or whole proposal bytes |
| C77 | A proposed editorial deadline expires before merge; later identical runs still see the recommendation in accepted source | Keep expired record/deadline in pending source and omit it from artifact; repeated checks retain removal bytes/time and cannot restart grace; source cleanup may follow accepted removal |
