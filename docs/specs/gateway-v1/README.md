# Inference gateway interoperability profile

Status: **beta, specification revision 1.0.0-beta.1**. This profile describes
checked-in behavior at `80550c9b`, subject to feature, configuration and
provider support. It defines BitRouter's gateway boundary, not a replacement for
vendor protocols. The namespace `/v1` is part of endpoint paths, not this
document's revision. Incompatible beta revisions are possible. MUST, MUST NOT,
SHOULD and MAY express requirements on conforming peers.

## Endpoints and upstream contracts

| Method/path | Inbound protocol |
| --- | --- |
| POST `/v1/chat/completions` | [Chat Completions](https://platform.openai.com/docs/api-reference/chat) |
| POST `/v1/messages` | [Messages](https://platform.claude.com/docs/en/api/messages) |
| POST `/v1/responses` | [Responses](https://platform.openai.com/docs/api-reference/responses) |
| POST `/v1/decisions` | Decisions, using the [public DTOs](../../../crates/bitrouter-ai/src/decisions.rs) |
| GET `/v1/models` | Available only when model listing is enabled |
| GET `/health`, `/metrics` | Host health and metrics; not inference protocols |

MCP endpoints are described in the [MCP gateway profile](../mcp-gateway-v1/README.md).
Provider/model configuration and auth determine which operation can actually
run. A listed model or catalog record is not proof of adapter availability,
authorization, pricing or support for every operation. Requests requiring an
unsupported conversion or operation MUST fail rather than silently claim faithful
execution. The current set of accepted fields, extension handling and failure
behavior is described by the relevant public DTO and adapter; callers SHOULD
consult the corresponding vendor specification for the base protocol.

## Streaming and termination

Streaming uses server-sent events where supported. Chat Completions ends with
`[DONE]`; Messages uses `message_stop`; Responses has a completed, incomplete or
failed terminal event. Decisions is JSON. A finish reason, successful HTTP
status, EOF, keepalive or a decoded ordinary event does not by itself prove a
successful protocol end. An upstream/transport error after HTTP headers cannot
change the already committed HTTP status. An implementation MUST NOT present a
truncated or mismatched stream as successful completion, and MUST NOT synthesize
a successful terminal from a transport error. The application may send its Chat
error-then-DONE trailer; that trailer is not proof of success.

Same-protocol transport preserves supported extension fields and absent/null
distinctions where represented by the public wire types. Cross-protocol
conversions can be lossy or unsupported; client applications requiring exact
fidelity SHOULD use a compatible native protocol and verify required features.
A client MUST tolerate unknown response fields, but MUST NOT interpret an
unknown terminal event as success. The SDK does not automatically replay a request after uncertain delivery;
the gateway may apply configured upstream retry or fallback policy. No
provider-side exactly-once execution is promised, and a disconnect may leave
the upstream outcome unknown.

## Shared gateway profile

Shared mode is explicitly enabled and uses independently published routing and
credential authority. It does not inherit local provider configuration. Its
inference transport and service readiness, authentication, resource bounds,
HTTP/1 and optional HTTP/2 behavior are described in the
[shared gateway operator guide](../../../skills/bitrouter/references/shared-gateway.md).
A lost owner lease or connection is not evidence that an upstream invocation
stopped. A client MUST treat uncertain work as unknown rather than retrying it
as a fresh execution. The independently authenticated administration transport
is the [shared operator API](../shared-operator-v1/README.md). The
[shared request identity profile](shared-execution.md) specifies headers,
canonicalization, recovery and a compatibility vector.

## Scope and conformance

Authentication, routes and model availability are deployment-specific. Unknown
fields and validation are governed by each protocol and its selected adapter;
this beta profile does not promise that an arbitrary unknown request field is
forwarded. See [conformance](conformance.md) and the [adapter contracts](../../../crates/bitrouter-ai/src/protocol/).
