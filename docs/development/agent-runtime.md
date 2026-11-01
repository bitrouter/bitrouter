# Agent runtime ownership

This guide follows the application as its architecture evolves. Serialized
messages are defined once in the [service API](../specs/service-api/README.md).
The [source guide](../../apps/bitrouter/HARNESS.md) maps the current modules.

## Core and harness

Application Core owns model/context routing and managed scheduling. Harness owns
workspace tools, complete session history, durable checkpoint storage and ACKs.
An SDK client transports public values; it does not execute tools, persist
sessions or acknowledge checkpoints automatically. A public DTO alone confers
neither execution nor storage authority. Keep runtime services in the application
and dependency-light data contracts in `bitrouter-ai`.

The native `ThreadService` owns one durable Thread/Turn path. CLI, HTTP and native
ACP project it; they must not create independent queues, permission registries or
provider loops. Thread, Turn and Item identities are durable execution facts;
transport connection generations and request IDs are delivery identities.
Disconnecting observation does not cancel accepted execution or answer approval.
Explicit cancellation and close use the service's own admission/cleanup barriers.

Accepted input and effect intent are committed before execution. Complete tool
results are reused on supported recovery; uncertain effects, accounting or lost
ownership block execution rather than being replayed under a new identity.
Current workspace, caller, configuration and feature checks remain necessary
when reconstructing an execution. Reading stored history does not start work.
The configured execution owner retains cleanup custody until workers actually
finish; a timeout is not evidence that a provider or shell stopped.

## External ACP controller

External ACP sessions retain the upstream harness's session identity, execution
and complete history. BitRouter's controller owns a connection and routing/cost
adaptation, and can record opt-in observed evidence. The transport-generic
client is shared by CLI consumers; a UI adapter does not implement another ACP
stack. Standard capability declarations, initialize gates and current protocol
permissions apply at that boundary.

The CLI correlation record uses the harness-native `session_id` and controller
instance. A locally minted `record_id` alias is not a public session identity.
Agent authentication and provider-routing authentication are distinct. The
controller cannot infer a harness's login command from its name; it consumes
protocol-declared authentication information. See
[ACP authentication](acp-authentication.md) for that interaction.

## Tools and resources

[Native tools](native-tools.md) describe the six built-in tool interfaces and
filesystem/interpreter constraints. [Harness resources](harness-resources.md)
describe project instructions, MCP source grants, skills, immutable inventories
and consent. Preserve those resource boundaries on every frontend; cached bytes
or metadata do not authorize execution. Recording, checkpoints and learning use
observed evidence through the [evidence workflow](evidence-learning.md).
