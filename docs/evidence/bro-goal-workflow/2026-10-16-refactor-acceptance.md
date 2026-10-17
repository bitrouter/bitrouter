# BRO execution refactor acceptance

Scope: approved refactor spec v0.2, RF01–RF12 / A01–A22. This refactors BRO execution while retaining existing GoalStore, ExecutionStore, native checkpoint and supervisor facts. Future core persistence, complete Turn-fixed routing, dynamic graph edits and Goal owner handoff remain deferred.

## Evidence levels

- Final workspace regression, strict Clippy, fmt, doctests and generated-artifact check: see the adjacent machine-readable source binding and result summary.
- Native: actual SDK requests, tool execution, SQLite persistence, checked dependency handoff, measured content-reception overlap and controlled cold checkpoint restoration. Model answers and semantic judgments in those tests are fixtures.
- ACP: Cargo-built Rust protocol processes, actual private nonce persistence/load, shared admission/permission/stop, replacements and unknown-effect refusal. Confirmed same-session continuation currently covers a complete journal before the first prompt intent, after actual source shutdown/join. It preserves Thread/Turn and cumulative duration, loads the original private session and submits the original input once. Possibly submitted prompts, uncertain startup, incomplete history and unsupported loading remain blocked.
- Actual vendor harness/provider requalification on this refactored source: **live gate unpassed**. Earlier R16/provider/input campaigns bind their original source; they are not evidence of this new engine's live qualification. Fixtures do not certify all harnesses, billing limits, model quality or private context.
- Hosted CI is read at the exact final PR head separately from local validation.

## Acceptance mapping

| IDs | Implemented behavior and local proof |
| --- | --- |
| A01, A21 / RF01 | Existing stores/checkpoints, compatible optional associations, explicit writer fence and old-record history; cold reads do not dispatch. No new core backend or state-authority migration. |
| A02, A16 / RF10 | Stable per-action keys, changed-body rejection, accepted/link gaps, missing-acceptance reconciliation, owner/source/version fences and no replay after unknown acceptance. `goal_runtime::tests::threads`, `start_recovery`, shared service/recovery tests. |
| A03 / RF02, RF10 | Native, generation and managed ACP use ThreadService admission/runner/commit/control. Supervisor is the ACP process/protocol adapter. Shared ownership/takeover rejection tests and source audit; old unbound records retain read/reconciliation consumers. |
| A04 / RF04 | Goal owner and input Threads; worker Threads reference the same Goal/work source. Ordinary delegation does not create Goals or finish the root. Owner-generation, native and managed-ACP tests. |
| A05 / RF05 | Thread config/selector is the policy scope; workers may reuse selectors. Requirements, prepared input and reports retain their versions. No new Policy registry or complete Turn-fixed core guarantee. |
| A06, A07, A18 / RF06 | Frozen MCP preparation hashes, actual SDK reports, supplied-versus-adopted distinctions, bounded evidence and explicit unknowns. Core stepwise context/fallback remains; private ACP context/provider is uncontrolled. Resource-edit, MCP-binding, SDK/native evidence tests. |
| A08 / RF08 | Workflow combines work dependencies, owner/child links, execution references and handoffs. Relationship semantics are retained; submission order creates no dependency edge. Domain workflow tests and read-only projection. |
| A09, A10 / RF08, RF11 | Worker final text alone cannot unlock dependents; current checks/review and content-bound handoff fix actual material versions. Native/ACP workflow integration and evidence tests. |
| A11, A12 / RF08, RF09 | Versioned requirement/worker changes invalidate affected results, preserve independent branches and keep original work/attempt history. `workflow_edits`, archival/native-history and dependency tests. |
| A13 / RF03 | Native independent branches overlap measured reception intervals; conflicting relative writes use isolated workspaces and join consumes checked outputs. `native_branches_overlap_isolate_conflicting_local_writes_and_read_checked_dependencies`; ACP workflow tests separately. |
| A14, A15 / RF07, RF09 | Confirmed pre-prompt same-session load retains Thread/Turn; worker replacement uses a new Thread after old stop/effects reconciliation. Five-case SQLite/Rust-process matrix, lost restore-start journal guard, original-owner stopped-history checks and worker-switch tests. Unknown effects block conflicting replacement. |
| A17 / RF11 | Original Enter input/operation, saved requirements, accepted work, actual adoption and verification have distinct receipts. Current Goal is default; System One proposals still escalate to reasoning, runtime commits final authority. Input and PTY tests. |
| A19 / RF10 | Shared permissions, revocation, shutdown, roots/grants, capacity, cumulative budgets and unknown-usage holds. Native quote waits retain original frozen actions; cancellation before executor polling records no-dispatch only for that call. Budget/SDK/control and recovery matrices. |
| A20 / RF12 | Verified reviewable default delivery; explicit human acceptance, limits, blockers, pause and stop remain distinct. Completion/repair/budget tests. |
| A22 / RF12 | Goals/current Goal/read-only Workflow, direct native shortcuts and transparent ACP entry retained; retired `bro code <agent>` stays rejected. UI detach/reconnect/outbox and CLI tests. |

The mapping states the implemented local scope. It does not turn design directions, historical live runs, model claims or platform-unrun tests into verified facts.

## Reproduce

```sh
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 \
  BRO_GOAL_FIXTURE_KEEP_FAILURES=1 cargo nextest run --workspace --all-features --no-fail-fast --test-threads 2
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt -- --check
cargo test --workspace --all-features --doc
cargo run -p dist-helper -- check
```

