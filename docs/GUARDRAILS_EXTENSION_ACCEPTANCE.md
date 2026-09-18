# Guardrails extension: implementation and acceptance

> Current scope: compiled-only SDK extensions (ROUTER_EXTENSION_SPEC v0.7).
> All HTTP/service/probe results below are historical evidence for superseded
> implementations. They do not validate the current change. Current verification
> is recorded in the latest sections below; remote CI/release is separate.


Date: 2026-09-17. Scope: local 6A–6C changes in this worktree. This is not a
claim that the branch is merged, hosted CI has passed, or artifacts are published.

## Delivered scope

| Increment | Implementation |
| --- | --- |
| 6A | `bitrouter-checker-protocol::v1` shared by host and service; strict bounded codecs; SDK compatibility types retained. Matcher/config default build has no SDK dependency; legacy hooks require explicit `sdk`. |
| 6B | Package `bitrouter-guardrails-service`, binary `bitrouter-guardrails`, strict startup input/block rules, ordinary synchronous callback, bounded HTTP adapter, bearer auth, separate release archives. |
| 6C | Default host matcher dependency/assembly removed. Any old plugin key blocks startup and config validation; saved state is invalid; reload reports a safe migration-specific failure. Migration docs, skill and CI updated. |

No custom selector registry/algorithm, output checker, process supervisor or
extension marketplace is included. Existing policy-lock and process-local receipt
ownership remain in the host.

## Validation results

| Check | Result |
| --- | --- |
| Full workspace nextest, all features | 3,478 passed; 22 skipped. Final run uses `--no-fail-fast`. |
| Workspace clippy, all features/tests, `-D warnings` | Passed. |
| Workspace doctests, all features | 5 passed; 1 ignored. |
| Workspace rustdoc, all features, `-D warnings` | Passed. |
| SDK without defaults, then config_file alone | Both passed. |
| Matcher without defaults, then explicit sdk | Both passed. Default-only rustdoc also passed. |
| Shared protocol tests and standalone service check | Passed. |
| Dependency isolation | Default bro has no matcher/service dependency; matcher/service have no SDK dependency. |
| cargo-dist generated workflow consistency | `dist generate --check` passed. |
| Default bro build | Passed with the default dependency closure. |
| Committed dist/schema artifacts | `cargo run -p dist-helper -- check` passed. |
| Formatting and patch whitespace | `cargo fmt --all -- --check` and `git diff --check` passed. |
| Final default-build process rerun | All seven scenario groups passed; exactly one allowed upstream dispatch, zero dispatches for every rejection/failure scenario. |

Logs are `/tmp/guardrails-nextest-final.log`, `/tmp/guardrails-clippy-final.log`,
`/tmp/guardrails-doctests.log`, `/tmp/guardrails-rustdoc.log`, and
`/tmp/guardrails-extra-results.json`. The initial regression run exposed two
stale full-stack assertions (old plugin HTTP 400 versus checker 403; physical
versus logical root span identity). Both were corrected without removing zero
upstream/metering or trace-parent assertions; the complete rerun above passed.

Debug linking emitted the pre-existing macOS large `__eh_frame` warning, and
Cargo reported a future-compatibility notice for `proc-macro-error2`. These did
not fail compilation, strict clippy, or tests. No warning suppression was added.

## Process-level acceptance

`tools/guardrails_e2e.py` runs a real `bro` daemon and two real checker executables
in an isolated temporary home. A counted local HTTP fixture supplies model
responses; **no real provider/model calls are made**. It verifies:

- Different router bindings and cross-fragment matching with newline separators.
- Allow dispatches exactly once; deny, malformed response and timeout dispatch
  zero times in both streaming and non-streaming request modes.
- A stopped checker fails closed.
- Probe is synthetic and does not set actual usage.
- Receipts work without an exporter and do not expose the provider credential.
- Saved binding changes require restart; restart changes incarnation and clears
  process-local receipts.

The Rust suites additionally cover strict protocol version/identity/size errors,
input projection, local authorization and policy ordering, output remaining
unchanged, missing credentials, service saturation before body buffering, and
retention of CPU admission permits after a caller cancels. Migration tests cover
null/empty/block/redact/coexisting configuration, CLI exit behavior, no database
creation or config rewriting, and a sanitized live reload report.

## Reproduction

