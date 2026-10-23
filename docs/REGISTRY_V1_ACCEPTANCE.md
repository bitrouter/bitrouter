# Registry v1 implementation acceptance

This records implementation evidence for wire version 1, revision
`1.0.0-beta.6`. The normative contract lives in
[`specs/registry-v1/`](specs/registry-v1/README.md). This file is development
evidence, not an additional protocol requirement.

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

## External execution boundary

The public OpenRouter audit is exercised live. Paid inference and live
account-scoped provider feeds are not claimed: their transport, credential,
mapping and failure behavior is covered by isolated real HTTP/TLS fixtures.
Repository ignored tests retain their existing opt-in status. The publication
workflow is committed but no remote promotion, release, merge, or deployment is
performed as part of these local acceptance checks.
