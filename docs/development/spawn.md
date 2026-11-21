# ACP launch, routing and cleanup

`bro acp serve` serves native BRO sessions over stdio. Specify a bundled adapter
with `bro acp serve AGENT` to supervise that adapter and bridge the manager's
ACP connection. A local operator can supply an explicit stdio command with
`bro acp serve --agent NAME --direct --config agents.yaml`.
For a one-shot prompt, use `bro acp --workspace PATH prompt --agent AGENT PROMPT`.

## Routing and authority

A gateway-bound adapter uses the endpoint and client credential selected by
BRO. Provider credentials and model routing belong to the gateway. A direct
adapter uses its own process configuration and credentials. Native sessions
use the local BRO worker and checkpoint journal. See [runtime ownership](agent-runtime.md)
and the [agent catalog](catalogs.md).

## Cleanup

BRO validates the selected adapter and required routing data before launch.
ACP managers handle permission requests forwarded from external adapters.
The one-shot prompt declines permission requests. Adapter transport and
process cleanup belong to the local BRO invocation. The workspace, session
state and approval records stay with the local owner.

See [local BRO commands](../CLI.md#local-bro-threads-and-agents) and
[ACP authentication](acp-authentication.md).
