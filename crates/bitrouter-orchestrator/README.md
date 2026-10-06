# BitRouter Orchestrator source guide

This crate implements the native BRO coding-agent runtime and the external-agent
ACP stack. The application owns host transport and database assembly; the SDK
owns model routing and provider execution.

## External agents over ACP

Enable the optional `acp` feature to use `bitrouter_orchestrator::acp`. Native
runtime consumers can leave it disabled to avoid the ACP conductor and its
trace viewer. `apps/bitrouter` enables it for existing ACP commands and sessions.

| Module | Responsibility |
| --- | --- |
| `acp/controller.rs` | Manager-facing ACP server, lifecycle forwarding, route controls and attributed cost |
| `acp/client.rs` | Shared client for a child agent or an in-process controller, updates, permissions and cancellation |
| `acp/up.rs` | Agent-process transport, initialize-only health checks and confirmed process cleanup |
| `acp/capture.rs` | Capture port and protocol events; the app supplies durable storage |
| `acp/translate.rs`, `acp/telemetry.rs` | Typed session updates, NDJSON contract and context usage |

The app injects route/cost services and retains CLI/UI adapters. This extraction
does not attach external ACP sessions to native `ThreadService` execution. Agent configuration data remains in
`bitrouter_sdk::config::agent` beside the shared `Config`, without ACP runtime
dependencies.

## Native ACP ingress

`acp/native/` implements negotiated ACP v1 and draft v2 over one existing
`ThreadService`: stable native IDs, bounded history/live projection, permission
reattachment and durable cancel/close controls. The app supplies an authenticated
local caller, host-authorized immutable resources and an OS-local stdio bridge.
EOF affects observation only. The core owns single-Thread closure and records
cancelled queued Turns before joining active cleanup; completion retries do not
cancel newer work. Runtime format 5 retains supported 2/3/4 reads.

See [the native ACP contract](../../docs/BRO_NATIVE_ACP_SERVER_SPEC.md) and
[local acceptance](../../docs/BRO_NATIVE_ACP_SERVER_ACCEPTANCE.md). External
client/controller paths remain v1; native v2 is pinned to the SDK's draft schema.

## Read the contracts first

| Module | Responsibility |
| --- | --- |
| `thread.rs` | Durable conversation settings, status, directory, recovery inspection and Thread history contracts |
| `turn.rs` | One accepted input: status, receipt, snapshot, lifecycle, approval, steering and verification evidence |
| `item.rs` | Stable BRO call identity, provider call correlation and bounded live Item presentation |
| `store.rs` | Authoritative execution facts, versioned commits, ownership and bounded durable queries |

`item` does not introduce a second message model. SDK `Message` remains the
model-content contract. A tool call has a stable BRO Item ID as well as the
provider's call ID; these identities serve different purposes.

Public types are imported from their owning module, for example
`turn::TurnSnapshot` and `item::CallRecord`. There are no compatibility re-exports
from `service`, `thread` or `store`.

## Follow one execution

1. `service/threads.rs` creates or loads the Thread and checks its caller and grants.
2. `service/admission.rs` accepts a keyed input and commits its Turn identity.
3. `service/runner.rs` drives the Agent while handling commit, approval, model-boundary
   and live-output channels.
4. `agent.rs` runs the model/tool loop. `agent/stream.rs` collects provider output;
   `agent/batch.rs` schedules bounded read groups and exclusive tools.
5. `tools.rs` declares, validates and dispatches the six tools. Implementations live
   in `tools/{read,glob,grep,write,edit,shell}.rs`.
6. `service/verification.rs` runs the optional configured check.
7. `service/commit.rs` commits settlement and the public projection through the same
   Thread authority; `service/queue.rs` advances accepted FIFO work.

`harness/instructions.rs` loads global and project-root-to-working-directory
instructions on the first active execution of a live Thread. Each directory
selects `AGENTS.override.md`, `AGENTS.md`, or a configured fallback filename.
The body enters durable user context; `context.rs` adds the scope and precedence
policy to system instructions. The model reads deeper directory rules itself.
Thread snapshots survive Turns; a new server session refreshes them at a safe
model boundary with an explicit replacement or removal message. MCP inventories
retain their independent frozen-binding check. Cold browsing reads no instruction files.

`service/state.rs` holds live Thread/Turn records and shared resources. Splitting
methods into files does not create additional state owners, runners or commit
protocols. Each Thread retains one version and commit lock. Methods named `*_serialized` expect their callers to acquire the commit gate
before entry.

Steering enters at `service/steering.rs` and the model boundary in `control.rs`.
Recovery, store ownership and workspace exclusion remain in
`service/{recovery,ownership,workspace,startup}.rs`. Reading or loading stored
state does not by itself authorize replay or continuation.

## Test organization

Tests needing private runtime state remain child modules under `src/`, loaded
with `#[cfg(test)] mod tests;`. A separate source file does not make such a test
an external integration test.

- Agent and tool suites live in `agent/tests.rs` and `tools/tests/mod.rs`.
- Service behavior suites live in `service/tests/`, indexed by `service/tests.rs`.
- Shared service fixtures, request constructors and synchronization helpers live
  in `service/tests/support.rs`; suites do not borrow helpers from other suites.
- Focused workspace/reconstruction tests remain beside those implementations in
  `service/workspace/tests.rs` and `service/recovery/tests.rs`.
- Store suites live under `store/`.

Crate-root `tests/` is for tests that exercise only public APIs. App transport,
database assembly and PTY tests remain with `apps/bitrouter`. Do not expand public
visibility just to move a private-state test into an integration-test crate.
