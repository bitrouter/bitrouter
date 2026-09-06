---
type: added
title: "The ACP agent surface forwards v1 session updates, cost, and provider control"
pr: 816
---

Three additions to `bitrouter acp serve`, for any ACP client, not just ours:

- The five stable v1 `session/update` variants that `translate` was previously
  swallowing are now forwarded.
- `UsageUpdate.cost` carries **router-measured** cost from the metering store.
  BitRouter sits in the agent seat, so it can report what no upstream harness
  can.
- `providers/list` and `providers/set` are served over the schema crate's
  `unstable_llm_providers` types, dispatched as raw JSON-RPC, so a client can
  enumerate and switch the provider mid-session.

No credential crosses the `providers/*` wire. That is asserted at two levels,
both against *serialized bytes* rather than struct fields, so `_meta` smuggling
fails too.

ACP **v1 only**: `unstable_protocol_v2` is not enabled anywhere. v2's
`PromptResponse` is `{}` with no `stop_reason`, so a v2 gateway is a separate
piece of work.
