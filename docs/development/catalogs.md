# Agent and runtime catalogs

This development guide follows the current implementation. Update it with
changes to ownership, adapters and runtime behavior. Format compatibility is
maintained in the [specification index](../specs/README.md).

Agent sources in `registry/agents/` describe an agent's ID, ACP version and
routing behavior. Runtime sources in `registry/runtimes/` describe available
agent invocations. Build output includes `dist/registry/agents.json` and
`dist/registry/runtimes.json`, each with a `data` array. Agent IDs are bare;
`runtime/agent` addresses an agent via a runtime, and `local/` may be elided.
An agent's presence in the catalog alone does not mean a runtime can launch it.
Runtime entries may include agents outside the curated agent catalog.

Current runtime entries use stdio transport with a command and arguments. The catalog can cause a process to run with user privileges: a
consumer must treat catalog source and command selection as security-sensitive,
not execute unfamiliar routing kinds, and validate input before launch. Routing
kinds and templates are a closed set in the implementation; they are data
selecting reviewed behavior, not arbitrary executable expressions. A direct
binary declares `requires_binary`; an unpinned package runner is advisory, but
an `acp_compat_1` conformance record requires a pinned invocation.

A conformance record belongs to an agent/runtime pair and records measured
handshake and routability separately, together with suite version, agent
version, producer and date. A tier that did not run is absent; a failure cannot
be represented as a pass. Third-party evidence identifies its source. No record
is not evidence of failure. `session/new`/prompt/cancel lifecycle measurement
is not currently part of the implemented conformance record.

The [registry contributor guide](../../registry/README.md) describes current
fields and placeholders; [validator implementation](../../helpers/dist-helper/src/registry/agents.rs)
is the detailed source for accepted kinds and bounds. Unknown executable
semantics must fail closed. An additive field is not a grant to execute or a
promise that older readers preserve it.