For the focused ACP matrix, select `test(acp_pre_prompt_recovery_loads_only_the_original_stopped_session)` in the bitrouter library. Tests build their Rust peers through Cargo; no Python interpreter is required. Failure-retention output identifies the filesystem/SQLite directory. Such copies are not process images or proof of arbitrary crash takeover.

Raw failures, exact source maps/tars, configurations, runners, fixture images, extraction audits and recoverable Git bundles belong to the independent refactor archive. The adjacent final summary pins its immutable commit and checksum manifest. No raw multi-round log is added here.

Final current-source verification is `fixture-image-final123`: native/ACP feature isolation, ACP-free native tree, Rust1.93, three repeated mock ACP batches, four Goal PTY cases, two typed startup guards and one focused native parallel case, complete4757 workspace tests, strict Clippy/fmt/doctests and generated artifacts. Two outer test slots preserve all within-workflow concurrency and the strict output-leak detector.

Linux CI previously enumerated 26/36 integration targets; the native-goals shard now covers the other ten, with one build job and zero retries. Its first execution exposed a PTY fixture startup defect: asynchronous shell stdin initialization preceded the fixture's <&0. The fixture now saves the PTY descriptor before background launch, reconnects it explicitly and closes the spare descriptors. Product terminal checks, test assertions and time bounds are retained. [POSIX shell semantics](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/V3_chap02.html) explains the descriptor ordering. Actual Linux/other-platform qualification is read at the final pushed head separately.

Initial archives remain pinned independently; the final summary pins the latest local-source annex, and the PR acceptance comment pins the later hosted-CI archive. Original failures, incomplete runs and source applicability are retained. Hosted temporary filesystems were not exported and no reconstructed hosted fixture image is claimed.

The next Linux head passed the active Goal scenario, then exposed premature terminal reacquisition during suspension. The shared helper now installs a SIGCONT listener before SIGSTOP and awaits continuation before all four UI callers restore terminal modes. The PTY case additionally rejects bracketed-paste re-enablement before explicit continuation. Terminal-state assertions and time bounds are unchanged. Linux matrix jobs collect all failures with --no-fail-fast, retaining target selection and retries. Original Linux failure and the intermediate compile failure remain archived. Final exact-head hosted results remain separate from macOS local proof.

Unix input-source follow-up: Linux d0da ran all212 cases:210 passed,2 failed,6 skipped. Terminal suspension assertions passed; resize stranded keyboard input and another case timed out on permission submission. The existing macOS pollfd/libc event-source features are now enabled for Unix, including Linux. No Rust implementation/test bytes changed from full77, and the macOS all-feature dependency tree before/after is byte-identical. `unix-tty-focused88` passed four Goal PTY cases, feature/MSRV checks, strict Clippy/fmt, doctests and generated artifacts. The4,757-case full regression remains explicitly bound to full77; this is not a fresh full88 run. Actual Linux qualification follows at the final pushed head. All original assertions, waits and retries remain.

DAG-stage follow-up: exact b32fc Linux native-goals212 passed (6 skipped,zero retries), including all four PTY scenarios. Linux library then failed at the45s handoff wait after left/right handoffs and join Turn completion, before join review/handoff. The test now independently bounds concurrent upstream handoffs, dependent join handoff, and root acceptance at45s each. Aggregate fixture allowance increases from90 to135s; all overlap/isolation/checked-dependency/delivery assertions and product budgets remain. Lib/bin CI collects all failures without new retries. Current full123 supersedes focused88 for final local acceptance; full99 remains a failed attempt; actual next-head hosted results remain separate. Original failure log is retained; hosted failing filesystem images were not exported.

Fixture image follow-up: full99 had4756pass/1pre-protocol ENOENT and is unqualified; intermediate111 failed compilation before tests. Cargo public executable sampling observed921 missing-path checks/161inodes across160 fresh builds, but direct spawn probes did not reproduce the original errno. Original causality remains inferred. The test-only shared builder now serializes build/publication and atomically publishes byte-verified immutable content-addressed images; all fixtures remain Cargo-built Rust. Current full123 qualifies the final source. The post-fix image probe observed zero path gaps,one inode and zero OS-spawn errors during160 public-artifact refreshes; the start probe intentionally omits protocol control env and does not assert successful protocol completion. No assertions,retries or runtime logic were changed by this fixture fix.

Independent immutable archive: [a5b49254](https://github.com/bitrouter/bitrouter/tree/a5b49254addd8d75e61ac915b6a3e9f69a23258e), retaining 4259 original files (90797722 bytes). Its SHA256SUMS digest is `b3f98679688d229e175c14bef43eca935e3073f91b8bd614435a4457e903d842`. Nested fixture directories are packaged intact; PAYLOAD.json, CORRECTION_PAYLOAD.json, LINUX_PTY_PAYLOAD.json and TERMINAL_CONTINUE_PAYLOAD.json UNIX_TTY_PAYLOAD.json and FIXTURE_IMAGE_PAYLOAD.json map original bytes/modes/hashes. All 1418 published files were verified from an independently extracted Git tree. The source checkpoint is `3de44d2c3baaf6618f9769da43d906514e9240bf`; final publication changes only explicit documentation/skill/plugin metadata, with executable and test inputs unchanged from complete full123.