```sh
CARGO_INCREMENTAL=0 cargo nextest run --workspace --all-features
CARGO_INCREMENTAL=0 cargo clippy --workspace --all-features --tests -- -D warnings
cargo fmt --all -- --check
CARGO_INCREMENTAL=0 cargo test --workspace --all-features --doc
CARGO_INCREMENTAL=0 RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps
CARGO_INCREMENTAL=0 cargo build -p bitrouter --bin bro
CARGO_INCREMENTAL=0 cargo build -p bitrouter-guardrails-service --bin bitrouter-guardrails
python3 tools/guardrails_e2e.py --bro target/debug/bro \
  --checker target/debug/bitrouter-guardrails --output /tmp/guardrails-e2e
cargo run -p dist-helper -- check
dist generate --check
```

The service-only local distribution command for the current workspace version:

```sh
CARGO_INCREMENTAL=0 dist build --artifacts=local \
  --target aarch64-apple-darwin \
  --tag bitrouter-guardrails-service/v1.0.0-alpha.31 --output-format=json
```

This command selects an application locally; it creates no Git tag or remote
release. The repository's normal versioned release can carry separate host and
service archives. `precise-builds = true` keeps their Cargo feature sets separate.

## Local artifact evidence before directory relocation

The optimized macOS ARM64 service archive was rebuilt, extracted into a fresh
temporary directory and executed independently. Version/help and startup rejection
of `redact` and invalid regex were checked; the extracted executable passed the
post-removal process harness described above.

- Archive: `target/distrib/bitrouter-guardrails-service-aarch64-apple-darwin.tar.xz`.
- Version: `bitrouter-guardrails 1.0.0-alpha.31`.
- SHA-256: `6f9bedd628532549ced76884dfbfdc70a12c70672c5a99d490771fd06299179b`.
- Local metadata: `/tmp/guardrails-artifact-final.json`.
- Process evidence: `/tmp/guardrails-e2e-after-removal/report.json`; logs are in
  the same directory. Final default-build rerun: `/tmp/guardrails-e2e-final/report.json`,
  using the same extracted checker and a separately rebuilt default `bro`.

## Remaining release and coverage gates

- Only the local macOS ARM64 artifact is executed here. Six-platform distribution
  planning is not evidence that Linux/Windows/other macOS builds have run.
- Hosted CI and actual release publication remain external gates. Before shipping
  removal of built-in guardrails, make the separate service artifact available
  with its migration guide. The implementation and directory-verification runs
  did not publish artifacts; subsequent draft PR preparation does not complete
  the release gate.
- Input-only router bindings do not replace global/direct-model coverage or
  streamed output block/redact. Removing the legacy key is not evidence of
  equivalent protection; operators must review their own entry points and output
  requirements.
- Matcher absence is established by dependency trees and assembly changes. Other
  `regex` users remain; no binary-size improvement is claimed without an A/B build.

## Directory relocation follow-up (2026-09-17)

Both packages now live under `extensions/guardrails/`: `matcher/` preserves
package `bitrouter-guardrails`, and `service/` preserves package
`bitrouter-guardrails-service` and binary `bitrouter-guardrails`. The workspace
membership, relative dependency path and current documentation links were
updated. All 13 relocated Rust files are byte-identical to their pre-move
contents; no runtime or configuration semantics changed.

Verification after the move:

- Locked Cargo metadata resolves each package exactly once at the new path.
- Full workspace all-feature nextest: 3,478 passed, 22 skipped. One unchanged
  optimization test (`default_derived_explore_rejects_independent_identity_mutations`)
  was reported as passed/leaky; its isolated rerun passed without the leak flag.
  This observation is retained rather than represented as a cleanup fix.
- Workspace clippy with `-D warnings`, fmt and doctests passed. Strict rustdoc
  for both relocated packages, matcher without defaults and service checks passed.
- Dependency trees retain the previous host/matcher/service separation.
- `dist generate --check`, distribution planning and the macOS ARM64 archive
  rebuild passed. The rebuilt archive was extracted and all seven process E2E
  scenario groups passed with exactly one allowed upstream dispatch.
- Logs: `/tmp/guardrails-layout-nextest.log`,
  `/tmp/guardrails-layout-leak-recheck.log`,
  `/tmp/guardrails-layout-results.json`, `/tmp/guardrails-layout-driver.log`.
