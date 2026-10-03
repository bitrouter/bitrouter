# Managed core harness integration

The `bro serve` inference listener also exposes the negotiated managed-core
profile. This is separate from native task/ACP execution: existing harnesses do
not automatically become durable managed-core clients.

1. Use an active `brvk_` virtual key on both HTTP and WebSocket requests. Managed
   endpoints require authentication even when ordinary inference uses
   `server.skip_auth: true`. The session namespace includes the user and key ID.
2. Read `GET /v1/orchestrator/capabilities`. Respect negotiated limits and
   `unsupported_features`; do not infer complete OpenAI multi-agent support.
3. Connect `GET /v1/orchestrator/channel` with the same credential and
   `BitRouter-Beta: orchestrator_core=v1`. Send a v1 `session.bind` envelope with
   the advertised core instance, harness grant, tool manifest and durable head.
   Persist `checkpoint.proposed` atomically and return its exact ACK before
   submitting work. A new empty binding receives its first checkpoint this way.
4. Submit `POST /v1/responses` with the same beta header, `multi_agent.enabled`,
   and the `bitrouter` execution/session/epoch/operation extension. Initial input
   is a task string. `stream: true` selects SSE. Core owns agent/model/context
   scheduling; the harness executes only authorized `tool.execute` commands.
5. Before tool execution, verify both the invocation's authorizing checkpoint
   and its response's completed pending-call map. Streamed function arguments
   never authorize an effect. Response completion may still leave the run waiting.
6. Retain result operation IDs. A continuation names `previous_response_id` and
   submits `function_call_output` items with `call_id`, string `output`, and
   `bitrouter: {operation_id, status, evidence?, workspace_revision?}`. Use the
   same operation ID and exact outcome if also submitting `tool.result` on the
   channel. The whole result batch and successor acceptance share one ACK.
   Verification and material requirements belong on the initial request;
   continuations inherit them and must omit those request fields.
7. On channel loss, stop dispatch and reconcile the same grant/head through
   `session.bind`. Retain uncertain operations and query their dispositions;
   never invent a new input operation merely because the HTTP consumer left.
   Durable state remains harness-owned. The current remote transport rejects
   restoration with running tools until its clock-handoff bridge is available.
   For other uncertain tool states use `session.restore` with fresh observations
   and cumulative activity evidence. A retained local owner is fenced first.
   If binding/restoration is interrupted, reconnect with the same grant and
   actual durable head: core retains its initialization and exact pending batch.
8. Release only settled sessions. `session.release` commits the ownership fence;
   close the channel after receiving the receipt to free the registry slot.

HTTP examples and the full wire contract are in the repository's
`docs/ORCHESTRATOR_CORE_SPEC.md`. Client functions come from the bound manifest;
request-level `tools` overrides are rejected. Unsupported input fields fail
explicitly, including stateless transcript replay and provider encrypted state.
Collaboration observation uses `bitrouter.*` events. The core model pipeline
retains the key's authentication/policy hooks; credentials are volatile and do
not belong in harness checkpoints.

The preview host admits four sessions and sixteen HTTP consumers. Query actual
capabilities rather than copying these defaults into a client. Active descendant
turns include waiting agents and exclude the root; their limit is separate from
model concurrency. Finished contexts can be reused subject to the same limit.
Descendant-assigned queued followups reserve their next slot so another spawn
cannot block the assigning agent's completion indefinitely. SSE currently emits
output deltas after the exchange has reached its durable boundary; it does not
provide live provider-token streaming.
