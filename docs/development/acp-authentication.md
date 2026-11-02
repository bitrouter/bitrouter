# ACP authentication boundaries

ACP agent authentication and provider routing authentication are independent.
The external harness authenticates its own upstream account; the daemon obtains
provider credentials through its configured provider login/import path. A
failure on one axis must not be presented as a failure of the other.

The shared ACP client retains `initialize.authMethods` and recognizes
`auth_required` by its protocol error code, not diagnostic text. It can invoke
the selected declared authentication method. A frontend may offer only methods
it can actually drive; it must not invent a harness-specific login command,
claim authentication on cancellation, or retry a failed task under a new
identity merely because login was attempted. Terminal authentication is an
explicit capability and requires appropriate terminal ownership.

Provider-login commands are different: a configured provider may explicitly use
its vendor CLI to acquire or import a credential. It does not need to launch an
ACP adapter to do so. Credentials, method arguments and provider errors must not
be exposed as user-visible capability metadata or mixed into model input.

The current [client](../../apps/bitrouter/src/harness/acp/client.rs) and
[provider commands](../../apps/bitrouter/src/commands.rs) implement these two
paths. Controller forwarding and harness-native session identity are described
in the [runtime guide](agent-runtime.md#external-acp-controller); the base ACP
messages follow the [upstream protocol](https://agentclientprotocol.com/protocol/initialization).