- Final process report: `/tmp/guardrails-layout-e2e/report.json`.
- Current archive metadata: `/tmp/guardrails-layout-artifact.json`.
- Current archive SHA-256: `a09b0c9e548cdeb8b147cf2da54eca1873015b70e1c9b85678298c1d1c893dfb`.

The archive path is unchanged and now contains this rebuilt artifact, replacing
the earlier archive at that path. Earlier hashes above document the pre-move
verification. Cross-platform execution and public publication remain pending.


## Regex checker / native capability follow-up (2026-09-17)

The preceding sections are historical evidence for 6A–6C and the first directory
move. This subsequent change renames the implementation directory to
`extensions/regex-checker/` and the new service package/binary to
`bitrouter-regex-checker`. The matcher package remains `bitrouter-guardrails`.
Earlier archive names and hashes above describe the earlier builds, not this
refactor. No new release or public artifact publication is claimed here.

`request-check` is the capability; regex-checker is one extension implementing it.
The common callback/decision contract lives in
`bitrouter-checker-protocol::capability`. Native custom hosts and the HTTP service
reuse the same matcher callback. `RequestCheckRuntime` owns both execution paths;
projection, binding, rejection and receipts continue through the existing pipeline.
Native registration is explicit and checked against a code/rules revision. Native
probe results do not claim HTTP reachability or wire-protocol validation.

Validation of this refactor:

- Full workspace all-feature nextest: **3,483 passed, 22 skipped**, no leak flag.
- Workspace all-target/all-feature clippy with `-D warnings`, fmt, doctests and
  strict rustdoc passed. SDK minimal/config-only, matcher minimal/SDK compatibility
  and independent-service checks passed.
- New gateway integration tests run the same regex callback natively and through
  a real HTTP service, covering cross-fragment decisions, two router bindings,
  receipts, version attribution and probe/actual-use separation. Separate tests
  cover malformed native decisions, timeout and unbound-registration inactivity;
  streaming and nonstreaming failures have zero model dispatches.
- Native cancellation/timeout test confirms that a started callback holds its
  admission slot after the caller stops waiting; a queued invocation times out
  without starting more CPU work. Missing/mismatched/extra registration and mixed
  transport config are rejected. Revision changes alter binding identities.
- Dependency trees confirm default bro has no matcher/service in normal/build
  dependencies and the independent service has no SDK/host. Example and gateway
  tests deliberately link these libraries through development dependencies.
- Config schema regenerated; `dist-helper check`, `dist generate --check` and
  `dist plan` passed. The plan lists the new binary/archive names on six targets.
- Fresh debug bro and renamed checker processes pass all seven existing process
  E2E scenario groups; exactly one allowed request reaches the mock upstream.
- Fresh compiled custom-host example passes real-socket allow, nonstreaming deny,
  streaming deny and unbound-router scenarios. Exactly two allowed requests reach
  its counted mock upstream; both denials return 403 without upstream calls.
- Both process suites use local mock providers; no real model provider was called.

Evidence: `/tmp/regex-nextest.log`, `/tmp/regex-validation-results.json`,
`/tmp/regex-checker-e2e/report.json`, `/tmp/regex-native-e2e/report.json`,
`/tmp/regex-host-tree.txt`, `/tmp/regex-service-tree.txt`.
Native process driver for this local run: `/tmp/regex-native-e2e.py`; reproducible
custom-host source: `apps/bitrouter/examples/native_regex_checker.rs`.
Cross-platform CI, public release and this follow-up's release archive build
remain separate gates. Native revisions are declared identities, not attestations;
trusted callbacks are not sandboxed or forcibly terminated by deadlines.

## Unified Extension author entry (2026-09-18)

The current worktree implements ROUTER_EXTENSION_SPEC v0.5 U1–U3 for the
existing request-check capability. This is local implementation/validation,
not a merge, CI or release claim.

- `bitrouter::extension::ExtensionApi::request_check(id, revision, callback)`
  collects typed native implementations. Duplicate/invalid registrations make
  the collection unusable even if the author ignores the returned error.
- `assemble::build_app_with_extensions` runs the registration function and
  validates bindings before database startup. Default host assembly uses an
  empty registration function; the legacy map entry shares the same private
  host assembly and request-check runtime.
