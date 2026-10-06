# BitRouter AI refactor progress

Implementation follows the
[reviewed design](BITROUTER_AI_REFACTOR_SPEC.md). This file distinguishes completed
batches from the final AI contract. The initial source baseline is PR #953 head
`8e267e720795b1b770bd1a72c758fda8f909f612`.

Current delivery on 2026-10-06: [Draft/WIP PR #962](https://github.com/bitrouter/bitrouter/pull/962)
targets `main`. The review branch includes main through `31cf68ed`, with the
AI documentation and main's daemon-upgrade index entries both retained.
Post-sync local validation passed: 3,730 workspace tests, 22 skipped; independent
doctests 6 passed/1 ignored; strict Clippy including tests, formatting, strict
Rustdoc, distribution checks, five isolated AI feature test builds/dependency
trees, SDK default/config-file builds and pinned public API guards. SDK public
dependencies remain 16 with no telemetry types. The test run uses two threads
and the established application-only debug-info override.

The batch sections below are historical capture records; their local/uncommitted
status statements describe those captures. The implementation snapshot is now
committed and pushed for WIP review. Hosted CI is tracked on the PR separately.
Real-provider/OAuth/Keychain validation, external consumer migration and remaining
spec acceptance are still unfinished; the PR is not a release or merge request.

## Batch 1: model semantic ownership and history corrections

Implemented and locally validated on 2026-10-05.

- Created `bitrouter-ai` as the sole owner of model content, messages, prompts,
  tool definitions/results, generation options, usage, protocol identifiers,
  compatibility metadata, generation results and stream events.
- Moved existing model-type regression coverage with the types. Metadata
  validation returns a model-domain error independent of HTTP status policy.
- Migrated workspace consumers to direct AI imports and Cargo dependencies.
  Removed old SDK type definitions and public aliases. The SDK retains routing
  targets, header policy, execution records and pipeline request/response
  envelopes.
- Corrected Gemini-to-Chat tool-result cardinality: each result becomes its own
  Chat tool message, retaining order and call IDs. Empty or non-result Tool
  messages are rejected rather than emitted with missing data.
  The existing approval-only fixture now asserts that rejection for Chat;
  Messages/Gemini still retain their baseline omission pending loss admission.
- Corrected Responses input ordering: contiguous text/media is emitted before
  the next standalone item; later content follows that item.

The two focused cardinality/order tests failed against the original codecs and
passed after correction. These exercise local conversion paths, not providers.
The initial workspace `cargo check --workspace --all-features` passed.

### Breaking import migration

| Before | After |
| --- | --- |
| `bitrouter_sdk::language_model::types::Prompt` (or its pipeline-module alias) | `bitrouter_ai::types::Prompt` |
| SDK model types such as `Content`, `Message`, `Tool`, `GenerationParams`, `GenerateResult`, `Usage`, `StreamPart`, `ApiProtocol`, `Capability` | Corresponding names under `bitrouter_ai::types` |
| SDK compatibility types such as `ReasoningEffortConfig`, `ModelCompatibility`, `ProtocolList`, `AuthScheme` | Corresponding names under `bitrouter_ai::types` |
| `ReasoningEffortConfig::validate()` returning the SDK error | Returns `bitrouter_ai::error::Result<()>`; caller applies gateway/config error policy |

Add `bitrouter-ai` as a direct dependency. Serde field names and model payload
shapes are unchanged. Codec traits still live in the SDK in this batch. No
compatibility re-export facade is introduced.

Known external users include `bitrouter-cloud`; their migration/release must be
coordinated separately. This worktree verifies workspace callers and does not
claim that external checkouts have migrated.

Publish the new AI package before crates that depend on it. AI changes are now
included in the application's release changelog; package publishing itself is
not part of this local batch.

### Validation

- Dependency inspection: AI depends only on serde, serde_json, schemars and
  thiserror; its tree contains no SDK, Axum, ACP, MCP or Tokio.
- `cargo check --workspace --all-features`: passed.
- `cargo nextest run --all-features`: 3,557 passed; 22 skipped. This includes
  moved model-type coverage and the new conversion regression cases.
- `cargo clippy --all-features` and `cargo fmt -- --check`: passed without new
  source lint warnings. The toolchain reports an existing dependency future
  compatibility warning; the macOS test linker reports a large unwind table.
- `cargo test --doc --all-features`: 5 passed; 1 ignored; no failures.
- Pinned `cargo-public-api 0.52.0` / `nightly-2026-05-05` extraction: passed;
  the regenerated SDK public dependency manifest adds only `bitrouter_ai`.
- `dist-helper registry validate`, `registry build` and `check`: passed.
  Catalog output is unchanged. `generate-schema` refreshed one capability
  description to remove an old SDK documentation link; schema fields and
  validation rules are unchanged. Validation retains existing ACP catalog
  conformance/pinning warnings, not live conformance evidence.
- CI YAML syntax, local documentation links and `git diff --check`: passed.
  The existing isolated-dependency CI loop now includes AI.

These are local macOS checks. Hosted CI, Windows, real-provider behavior,
external consumer migration and publication have not been verified in this
batch. No credentials, catalog activation or harness wiring changed.

## Batch 2: codec, framing and selected-target ownership

Implemented locally on 2026-10-05.

- Moved the four bidirectional codecs, adapter/transport dispatch, stream
  encoders/decoders and their existing conversion/schema fixtures into AI.
  Protocol schema snapshots moved unchanged. SDK codec definitions and exports
  were removed rather than forwarded.
- Moved `SseFrame` to AI. SDK retains stream hooks, usage accumulation,
  keepalive scheduling, delivery authorization and HTTP response policy.
- Added `ModelTarget` for effective provider/model connection values and wire
  compatibility. `RoutingTarget::model_target()` resolves credential/endpoint
  overrides before passing a selected target down. Account identity, caller
  headers, header policy and route alternatives remain above AI. Credential
  debug output is redacted.
- Replaced codec dependencies on `BitrouterError` with domain `ModelError`.
  SDK adapts domain errors at HTTP, execution and stream boundaries, retaining
  its existing request/response classification and fallback behavior.
- Stream encoders now accept caller-selected `StreamError` presentation. SDK
  supplies its existing public message, type, code and status. AI does not
  select gateway HTTP policy or sanitize arbitrary caller messages.
- Migrated workspace callers, including the Antigravity custom adapter,
  telemetry and app continuation/semantic-commitment consumers. Commitment
  computation remains semantic evidence; SDK still owns credential authority,
  record ownership, delivery authorization and continuation substitution.
- Added regression coverage for effective target overrides, credential debug
  redaction, domain-to-SDK policy mapping, diagnostic suppression and rate-limit
  terminal frames across all four protocols. Added a CI dependency guard for AI.

### Breaking import migration

| Before | After |
| --- | --- |
| `bitrouter_sdk::language_model::protocol::*` and pipeline-module codec aliases | Corresponding names under `bitrouter_ai::protocol` |
| SDK `SseFrame` | `bitrouter_ai::stream::SseFrame` |
| Codec/transport trait methods using SDK `Result` | `bitrouter_ai::error::Result` |
| Codec/transport trait methods accepting SDK `RoutingTarget` | `bitrouter_ai::target::ModelTarget` |
| `StreamEncoder::encode_bitrouter_error` | `encode_stream_error(&StreamError)`; SDK callers use `error.stream_error()` |
| SDK Responses schema, continuation-ID codecs and semantic commitments | Corresponding names under `bitrouter_ai::protocol::responses` |

This move retains baseline conversion behavior and the Batch 1 corrections.
It adds no new permissive conversion, transcript repair or loss-admission
policy. Default loss admission, Core mapping and standalone invocation remain
unfinished. Moving semantic commitments does not move continuation authority.

### Validation

- `cargo test -p bitrouter-ai --no-default-features`: 341 passed; 1 custom
  protocol example ignored. This resolves and tests AI independently of SDK.
- `cargo check --workspace --all-features`: passed.
- `cargo nextest run --all-features`: 3,558 passed; 22 skipped, including the
  three new SDK boundary regressions. Original framing/conversion/schema tests
  follow their AI implementation.
- `cargo clippy --all-features` and `cargo fmt -- --check`: passed.
- `cargo test --doc --all-features`: 5 passed; 1 ignored.
- Strict AI/SDK Rustdoc (`-D warnings`) on the pinned public-API nightly: passed.
  Corrected a model-type link to an SDK-owned execution record, and removed
  stale prose implying that baseline conversions were lossless.
- Pinned `cargo-public-api` extraction: passed. The SDK public dependency
  manifest remains unchanged from Batch 1; old SDK codec/type/framing paths
  are absent and no forwarding facade was added.
- AI normal/build tree (`--all-features`): no SDK, application, Core,
  orchestrator, Axum, ACP or MCP dependency. Its transports now depend on
  reqwest and its HTTP/runtime dependencies, so Batch 1's no-Tokio observation
  no longer describes the current crate.
- `dist-helper check`: schema and registry outputs are current; this batch
  changes neither catalog entries nor wire schema snapshots.
- CI YAML syntax, moved snapshot content, documentation links and
  `git diff --check`: passed locally.

Full validation used `CARGO_INCREMENTAL=0` after a scoped `cargo clean -p
bitrouter --profile dev` recovered space from this worktree's build artifacts.
The existing dependency future-compatibility and macOS unwind-table warnings
remain. Hosted CI, Windows/MSRV, real providers, known external consumers and
package publication have not been verified. No login, credential storage,
provider activation, CLI surface or harness wiring changed.

## Batch 3: standalone HTTP invocation and SDK delegation

Implemented locally on 2026-10-05.

- Added `bitrouter_ai::client::ModelClient`, `ModelStream` and AI-owned
  `HttpTimeouts`. Direct stream/non-stream calls take a selected `ModelTarget`,
  canonical prompt and caller cancellation token. The target now carries its
  explicit protocol. Request projection changes selected model and stream mode
  without mutating the caller's source prompt.
- Direct calls make one attempt with explicit effective credentials. They do
  not read environment credentials, select accounts, perform login/refresh,
  load catalogs or apply routing fallback. An injected `OutboundDispatch`
  supports custom adapter/transports. Empty built-in credentials fail before
  network dispatch.
- Moved shared HTTP client construction, request deadlines, send/body reads,
  stream decoding, Responses terminal validation and Retry-After parsing into
  AI. SDK `HttpExecutor` delegates those operations while retaining provider
  body shaping, selected-account refresh/retry, caller/trace/header policy,
  continuation authorization/substitution and pipeline envelopes. Timeout
  values/config defaults and the existing one-refresh-on-401 policy are retained.
- Added model-domain configuration, transport, decode, HTTP-response, timeout
  and cancellation errors. AI preserves native HTTP status/retry hints; SDK
  retains its existing gateway/fallback classification. SDK cancellation maps
  to `request_cancelled` / 499 and fails without trying another target.
- Extracted generic exact-credential diagnostic filtering into AI's
  `DiagnosticRedactor`. Direct calls filter effective keys and final request
  credentials from errors/provider bodies. SDK composes it with its existing
  continuation substitutions and caller-visible message policy. Model content
  is not scrubbed as a diagnostic.
- Shared stream decoding now reports invalid response when EOF supplies no
  canonical terminal part (`Finish` or Responses `ResponseCompleted`), instead
  of treating a truncated Chat/Messages/Generate Content stream as successful.
  Responses' existing stricter terminal/id rules remain. Emitted content/usage
  precede a later error; cancellation/drop releases the owned response.
- Token cancellation waits for a custom authentication future to finish, then
  prevents dispatch. Dropping the call future can still drop authentication.
  This establishes no durable refresh/store guarantee: credential rotation,
  concurrent refresh and persistence failure remain work for the auth batch.
- Removed obsolete SDK networking/decoder helpers and direct
  `eventsource-stream` / `httpdate` dependencies; the Retry-After regression
  follows its AI implementation. Added AI-only local TCP fixtures and SDK
  invocation-to-policy coverage. CI now runs the AI tests independently of
  workspace feature unification and excludes the legacy providers crate from
  the AI normal/build dependency tree.

### Breaking import migration

| Before | After |
| --- | --- |
| `bitrouter_sdk::language_model::executor::HttpTimeouts` | `bitrouter_ai::client::HttpTimeouts` |
| Selected `ModelTarget` without protocol | Explicit `api_protocol` on `ModelTarget` |
| Gateway-error-only invocation | Domain `ModelError`; SDK policy adapts it at the gateway boundary |

The [AI README](../crates/bitrouter-ai/README.md) describes the direct-call
contract. A compile-checked Rustdoc example constructs a selected Responses
target and invokes it without an SDK `Config`, router or server.

### Validation

- `cargo check -p bitrouter-ai --no-default-features`: passed without relying
  on SDK/application Tokio features.
- `cargo test -p bitrouter-ai --no-default-features`: 355 passed (342 unit,
  13 integration); the new direct-call example compiles and the existing custom
  protocol example remains ignored. HTTP fixtures cover all four protocols,
  source immutability, explicit auth, malformed success, Responses lifecycle,
  late errors/usage, cancellation before headers/during body/during streaming,
  stream drop, idle timeout and optional total deadline. TCP disconnects are
  observed after cancellation/drop; fixed sleeps are not the release evidence.
- `cargo nextest run --all-features`: first full run passed all 3,571 tests,
  with 22 skipped. After dependency cleanup, the final full run with
  `--no-fail-fast --test-threads 4` also passed all 3,571 tests, with 22 skipped.
  An intermediate run and isolated retries failed the existing
  `local_cli::tests::probes_compatible_old_failed_and_hung_workers` on its first
  compatible script probe at the two-second budget. Subsequent unchanged
  isolated/direct runs and the final full run passed. The cause is unconfirmed;
  neither that test nor its timeout was changed or skipped.
- `cargo clippy --all-features` and `cargo fmt -- --check`: passed.
- `cargo test --doc --all-features`: 6 passed; 1 ignored.
- Strict AI/SDK Rustdoc (`-D warnings`) and pinned public-API extraction:
  passed. The SDK public dependency manifest matches; the old SDK timeout,
  model-type and codec paths are absent. No forwarding facade was added.
- Default/all-feature AI normal/build trees pass the CI dependency guard:
  no SDK, legacy providers, Core, orchestrator, app, Axum, ACP or MCP dependency.
- `dist-helper check`: passed; generated schema/catalog artifacts are current.
  CI YAML syntax, documentation links and `git diff --check` pass locally.

AI-01 is demonstrated by local HTTP fixtures, and AI-11 by dependency isolation.
These do not prove provider authentication refresh, production storage, Core
mapping or real-provider conformance. The existing macOS unwind-table and
dependency future-compatibility warnings remain. Hosted CI, Windows/MSRV,
real providers, downstream external consumers and package publication have not
been verified. Changes remain local, uncommitted and unpushed.

## Batch 4: auth contracts and selected-account transactions

Implemented locally on 2026-10-05. This is the shared auth foundation and its first
OAuth consumer; it does not finish every provider's authentication extraction.

- `AuthApplier`, `AuthAppliers`, `AppliedAuth`, `CredentialAuthority` and
  `ContinuationAuthority` now have one owner in `bitrouter_ai::auth`. Appliers
  receive `ModelTarget` and return model-domain errors. The old SDK auth module
  and auth reexports are removed; known SDK, provider and application consumers
  use AI directly. Authority digest domains and final wire-scheme validation are
  preserved. Gateway fallback/status classification remains SDK policy.
- `Credential` and `OAuthToken` moved to `auth::credentials` without changing
  their persisted wire representation, legacy file migration or redacted Debug.
  `ModelTarget.account_label` is an explicit slot selected by the caller, not an
  AI account-discovery policy.
- `ModelClient::with_auth_appliers` supports registered body preparation and
  authentication, plus one 401 recovery/rebuild for the same explicit target.
  There is no provider/account/key fallback. Diagnostics capture both attempts'
  wire credentials and discard opaque auth extension text while preserving
  domain status, retry hints and bounded credential-store failure facts.
- `auth::store` defines an exclusive account transaction spanning read, refresh,
  staging and commit. Stores retain a staged replacement after commit failure,
  and acknowledge memory versus persistent storage separately. Explicit memory
  storage performs no ambient reads. `OAuthSession` owns an admitted refresh and
  commit in a Tokio task, so dropping its caller during either operation does
  not drop the returned replacement. Waiting for the lease remains cancellable.
- The Codex applier uses the injected transaction contract and bounded OAuth
  HTTP client. Explicit credentials bypass stored auth and 401 refresh; otherwise
  only the selected slot is read. Fresh credentials are reread rather than held
  in an applier-local cache, so login replacement/logout is visible without 401.
- The application's generic file store remains in providers, with its existing
  path/default-directory/permissions policy. Mutations reload the latest file
  under a process write lock and update the in-memory snapshot only after a
  successful write. The explicit AI file backend shares account leases across
  applier/backend instances in process, compares the selected original credential
  at commit, and rejects login/logout races. A failed write keeps the replacement
  pending; the next call retries persistence before any further rotation. Unix
  commit syncs the temporary file and directory around the atomic rename.

The lease/owned-task guarantee requires a running Tokio runtime and bounded store
and refresher implementations. Caller cancellation does not guarantee completion
through process/runtime shutdown. Pending replacements after a failed file commit
are process-local; a crash can lose them. The file backend does not provide
cross-process refresh or file-update exclusion. These limits are deliberate and
must not be reported as durable/multi-process guarantees.

Validation (local macOS):

- `cargo test -p bitrouter-ai --no-default-features`: 363 passed (342 semantic
  unit tests, 7 auth transaction tests, 14 HTTP invocation tests); 1 doctest
  passed and 1 existing custom-protocol example remained ignored. The auth tests
  observe lease release and committed replacement state after caller drop,
  including drop during commit; fixed sleeps are not the completion evidence.
- `cargo nextest run --all-features --no-fail-fast --test-threads 4`: all 3,583
  passed; 22 skipped. The first full run caught exposed selected-account Debug
  metadata and outdated assertions that confused provider-native status with SDK
  public projection; both are corrected and covered in the passing final run.
  The existing CLI probe test passed unchanged in both full runs.
- Codex/file fixtures cover separate applier/backend instances, explicit
  credential priority, fresh login/logout visibility, cancellation during
  rotation, persistence failure and retry without a second rotation,
  login/logout compare-and-commit conflicts, unrelated-account preservation,
  legacy serialization and selected path semantics through `symlink/..`.
  An AI client streams through the registered Codex applier using an injected
  memory store and verifies its account header and subscription body shape.
- `cargo clippy --all-features --tests -- -D warnings`, `cargo fmt -- --check`,
  and `git diff --check`: passed. `cargo test --doc --all-features`: 6 passed,
  1 ignored. Workspace Rustdoc with `-D warnings`: passed; the migrated SDK
  example is compile-checked.
- Pinned `cargo-public-api 0.52.0` / `nightly-2026-05-05` extraction: passed;
  the committed public dependency manifest matches, and old SDK auth paths are
  absent. No forwarding facade or dependency-manifest expansion was added.
- AI normal/build trees under default/all features remain free of SDK, providers,
  Core, orchestrator, app, Axum, ACP and MCP dependencies. AI standalone check
  and providers `--no-default-features` check passed.
- `dist-helper check`: passed; schema/catalog outputs are current and no registry
  data was changed. The pre-existing Anthropic header edit was preserved.

These fixtures demonstrate the shared transaction contract and Codex integration,
not every provider's credential-priority, refresh or continuation contract. The
other provider implementations, their external credential stores, and their
explicit/stored/ambient priority still need migration and acceptance evidence.
Auth mechanisms still live in providers until integration consolidation. The
existing linker unwind-table and dependency future-compatibility warnings remain.
Hosted CI, Windows/MSRV, real-provider conformance, external consumers and package
publication were not verified. Changes remain local, uncommitted and unpushed.

## Batch 5: provider credential priority and transaction adoption

Implemented locally on 2026-10-05. This batch migrates the remaining ordinary
subscription stores and the Claude CLI adoption adapter; hosted OAuth's richer
issuer/namespace/scope store is still a separate pending migration.

- `ModelTarget.credential_priority` records an explicit call override versus a
  caller-permitted fallback value. SDK snapshots mark per-call overrides explicit
  and configured provider values fallback. AI resolves no environment values.
  Wrong-kind, empty or failed stored credentials never fall through to a supplied
  fallback. No auth operation selects another account or opens login.
- Anthropic API keys now use injected account storage and honor explicit keys
  before storage. Claude Code, SuperGrok and Google AI use the shared owned
  `OAuthSession` transaction; applier-local access-token caches/gates are removed.
  Each call reads the selected slot, and each can recover that same slot once
  after a 401. Codex also consumes the new provenance contract.
- Claude environment use is an explicit application assembly step through
  `with_fallback_token`; `new`/`with_store` do not capture it. Stored OAuth or a
  live CLI adoption marker has priority. An invalid/missing live session behind
  a marker cannot silently use an environment token.
- Claude live adoption composes the selected marker lease with a process-shared
  lease for its actual CLI source. Refresh commits back to that exact source,
  preserves unrelated envelope fields, and compares original token/source before
  writing. Persistence failures retain the replacement across applier instances
  and aliases. Removing an alias prevents that alias from committing/dispatching
  without deleting the external CLI account; its pending rotation remains usable
  by a still-authorized adoption. Actual live-login replacement/logout rejects
  the old rotation. Replacement expiry/refresh-token absence is persisted rather
  than inherited from the old token.
- Copilot reads the selected GitHub slot on each call. Its ephemeral token cache
  is bound to that exact source authority, invalidates on login/logout/replacement,
  and serializes exchange within an applier. Explicit GitHub credentials bypass
  stored selection; derived Copilot tokens never overwrite the GitHub credential.
  Separate appliers can exchange separately; this does not rotate the source token.
- Google project caching is bound to credential digest and endpoint, rather than
  account label. Structural body preparation is account-independent; final auth
  binds project and Authorization using the same resolved token on the built JSON
  request. Client-secret probing stops on uncertain I/O or rejected user grants;
  only `invalid_client` permits another client-secret candidate.
- Hosted model auth now honors explicit/stored/fallback provenance and refuses
  fallback after stored-origin or refresh errors. Its metadata-rich manager's
  refresh ownership/persistence contract still needs migration; no general OAuth
  conversion drops issuer, namespace, scope or refresh-token-expiry fields.
- File transaction support is available without `pkce` because static Anthropic
  and Copilot consumers use it. Tokio sync is a normal provider dependency; browser,
  PKCE crypto/keyring and hosted-only dependencies retain their feature gates.
  The shipped BitRouter skill reference documents the changed credential priority.

Guarantees remain process-local and require a running Tokio runtime. External CLI
or other-process refresh/writes require shared coordination not supplied here.
Pending replacements are volatile after persistence failure. File fixtures do not
prove real Keychain behavior, real-provider conformance or crash durability.

Validation (local macOS):

- Full `cargo nextest run --all-features --no-fail-fast --test-threads 4`:
  3,594 passed, 22 skipped. The prior CLI probe test passed unchanged. An initial
  focused run passed 1,323 existing tests; the expanded run caught the detached
  alias pending-state edge, which is corrected and covered in the final full run.
- AI standalone `cargo test -p bitrouter-ai --no-default-features`: 364 passed
  (342 semantic, 8 auth, 14 invocation); 1 doctest passed, 1 existing example
  ignored. Workspace doctests: 6 passed, 1 ignored.
- Provider regressions cover explicit/stored/fallback priority, wrong-kind and
  rejected stored credentials, source replacement/logout invalidating Copilot
  cache, Google project/auth binding, hosted stored-origin rejection, and Claude
  live CLI cancellation/commit/metadata preservation/conflicts/alias retention.
  Lease release and persisted source state provide cancellation completion
  evidence; no fixed sleep is used for these new assertions.
- `cargo clippy --all-features --tests -- -D warnings`, `cargo fmt -- --check`
  and `git diff --check`: passed. Workspace Rustdoc under `-D warnings` passed.
- AI standalone check and provider default, `pkce`-only and `hosted`-only checks
  passed. Normal/build dependency guards preserve AI isolation and keep provider
  default builds free of keyring, rand and chrono. Optional browser/hosted
  capabilities are not folded into the unconditional file transaction backend.
- Pinned public-API extraction matches the SDK dependency manifest; no SDK auth
  forwarding facade is present. `dist-helper check` passed with unchanged schema
  and registry outputs. Local documentation links and plugin manifest JSON passed;
  the skill entrypoint remains unchanged at approximately 200 lines, with deep
  credential-priority detail in its provider reference. The pre-existing
  Anthropic header change is preserved.

These are local fixture/build results. Real provider and Keychain behavior,
Windows/MSRV, hosted CI, external consumers, cross-process exclusion and crash
recovery were not verified. Hosted OAuth transaction adoption, catalog/provider
consolidation, Core mapping and the remaining admission/ACP work are unfinished.
Changes remain local, uncommitted and unpushed.

## Batch 6: hosted OAuth refresh and full-envelope persistence

Implemented locally on 2026-10-05. Hosted OAuth keeps its specialized account
schema and consumes AI's bounded storage-error/durability vocabulary; converting
it to the generic `OAuthToken` would lose issuer, namespace, subject, granted
scope, client identity and refresh-token expiry.

- Managers at the same resolved file path share one process-local lease and
  pending replacement. After cancellable lease acquisition, an owned Tokio task
  completes bounded metadata discovery, refresh, staging and persistence even
  when the caller drops its future. Default-client and injected-client auth I/O
  are bounded at 30 seconds per discovery/exchange operation.
- A failed commit retains the complete returned rotation across manager instances.
  The next resolution retries that write before another refresh. A bearer is
  returned only after file and Unix parent-directory synchronization acknowledge
  the replacement. A retry also recognizes an already-renamed replacement.
- Login/logout writers reload and serialize file mutations under a process-wide
  write lock. Refresh compares the original credential before committing, so
  concurrent login replacement or logout rejects the old rotation instead of
  overwriting or resurrecting the account. Failed save/removal keeps the cached
  snapshot; a stale clear returns the login actually removed.
- `CredentialsStore::current_token` is removed: file storage no longer performs
  HTTP refresh. `CredentialManager` is the hosted resolution owner used by model,
  telemetry and Cloud consumers. Its public storage errors now carry AI's bounded
  `StoreError`; model auth maps these to `ModelError::CredentialStorage`, retaining
  SDK's fail-fast internal-error projection. No opaque server error body is exposed.
- Refresh preserves omitted envelope fields, records returned scope/token expiry
  and rotated secrets, and constructs outgoing identity from the same committed
  credential as its bearer. Known namespace/subject contradictions fail closed:
  returned material remains pending in memory, with no dispatch, file rewrite or
  second exchange, until the application replaces/removes the login. Metadata is
  cached by full issuer discovery URL, including path, rather than origin alone.
- Explicit/stored/permitted-fallback selection and origin confinement remain at
  the hosted applier boundary. Auth does not choose another account or open login.
  No CLI flag, environment name, default config or harness wiring changes here.

Validation (local macOS):

- Final `cargo nextest run --all-features --no-fail-fast --test-threads 4`:
  3,603 passed, 22 skipped. Hosted coverage includes process-shared/symlink-alias
  leases, concurrent managers, admitted versus waiting cancellation, full-envelope
  preservation, failed commit retention, login/logout conflict, identity rejection,
  stale clear, failed cache update and distinct issuer-path discovery caches.
- `cargo clippy --all-features --tests -- -D warnings`, `cargo fmt -- --check`
  and `git diff --check`: passed. Strict workspace Rustdoc passed. Separate
  workspace doctests: 6 passed, 1 existing example ignored.
- AI standalone `cargo test -p bitrouter-ai --no-default-features`: 364 passed
  (342 semantic, 8 auth, 14 invocation); 1 doctest passed, 1 existing example
  ignored. Provider default, `pkce`-only and `hosted`-only checks passed.
- Normal/build dependency guards passed: AI has no SDK/providers/route-runtime
  dependency, and provider default builds do not pull in keyring, rand or chrono.
  SDK source/public-API manifest and generated schema/catalog were not changed
  by this batch. Local spec/progress document links passed.
- `dist-helper check` passed with schema/catalog outputs unchanged. The existing
  Anthropic header edit is preserved; the batch delta is confined to hosted auth,
  its Cloud management regression, hosted Tokio features and refactor documents.

The macOS full build emitted a linker `__eh_frame` size warning; Cargo also
reported existing `proc-macro-error2` future incompatibility. Neither prevented
these checks. Hosted CI, real providers, Windows/MSRV and external consumers were
not validated. Changes remain local, uncommitted and unpushed.

The initial full run passed 3,602 tests and identified one management-client fixture
that accepted a contradictory refresh namespace. The updated fixture covers omitted,
unchanged and contradictory namespace responses, including no management dispatch
and no second exchange after rejection. This is an intentional correction to
identity handling, rather than preservation of the old unsafe expectation.

The guarantees require this process and its Tokio runtime to remain alive.
Pending replacements after write failure or identity rejection are volatile;
other-process exclusion, crash recovery and uncertain remote exchange outcomes
are not supplied. Namespace/subject checks detect conflicting AS responses;
opaque bearer identity and continuation remain dependent on the trusted issuer's
existing rotation contract, without new independent token attestation.

## Batch 7: AI catalog runtime and application-owned bootstrap

Implemented locally on 2026-10-05. This batch moves the consumed provider/model
schema and explicit catalog lifecycle to AI. Provider authentication mechanisms
and the SDK config/activation bridge still require further consolidation.

- `bitrouter_ai::catalog::{types,fetch,store}` owns registry schema, protocol
  conversion, bounded two-artifact fetching and source-keyed injected storage.
  `Catalog::new` starts empty with memory-only storage. Construction, inspection
  and `load` do not discover providers or read ambient paths/config/credentials.
  Network refresh requires a caller-supplied client, source and `NetworkPolicy`.
- Refresh parses both artifacts, validates identifiers/protocol sets and saves
  the complete snapshot before publishing it. Network, parse, validation and
  persistence errors retain the previous snapshot and its timestamp. No snapshot
  is represented explicitly as unavailable. Freshness is inspected separately
  under the caller's TTL; durability reports `Memory` or the backend's
  `Persistent` acknowledgement. Cancelling an observed fetch leaves the previous
  snapshot/store intact; catalog GET requests do not require auth rotation ownership.
- The providers crate's old runtime `registry::{types,fetch,cache}` owners and
  `load_or_cached`/`cached_registry` orchestration are removed, with no forwarding
  facade. Providers/app consumers import the AI schema directly. `is_public`
  describes access metadata; credential gating, status selection, classification,
  disabled entries and explicit config precedence remain in the config bridge.
- `apps/bitrouter/src/catalog.rs` selects the cache path, 24-hour TTL and explicit
  startup/reload refresh. It implements injected file storage with serialized
  process-local writers, atomic replacement and file/Unix-directory sync. Snapshots
  carry their selected source URL, preventing custom/public registry mixing.
  Unbound legacy files cannot prove their source; they are preserved on load but
  not adopted for either source until an explicit successful refresh writes a
  bound snapshot. Catalog files contain metadata, not saved account credentials.
- Existing `bundled_registry.rs` supplementation remains authoritative for the
  default public source; disabled/custom sources receive no public baseline.
  Onboarding/reload credential-variable metadata now consumes the cache
  under its declared source (including custom-source variable names) plus the
  public offline baseline without network I/O. This collects names only; it does
  not combine routing catalogs or change source selection. The provider helper takes
  an explicit optional catalog instead of reading a default cache itself.
  Shipped skill references are updated for offline bootstrap, source binding and
  editorial versus runtime sync; CLI command/flag/environment names are unchanged.
- Registry YAML, dist publishing, package-local ACP/model snapshots and the
  application build script stay in place. The current canonical-model type still
  carries only the consumed vocabulary ID; this is not a complete canonical
  capability API. Invocation continues independently of catalog/config/routing.

Validation (local macOS):

- Final `cargo nextest run --all-features --no-fail-fast --test-threads 4`:
  3,612 passed, 22 skipped. The initial run passed 3,611 before the final consumer
  review added custom-source reload-variable coverage. Catalog tests cover explicit
  empty/memory defaults, offline refusal without HTTP, complete pair publication,
  partial/status/parse failure, invalid identifiers, persistence failure, stale
  retention, source isolation, bounded requests and cancellation after an observed
  fetch. Existing projection/activation fixtures still pass, including repeated
  refresh preserving both active/disabled user entries and explicit model/endpoint
  settings. New file-cache tests preserve unknown-source legacy bytes and reject
  source mismatches without changing selected model/catalog policy.
- `cargo clippy --all-features --tests -- -D warnings` passed. Workspace
  Rustdoc under `-D warnings` passed. Separate workspace doctests: 6 passed,
  1 existing example ignored.
- AI standalone `cargo test -p bitrouter-ai --no-default-features`: 378 passed
  (356 semantic/catalog, 8 auth, 14 invocation); 1 doctest passed, 1 existing
  example ignored. Provider default, `pkce`-only and `hosted`-only checks passed.
- Normal/build dependency guards preserve AI isolation and keep provider default
  builds free of keyring, rand and chrono. SDK source/public-API manifest and
  generated schema/model/provider outputs are unchanged in this batch. The
  pre-existing Anthropic header edit is preserved.
- `cargo fmt -- --check`, `git diff --check`, local documentation links and
  plugin-manifest JSON passed. The shipped skill entrypoint remains unchanged
  at approximately 200 lines; source/offline details live in its provider reference.
  `dist-helper check` passed with schema/catalog outputs unchanged.

The macOS build emitted the existing linker `__eh_frame` size warning and Cargo's
`proc-macro-error2` future-incompatibility notice; checks still completed. Hosted CI,
real provider/catalog publication, Windows/MSRV and external consumers were not
validated.

This is local catalog lifecycle integration. It does not relocate the remaining
provider auth implementations or ACP data, implement Core mapping/loss admission,
validate external consumers or prove remote publication consistency. File writer
coordination is process-local; cross-process exclusion and crash recovery remain
backend/application concerns. Changes remain local, uncommitted and unpushed.

## Batch 8: native request integrations and shared OAuth grant ownership

Implemented locally on 2026-10-05. Codex, Copilot and SuperGrok request-time
implementations now belong to AI. Their old providers modules are removed,
without alias/re-export forwarding. Anthropic/Claude, Google and hosted adapters,
interactive login/import and application projection still need further extraction.

- `bitrouter_ai::providers::{codex,copilot,supergrok}` owns native request headers,
  JWT/subject authority derivation, Codex body adaptations and the Copilot exchange
  cache. Codex/SuperGrok receive an explicit `OAuthSession`; Copilot receives an
  explicit client/endpoint/store. These constructors discover no paths, environment,
  accounts, catalog or login UI. Their errors use provider-domain guidance rather
  than reading the SDK's CLI invocation name.
- `auth::oauth` owns the shared token-endpoint parser, refresh grant, confidential
  grant variant and bounded 30-second requests. Browser/PKCE URL construction and
  interaction stay above AI and consume its token decoder/error type. Refresh
  requires HTTPS except explicit loopback HTTP endpoints. This preserves the
  external HTTPS guard while enabling local fixtures/selected loopback services.
- Token diagnostics no longer include body previews or server descriptions;
  nonstandard error identifiers are bounded to `unknown_error`. Known codes such
  as `invalid_client` remain available for the existing Google client-secret
  decision. Empty grant material is a distinct credential failure; malformed
  endpoint responses are transport/protocol failures. Expiry arithmetic saturates
  instead of overflowing. Copilot bearers redact `Debug`; exchange errors omit
  response bodies and reject empty/expired returned credentials.
- Application assembly chooses the file backend and bounded HTTP client, then
  injects native sessions using the same public registration/client configuration
  as before. `DEFAULT_ACCOUNT` moves to AI as the single default-slot constant;
  persisted label/value shapes and native authority namespaces are unchanged.
  No account fallback, implicit catalog discovery or login was introduced.
- File-specific native regressions move to provider integration tests; pure
  headers/JWT/body tests live beside the AI implementations. Moved setup code
  propagates failures rather than using unwrap/expect/panic. Existing device-code
  root forwarding exports are retired and callers use its explicit submodule.
  Standalone native fixtures need neither SDK/providers nor file/path policy.

Local validation:

- `cargo nextest run --all-features --no-fail-fast --test-threads 4`:
  3,612 passed, 22 skipped. Separate `cargo test --doc --all-features`:
  6 passed, 1 ignored.
- `cargo test -p bitrouter-ai --no-default-features`: 396 tests passed;
  1 doctest passed, 1 ignored. The three new standalone fixtures cover native
  refresh/streaming, Copilot exchange/401 recovery and secret-safe diagnostics.
- `cargo test -p bitrouter-providers --all-features --test subscription`:
  20 file-backend integration regressions passed.
- `cargo clippy --all-features --tests -- -D warnings`, `cargo fmt -- --check`
  and strict workspace Rustdoc (`RUSTDOCFLAGS='-D warnings' cargo doc --workspace
  --all-features --no-deps`) passed. Two relocated Rustdoc links were repaired.
- Independent providers checks passed with no default features, `pkce` alone
  and `hosted` alone. AI's normal/build dependency tree contains no SDK,
  providers, Core/orchestrator, server/MCP or keyring dependency; providers'
  default graph also excludes the optional keyring/PKCE dependencies.
- `cargo run -p dist-helper -- check` passed. SDK/public-API manifests,
  generated schema/catalog, CI, skills and the pre-existing Anthropic header
  diff are unchanged from the start of this batch; plugin manifests parse as JSON.

The workspace emitted the existing macOS linker `__eh_frame` size warning and
`proc-macro-error2` future-incompatibility notice; these did not fail the checks.

AI-only local fixtures demonstrate subscription streaming and Copilot non-stream
401 recovery. Codex's existing non-stream SSE bridge remains in the SDK executor;
this batch does not claim that native invocation migration is finished. Ordinary
shared grant ownership does not flatten hosted OAuth's rich metadata envelope.
File coordination/pending rotations remain process-local, and uncertain remote
outcomes, cross-process exclusion and crash recovery are not proven. Real provider,
Keychain, Windows/MSRV, hosted CI and external-consumer validation are outstanding.
All changes remain local, uncommitted and unpushed.

## Batch 9: Codex non-stream generation over upstream SSE

Implemented locally on 2026-10-05. AI-only `ModelClient::generate` can now invoke
the Codex subscription backend, which requires streaming Responses requests.
Previously, AI sent `stream: false` and interpreted the SSE body as JSON; only
the SDK's private bridge supported this call. A failing standalone HTTP fixture
reproduced that gap before implementation.

- The Codex integration owns the upstream streaming requirement. `generate`
  reuses the selected-target stream/auth path, preserving source history,
  request deadlines, same-account 401 recovery and diagnostic scrubbing.
- `stream::collect::collect_generate` owns canonical result aggregation. The
  SDK's former aggregation implementation is deleted; its remaining private
  wrapper delegates to AI and builds the pipeline envelope after applying its
  normal request/header/continuation policy. No SDK public signature or dependency
  manifest changes are required.
- Collection consumes through EOF, propagates late errors, requires a terminal,
  retains explicit empty/text/reasoning block boundaries, assembles interleaved
  calls by id, and retains available tool metadata and thinking signatures.
  Conflicting call names/metadata and contradictory block/terminal identities
  are rejected. Usage reports are snapshots: the last report wins. Responses
  `incomplete` maps to `Length`; collection executes no tools or agent turns.
- Standalone fixtures cover native block/custom-tool input, final usage,
  truncated/failed/post-terminal streams, refreshed selected-account invocation
  and observable TCP release after token cancellation or dropping `generate`.
  Collector regressions also cover signed reasoning, server-tool pairs and
  trailing usage after a terminal. Ordinary provider JSON invocation remains
  covered by the existing four-protocol tests.

Local validation:

- `cargo test -p bitrouter-ai --no-default-features`: 406 tests passed;
  1 doctest passed, 1 ignored. This includes six new collector regressions and
  four new standalone Codex generation fixtures.
- `cargo nextest run --all-features --no-fail-fast --test-threads 4`:
  3,622 passed, 22 skipped, including the existing SDK Codex bridge regression.
  Separate `cargo test --doc --all-features`: 6 passed, 1 ignored.
- `cargo clippy --all-features --tests -- -D warnings`, `cargo fmt -- --check`,
  strict workspace Rustdoc (`RUSTDOCFLAGS='-D warnings' cargo doc --workspace
  --all-features --no-deps`) and SDK's no-default-features build passed.
- Pinned `cargo-public-api 0.52.0` / `nightly-2026-05-05` extraction passed;
  the SDK public dependency manifest still matches and both CI sentinels are
  present. AI's normal/build dependency guard excludes SDK/providers,
  Core/orchestrator, server/MCP and keyring dependencies.
- `cargo run -p dist-helper -- check` passed; generated schema/catalog are
  current. Provider/application files, public dependency manifests, generated
  data, CI, skills and the pre-existing Anthropic header diff are unchanged
  from the start of this batch. Local documentation links and diff checks passed.

The workspace still emits its existing macOS linker `__eh_frame` size warning
and `proc-macro-error2` future-incompatibility notice; neither failed the checks.

This completes the existing Codex SSE-to-result aggregation extraction, not full
native output fidelity. Collection can only retain fields already surfaced by
`StreamPart`: Responses encrypted reasoning, native MCP/approval items and some
result/block metadata are still codec/loss-admission work. Result-level metadata
and stop details absent from the event representation are not invented. Streaming
callers still receive already emitted parts/usage before a late error; the
single-result API does not return a partial successful generation. Core mapping,
remaining provider adapters, real-provider validation, hosted CI and external
consumer migration remain outstanding. All changes remain local, uncommitted
and unpushed.

## Batch 10: Anthropic and Claude Code request integration ownership

Implemented locally on 2026-10-05. Anthropic Platform API and Claude Code
subscription request mechanisms now live in AI, without a providers alias or
re-export facade for the removed appliers.

- `providers::anthropic::AnthropicApiKeyApplier` receives an explicit credential
  backend and owns `x-api-key`/version headers, explicit/stored/fallback priority
  and unchanged Platform API request bodies. `providers::claude_code` receives
  an `OAuthSession` plus an explicitly permitted fallback token and owns OAuth
  headers, beta union, the existing agent identity/LiteLLM body adaptation and
  same-account unauthorized recovery. Neither constructor discovers paths,
  environment, accounts, registry metadata, Keychain or login UI.
- Application assembly selects the file backend, optional live Claude CLI source,
  environment fallback, bounded refresh client and public Anthropic OAuth client
  registration. The composite marker/live transaction remains in
  `bitrouter_providers::claude_code::store::ClaudeStore`; its now-public constructor
  is used by assembly and integration tests. Environment capture remains above AI.
  A provider model call still cannot start login or select another account.
- Shared Anthropic header constants move to AI without value changes. The source
  was copied byte-for-byte, preserving the pre-existing Rustdoc edit's AI owner;
  strict Rustdoc then required that one link to use AI's internal `crate::` path.
  All other header-file bytes match the pre-batch snapshot. Old providers Anthropic modules
  and request applier constructors are deleted, and workspace consumers use the
  new owner. Missing-auth messages use provider-domain guidance rather than an
  SDK CLI-invocation dependency. Credential labels and saved value formats remain
  unchanged; no credential file migration or catalog/config rewrite is performed.
- The six Anthropic and fourteen Claude Code regressions move into provider
  integration tests, including fresh/stored/explicit priority, idempotent body
  adaptation, beta union, live CLI rereads and refresh completion after caller
  cancellation. Moved setup helpers propagate errors. Three new AI-only memory/
  local HTTP fixtures cover JSON/SSE calls, native headers/body, refresh-grant
  rotation, one 401 recovery, source-prompt preservation and invalid-slot refusal.

Local validation:

- `cargo test -p bitrouter-ai --no-default-features`: 409 tests passed;
  1 doctest passed, 1 ignored. The three new fixtures use only AI, memory
  credentials and loopback HTTP; no SDK/providers/application state is required.
- `cargo test -p bitrouter-providers --all-features --test subscription`:
  40 passed, including all 20 relocated Anthropic/Claude Code regressions.
- `cargo nextest run --all-features --no-fail-fast --test-threads 4`:
  3,625 passed, 22 skipped. Separate `cargo test --doc --all-features`:
  6 passed, 1 ignored.
- `cargo clippy --all-features --tests -- -D warnings`, `cargo fmt -- --check`
  and strict workspace Rustdoc (`RUSTDOCFLAGS='-D warnings' cargo doc --workspace
  --all-features --no-deps`) passed. The migration-only headers Rustdoc link was
  repaired before the successful strict Rustdoc run.
- Providers build independently with no default features, `pkce` alone and
  `hosted` alone. AI's normal/build dependency guard excludes SDK/providers,
  Core/orchestrator, server/MCP and keyring dependencies. Providers' default
  graph excludes `keyring`, `rand` and `chrono`.
- Pinned `cargo-public-api 0.52.0` / `nightly-2026-05-05` extraction passed;
  the SDK public dependency manifest still matches and both CI sentinels are
  present. `cargo run -p dist-helper -- check` passed; generated schema/catalog
  are current. SDK source/manifests, CI, skills and generated data are unchanged
  from the pre-batch diff. Documentation links/plugin JSON checks passed.

The workspace still emits the existing macOS linker `__eh_frame` size warning
and `proc-macro-error2` future-incompatibility notice; neither failed the checks.

This extracts request mechanisms; application login/import, CLI adoption and
activation policy remain above AI. Live CLI and file writer coordination are
process-local, without cross-process exclusion or crash recovery proof. Google
and hosted provider extraction, native stream fidelity/loss admission, Core/ACP
integration and known external-consumer migration remain outstanding. Real
provider/Keychain, Windows/MSRV and hosted CI validation are not claimed. Changes
remain local, uncommitted and unpushed.

## Batch 11: Google AI request mechanism and custom protocol ownership

Implemented locally on 2026-10-05. The `google-ai` Antigravity integration now
lives in AI: native authentication, project binding, confidential refresh and
the custom Code Assist protocol share the same lower-level owner.

- `providers::antigravity::AntigravityAuthApplier` receives an explicit
  `OAuthSession` and bootstrap HTTP client. It preserves existing bearer/client
  headers and `{model, project, request}` shaping; its project cache is keyed by
  exact bearer authority and origin. Explicit credentials bypass stored auth;
  invalid selected slots never switch to a fallback credential.
- `protocol` owns `v1internal:*` endpoints, response/SSE envelopes and dispatch
  registration while composing Gemini codecs. Registration is explicit: no
  catalog or provider discovery is added to ordinary model calls. Old providers
  protocol/applier definitions are removed without alias/re-export forwarding.
- `refresh::AntigravityRefresher` receives endpoint/client metadata and a
  caller-permitted secret-source callback. The source is deferred until refresh;
  only `invalid_client` permits trying another client secret. The last successful
  secret is cached with the same behavior as before; invalid grant, transport or
  malformed responses abort. Missing refresh material is a credential failure
  (401), rather than a transport failure. Token diagnostics remain bounded.
- Application assembly retains public client metadata, its bounded client
  configuration, selected file store and `agy` secret discovery policy. The
  providers module now only supplies the local `agy_client` source; environment,
  binary and Keychain discovery/import remain above AI. Credentials and catalog
  values are not migrated or rewritten, and no CLI/config surface changes.
- Project bootstrap enforces its existing 30-second bound even with an injected
  client. HTTP failures preserve native status before JSON decoding, including
  non-JSON 429 responses. Transport diagnostics omit URLs and response payloads.
- Two file regressions move to provider integration tests; two pure/memory and
  five protocol regressions move to AI. Five new AI-only HTTP fixtures cover
  JSON/SSE calls, secret probes/cache, one same-account 401 recovery, credential/
  origin project isolation, source-prompt preservation, status/error redaction,
  explicit/invalid-slot priority and refresh completion after dropping `generate`.
  The cancellation fixture holds a real returned token behind an explicit
  barrier, then verifies commit and the next call without a second grant.

Local validation:

- `cargo test -p bitrouter-ai --no-default-features`: 421 tests passed;
  1 doctest passed, 1 ignored. Five new local HTTP fixtures and seven relocated
  Google pure/memory/protocol regressions require no SDK/providers or ambient
  client/credential discovery.
- `cargo test -p bitrouter-providers --all-features --test subscription`:
  42 passed, including the two relocated Google file-store regressions.
- `cargo nextest run --all-features --no-fail-fast --test-threads 4`:
  3,630 passed, 22 skipped. Separate `cargo test --doc --all-features`:
  6 passed, 1 ignored.
- `cargo clippy --all-features --tests -- -D warnings`, `cargo fmt -- --check`,
  strict workspace Rustdoc (`RUSTDOCFLAGS='-D warnings' cargo doc --workspace
  --all-features --no-deps`) and providers' no-default, `pkce`-only and
  `hosted`-only builds passed.
- AI's normal/build dependency guard excludes SDK/providers, Core/orchestrator,
  server/MCP and keyring dependencies. Providers' default graph excludes
  `keyring`, `rand` and `chrono`. Migrated source/setup passed panic, bypass and
  ambient-discovery scans; compile-time OS/architecture constants remain usable.
- Pinned `cargo-public-api 0.52.0` / `nightly-2026-05-05` extraction passed;
  the SDK public dependency manifest still matches and both CI sentinels are
  present. `cargo run -p dist-helper -- check` passed; schema/catalog are current.
  SDK source/manifests, generated data, CI, skills, Anthropic headers and local
  `agy` discovery/import source are unchanged from the pre-batch snapshot.
  Local documentation links/plugin JSON and diff checks passed.

The workspace still emits the existing macOS linker `__eh_frame` size warning
and `proc-macro-error2` future-incompatibility notice; neither failed the checks.

This is local Google mechanism extraction. Secret source callbacks and storage
policy are caller-controlled; their filesystem/Keychain behavior is not an AI
default. Grant/cache coordination remains process-local, without cross-process
or crash recovery proof. Existing codec fidelity limitations are retained rather
than declared resolved. Hosted adapter extraction, Core/ACP mapping, loss
admission and known external-consumer migration remain outstanding. Real
Google/agy/Keychain, Windows/MSRV and hosted CI validation are not claimed.
All changes remain local, uncommitted and unpushed.

## Batch 12: hosted request authentication and full-envelope session ownership

Implemented locally on 2026-10-05. AI's optional `hosted` feature owns the native
hosted request applier, complete credential schema, OAuth resolution/refresh,
metadata discovery and shared token-envelope decoder. The application still
selects account storage, activation, login/logout, HTTP defaults and onboarding.

- `providers::hosted::session::HostedSession` accepts one selected credential
  key, an injected `HostedCredentialStore` and an explicit HTTP client. A leased
  `HostedCredentialTransaction` retains the entire rotated envelope and rejection
  state. Lease acquisition remains cancellable; an owned task completes admitted
  refresh, staging and commit independently of a dropped model/Cloud caller.
- The existing process-local file lease, pending replacement and compare/commit
  backend implement that contract. `CredentialManager` now owns file operations
  and session assembly. Its duplicate bearer/refresh resolution methods are
  removed; model auth, telemetry, management and standalone Cloud glue all use
  `manager.session()`. Login/logout races, failed writes and symlink-path sharing
  retain their existing storage authority and tests. No crash/cross-process
  durability claim is added.
- `BitrouterAuthApplier` receives the AI session and application onboarding text.
  Explicit overrides bypass storage; fallback requires an absent selected slot.
  A provider/account mismatch or stored origin failure cannot enable fallback.
  Namespace/issuer continuation authority remains stable across token rotation.
- Native discovery/exchange I/O remains bounded at 30 seconds with HTTPS or an
  explicit loopback HTTP endpoint. Token refresh now rejects HTTP error status
  even when its body contains access material, and rejects a successful HTTP
  error envelope. Metadata/token failure diagnostics omit server payloads and
  endpoint URLs. Token TTL arithmetic checks conversion/addition overflow.
- The metadata and token-envelope decoder also serve application device login,
  with a single owner. Old provider applier/metadata modules and refresh/token
  declarations are deleted; workspace callers import the AI definitions directly.
  Credential-file tags and legacy OAuth decoding remain compatible. There are
  no public re-export aliases. AI's hosted chrono dependency is default-off and
  enabled explicitly by the application/provider hosted feature.
- Six AI-only HTTP fixtures cover JSON/SSE and source-prompt preservation, full
  envelope/identity retention, failed-commit retry without another grant,
  cancellation after observable rotation staging with a held commit barrier,
  contradictory identity retention/no dispatch, safe HTTP/parser failure and
  selected-slot/origin/fallback boundaries. Existing file-backed hosted applier
  tests move to the provider integration suite; transaction/Cloud sharing tests
  continue through the AI session.

Validation (local macOS):

- Workspace `cargo nextest run --all-features --no-fail-fast --test-threads 4`:
  3,637 passed, 22 skipped. Existing file/Cloud tests pass through the AI session,
  including process-shared leases, cancellation, failed writes and login/logout
  conflicts. After Clippy's one test-only `ok_or_else` simplification, AI's final
  all-feature test run passed 442 tests (399 unit and 43 integration). Default AI
  passed 421 tests with hosted disabled. Provider focused coverage passed 173 unit
  and 55 integration tests, including the 13 relocated hosted applier cases.
- Strict workspace `cargo clippy --all-features --tests -- -D warnings` and
  formatting passed. Workspace doctests: 6 passed, 1 pre-existing ignored custom
  transport example. `RUSTDOCFLAGS='-D warnings' cargo doc --workspace
  --all-features --no-deps` passed.
- Provider default, `pkce`-only and `hosted`-only checks passed. Default/hosted AI
  normal/build trees contain no SDK/providers, Core/orchestrator, Axum, rmcp,
  keyring or rand. Default AI/provider trees exclude chrono; hosted explicitly
  enables it. Default providers also remain free of keyring and rand.
- Pinned `cargo-public-api 0.52.0` on `nightly-2026-05-05` passed both CI
  observability sentinels and the SDK public-dependency comparison: 16 foreign
  crates unchanged, with no OpenTelemetry public types. `dist-helper -- check`
  passed; schema/registry artifacts remain current and unchanged.
- The moved credential schema, original file persistence and transaction code
  match the batch-start snapshot. Protected SDK/provider sources (96 files) and
  161 tracked diff sections outside this batch remain unchanged. Documentation
  links, plugin JSON and `git diff --check` passed.

The existing macOS `__eh_frame` linker and dependency `proc-macro-error2`
future-compatibility notices remain. The first dist-check build exhausted local
disk space; scoped cleaning of this workspace's `bitrouter` dev artifacts allowed
that check and the remaining guards to pass, without changing source.


No CLI flag, environment name, listen port, default configuration or harness
wiring is changed. Registry data and bundled dist files are unchanged. Native
output fidelity, conversion-loss admission, Core mapping, ACP relocation and
old provider/SDK retirement remain separate work. Changes remain local,
uncommitted and unpushed; hosted CI and real-provider validation are not claimed.

## Batch 13: completed Responses reasoning fidelity and bounded loss guards

Implemented locally on 2026-10-05. This batch retains completed native reasoning
as output data and corrects Responses result ordering; it does not authorize
cross-target reasoning replay or declare the general loss contract complete.

- `types::NativeReasoning::Responses` retains a completed source item, including
  encrypted content, item identity/status, separate summary/content arrays and
  extension fields. It has a redacted Debug implementation. `Content::Reasoning`
  and `StreamPart::ReasoningEnd` add optional native output; start markers carry
  source protocol and text deltas carry summary/text lane. All absent fields
  deserialize to legacy defaults and are omitted on serialization. Workspace
  constructors and pattern matches migrate directly without public reexports.
- Non-stream Responses parsing keeps native reasoning even with an empty visible
  summary. Streaming retains `response.output_item.done`, ignoring incomplete
  encrypted material in `.added`. Per-item visible lanes verify the final
  snapshot and emit only missing suffixes, rather than repeating deltas. The
  shared collector keeps the completed capsule alongside visible text.
- Responses JSON output emits reasoning in its original content position rather
  than concatenating all reasoning at the head. Existing text/citation runs,
  client/server/MCP call projections, approvals and files keep their existing
  representations and follow content order. Native stream starts preserve the
  Responses item id; lane frames and completed item/response output keep the
  terminal native payload. Summary/content part indexes are still normalized
  on the streaming projection; index fidelity remains follow-up work.
- A completed native snapshot must match visible text in collection and JSON/SSE
  encoding, so it cannot restore text changed by hooks. The regex stream hook
  preserves text-lane metadata when rebuilding a replaced reasoning delta.
  Malformed captured identity/encrypted fields, text-part arrays and contradictory snapshots fail with diagnostics
  that do not echo payloads or opaque values.
- All four request codecs reject direct canonical history containing this new
  native capsule until selected-target/account replay admission exists. The
  three other client codecs reject output/stream endings that cannot carry it.
  These are bounded guards for a newly represented native effect, expressed as
  model-domain errors. They do not implement structured conversion reports,
  route candidate exclusion/ranking or the full ingress/output loss matrix.
  Legacy Responses request parsing still needs native preservation/diagnostics;
  existing credential-bound continuation policy remains above AI.
- Nine focused tests cover JSON item order and empty summary; completed-versus-
  partial encrypted data; collector/encoder parity; terminal-only text and missing
  suffixes; distinct visible lanes and native close identity; source/debug/legacy
  serde; JSON and forced-Codex-SSE model calls; zero extra dispatch for unadmitted
  history; altered text, malformed payload and incompatible output rejection.
  The first fidelity regressions failed on the batch-start codecs and passed
  after implementation. The stream conversion matrix now explicitly asserts the
  new native loss rejection instead of freezing silent omission as success.

Native field and lifecycle checks were grounded in the official
[Responses streaming reference](https://developers.openai.com/api/reference/resources/responses/streaming-events)
and [reasoning item schema](https://developers.openai.com/api/reference/resources/responses/subresources/input_items/methods/list),
read on 2026-10-05. These define the source wire, not target/account authority or
live-provider conformance.

Validation (local macOS):

- Workspace `cargo nextest run --all-features --no-fail-fast --test-threads 4`:
  3,646 passed, 22 skipped. AI default tests: 430 passed; AI all-feature tests:
  451 passed (399 unit and 52 integration), including all nine native-reasoning
  regressions. The inherited stream-matrix setup now returns errors with `?`;
  its final focused rerun also passed.
- Workspace strict Clippy (`--all-features --tests -- -D warnings`), fmt and
  workspace type checks including tests passed. Workspace doctests: 6 passed,
  1 pre-existing ignored custom-transport example. Strict workspace Rustdoc
  (`RUSTDOCFLAGS='-D warnings'`, all features, no dependency docs) passed.
- Provider default, `pkce`-only and `hosted`-only checks passed. AI normal/build
  trees retain no SDK/providers, Core/orchestrator, Axum, rmcp, keyring or rand.
  Default AI/provider builds remain free of hosted chrono; default providers
  remain free of PKCE keyring/rand.
- Pinned SDK public-API extraction passed both observability sentinels, the
  public-dependency comparison (16 foreign crates unchanged) and the no-public-
  OpenTelemetry-type guard. `dist-helper -- check` passed; committed schema and
  registry artifacts remain current, with no manifest or registry-data change.
- 89 auth/provider/client/Cloud files match the batch-start snapshot, and
  176 tracked diff sections outside the field migration/documents are unchanged.
  Local documentation links, plugin JSON and diff whitespace passed. New native
  payload/data tests use fallible setup rather than unwrap/expect/panic bypasses.

The existing macOS linker `__eh_frame` and dependency `proc-macro-error2`
future-compatibility notices remain. Only this workspace's disposable dev and
old incremental build caches were cleared for validation; source was preserved.


No CLI flag, environment name, port, default config, catalog data or harness
wiring changes. Core native mapping, general loss diagnostics/admission,
remaining stream/native tool fidelity and ACP/provider/SDK retirement remain
separate work. Changes are local, uncommitted and unpushed; real-provider,
external-consumer and hosted-CI validation are not claimed.

## Batch 14 — Native ingress preservation and initial conversion admission

Implemented locally:

- Responses request parsing retains each reasoning item as native history in its
  source position, including encrypted-only and empty-summary items. Summary and
  raw reasoning text remain visible in canonical content while the complete item
  stays in the redacted native capsule. Malformed native input returns a safe
  request error rather than a provider-response error.
- AI owns `conversion::ConversionReport` and typed `ModelError::Incompatible`.
  Refusals contain stage, structural location, categorical wire, bounded reason,
  semantic effect and disposition. Reports contain no transcript text, native
  item IDs, custom-protocol names, credentials or selected-account identity.
  Initial rules cover retained native reasoning and unsupported structured
  outputs; they do not certify all other codec effects as lossless.
- Responses native history requires an unproven target/account replay authority;
  another built-in wire cannot represent that native item. Custom-wire native
  compatibility is unclassified, so it receives an unknown-effect exclusion.
  These have distinct diagnostic reasons/dispositions. No permissive degradation switch or replay bypass was
  added. Non-Responses result and stream encoders preserve the report when they
  refuse native reasoning loss, rather than returning an unstructured guard error.
- `OutboundAdapter::admission` and `ModelClient::render_request` share request
  admission. SDK `HttpExecutor::preflight` uses that same selected-target renderer,
  and `DispatchExecutor` delegates to its selected executor. Custom executors have
  the native-history default rule and must add their representation-specific
  preflight checks; preflight is synchronous and performs no provider/auth I/O.
  Request execution still renders and validates again from the original Prompt.
- Both pipeline execution modes exclude incompatible candidates before fallback
  delay, hop observers, auth or HTTP. `ObserveHook::on_conversion_excluded` receives
  the concrete target and content-free report independently of attempted-provider
  reliability observations. The callback has the existing panic-isolation policy.
  Exclusions do not invoke execution failure hooks or fallback failure policy.
  An all-excluded chain returns the collected typed incompatibility report; mixed
  attempt failures still use the existing attempt-error aggregation.
- SDK retains the report in `BitrouterError::Incompatible` and maps pre-dispatch
  incompatibility to 400 and output incompatibility to 502 with a stable public
  code. The existing public error envelope/SSE remains a safe summary; the full
  structured report is available to typed callers and exclusion observers.
  Core recording and richer gateway diagnostic serialization remain later work.
- Wiremock is an SDK development-only dependency for actual HTTP preflight tests;
  no production dependency, catalog, CLI or authentication behavior was added.

Evidence and limits:

- The new encrypted-only ingress regression failed against the batch-start code:
  the native item disappeared and only two of three input items remained.
- Focused tests exercise retained input ordering, report locations/serialization,
  content-free Debug/diagnostics, safe malformed-input status, direct/router
  structured-output refusal, delegated HTTP executor preflight, ordinary and SSE
  candidate exclusion, all-excluded native history, and request/output status
  mapping. The HTTP fixture proves one admitted request and zero excluded
  requests; hop counters prove exclusions are not provider failures.

Final local validation after the custom-wire unknown-effect classification:

- AI default: 433 unit/integration tests passed; all features: 454 passed. The
  completed/ingress reasoning suite has 12 tests. AI's compile doctest also
  passed; the pre-existing custom transport example remains ignored.
- Full workspace nextest with all features: **3652 passed, 22 skipped**, 51.215s
  test execution. Strict workspace Clippy (`--all-features --tests -- -D warnings`)
  and fmt passed. Separate workspace doctests: 6 passed, 1 ignored.
- Strict workspace Rustdoc, provider default/PKCE-only/hosted-only checks and
  `dist-helper -- check` passed. No generated schema/catalog update was required.
- Pinned SDK API extraction passed both observability sentinels, the exact
  16-public-dependency manifest and the no-public-OpenTelemetry guard. AI's
  default/hosted normal/build trees preserve the dependency direction and hosted
  feature isolation; default providers retain no chrono/keyring/rand.
- 175 tracked diff sections outside the batch's expected files remain unchanged.
  Cargo.lock's only new batch dependency is SDK's development-only wiremock.
  Plugin JSON, local documentation links and diff whitespace passed. No new
  allow/unwrap/panic bypass was added; the mock's `.expect(1)` specifies HTTP
  request cardinality.
- Only this workspace's disposable package build caches were cleared for disk
  space. Source and prior uncommitted changes were preserved. Existing macOS
  linker `__eh_frame` and dependency future-compatibility notices remain.

General ingress diagnostics, remaining task-semantic/unknown effects, nonessential
classifications, explicit classified degradation, actual replay-authority proofs,
Core mapping, native stream indexes/MCP fidelity and ACP/old-owner retirement are
still open. Other Responses input-item shapes retain the historical lenient
handling; this batch does not claim complete ingress-loss admission. No live
provider, external consumer or hosted-CI proof is claimed. Changes remain local,
uncommitted and unpushed.

## Batch 15 — Initial four-protocol ingress loss diagnostics

Implemented locally:

- All four built-in inbound adapters check initial omissions after envelope
  deserialization and before returning a canonical Prompt. Unknown/unmapped
  content blocks, unsupported complete content values, unknown Responses input
  items and message items without roles now return a typed incompatibility.
  Reports aggregate detected omissions using original wire indices, without
  including raw type names, item IDs, transcript/media data or credentials.
- The AI-owned report adds `RequestIngress`, ingress locations and bounded
  reasons/dispositions. An unknown mapping has an unknown-effect request refusal;
  recognized payloads that cannot fit the current system/tool-result slot have
  a task-semantic refusal. These rules have no permissive fallback switch.
- System text extraction cannot silently erase media/non-text payloads. Known
  plain strings/text blocks remain supported, including existing implicit
  Messages system text and its cache-control metadata. The supported system
  slot remains text; no media-to-text replacement was introduced.
- Chat/Responses tool-output content arrays and Messages nested tool-result
  arrays refuse unsupported members before their existing filter/flatten code
  can discard them. Messages error tool results refuse image payloads that the
  current ErrorText representation would erase. Arbitrary JSON values in slots
  that already retain them, and raw MCP JSON result payloads, remain supported.
- Messages thinking without its required continuity token now returns a
  replay refusal rather than deleting the reasoning block and continuing with
  the visible answer. Token presence is not a target/account authority proof;
  that broader admission work remains open.
- Gemini parts with multiple recognized payload carriers are refused before the
  existing first-match parser chooses one and omits the others. Ordinary
  function calls/results, signatures and media remain supported. Known Responses
  heterogeneous items and promoted additional_tools declarations remain supported.
- Gateway production wiring already used these inbound adapters and the existing
  SDK error mapping, so no new production server/pipeline/auth logic was needed.
  Actual HTTP handler tests verify all four endpoints, in ordinary and streaming
  modes, return 400 with the existing incompatibility code before any executor
  call. Full reports remain available to typed callers; public HTTP errors retain
  the existing content-free summary.
- The historical unknown Responses-item and unsigned-thinking tests now assert
  the approved strict alpha contract. Unknown output/SSE control-event handling
  retains its separate existing behavior; this batch changes request ingestion.

Evidence and limits:

- Six regression tests failed before the functional guards were added, showing
  acceptance after source content was omitted. All passed with the guards.
- Focused tests additionally cover complete unsupported values, aggregated
  locations, continued acceptance/preservation of known media/JSON/signatures,
  tool declarations and opaque MCP results. Original source indices and absence
  of secret/type/payload strings are checked in Debug and serialized reports.
- The new SDK HTTP handler test covers eight endpoint/mode cases and checks a
  safe 400 response plus zero executor calls. These are local in-process gateway
  tests, not real-provider or production evidence.

Final local validation:

- AI default: 442 unit/integration tests passed; all features: 463 passed. The new
  ingress suite has 9 tests. AI's compile doctest passed, with the pre-existing
  custom transport example ignored.
- Full workspace nextest with all features: **3662 passed, 22 skipped**, 92.839s
  test execution. Strict workspace Clippy (`--all-features --tests -- -D warnings`),
  fmt and strict workspace Rustdoc passed. Separate doctests: 6 passed, 1 ignored.
- Provider default/PKCE-only/hosted-only checks and `dist-helper -- check` passed.
  No generated catalog/schema or Cargo.lock update was required.
- Pinned SDK API extraction passed both observability sentinels, the exact
  16-public-dependency manifest and the no-public-OpenTelemetry guard. AI's
  default/hosted normal/build trees and default provider feature isolation passed.
- 181 tracked diff sections outside expected batch files remain unchanged. No
  new allow/unwrap/expect/panic bypass was added in snapshotted Rust files. Plugin
  JSON, local document links and diff whitespace passed. Disposable local app
  build artifacts were cleared for validation; source was preserved. Existing
  macOS linker and dependency future-compatibility notices remain.

These are initial structural ingress rules, not a complete wire-schema validator
or a claim that every accepted attribute round-trips. Nested attributes/native
payload details, other outbound/client-encoding losses, nonessential classification,
classified degradation, causality and target/account replay-authority proofs,
Core recording/mapping, native stream fidelity and ACP/old-owner retirement remain
open. No CLI flag, port, env/default config, harness wiring, catalog data or
production dependency changed. Work remains local, uncommitted and unpushed.

## Batch 16 — Initial structural request projection admission

Implemented locally:

- All four built-in request renderers now apply the shared initial request
  admission, including callers that invoke codecs directly. ModelClient and SDK
  HTTP preflight use the same rules; the router retains candidate selection,
  fresh source projections, exclusion observations and attempt accounting.
- Reasoning that Responses would omit, reasoning without the continuity material
  required by Messages, omitted history sources, unsupported tool-role content
  and ordinary results that Chat/Messages would drop outside the Tool role are
  refused. Source/citation omission is unclassified rather than declared harmless.
- Provider-executed and dynamic tool history cannot silently disappear or become
  ordinary client calls. Native Messages MCP history retains its server identity
  and JSON/error-JSON result; unsupported role/output/server-identity shapes are
  excluded. Other account/model-specific replay authority is not certified here.
- Tool-result content reports each media/file-reference member the selected
  renderer would omit or flatten: Chat file IDs, Messages file IDs/non-image
  media and Gemini non-text tool-result payloads. Responses retains these native
  content parts, including text/file-ID/text order. No media-to-text replacement
  or fabricated file bytes were introduced.
- Approval requests with no request representation and approval responses without
  a faithful target slot are refused. Responses approval responses preserve their
  ID/boolean, but cannot silently lose a supplied reason. A denial marked for
  approval suppression needs a matching denied response in the source; otherwise
  it cannot disappear. The latter is a bounded local pairing check, not complete
  causality or a selected-account native-state proof.
- Represented Anthropic/Gemini continuity material on reasoning/tool calls cannot
  cross a request wire that omits it. This also prevents redacted thinking from
  being forwarded as plain reasoning text. Same-wire preservation does not prove
  selected model/account authority, and no permissive replay override was added.
- Provider-defined declarations use their existing native-family identity. Chat
  declaration omission is refused; foreign/unclassified translation is excluded.
  Non-object declaration arguments cannot become an empty object. Existing native
  declarations remain eligible under these initial rules, without a complete
  native-schema/model-capability certificate. No catalog-specific hardcoded model
  list or new capability matrix was introduced.
- Antigravity explicitly delegates admission to the Gemini codec it already wraps.
  A generic custom wire remains unclassified; its name never enters the report.
  The two reused Messages metadata render helpers are crate-private, with no
  new public re-export facade or provider/auth/account discovery logic.

Evidence and limits:

- Six new regressions failed against the old request-rendering behavior and
  passed after the admission rules. The projection suite additionally tests
  continuity tokens, native/foreign MCP, denial pairing, non-object arguments,
  output-only approval requests and known/custom native declaration projection.
- The real HTTP executor fallback test covers ordinary and SSE modes: one lossy
  Chat candidate excluded, one Responses request issued, file ID and surrounding
  text preserved exactly, one hop start and zero provider-failure hops. The
  source Prompt remains the input for each candidate; no lossy intermediate
  projection is fed into fallback.
- Historical request tests that froze foreign-tool forwarding or dropped history
  now assert the approved strict alpha contract and retain native/ordinary-path
  positive assertions. Output/client rendering and SSE control-event behavior
  retain their separate existing contracts.
- SDK library tests: 702 passed, 2 pre-existing ignored, before final workspace
  validation. This is local evidence, not real-provider, external consumer or CI
  delivery proof.
- Final workspace validation: 3,675 nextest tests passed, 22 skipped; independent
  doctests passed (6 passed, 1 ignored). AI default and all-feature test runs,
  strict Clippy including tests, formatting and strict workspace Rustdoc passed.
- Provider default/PKCE/hosted checks and `dist-helper check` passed. The pinned
  SDK public API retained its 16 declared foreign crates and exposed no telemetry
  types; AI dependency direction and default/hosted feature isolation passed.
  Plugin JSON, local documentation references and diff whitespace were valid.
  All 181 unrelated prior tracked diff sections and Cargo.lock were preserved.

Remaining attribute/schema/status and boundary conversions, complete causal
validation and actual replay-authority proofs, nonessential/degradation policy,
Core recording/mapping, native stream fidelity and ACP/old-owner retirement are
still open. Passing the report is not a complete lossless/authority certificate.
No CLI flag, port, env/default config, harness wiring, registry data or production
dependency changed. Changes remain local, uncommitted and unpushed.

## Batch 17 — Initial attribute, schema and tool-status projection admission

Implemented locally:

- Shared request admission now refuses explicit function-tool strict flags on
  Messages/Generate Content and structured-output strict flags, names and
  descriptions that those serializers omit. Both true and false are explicit
  caller choices; schema-constrained sampling alone is not an equivalence proof.
  The common schema-only subset and the OpenAI-family native slots remain usable.
- Gemini tool schemas are compared against the actual existing serializer.
  Unchanged schemas and the bounded single-base-type/nullable representation
  remain admitted, including nested properties/items/anyOf. Deleted keywords,
  distinct type unions, null-only fallback and conflicting nullable declarations
  are excluded as unclassified effects. The source schema is never edited, and
  no second keyword allowlist or speculative model-capability matrix was added.
- A filename needs a slot in the actual selected file renderer. Document names
  survive on Chat/Responses, while names omitted by image/audio/native media
  branches or Messages/Gemini are refused as unclassified omissions. Filename
  and schema-metadata omissions were not declared harmless without evidence.
- Tool-result errors retain their native Messages error flag. Chat/Responses/
  Gemini cannot silently turn ErrorText/ErrorJson into ordinary success results.
  Unpaired execution-denied results cannot become plain text on any built-in
  wire. A paired Responses denial still uses its denied approval response, but
  cannot lose a supplied result reason. Local pairing is not full causality or
  selected-account replay-authority validation.
- These rules apply to direct codecs, ModelClient and SDK HTTP preflight through
  the existing shared admission. Diagnostics contain only bounded categorical
  reasons and original tool/message/content coordinates, without schema paths,
  filenames, names, descriptions, denial/error payloads or credential identifiers.

Evidence and limits:

- Seven new attribute/constraint/status regression tests: six reproduced the old
  silent losses before implementation; the equivalent nullable/single-type case
  already passed and remains admitted. All seven pass with the new rules.
- The real HTTP executor fallback test covers ordinary and SSE modes: the lossy
  Gemini candidate is excluded, one Chat request retains the original complete
  tool schema, and observations show one exclusion, one attempt, zero provider
  failures. Direct ModelClient preparation returns the same schema report.
- Ten historical request expectations now assert the approved strict alpha
  contract, while keeping native/common-subset positive cases. Existing output
  encoding and SSE control-event contracts are unchanged.
- The assembled application structured-output matrix passes all 16 cells:
  12 admitted native/common-subset routes retain their schema; the four named
  OpenAI-source routes to Messages/Gemini return content-free 400 errors in
  ordinary and SSE modes with zero upstream requests. OpenAI ingress requires a
  schema name, so these four HTTP edges have no unnamed common-subset fixture;
  Messages/Gemini ingress supplies the schema-only positive cells. A direct
  canonical schema-only call remains eligible under the current rules.
- Final workspace validation: 3,683 nextest tests passed, 22 skipped; independent
  doctests passed (6 passed, 1 ignored). AI default/all-feature tests, strict
  Clippy including tests, formatting and strict workspace Rustdoc passed.
- Provider default/PKCE/hosted checks and `dist-helper check` passed. The pinned
  SDK public API retained its 16 declared foreign crates and no telemetry types;
  AI dependency direction and default/hosted feature isolation passed. Plugin
  JSON, diff whitespace and all 23 local documentation references were valid.
  All 180 unrelated prior tracked diff sections and Cargo.lock were preserved.

These are additional bounded projection rules, not complete default loss admission.
Remaining ingress attributes, media subtype/options, JSON and content boundaries,
native schema/model validation, complete causality/authority proofs, classified
degradation, Core mapping/recording, native output fidelity and ACP/old-owner
retirement remain open. No CLI, listen port, env/default config, harness wiring,
registry data or production dependency changed. Work remains local, uncommitted
and unpushed, with no real-provider or hosted-CI evidence from this batch.

## Batch 18 — Initial JSON, content-boundary and nested-attribute admission

Implemented locally:

- Messages/Generate Content cannot replace a malformed canonical argument string
  with `{}` on request projection. Responses ingress refuses non-string argument
  or custom-input values before the existing string mapper erases them. Valid
  represented arguments and native string slots retain their current behavior.
- Chat and Messages tool-result arrays retain their Content representation even
  when every part is text. Chat messages with several text blocks use ordered
  native parts rather than concatenating them into one string. Chat refuses
  multiple reasoning blocks, late reasoning and text/media after a tool call
  where the current field-based representation would change boundaries/order.
- Messages ingress refuses ordinary tool results occurring after non-tool
  content before its current partition can move results ahead of that content.
  Leading tool-result groups remain supported. Multiple error-text parts are
  refused before concatenation; the canonical error union still has no
  error-plus-content-array slot.
- Gemini object tool results remain unchanged. A synthetic `result` wrapper for
  scalar JSON/text and a concatenated text-only content array are unclassified
  and excluded. Known omitted media/file references retain their per-part
  diagnostics instead of being hidden by a whole-result shape report.
- JSON encoding into Chat/Responses/Messages string-only tool slots remains
  eligible: inverse JSON decoding retains the complete value. Error/denial status
  admission remains independent. The original typed Prompt is authoritative;
  a projected text string is never fed back as fallback's canonical source.
- Initial ingress attribute checks cover Chat tool-result filenames/image detail,
  Responses tool-result filenames/detail and ambiguous media carriers, Messages
  document title/context/citations and nested tool-result cache controls, and
  Gemini function-response parts/continuation/scheduling attributes. Refusals
  retain original wire coordinates and expose no attribute names, values or
  decoder error text in the report. These are consumed-shape rules, not complete
  wire-schema validation or a new capability matrix.

Evidence and limits:

- Twelve new boundary/argument/attribute tests pass. Ten actual loss/preservation
  regressions reproduced the old behavior; JSON inverse-decoding and native
  Messages MCP JSON-error controls verify representations that remain available.
  Two historical expectations now retain array boundaries or refuse an
  unclassified wrapper; existing JSON encoding and multiple-result cardinality
  coverage remain in place.
- The real HTTP executor fallback test covers ordinary and SSE modes: one Gemini
  candidate excluded, one Chat request, inverse JSON decoding equal to the
  original array, one attempt and zero provider failures. Direct ModelClient
  preparation returns the same content-free shape report.
- Gateway tests additionally cover a nested-attribute loss on all four inbound
  wires in ordinary/SSE modes, returning safe 400 errors before any executor call.
- Workspace nextest: 3,696 passed, 22 skipped. After a semantics-preserving
  consolidation of two identical Gemini diagnostic branches required by Clippy,
  the 21 affected boundary/ingress tests were rerun successfully. Strict Clippy
  including tests, formatting, independent doctests (6 passed, 1 ignored), strict
  workspace Rustdoc and AI default/all-feature tests passed.
- Provider default/PKCE/hosted checks and `dist-helper check` passed. Pinned SDK
  public API guards retained the 16 declared foreign crates and no telemetry
  types. AI dependency direction, default/hosted feature isolation, plugin JSON,
  diff whitespace and all 23 local documentation references passed. All 180
  unrelated prior tracked diff sections and Cargo.lock were preserved.

Remaining native attributes/media options, other boundary transformations, full
schema/model validation, admitted-effect recording, classified degradation,
causality/provenance/account authority, Core mapping/recording, output/stream
fidelity and ACP/old-owner retirement remain open. Empty reports pass only these
initial rules. No CLI flag, port, env/default config, harness wiring, registry
data or production dependency changed. Work remains local, uncommitted and
unpushed; this batch has no real-provider or hosted-CI evidence.

## Batch 19 — Equivalent-conversion reports and candidate observations

Implemented locally:

- ConversionReport retains refusals in `issues` and detected classified
  equivalent effects in a separate `admitted` list. Bounded JSON tool-result
  encoding and Gemini single-type/nullable schema normalization are recorded
  with original structural locations. Unchanged/native forms produce no effect
  entry. Unclassified schema cleanup is not mislabeled as equivalent. Older
  serialized reports default the new list to empty.
- Equivalent records never waive task/unknown/replay refusals, including when
  both lists are populated. These classifications contain no source text, JSON
  keys/schema paths, opaque IDs, credentials or account/endpoint identity.
- ModelClient::render_request_with_report returns a fresh selected-target body
  and its initial assessment without authentication/I/O. Existing body-only
  rendering delegates to this same preparation. Registered wrapper admission
  continues to delegate to its actual semantic codec.
- Executor::preflight now returns ConversionReport; HTTP and dispatch executors
  preserve that assessment. This is an intentional alpha trait-signature change
  for custom executors. The SDK independently rechecks refusal lists, so an Ok
  result containing refusals cannot reach execution.
- ObserveHook::on_conversion_admitted is an immutable eligible-candidate callback
  before hop start, emitted only for detected equivalent effects. Empty reports
  produce no conversion callback. Hook panic handling follows the existing
  read-only observer contract. The callback records admission, not proof of
  dispatch/completion; provider-attempt observations retain their own lifecycle.
  Excluded candidates keep separate observations, and aggregated exclusions
  retain local equivalent classifications without calling them admitted targets.

Evidence and limits:

- Four AI regressions failed before equivalent-effect classification and now
  pass: JSON encoding, actual schema normalization, coexistence with a native
  replay refusal, and direct selected-target preparation. Privacy/serialization
  and legacy-report compatibility are covered.
- Ordinary and SSE HTTP tests cover one excluded Gemini candidate, a real
  admitted Chat 500 failure and an admitted Responses retry. Observation order
  is exclusion, admitted effect, hop start, actual failure, admitted effect,
  hop start. Both issued requests inverse-decode to the original JSON value;
  exclusions do not add attempts or failures, and fallback uses the original
  typed source.
- Both modes also reject a custom executor's successful preflight result that
  contains refusals before any execution, and omit conversion callbacks for
  unchanged ordinary requests.
- Final workspace validation: 3,702 nextest tests passed, 22 skipped. AI
  default/all-feature tests, strict Clippy including tests, formatting,
  independent doctests (6 passed, 1 ignored) and strict workspace Rustdoc passed.
- Provider default/PKCE/hosted checks and `dist-helper check` passed. Pinned SDK
  public API guards retained 16 declared foreign crates and no telemetry types;
  AI dependency direction/default-hosted feature isolation, plugin JSON, diff
  whitespace and 23 local documentation references passed. All 178 unrelated
  prior tracked diff sections and Cargo.lock were preserved.

Core/durable conversion recording, other equivalent/nonessential classifications,
explicit scoped degradation, remaining ingress/media/options/boundary/native-schema
effects, full causality/account authority, output/stream fidelity and ACP/old-owner
retirement remain open. These reports are not complete lossless/authority proofs.
No CLI flag, port, env/default config, harness wiring, registry data or production
dependency changed. Work remains local, uncommitted and unpushed, without
real-provider or hosted-CI evidence from this batch.

## Providers-retirement plan revision — 2026-10-06

Plan updated after Batch 19; no Batch 20 implementation or new runtime validation
is claimed by this revision. The next implementation batch changes from further
conversion-recording work to extracting reusable explicit login mechanisms.

The migration separates mechanisms from application policy:

- AI receives explicit PKCE/authorization-code/device-code mechanisms, the
  provider registration inputs used by current login/refresh callers, and hosted
  authorization/poll/revoke/token-envelope mechanisms. Browser/terminal actions,
  CLI labels/errors and account/login-method selection stay in the application.
  These operations remain explicit; a model call never starts login.
- A subsequent batch extracts the existing ordinary explicit-path credential
  file backend into an opt-in AI backend. Application default-directory/filename
  selection and migration policy remain above AI. Existing labeled/legacy
  reads, permissions, atomic writes, compare-and-commit, pending replacements
  and path/account coordination must survive the move.
- The hosted file backend and account manager move to the application Cloud
  account module, preserving their complete credential format and current
  injected AI store/session contracts. Product scopes, flag/env precedence,
  activation/config mapping and external CLI/Keychain/binary discovery also
  stay in application modules. Model, Cloud management and telemetry retain
  shared refresh/commit coordination.
- Delete `bitrouter-providers` only after current and known downstream callers,
  tests/features and CI/release/docs references are migrated and removal checks
  pass. The current workspace's direct consumer is the `bitrouter` application;
  that inventory does not verify external consumers. No compatibility facade
  or new Cloud/auth/storage crate is planned.

The [revised migration sequence](BITROUTER_AI_REFACTOR_SPEC.md#revised-next-steps-retire-bitrouter-providers)
contains the source-to-owner inventory and validation gates. Providers-package
removal can proceed independently of remaining conversion/Core/ACP work once
actual dependencies are migrated; it does not establish all AI-12 requirements
or complete obsolete SDK API retirement. Existing local test results remain
pre-removal evidence, not proof that the deletion or new extraction works.

## Batch 20 — Explicit login mechanisms and application interaction boundary

Implemented locally:

- PKCE generation, authorization-code exchange, loopback/state handling and
  callback-driven login orchestration move to `bitrouter-ai::auth`. Provider
  registration constraints move to `bitrouter-ai::providers::login` without CLI
  display labels. The old six OAuth source modules are deleted, and application
  assembly/commands and provider integration tests import the AI owners directly.
- AI's `pkce` feature gates random generation and loopback transport. Device-code
  authorization/polling is available in the default build over explicit inputs.
  `hosted-login` gates hosted authorization, polling, revocation and full token
  envelope construction; `hosted` alone retains native request authentication
  and refresh without login's random dependency. AI adds no SDK/app/Keychain
  dependency. CI now exercises isolated PKCE and hosted-login test builds.
- Application interaction owns terminal hints, callback HTML, command-specific
  guidance, the existing 15-minute login timeout and login/account selection.
  `LoginUx::show_authorize_url` receives a manual-only flag; it no longer receives
  product hint text from the library. Login returns a token for caller-chosen
  storage and never starts from a missing-credential model call.
- Hosted flow accepts explicit `LoginParams`, independent of product Settings.
  Cloud flag/env/default scope resolution, file persistence, account-manager
  assembly and shared model/management/telemetry refresh coordination stay in
  their existing owners. The old hosted account flow module is deleted.
- A total async PKCE deadline bounds listener, manual-only input, interaction
  callbacks and token exchange. This fixes the previously unbounded manual-only
  branch. Ordinary/hosted device polling retain expiration through pending and
  slow-down, bound each in-flight poll and issue no poll after expiration. This
  fixes the ordinary application's unbounded loop and hosted post-expiry poll.
  Requests also have bounded HTTP timeouts with caller-owned clients.
- Device-token success requires successful HTTP status, no OAuth error and a
  nonempty access token. Code exchange also rejects an empty initial token.
  Selected server error codes are retained without body previews or arbitrary
  descriptions; PKCE verifiers, callback/pasted codes and device/user codes have
  redacted Debug. Refresh rotation/commit behavior is unchanged.

Evidence and limits:

- The new AI-only integration target failed to compile before the login owners
  were extracted. Moved coverage retains PKCE vectors, provider redirect/client
  constraints, state/manual behavior, actual loopback requests and token-exchange
  HTTP construction/decoding. The exchange fixture now uses the production
  internal exchange rather than duplicating its body implementation; constructor
  HTTPS admission remains separately tested.
- New tests exercise manual-only timeout, cancellation after observing a bound
  listener and pending prompt, and successful port rebinding after cancellation.
  Ordinary device HTTP fixtures exercise request inputs, safe malformed/failing
  replies, expiration during sleep/in-flight HTTP, and pending/slow-down without
  extending the original deadline.
- Hosted AI-only HTTP fixtures exercise no post-expiry request, failing HTTP with
  a token-shaped body, actual in-flight expiration/cancellation, and successful
  login/revocation retaining selected AS/client/scope, namespace/subject and refresh
  expiry. Caller-ready notification and request bodies are verified. These are
  local fixtures; they do not prove a real provider's login or grant behavior.
- Final workspace nextest: 3,712 passed, 22 skipped. The first complete run
  had one unchanged local CLI version-probe assertion failure; its isolated
  rerun passed, and the second complete run passed with two test threads. No
  local CLI source was changed. The earlier build exhausted local disk before
  tests; completed task-owned artifacts were cleaned and validation used an
  application-only `profile.dev.package.bitrouter.debug=0` command override.
  Source/config files and test features were unchanged by that override.
- AI default, hosted, PKCE and hosted-login test builds passed; the focused
  all-feature AI build passed as well. Provider default/PKCE/hosted checks,
  strict Clippy including tests, formatting, independent doctests (6 passed,
  1 ignored), strict workspace Rustdoc and `dist-helper check` passed.
- Pinned SDK public API guards retained 16 declared foreign crates and no
  telemetry types. Four isolated AI dependency trees preserve no SDK, provider,
  app, Core, orchestrator, Axum, MCP or Keychain edge; default/hosted have no
  rand, and only hosted/hosted-login select chrono. Login features select rand
  without importing keyring. Default providers still excludes rand/keyring/chrono.
- Plugin JSON, diff whitespace and 31 local documentation references/anchors
  passed. All 238 unrelated dirty-file states were preserved; Cargo.lock changed
  only to move the existing rand dependency from providers to AI.

Dropping a login future releases library-owned async work and loopback resources;
already-started blocking stdin work in the application cannot be stopped by
merely dropping its async wrapper. This existing UI limitation is documented,
not claimed solved. Cancellation may occur after the server issues an initial
grant; this batch does not claim recovery/durable persistence of that uncertain
initial grant. Existing refresh-rotation transaction guarantees remain separate.
No file format, CLI flag, listen port, environment/default config, harness wiring
or registry data changed. Ordinary file-backend extraction, application policy/
Cloud account relocation, providers-package removal and known downstream migration
remain pending. Work is local, uncommitted and unpushed.

## Batch 21 — Optional ordinary credential files and product location policy

Implemented locally:

- AI's opt-in `file-store` feature owns the ordinary credential snapshot/writer
  and selected-account file transactions in `auth::file::{snapshot,backend}`.
  Existing store/transaction APIs and regression coverage move to one owner;
  the providers `oauth` module and both storage implementations are deleted
  without forwarding exports. Providers' current activation/live-adoption glue
  and application callers import AI directly.
- `apps/bitrouter/src/provider_credentials.rs` selects the existing XDG,
  Windows LOCALAPPDATA and home precedence and `oauth-tokens.json` filename.
  Startup/reload, onboarding, login/logout and Claude marker activation use this
  application helper. The file mechanism chooses no default directory/account
  and reads no HOME/XDG credentials implicitly. Relative explicit paths bind to
  the cwd at construction, preserving filesystem rather than lexical symlink/..
  semantics for existing ancestors.
- Administrative snapshots and refresh leases share selected-file resolution,
  including physical aliases before missing parent directories are created.
  Administrative logout through a file symlink updates the selected target and
  preserves the link. Composite Claude backends retain that same bound marker
  path, preventing a later alias retarget from authorizing a revoked rotation.
- Labeled/legacy reads are nonmutating; subsequent mutations preserve the existing
  labeled schema, unrelated slots, Unix owner-only permissions, atomic replacement
  and compare-and-commit behavior. Failed writes preserve the caller snapshot
  and retained transaction replacement. Snapshot Debug omits file paths, provider/
  account identities and credential values.
- The feature adds no production dependency. AI dev tests use the workspace's
  existing tempfile package. Application and remaining providers explicitly
  enable `file-store`; hosted envelope files/settings and external CLI/Keychain
  discovery remain above AI. CI tests the standalone file-store feature.

Evidence and limits:

- AI-only storage tests failed to compile before the new owner existed and now
  exercise legacy read-only loading, stale administrative writers preserving
  unrelated slots, actual pending leases across instances, failed-commit retry,
  and cancellation after observing an owned refresh operation. A fresh backend
  waits for the old operation to commit and reuses the rotated file credential
  without a second refresh. Alias and isolated-process cwd-change cases cover
  selected-path identity without mutating parent-process environment.
- A new live-adoption regression reproduced the previous authorization bug:
  after revoking the original marker and retargeting its alias, stale commit
  returned Persistent and overwrote the CLI token. Retaining the AI backend's
  bound marker path now returns Conflict and leaves the CLI credential unchanged.
- Existing snapshot/transaction and provider subscription coverage moves with
  the implementation; application location-policy coverage tests precedence and
  empty/missing inputs without loading the user's credential files.
- Final workspace nextest: 3,721 passed, 22 skipped. Validation retained the
  prior application's debug-info-only command override and used two test threads;
  source/config files and runtime test features were unchanged by the override.
  AI default and standalone file-store tests, provider default/PKCE/hosted checks,
  strict Clippy including tests, formatting, independent doctests (6 passed,
  1 ignored), strict workspace Rustdoc and `dist-helper check` passed.
- Pinned SDK public API guards retain 16 declared foreign crates with no telemetry
  types. Five isolated AI trees, including file-store, contain no SDK/provider/
  Core/orchestrator/app, Axum, MCP, ACP, Keychain or production tempfile edge.
  File-store alone adds no rand or chrono; existing login/hosted isolation remains.
  Plugin JSON, diff whitespace and 31 local documentation references passed.
- All 245 unrelated dirty-file states were preserved. Cargo.lock changed only
  to add the already-existing tempfile package to AI's dev-dependency list;
  file-store adds no production package or changes to persisted schemas.

File coordination and retained rotations remain process-local, with no new
cross-process exclusion or crash recovery proof. Dynamic filesystem changes
outside the caller's protected storage scope remain the caller's responsibility.
No CLI flag, listen port, environment variable, default location/config, harness
wiring, registry data or persisted credential schema changed. Alpha imports move
from providers OAuth storage to AI file modules; AI removes the product default
path API and distinguishes invalid selected paths from missing product locations.
Remaining product policy/Cloud account relocation, package removal, Core/ACP work
and known downstream migration are unfinished. Work is local, uncommitted and
unpushed, without real-provider or hosted-CI evidence from this batch.

## Batch 22 — Application provider policy and Cloud account ownership

Implemented locally:

- Product provider configuration/activation moves to `apps/bitrouter/src/providers`:
  `apply`, `builtin`, `entry` and `registry::apply`. The compiled-in Cloud TOML
  moves byte-for-byte to `apps/bitrouter/providers/bitrouter.toml`. AI catalog
  metadata still drives model/protocol/pricing mapping and existing override,
  credential admission, default and reload behavior.
- Claude environment/live-store adoption, Codex/Grok/Claude imports, OS Keychain
  access and Antigravity binary/secret discovery move into the same application
  provider module. AI receives explicit selected stores, sessions and permitted
  source callbacks as before; it gains no application/discovery dependency.
- Cloud envelope persistence, leases/pending rotations, manager and Settings move
  to `cloud::account`. Existing file formats, locations, scopes and settings
  precedence are retained. Model authentication, management/API clients, ACP
  fallback and telemetry use the existing application-shared AI session.
- All workspace source/test consumers import the new owners directly. The old
  providers source/data/tests, Cargo package/dependency/lockfile entry and active
  CI package subject are removed without an empty package or forwarding facade.
  Subscription integration tests move to the application's test target and Linux
  CI shard. The existing keyring dependency moves to the application; no new
  production package is introduced. Release configuration already has no
  providers-package entry.
- Relocated configuration tests set environment variables only on isolated child
  processes and verify that the exact test actually ran. Moved tests return
  errors instead of using panic shortcuts. Invalid embedded Cloud data is logged
  without panicking; the committed TOML parsing regression remains.
- Active development/contributor/API docs point to current owners; historical
  audit links retain their baseline revision. Legacy provider User-Agent strings
  in the application are preserved to avoid changing request metadata during
  this ownership move.

Workspace consumer inventory:

| Consumer | Migration/evidence scope |
| --- | --- |
| `apps/bitrouter` library and `bro` binary | Imports, startup/reload, default configuration, login/import/logout, Cloud consumers and account assembly migrated |
| Provider subscription/file/account regressions | Relocated with their implementation; now compiled as app unit/integration tests |
| SDK, telemetry, guardrails matcher, dist-helper | No retired providers dependency; model integration remains AI-owned; stale current source comments corrected |
| Core/orchestrator and other external consumers | Not workspace members here; their migration and publication are not verified by this checkout. Local removal does not assert completion of AI-12 |

Evidence and limits:

- Focused post-move application/provider/Cloud tests pass (232 cases). Existing
  credential/schema, selected-account, logout/login races, pending replacement,
  alias and cancellation regressions move with the implementations.
- The shared-session fixture now also issues a real management request alongside
  model authentication and telemetry resolution, verifies the rotated bearer and
  namespace path, observes one refresh, and checks the persisted refresh token.
- Full workspace nextest: 3,721 passed, 22 skipped. All original moved test
  functions remain. Validation uses two test threads and the established
  application-only `profile.dev.package.bitrouter.debug=0` command override;
  runtime features, source configuration and executable behavior are unaffected.
- Independent AI default, PKCE, hosted, hosted-login and file-store tests pass.
  Strict Clippy including tests, formatting, independent doctests (6 passed,
  1 ignored), final strict workspace Rustdoc and `dist-helper check` pass.
  The first Rustdoc attempt caught a moved module link; the link and adjacent
  stale module explanations were corrected in documentation-only lines.
  Executable Rust remained unchanged after the full runtime test run; final
  Clippy, formatting and Rustdoc were rerun after those documentation corrections.
- Pinned SDK public API keeps its 16 declared foreign crates and exposes no
  telemetry types. SDK default/config-file builds pass. Five isolated AI
  normal/build trees preserve no application, SDK, retired providers, Core,
  orchestrator, Axum, MCP, ACP, Keychain or production tempfile edge; existing
  chrono/rand feature isolation remains. The app keeps its existing product
  Keychain dependency without linking providers or the guardrails implementation.
- Locked workspace metadata confirms the retired package and every dependency
  declaration are absent; no old source import, forwarding export or relocated
  panic/unsafe shortcut remains. Lockfile changes remove only that package and
  re-home the already-selected keyring dependency to the application. Cloud
  credential schema/backend/settings and embedded Cloud TOML are byte-identical
  to the pre-batch owners; no new production package or data migration is added.
- All 216 other dirty-file states are preserved across 103 intended paths.
  Plugin JSON, diff whitespace, 39 local Markdown references/anchors and the
  historical source-audit Git link pass. Baseline macOS compact-unwind linker
  messages and the proc-macro-error2 future-compatibility notice remain local
  environment/dependency warnings; no final check failed.

Storage coordination remains process-local; changing ownership adds no
cross-process exclusion or crash recovery. No CLI flag, listen port, environment
variable, default configuration/location, harness setup step or persisted schema
changes. Registry edits only correct implementation comments, with generated data
unchanged. This is an alpha breaking package/import/feature removal. No real
provider, OS Keychain interaction, hosted CI, external migration or publication
is claimed. Changes remain local, uncommitted and unpushed.

## Remaining implementation

| Spec phase | Remaining work |
| --- | --- |
| 0. Conversion semantics | Initial four-wire structural ingress/projection, selected attribute/schema/status/argument/boundary rules, JSON/schema equivalent reports and candidate observations are implemented; finish remaining attributes/media/options/boundaries, native schema/model validation, other classifications/scoped degradation, Core/durable recording, causality/provenance/authority and late-output rules |
| 1. Types/codecs | Extraction complete locally; agree and implement Core mapping |
| 2. Invocation/auth | Selected invocation, ordinary/hosted auth sessions, native provider request mechanisms and Codex SSE aggregation are in AI; explicit login mechanisms are also extracted locally; finish remaining native output fidelity |
| 3. Catalog/integrations | Catalog lifecycle/schema and application offline/persistence integration are complete locally; ordinary explicit-path storage and application provider/Cloud account ownership are complete locally; finish remaining integration acceptance |
| 4. ACP data | Review exact source/artifact locations and move data/schema/build consumers together |
| 5. Old APIs | Providers package removed and workspace consumers migrated locally; separately retire obsolete SDK implementations and complete known external consumer migration |

The completed batches implement bounded initial admission rules, not complete
default loss admission, transcript repair, Core integration or ACP relocation.
They do not claim AI-01 through AI-12 are all satisfied. Later
batches must add their acceptance evidence before those requirements are marked
complete.
