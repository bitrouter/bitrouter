# Guardrails extension: implementation and acceptance

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