- Native revision configuration, registration, legacy runtime activation and
  wire responses share one validator. `rules:v1` is now rejected before startup;
  the accepted grammar is 1–128 ASCII letters/digits or `. _ + - /`.
- The runnable regex example and author guides use the new entry. Legacy
  Plugin/global/output/redact/migration facilities remain explicitly documented
  custom-host compatibility APIs; they are not silently converted into input checks.

| Acceptance | Evidence |
| --- | --- |
| UE01 | Integration test registers multiple callbacks in a different order from router bindings; two routers execute their declared checks and leave the unbound instance inert. |
| UE02 | Registration tests cover invalid IDs/revisions and duplicates. Host tests prove explicit errors, ignored duplicate errors, missing/extra registrations and revision mismatch fail before creating a database. Existing configuration tests reject unknown bindings. |
| UE03 | Native/HTTP shared-decision and receipt integration coverage plus process runs below; existing timeout/cancellation admission tests continue to pass. Wire/config structure and schema remain unchanged. |
| UE04 | Source review confirms the public facade exposes registration only, with a private collection; no builder/context/reporter/migrations/global-hook accessors. SDK compatibility and existing host tests pass. This is API scoping, not a Native sandbox. |
| UE05 | Legacy SDK hooks are unchanged in behavior and retain their tests. Guides distinguish input-only, output block/redact and global/direct-model coverage; removal requires an announced breaking SDK release with migration notes. |
| UE06 | Default bro and both delivery examples build. Normal/build dependency trees exclude matcher/service from bro and SDK from standalone matcher/service. No manifest/dependency or embedded-extension addition was made; no size reduction is claimed. |

Validation on the final source:

| Check | Result |
| --- | --- |
| Workspace nextest, all features | 3,491 passed; 22 skipped. One unchanged optimization test had a leaky-process flag; isolated rerun passed without that flag. |
| Three unified-entry integration tests | Passed before the complete workspace rerun. |
| Workspace clippy, all features and all targets, `-D warnings` | Passed. |
| Workspace doctests | 5 passed; 1 ignored. |
| Workspace rustdoc, all features, `-D warnings` | Passed. |
| SDK without defaults; SDK config_file only; matcher without defaults | Passed. |
| Formatting, patch whitespace, dist/schema check | Passed. |
| Default bro, standalone regex checker and Native example builds | Passed. |
| Real HTTP daemon + checker processes | Seven scenario groups passed; one allowed upstream call, zero additional calls on deny, timeout, malformed response or stopped service. |
| Real Native example process | Allow returns 200; non-streaming and streaming deny return 403 without upstream dispatch; a router without the binding remains unguarded. Two total allowed upstream calls. |

Both process tests used counted local mock model providers. No live model,
production deployment, public release or cross-platform execution was tested.
The existing macOS linker unwind warning and dependency future-compatibility
notice remain; no suppression was introduced.

The first full run found an invalid test fixture using plain HTTP for a
non-loopback provider placeholder. The fixture was corrected to HTTPS without
relaxing production URL validation. The later complete run above passed.
The leaky flag was for
`optimization::controller::file_tests::first_mutating_run_activates_a_frozen_config`;
no optimization source was changed and its clean isolated rerun is not a claim
that a cleanup race has been fixed.

Logs and process evidence: `/tmp/unified-extension-validation/results.json`,
`nextest.log`, `nextest-initial.log`, `leak-recheck.log`, `dependencies.json`,
`http-e2e/report.json`, and `native-process-report.json` under the same directory.
The native process runner used `/tmp/regex-native-e2e.py`; its captured host log
is `/tmp/regex-native-e2e/host.log`.

The public facade currently lives in the product host crate, with registration
functions in custom-host integration modules. Reusable matcher/business code
still depends only on the lightweight capability contract. No independent
extension SDK, evaluator/selector entry, universal registry or remote process
manager is part of this increment. Legacy API removal remains a future explicit
breaking change; the current delivery is one recommended author entry with a
bounded compatibility path.

## Compile-only SDK extension consolidation (2026-09-18)

This section supersedes the Native/HTTP deployment and protocol expectations in
prior sections. The current contract is ROUTER_EXTENSION_SPEC v0.6.

Delivered changes:

