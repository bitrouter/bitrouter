# Registry v1 implementation acceptance

This records implementation evidence for wire version 1, revision
`1.0.0-beta.6`. The normative contract lives in
[`specs/registry-v1/`](specs/registry-v1/README.md). This file is development
evidence, not an additional protocol requirement.

## Stack integration (2026-10-29)

This integration includes main `66cd3b8d`, P0 `a8019d28`, and shared gateway
`753dd139`. The dated campaigns below retain their original source identity;
the integrated runtime and hosted checks are separate acceptance gates.

Catalog migration retains main's four new canonical recommendations and new
native offers, with exact decimal tariffs and explicit identity bindings.
Recommendation retirement does not remove still-active provider offers.
Fresh full OpenRouter admission passed for all 476 canonical references
(67 distinct IDs); 15 deliberately unbound native offers remain manually
addressable. The resulting public document SHA-256 is
`ceebff5564663efb504d57b7d8042a1ffcbc08cdd8c5a9e65fe83e1e52479189`.
These counts describe this observation and are not fixed assertions.

Independent review found missing long-context Terra tariffs. OpenRouter's
[official schema](https://openrouter.ai/openapi.json) defines
`min_prompt_tokens` as a strictly greater condition; the
[OpenAI Terra model reference](https://developers.openai.com/api/docs/models/gpt-6.1-terra)
also specifies prompts above 272,000 tokens. The OpenRouter Terra/Pro and
direct OpenAI Terra interfaces now include this exact tier. New Azure Terra
rates remain omitted because its complete deployment tariff was not verified;
omission means unavailable pricing, never free usage. Source validation,
artifact generation and fresh canonical admission pass after this repair.

## Scope

- Complete public documents, open extension vocabularies, recoverable admission
  errors, exact decimal tariffs, independent interface contracts, and native
  routing selectors.
- Independent curated recommendations and provider inventories, mapped and
  authenticated feeds, complete cursor listings, staged reconciliation,
  ownership/checkpoints, fresh global canonical identity audit, and trusted
  publication preserving older revision paths.
- Local-first host startup, exact-source caches, background refresh, fenced
  atomic routing replacement, ordinary reload coexistence, local overrides,
  request-specific interface selection, and request-owned billing evidence.

## Reproducible checks

Run from the repository root:

```sh
cargo run -p dist-helper -- registry validate
cargo run -p dist-helper -- registry build
cargo run -p dist-helper -- generate-schema
cargo run -p dist-helper -- check
cargo run -p dist-helper -- registry audit --report target/registry-audit.json
cargo build --workspace --all-features --examples
export BITROUTER_TEST_EXAMPLES_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug/examples"
cargo nextest run --workspace --all-features --test-threads 4 --no-fail-fast
cargo test --workspace --all-features --doc
cargo clippy --workspace --all-features --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo fmt -- --check
```

The audit requires a fresh public OpenRouter response. Offline build/check
success does not constitute identity-audit success. Account-scoped listing
feeds use their declared environment variables; CI binds repository secrets.

Verified on macOS on 2026-10-24 (local date):

| Check | Result |
| --- | --- |
| Complete workspace nextest, all features | 4,953 passed, 150 existing ignored; 598.097 seconds |
| Standalone registry crate tests | 44 passed, including TLS credential isolation |
| Registry/AI ownership regression run | 203 passed |
| Workspace documentation tests | 11 passed, 1 existing ignored |
| Workspace rustdoc, all features, `-D warnings` | Passed |
| Workspace Clippy, all features and targets, `-D warnings` | Passed |
| Minimal and server-tools production profiles, `-D warnings` | Passed |
| Rust formatting | Passed |
| Registry validate/build and generated-artifact check | Passed; 57 recommendations, 45 providers |
| Fresh OpenRouter canonical identity audit | 455 references, 63 distinct IDs, 15 native-only offers, zero mismatches |
| AI normal dependency-tree boundary | Passed; no registry JSON engine or runtime dependency |

The live audit observed the full-list response at
`2026-10-23T20:46:24.318129Z`, SHA-256
`37da406b4b3d7758f692526acf5aeea5d457fb044086861832a9552e1ee60907`.
The admitted public document SHA-256 is
`c6bb2ad2c2e0f9f18b525bbb39f2cf0cba5430050d3ac232930e09db3c183eb6`.
Full local command logs and structured reports are retained in the gitignored
`docs/superpowers/evidence/registry-v1/` working directory.

## Lifecycle and failure coverage

`apps/bitrouter/tests/registry_lifecycle.rs` launches the official `bro` host
against test-owned HTTP servers, with isolated homes, caches, sockets,
configuration, credentials and database. It exercises:

1. Readiness and a local route while the first registry response is blocked.
2. Background admission and subsequent requests using the newly available
   registry route without restarting the host.
3. Pending on-disk configuration remaining unapplied during registry refresh.
4. Explicit configuration reload switching registry sources and hot routes.
5. Exact-source cache persistence, opaque metadata, offline restart, and normal
   rejection of a malicious response while prior admitted routes continue.
6. Source isolation, `auto_refresh: false`, disabled registry, and orderly
   worker shutdown. The test never operates an already-running user daemon.

Production-worker tests in `gateway/catalog/refresh.rs` cover the actual
24-hour success / 1-hour failure schedule with virtual time, unchanged-body
success, cancellation, and superseded source responses.
Cache/reload tests cover busy publication, atomic replacement, rollback,
invalid timestamps, memory-only persistence failure, withdrawn login providers, and publication
serialization. Request tests cover independent capability/limit/price scope,
unknown contracts, exact selectors, subscriptions, auth placement, and selecting
another interface or native offer for the effective request's reasoning level
or Decisions question/input requirements. Evolution tests verify both request
compatibility and digest invalidation for non-first native offers.

The shared crate tests malformed/future documents, strict duplicate-key and
resource bounds, exact integer/decimal forms, underflow sign, unknown private
object keys, scoped requirements, both decision wire vocabularies, and exact
token/meter charges. Child-process probes reject numeric bombs, extreme pointer
paths, and deep YAML aliases, then admit another valid input in the same process.
Real TLS fixtures verify mapped multi-page authenticated listing and failure
without partial admission or credential downgrade. Helper tests verify complete
candidate transactions, crash recovery, concurrent-edit rejection, identity
authority, curation/inventory independence, and revision-preserving promotion.

## Integration findings and review closure

- Standard `serde_json/arbitrary_precision` affected unrelated buffered Serde
  enums in the SDK and local IPC. Registry exact JSON now uses a separate
  package; strict standard `RawValue` traversal remains the public ingress.
  Cross-package projections use bytes; persisted exact tariff evidence uses
  JSON text. A regression prevents global feature contamination from returning.
- Exact admitted snapshots belong to the application catalog. The public
  `bitrouter-ai` contract retains its original lightweight projection and
  dependency boundary; its existing gate was preserved and passed.
- TLS fixtures use an ephemeral private CA, a short-lived leaf, and an explicit
  Rustls crypto provider. Certificate verification remains enabled.
- Feed URL/pagination parsing is separate from secret resolution. The transport
  enforces HTTPS, disables redirects, and marks configured header values as
  sensitive. TLS fixtures cover header-only bearer/custom authentication for
  both listing and identity requests; plaintext addresses fail before secret
  lookup or connection, and header debug output redacts credentials.
- Minimal gateway profiles compile without the Core recommendation selector.
  Registry admission owns JSON Schema validation in every application profile;
  the dependency gate still rejects harness result validation in profiles that
  do not enable the harness.
- Windows source publication opens copied files with write access before
  flushing, as required by `FlushFileBuffers`; other platforms retain read-only
  handles. The rollback regression proves it reaches the injected late output
  failure, so an earlier preparation error cannot make the test pass. Windows CI
  allows 60 minutes for the observed 43-minute cold build/full suite plus cache
  upload and cleanup; existing per-test deadlines remain in force.
- Upstream catalog integration rolls two old Grok recommendations out of the
  curated list while retaining independent provider inventories. New Azure and
  Novita offers have reviewed native/canonical bindings and fresh global audit.
  Source-only curation tombstones prevent later discovery from recommending
  retired entries again while their independent provider offers remain active.
  Fast-forward stack refreshes preserve the dependency on the gateway change.
- A staged response is not a successful check while waiting behind a reload.
  `checked_at` advances only after successful fenced routing publication.
- Selection and final validation share content-free request requirements.
  Registry refresh also invalidates evolution contracts when any alternative
  offer in the provider contract changes.
- Independent reviews covered each implementation stage, exact JSON isolation,
  and the final complete change. All reported P1/P2/P3 findings were repaired
  and independently rechecked. Review did not substitute for execution tests.
- An initial full run stopped on an ACP PTY fixture closing its connection
  during initialization, before the requested late-disconnect scenario. The
  diagnostic rerun and final full run use CI's prebuilt-fixture mechanism and
  four test threads; no product behavior or test assertion was weakened.

## Live sync and host routing

On 2026-10-25 (Asia/Shanghai), the explicit live experiment in
`apps/bitrouter/tests/registry_lifecycle/live_sync.rs` passed the complete chain:

1. Create an isolated source tree with an empty OpenRouter provider inventory,
   an empty identity ledger, and no curated recommendations.
2. Execute the production `dist-helper --root DIRECTORY registry sync --write`
   process. It fetches the real public OpenRouter identity and provider lists,
   verifies every binding, writes the provider source and ledger, and generates
   the three registry artifacts through the ordinary publication transaction.
3. Compare the written provider inventory with the generated artifact and verify
   all three artifacts against the sync report's exact SHA-256. Serve the
   generated file directly, without constructing or rewriting a registry body.
4. Start the official `bro` binary with an isolated home, cache, database, and
   control socket. While the first registry download is blocked, the selected
   canonical route returns 404. Release those exact artifact bytes, then send
   actual HTTP chat-completions requests using both the canonical ID and the
   reserved encoded native selector. The test-owned inference server verifies
   the exact native model ID and fixture bearer credential.
5. Wait for cache publication and compare its complete exact document with the
   producer document, including unknown data. Cache JSON whitespace is private
   storage encoding, so its byte digest is recorded separately. Restart with
   automatic refresh disabled and route again from cache without another fetch.

The model is selected from the generated inventory at run time; the test does
not freeze a catalog ID or count. It also proves that an offer absent from the
curated recommendation list remains explicitly routable.

The all-features local run recorded:

| Evidence | Result |
| --- | --- |
| Live provider inventory and global identity audit | 693 offers, 693 verified IDs, zero mismatches |
| Selected canonical/native ID | `aion-labs/aion-2.0` |
| Explicit native selector | `openrouter:~native/aion-labs%2Faion-2.0` |
| Published artifact and HTTP-served SHA-256 | `d4ed2ed0e4fc8ff1362c8c03b2802b1d5bfb32ad0358178564542b289aac86df` |
| Cache complete-document equality | Passed |
| Cache payload SHA-256 (private encoding) | `daba9d80bbbd2c40043aaadb2fb3d962d25cfd5ddbe2a5e53c3b09b7b679211a` |
| Inference receipts | Three requests with the exact native ID and fixture credential |
| Offline cache restart | Passed; no additional registry download |
| All-features live experiment | Passed in 11.42 seconds |
| Minimum gateway live experiment (CI command) | Passed in 9.75 seconds |

The identity response was fetched at `2026-10-24T17:38:25.594Z`, SHA-256
`dff6e65a8250f785ca7ceb35b4032c3c96249c2ba617e5d0b05c974159eb42f0`.
These are experiment results, not required future catalog contents. The experiment
uses a separate source tree rather than updating the committed catalog.

Reproduce from the repository root (use an absolute helper path if overriding
`CARGO_TARGET_DIR`):

```sh
cargo build --locked -p dist-helper --jobs 2
BITROUTER_TEST_DIST_HELPER_BINARY="$PWD/target/debug/dist-helper" \
BITROUTER_TEST_REGISTRY_EVIDENCE_DIR="$PWD/target/registry-closed-loop" \
cargo test --locked -p bitrouter --no-default-features --jobs 2 \
  --test registry_lifecycle \
  live_sync::provider_sync_artifact_is_consumed_by_official_host_and_routes_requests \
  -- --ignored --exact --nocapture
```

The test is excluded from the ordinary offline suite because it requires the
public OpenRouter API. The separate CI job `live registry sync and host
consumption` explicitly runs it with the minimum gateway build and preserves
`registry-provider-closed-loop` evidence on success or failure. Evidence includes
the sync report, public identity response, written provider source, generated
artifact, structured byte/document comparisons, inference receipts, and host
diagnostics. Local run evidence is retained under the gitignored
`docs/superpowers/evidence/registry-v1/closed-loop/` directory.

## External execution boundary

The public OpenRouter identity audit and provider catalog sync are exercised live.
Registry consumption and inference routing use the actual host process and
HTTP client, with test-owned artifact and inference servers. Paid inference and live
account-scoped provider feeds are not claimed: their transport, credential,
mapping and failure behavior is covered by isolated real HTTP/TLS fixtures.
Repository ignored tests retain their existing opt-in status. The publication
workflow is committed but no remote promotion, release, merge, or deployment is
performed as part of these local acceptance checks.
