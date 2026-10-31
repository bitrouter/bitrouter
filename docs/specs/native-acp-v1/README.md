# Native ACP interoperability profile

Status: **beta, specification revision 1.0.0-beta.1**. This document describes
the native BitRouter ACP adapter at `80550c9b`. ACP wire v1, draft ACP wire v2,
and the BitRouter metadata version are separate version spaces. This beta may
change incompatibly; availability depends on build features. MUST, MUST NOT,
SHOULD and MAY express requirements on conforming peers. The base protocol is
[Agent Client Protocol](https://agentclientprotocol.com/).

## Connection and capability discovery

`bro acp serve` starts the native stdio bridge; `bro acp serve <agent>` is the
separate external-agent proxy. A native bridge connects to a local daemon; its
stdio carries ACP protocol messages. Native mode needs an explicit model or
`chat.model`. Initialization negotiates supported ACP v1 or v2 for the
connection; unsupported versions and unsupported methods fail visibly.
Consumers MUST use the advertised capability set instead of assuming that
optional methods exist. The native adapter's `_meta.bitrouter.version` is `1`;
its `schemaVersion` is `1.10.2`. `nativeExecution` reflects the `core` build
feature. Queue resume and close are advertised only when supported.

## Session and execution

An ACP session identifies a native thread. Supported lifecycle operations
include initialization, new, list, prompt, cancel, and version-appropriate
load/resume; close and queue resume depend on advertised native execution.
The v1 prompt returns its result when the turn settles. The v2 prompt returns
acceptance with the admitted user message identity and completion is projected
through updates. Consumers MUST NOT treat acceptance as successful completion.

A client disconnect detaches transport; it does not cancel accepted work.
Cancellation cancels active work and leaves accepted unstarted work in a
paused queue. Explicit close cancels active and queued work while retaining
history for authorized reopening. Pending approval is native state and may be
reattached on reconnect to the same live daemon; transport request IDs do not
confer approval authority. The advertised `_bitrouter/session/resume_queue`
extension resumes a paused queue subject to current authorization. Client info,
requested working directory and possession of a session ID do not grant access.

## Operation identity

Retry-capable clients may supply `_meta.bitrouter.idempotencyKey` for durable
operations. The key is scoped to the caller, Thread and operation; JSON-RPC
request IDs are transport identities, not idempotency keys. A matching retry
returns the original receipt and changed content conflicts. A client that lost
an acknowledgement without a reusable key SHOULD inspect authoritative history
rather than automatically submit a new prompt. Load or resume reattaches; it
does not itself resume a paused queue. A busy or queued prompt is rejected
rather than silently reordered.

## Validation and evolution

Invalid members in `mcpServers`, `prompt` or `additionalDirectories` are rejected
rather than dropped. Non-null `_meta` must be an object. A peer MUST NOT assume
that enabling draft v2 enables every draft extension, or that a response from a
future schema revision has the same meaning. Unknown optional metadata MAY be
ignored; unsupported required capabilities MUST fail visibly. See
[conformance](conformance.md) and the [native adapter](../../../apps/bitrouter/src/harness/acp/native/).