- SDK `extension::ExtensionApi` is the actual author entry; no extra API crate.
- Callback Input/Decision contain business data, not a wire request/response.
- HTTP checker service, checker-protocol crate, probe CLI/management routes and
  dedicated service release artifacts are removed.
- Existing `native.revision` config is preserved; former HTTP fields explicitly
  block migration, including null or mixed Native/HTTP declarations.
- Router binding, process-local receipts, resource limits and fail-closed behavior
  remain host-owned. Timeout/cancellation cannot terminate started synchronous
  code or free its concurrency slot early.
- Legacy SDK Plugin/global/output/stream/migration semantics remain available;
  they are not falsely described as replaced by an input-only capability.
- Default host excludes the matcher; a custom host explicitly links and registers
  it. Agent skill, CLI references and all three agent-plugin manifests agree.

Independent SDK and host reviews found no blocking correctness regression. The
host tests additionally verify ignored registration errors and missing/mismatched/
extra registrations fail before database creation.

Validation logs: `/tmp/compile-only-extension-validation/`. Process artifacts:
`/tmp/native-extension-e2e/`. These paths are local evidence, not release artifacts.

| Check | Current result |
| --- | --- |
| Host all-feature, all-target check | Passed |
| SDK no-default + config_file focused tests | 15 passed (before final receipt-field cleanup; full suite below covers final behavior) |
| Workspace all-feature nextest | 3,472 passed, 22 skipped; no failed or leaky tests |
| Formatting | Passed |
| Strict clippy (all targets/features, warnings denied) | Passed |
| Doctests / strict rustdoc | 5 passed, 1 ignored / passed |
| SDK minimal + config_file, matcher minimal | Passed |
| Generated schema / distribution consistency | Passed |
| Default host dependency tree | Matcher and removed protocol/service absent |
| Native real-process scenarios | 6 passed with freshly built example; exactly 3 counted mock upstream calls |

The full test build reported macOS linker compact-unwind size warnings and a
third-party `proc-macro-error2` future-compatibility warning. Tests completed
successfully. No live provider, production rollout, remote CI or release claim
is implied by local verification.

The completed local process run covered allow, deny without added upstream calls, an unbound
router, streaming allow, revision mismatch and rejected HTTP configuration.
It did not start the daemon management socket; local/remote management and
receipts are covered by Rust integration tests. The temporary Python driver and
its CI job were removed at the user's request; these results remain historical
local evidence, not a maintained process-test job. Remote CI has not been run
for this unpushed change.

## Internal state and runner simplification (2026-09-18)

The next slice preserves the five configuration/execution/evidence distinctions
while removing duplicate internal representations:

- Receipt admission order now drives newest-first listing and request-id lookup
  directly; redundant sequence counters and sorting are removed. Exact receipt
  lookup, retained-match counts, active-record retention and latest-started
  evidence semantics are unchanged.
- Each active checker owns its running bindings once. Management inventory is
  projected from those bindings and current receipts, including repeated bindings
  and configured-but-unbound registrations.
- The host runner accepts the frozen binding, business `Input` and progress
  reporter. `CheckerInvocation` is removed; request/router/invocation identity
  remains in the receipt. `CheckerResult { decision, revision }` replaces the
  duplicated `CheckerDecision` enum. The pipeline applies the business decision
  validation once; callbacks cannot attach revisions or mutate receipts.
- Custom `RequestCheckerRunner` implementations require a Rust API migration.
  Extension callbacks, configuration and management JSON remain unchanged.
  Legacy hook ordering and reload contracts are outside this slice.

Validation on the final source state (logs in
`/tmp/extension-simplification-validation/`):

| Check | Result |
| --- | --- |
| Workspace all-feature nextest | 3,472 passed, 22 skipped |
| Strict clippy, all targets/features | Passed |
| Formatting / diff whitespace | Passed |
| SDK minimal / config_file / matcher minimal | Passed |
| Workspace doctests | 5 passed, 1 ignored |
| Strict workspace rustdoc | Passed |

Expanded existing regressions cover reversed completion order with duplicate
request IDs, bounded newest-first lists, duplicate bindings shared across routers,
receipt-owned original identity, and malformed runner decisions stopping selection
and upstream execution in streaming and non-streaming paths. Existing cancellation,
permit ownership, eviction and no-stale-allow tests remain passing. These are local
checks; no new standalone process smoke run or remote CI result is claimed here.

