# Observable ACP content recording

This storage layer supports evaluation of already observed coding sessions.
It records protocol evidence; it does not execute tests, query repositories or
PRs, read private harness logs, define tasks, or score model quality.

## Ownership and configuration

`acp_recording.enabled: true` explicitly enables local content storage. It is
false by default and independent of the content-free `trajectory.enabled`
ledger. The configured application database stores the content until explicit
deletion. Neither enabling capture nor inspecting it invokes an evaluator or
publishes the content through the evaluation exchange.

The SDK controller accepts a `CapturePort` whose acknowledgment means durable
storage. The application supplies the database implementation for both the
external `acp serve` path and the in-process `code`/`run` path. The TUI's retained
journal and broadcast subscriptions are presentation mechanisms, not storage.

The native identity tuple is `(owner, configured agent source, native session
ID)`. Its hash is an internal database index, never a second user-facing session
ID. Connection IDs and RPC correlation IDs are separate namespaces. A reconnect
does not create another native session. The CLI currently uses the local
application owner and only exposes recordings from its selected database.

## Capture and canonical order

An outer controller proxy records requests before forwarding and responses
before delivery. It preserves the ACP SDK's response ordering and propagates
request cancellation through the originating responder. Authentication,
initialization, provider configuration, and unrelated session lists bypass
content capture. MCP launch descriptors are removed from recorded session setup
parameters because they may carry tool credentials. The live request is intact.

Requests, notifications, and results retain their structured payloads. A tool
update appends an event; it never overwrites a preceding tool node. Each raw
event has a stable `(connection ID, sequence)` reference. Canonical live events
also receive a per-session sequence in the same transaction as their insert.
A `session/new` request precedes its native ID: the reader exposes its original
setup parameters through a reference to the matching response. It does not
rewrite an earlier canonical prefix when that association becomes available.

Connection records begin in `recording` state. Only orderly completion with no
pending captured requests marks them `complete`. A write failure latches an
error and stops forwarding; an interrupted or unclosed connection never claims
to be a complete capture. If storage itself cannot accept a failure marker,
the preexisting unclosed record still exposes the incomplete state.

These semantics follow [ACP session setup](https://agentclientprotocol.com/protocol/v1/session-setup),
[prompt turns](https://agentclientprotocol.com/protocol/v1/prompt-turn), and
[tool calls](https://agentclientprotocol.com/protocol/v1/tool-calls).

## Replay, partial visibility, and lineage

Notifications inside a `session/load` replay window are retained in the raw
audit stream and excluded from the live canonical sequence. They are not new
executions. Text equality is never a deduplication key; two identical user
prompts remain two observed prompts. Without a stable history identifier or
verified replay boundary, the store explicitly reports uncertain continuity
across load/resume. It does not claim that an unobserved interval was empty.

Fork responses retain the parent native identity and the parent's observed
watermark at the fork request. The child remains an independent native session.
An absent parent prefix remains unknown. Checkpoint and family consumers must
resolve inheritance from those references; raw replay is not another copy of
the parent's execution costs. Observable compaction information remains in the
append log; absent early content cannot be reconstructed from private files.

## Routing and cost associations

Connections carry an optional locally established controller/principal
namespace. Within that namespace the store joins metering by the recorded ACP,
native root, or native thread ID. A session-level join is declared correlation,
not proof that a particular model request produced a tool call. Unknown
tool-to-request relationships remain explicit.

Associated requests expose actual model/provider, native turn/thread evidence,
charge provenance, and existing immutable route/guard ledger events. Those
events retain historical policy/decision information when it was recorded.
Missing block identities or decision context cannot be reconstructed from the
current configuration. A future block router must emit its revision at decision
time through that evidence path.

Cost is summed over unique request IDs, including children matched through a
root, with unknown prices counted separately. Root, child, repeated connection,
and replay totals must not be added together. Direct or remote unmetered work
cannot be labeled complete local metering.

## Local operations and deletion

`bro acp recordings list|show|delete` inspects or removes captured content.
`--agent` selects the configured source and `show`/`delete` take the native ID.
The command's `--config` selects the database using the same path anchoring as
the coding entrypoints. JSON includes original payloads and reference IDs;
`--human` provides a readable timeline.

Deletion removes locally recorded content and retains a metadata tombstone so
an in-flight recorder cannot recreate it. It does not delete harness-native
history, billing records, or the content-free route ledger. Recording must be
disabled to continue a deleted native session without further local storage.

## Validation boundaries

Controller tests exercise native multi-session lifecycle and bidirectional
callbacks with capture installed. Application tests drive a real controller
and ACP client against a deterministic protocol fixture, including more updates
than a lossy UI subscriber need retain, repeated prompts, tool input/output,
early notifications, reconnect/load replay, and durable shutdown. Storage tests
cover failure latching, owner/source separation, fork boundaries, deletion,
principal-scoped request unions, and unknown costs.

The [checkpoint layer](ACP_CHECKPOINT_SPEC.md) builds immutable prefix manifests,
assessment revisions, effective selection and inherited family projections on
these observations. Neither layer establishes judge accuracy or a causal reward
for individual model choices.
