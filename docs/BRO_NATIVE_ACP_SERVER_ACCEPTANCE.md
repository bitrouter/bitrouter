# Native BRO ACP local acceptance

Date: 2026-10-06. Scope: ACP migration and native ingress stacked on
[PR #945](https://github.com/bitrouter/bitrouter/pull/945), validated locally in
the `86fc` worktree. This record supplements
[the contract](BRO_NATIVE_ACP_SERVER_SPEC.md); it does not claim a published
release, hosted CI, Windows/Linux process validation or real IDE conformance.

## Implemented surface

`bitrouter_orchestrator::acp::native` negotiates v1 and draft v2 against SDK and
Conductor **3.0.0**, schema **1.10.2**. `bro acp serve` bridges OS-local stdio to the
host's existing ThreadService. Explicit `bro acp serve <agent>` keeps the external
controller path. There is no new scheduler, provider loop or ACP content store.

Native session IDs, user message IDs and tool IDs retain Thread/Item identities.
v1 completion waits for native settlement and worker exit; v2 acknowledges durable
acceptance before later updates. Native approvals retain their original request;
new ACP connections replace delivery generation, while native input controls
remain authoritative. Transport failure or a cancelled dialog never approves,
rejects or business-cancels a native input.

Cancel preserves a durable pause and unstarted inputs. Close commits all queued
cancellations plus active cancel intent under a single Thread fence, drops global
admission before joining workers and records completion only after known cleanup.
Same-key close retries return the stored result without waiting for or cancelling
newer work. Completed close retires ACP attachments, including after a lost reply.
History and keys remain. `session/resume` only reattaches; queue resume is explicit.

Format **5** adds private immutable Thread MCP bindings and keyed close records.
Formats **2/3/4** remain readable; appends update the root envelope atomically.
Requested MCP descriptors must match host authorization exactly. Cold operations
perform no discovery. The native Harness continues to own tools, MCP processes,
skills and AGENTS.md. Invalid prompt/MCP collection members fail before admission
rather than being discarded by the SDK's permissive deserializers.

## Local fixture evidence

The focused native suite contains **14 passing tests** in
[wire/session tests](../crates/bitrouter-orchestrator/src/service/tests/native_acp.rs)
and [close tests](../crates/bitrouter-orchestrator/src/service/tests/close.rs).
The controlled fixtures establish:

| Contract | Observed evidence |
| --- | --- |
| Both wire versions | Initialize/new/list/prompt/reopen/permission/cancel/close, v2 acceptance message ID, cross-version history and stable entity IDs |
| Unexpected disconnect | Observable held provider request continues after EOF; no native cancel fact is added; reconnect restores the same input and tool identity |
| Approval generation | Both version directions reject an old connection's late allow answer; current reject resolves once without writing the file |
| Cancel and queue | Paused accepted inputs survive reconnect; ordinary prompt cannot overtake them; only explicit queue resume activates retained work |
| Close | Active approval plus two accepted queued Turns are durably cancelled; queue-only close records withdrawal without activation; known worker/workspace ownership is released |
| Close retry and lost reply | A completion-commit gate proves accepted close outlives EOF; another Thread executes while closure waits; duplicate close commits one result; an old key leaves newer in-flight work running |
| Restart and failure | Closed SQLite history is readable after daemon restart without model replay; failed close completion preserves its intent and a recovery blocker |
| Unknown effects | A proven dispatched shell is joined on close, but uncertain effect status blocks successful close completion and retains evidence |
| History and output | UTF-8 streams reconcile once in v1, canonical v2 messages preserve IDs, requested replay exceeds a two-event hot cache; stalled output disconnects observation without cancellation |
| Invalid admission | Ungranted cwd, unauthorized MCP, unsupported media and oversized prompts admit no model work |

The [product subprocess test](../apps/bitrouter/tests/native_acp.rs) passes on
macOS. It launches a real `bro serve`, exchanges JSON-RPC over real
`bro acp serve` stdin/stdout, exits that bridge while approval is pending,
reconnects with the other version and executes one approved write. It verifies
settled v2 state, exactly two routed fixture model calls, explicit close and
SQLite history after a clean daemon restart. Stdout is parsed strictly as ACP.
The provider is a local SSE fixture, not a credentialed service.

Output production uses native byte limits for updates and responses, input
frame bounds, a 64-operation cap and a five-second write/reservation stall
interval. These are transport detach boundaries; native deadlines and explicit
cancel/close remain separate. No approval-disconnect idle timer was added.

## Workspace validation

Initial implementation checks on macOS used SDK/Conductor 2.2.0 and schema
1.9.1. They passed with `CARGO_PROFILE_DEV_DEBUG=0`,
`CARGO_PROFILE_TEST_DEBUG=0`, and `CARGO_INCREMENTAL=0`:

| Check | Result | Local log |
| --- | --- | --- |
| `cargo nextest run --all-features --no-fail-fast` | 3,761 passed; 22 skipped | `/tmp/bitrouter-native-acp-nextest-verified.log` |
| `cargo test --all-features --doc` | 5 passed; 1 ignored | `/tmp/bitrouter-native-acp-doctest-verified.log` |
| `cargo clippy --all-features` | Passed; no new source warnings | `/tmp/bitrouter-native-acp-clippy-verified.log` |
| `cargo fmt -- --check`, `git diff --check` | Passed | Direct command output |
| SDK all-feature dependency tree | No ACP runtime dependency | `/tmp/bitrouter-native-acp-sdk-tree.log` |
| Local links, plugin manifests, unrelated baseline edits | Valid links/JSON; baseline route/command edits preserved | Direct validation output |

During validation, stale required-agent help and a format-4 fixture assertion
were updated for the new CLI/format. Long-history fixtures explicitly use the
native runtime's larger context limit; the product AgentConfig default remains
unchanged. The slow-output fixture exposed the missing physical write stall
boundary, which was added before the final passing run. One final link attempt
ran out of disk space; removing only this worktree's regenerable incremental
build cache freed 3.6 GiB, and the exact final workspace command then passed.
The existing macOS large-unwind linker and proc-macro-error2 future-compatibility
notices remain; they did not prevent the final checks.

The focused fixture log is `/tmp/bitrouter-native-acp-focused-final.log`; process
proof is `/tmp/bitrouter-native-acp-process-tests.log`. Local `/tmp` paths identify
run provenance and are not committed artifact downloads.

## Schema 1.10.2 upgrade validation

The same macOS worktree now pins schema **1.10.2** and SDK/Conductor **3.0.0**.
The SDK upgrade is required by its exact schema dependency. Native `process`
and `stdio` features are explicit; the initialize metadata reports schema
1.10.2. The existing lifecycle handlers compile without further API changes.
The initial implementation logs above remain evidence for the earlier pair.

The upgrade was revalidated with the same build environment:

| Check | Result | Local log |
| --- | --- | --- |
| `cargo check --all-features` | Passed | `/tmp/bitrouter-acp-schema-check.log` |
| `cargo nextest run --all-features --no-fail-fast` | 3,761 passed; 22 skipped | `/tmp/bitrouter-acp-schema-nextest.log` |
| `cargo test --all-features --doc` | 5 passed; 1 ignored | `/tmp/bitrouter-acp-schema-doctest.log` |
| `cargo clippy --all-features` | Passed | `/tmp/bitrouter-acp-schema-clippy.log` |
| `cargo fmt -- --check`, `git diff --check` | Passed | Direct command output |
| SDK all-feature dependency tree | No ACP runtime dependency | `/tmp/bitrouter-acp-schema-sdk-tree.log` |

The full suite includes both wire versions, replacement approval ownership,
EOF during model work and close, queue pause/resume, slow output and the real
`bro` stdio/restart fixture described above. External controller/client suites
also pass. The pre-existing linker and future-compatibility notices remain.
Local links are valid and initial unrelated worktree edits are preserved.
The external gates below remain open.

## Stacked PR source validation

Before publishing the ACP branch on top of PR #945, the two unrelated routing
files were saved separately and excluded from the commit. All required checks
were rerun against the clean ACP source tree, then those routing edits were
restored byte-for-byte. The source tree has one fewer test than the earlier
worktree runs because the unrelated route-discovery test is excluded.

| Check | Result | Local log |
| --- | --- | --- |
| `cargo nextest run --all-features --no-fail-fast` | 3,760 passed; 22 skipped | `/tmp/bitrouter-native-acp-pr-nextest.log` |
| `cargo test --all-features --doc` | 5 passed; 1 ignored | `/tmp/bitrouter-native-acp-pr-doctest.log` |
| `cargo clippy --all-features` | Passed | `/tmp/bitrouter-native-acp-pr-clippy.log` |
| `cargo fmt -- --check` | Passed | `/tmp/bitrouter-native-acp-pr-fmt.log` |

The native v1/v2 lifecycle tests, external ACP suites and real stdio/restart
fixture pass on this source tree. Only this documentation record was added
after validation; no runtime or test source changed.

## Remaining external gates

- Real IDE behavior against the pinned draft v2 schema, especially cancelled
  idle followed by requires-action for a paused queue, remains unverified.
  Raw wire fixtures consume the final state and prove no queue activation.
- Windows named-pipe/process cleanup and Linux process behavior are not measured
  in this macOS run. The host reuses existing platform transport authorization.
- Credentialed providers, credentialed remote MCP and hosted CI are unverified.
  Existing MCP/process/recovery tests remain local controlled evidence.
- Abrupt power loss and operator repair of an unconfirmed owner/effect or an
  unfinished close remain native recovery boundaries, not automatic ACP resume.
- Remote ACP, fork/subagents, MCP-over-ACP, client file/terminal callbacks and
  mutable model/settings controls are outside this release.

Unrelated initial routing changes are excluded from the ACP commit and preserved
locally. CLI docs, the shipped skill and all three plugin manifest descriptions
reflect the native entrypoint; no origin MCP is registered.