## Shared foreground host and author API (2026-09-18)

This implements S1–S3 of `HOST_EXTENSION_DX_SPEC.md` v0.2 in the local worktree:

S3 supersedes the earlier extra-registration rejection reported above; those
earlier test results describe their own source snapshots.

- `bitrouter::host::serve_with_extensions` owns the existing foreground lifecycle.
  Official `bro serve` passes an empty registry; the regex example registers its
  implementation and calls the same host. Low-level assembly remains available.
- Six fragment/coverage author types now live in SDK `extension::request_check`.
  Old `language_model::request_checks` paths have no aliases; JSON stays unchanged.
- Valid unconfigured registrations produce sorted inactive-ID diagnostics, with
  no runtime entry, execution slots or inventory row. Configured instances still
  require a matching registration even without bindings. Registration errors
  remain sticky and fail before database assembly.

Extraction exposed two startup defects: readiness could precede listener binding,
and partial startup failure could leave PID/socket artifacts. The shared host now
binds every required listener before readiness and uses ownership-aware cleanup.
Existing telemetry/outbox shutdown also runs after post-assembly startup failures.
Unix socket ownership uses a stable `.sock.lock` file; the OS lock releases on
exit, while the file intentionally remains. It is not daemon-status evidence.

Independent review additionally caught unsafe PID-file truncation and Unix
cleanup tests under a Windows-only module. Both were fixed: PID creation rejects
foreign nonregular/invalid/live records and symlinks, creates exclusively, and
reclaims only a stale numeric PID; cleanup checks the owned record. Unix liveness
uses the existing rustix dependency and treats permission errors as live. Tests
now compile under the intended platform.

Maintained real-process tests are Rust-only in
`apps/bitrouter/tests/extension_host.rs`. They launch an isolated custom host test
binary and genuine `bro` subprocesses, use real TCP/Unix sockets and a counted
local mock provider, and cover:

- H01–H03: eight stream/nonstream allow, deny, timeout and invalid-result requests;
  exactly two upstream calls, matching local/remote receipts and incarnation,
  unused-registration exclusion, no initial actual usage, remote authentication.
- H04: saved revision changes reject reload, report restart-required and retain
  the running revision.
- H05: missing/duplicate/mismatched registrations precede DB creation; inference
  and remote-control port conflicts release owned endpoints. Unit tests also
  preserve foreign socket/PID files and exercise stale/replaced paths.
- H06: management stop, same-custom-binary restart with a new receipt incarnation,
  and SIGTERM cleanup. Existing shutdown tests cover pending settlement drain.
- H07: official `bro serve` with no checks, direct inference, `bro checks` and
  `bro stop` over the same shared host.

This is local macOS Unix process validation with mock inference, not paid/live
model validation or cross-platform execution. No Python harness was added.
Custom hosts remain foreground-only unless they implement their own launcher;
official `bro restart` must not replace their executable.

Reproduce process acceptance with:

```sh
cargo test -p bitrouter --test extension_host --all-features -- --nocapture
```

Validation logs for this slice live in `/tmp/host-extension-dx-validation/`.

| Check | Result |
| --- | --- |
| Workspace nextest, all features | 3,494 passed; 22 skipped |
| Rust real-process suite | 4 passed: three acceptance tests plus the subprocess entry test |
| Strict workspace clippy, all targets/features | Passed |
| Formatting / diff whitespace | Passed |
| SDK minimal / config_file / matcher minimal | Passed |
| Workspace doctests | 5 passed; 1 ignored |
| Strict workspace rustdoc | Passed |
| Generated config schema / registry consistency | Passed |
| Default host normal/build dependency tree | Matcher and removed protocol/service/API crates absent |

After the full suite, collision tests were tightened to require the specific bind
error and absence of readiness output. The process suite, its strict clippy and
workspace formatting were rerun successfully; production source did not change.
The suite includes the S2 author-input tests and S3 unused-registration,
sorted-diagnostic, sticky-error and configured-unbound regressions. Existing
identity, receipt, permit, reload and legacy-hook tests remain passing.

The build emitted the existing macOS compact-unwind size and third-party
`proc-macro-error2` future-compatibility notices. No check suppression was added.
These results do not represent remote CI, a merge, cross-platform process tests
or a published release.
