# BRO review corrections and simplification

Follow-up to source `8a44c6b1fc8a7d580de82412ab821e46c07c5d76` and the two independent implementation/testing reviews. The approved v0.2 scope and existing storage/execution authority remain unchanged. Workflow remains read-only.

## Changes

- Input classification and routing assessment construct their own questions, then use the same `DecisionRuntime::execute_controlled` admission, cancellation, bounded-outcome and acknowledgement path. Cancellation retains the executor's usage outcome; the input path no longer duplicates cost conversion. A paused Goal can still process explicit input.
- `ModelControl` retains only the original Goal ID and requirements revision. Current source, lease, authorization and budget are still reread at the existing gates.
- Permission projection reuses the same checked JSON within one operation, retaining raw optional fields. Run/request/hash validation, late-change rejection and checks across asynchronous delivery boundaries remain separate.
- ACP recovery waits for the restored prompt's existing permission observation before reading the private prompt counter. The peer writes that file before emitting the request, so no new file protocol, ignored parse errors or retry is needed.
- Four PTY scenarios share a concrete daemon start/stop helper. Normal shutdown requires an OK response, successful wait and exit 0. Failed readiness and shutdown timeouts still kill/reap the owned process; the original scenario failure takes precedence.
- Two real client IPC setups share a local fixture. Three pure projection tests use an explicit snapshot instead of opening/migrating SQLite. Actual database receipt tests, durable commit/projection comparisons, all entry combinations and complete terminal journeys remain.

## Current local proof

| Check | Result |
| --- | --- |
| Model-control, permission, UI/client and SDK controlled-decision regression | 33 passed |
| ACP pre-prompt recovery matrix plus actual Goal PTY | 5 tests passed: one five-case ACP matrix and four PTY scenarios |
| Workspace/all-feature nextest, two outer slots, no retries | 4,758 passed, 24 skipped; 1,037.049 seconds; no reported LEAK |
| Workspace/all-target/all-feature Clippy with `-D warnings`, fmt and generated artifacts | Passed |
| Workspace doctests | 6 passed, 1 ignored |
| SDK default features and orchestrator without ACP features | Both builds passed |

The new typed regression changes actual GoalStore state while a controlled executor is in flight. Requirement/budget invalidation cancels and settles reported usage once. Owner transfer cancels the call but refuses the old owner's outcome commit, retaining the original unknown-cost hold. A paused Goal's explicit input completes normally. The backend is a fixture; no actual vendor billing is attested.

The source manifest covers 999 executable, test, build and CI inputs. Its SHA-256 is `70c49f6ecf71312e1ae078f19d30a9e6ebfb28593af0d1b8d13b2958d21f7177`; no input drift occurred between focused checks, full regression and final static/default-feature checks. Exact-head hosted CI is recorded separately in the final delivery evidence.

Implementation files have a net reduction of 14 physical lines. Test setup cleanup is offset by the new cancellation regression; all Rust changes together are +166 lines. These counts include comments/blank lines and do not claim measured performance improvement.

## Evidence and limits

Original compile failures, the failed intermediate projection/cancellation fixture run, available source patches, passing command logs, the verification runner and three immutable Cargo-produced Rust fixture files belong to the independent archive. The [existing PR acceptance comment](https://github.com/bitrouter/bitrouter/pull/981#issuecomment-6224234670) pins the latest immutable annex and hosted results. Previous archives remain available at their original commits.

The private-file race was identified from the concurrent read/write paths; this correction does not claim reproduction or explain an earlier ENOENT/CI failure. Retained executable files are local file snapshots, not process/memory images or hosted temporary filesystem exports.

Actual vendor harness/provider requalification remains unpassed. Future core persistence, complete Turn-fixed routing, graph editing and Goal owner handoff remain deferred. Admission-wait predicate refactoring, entry-journey reduction and global test-pressure changes are outside this follow-up.

Reproduce the full suite with the existing [instructions](REPRODUCE.md). Focused checks use:

```sh
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 \
  cargo nextest run -p bitrouter -p bitrouter-sdk --lib --all-features --test-threads 2 \
  -E 'test(goal_runtime::model_control::tests::) | test(goal_code::tests::) | test(goal_permission_material::) | test(app_routing_decision_requires_) | test(routing_cancellation_settles_)'
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 \
  cargo nextest run -p bitrouter --lib --test goal_tui_pty --all-features --test-threads 2 \
  -E 'test(acp_pre_prompt_recovery_loads_only_the_original_stopped_session) | binary(goal_tui_pty)'
```
