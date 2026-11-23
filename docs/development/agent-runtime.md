# Agent runtime ownership

## Gateway and harness

BitRouter accepts inference requests, selects models and effort, applies
gateway policy and records usage. Tenant configuration, Policy and Eval are
service resources. BRO runs on the user's machine, schedules local agents,
owns their workspaces and stores its turns, approvals and tool effects in its local home. A client provides the gateway endpoint and its own client credential.
The gateway retains its provider credentials.

BRO records a turn's dispatch and tool checkpoints locally. A resumed turn
can reuse a committed reply or tool result. An unresolved effect requires
reconciliation before another dispatch. The gateway records its own admission,
metering and settlement using the authenticated principal. Caller-provided
session identifiers do not confer access to another thread or principal's
records. The [service API](../specs/service-api/README.md) defines the wire
boundary. The [BRO source guide](../../apps/bro/HARNESS.md) maps the implementation.

## External ACP controller

Use `bro acp list` for the bundled adapter catalog. `bro acp prompt` sends a
prompt through a selected gateway-bound adapter. `bro acp serve` supervises an
adapter over stdio for a local ACP controller. A custom stdio command and its
environment belong to the local operator. The controller and adapter exchange
ACP messages directly over the supervised pipes. See [ACP authentication](acp-authentication.md).

## Tools and resources

The user grants a workspace and chooses a native tool mode. BRO records tool
requests and approval decisions with the turn. It retains the content and
digest of each explicitly selected skill for subsequent turns. Gateway MCP
upstreams, service credentials and access policies are configured by the
service operator. See [local skills](../CLI.md#local-skills) and
[gateway MCP configuration](../CLI.md#mcp-configuration-and-gateway-inspection).
