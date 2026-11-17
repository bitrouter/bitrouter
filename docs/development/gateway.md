# Gateway adaptation

This development guide follows the current implementation. Update it with
changes to ownership, adapters and runtime behavior. Format compatibility is
maintained in the [specification index](../specs/README.md).

## Endpoints and upstream contracts

| Method/path | Inbound protocol |
| --- | --- |
| POST `/v1/chat/completions` | [Chat Completions](https://platform.openai.com/docs/api-reference/chat) |
| POST `/v1/messages` | [Messages](https://platform.claude.com/docs/en/api/messages) |
| POST `/v1/responses` | [Responses](https://platform.openai.com/docs/api-reference/responses) |
| POST `/v1/decisions` | Decisions, using the [public DTOs](../../crates/bitrouter-ai/src/decisions.rs) |
| GET `/v1/models` | Available only when model listing is enabled |
| GET `/health`, `/metrics` | Host health and metrics; not inference protocols |

MCP endpoints are described in the [MCP gateway profile](gateway.md#mcp-adaptation).
Provider/model configuration and auth determine which operation can actually
run. A listed model or catalog record is not proof of adapter availability,
authorization, pricing or support for every operation. Requests requiring an
unsupported conversion or operation must fail rather than silently claim faithful
execution. The current set of accepted fields, extension handling and failure
behavior is described by the relevant public DTO and adapter; callers should
consult the corresponding vendor specification for the base protocol.

## Streaming and termination

Streaming uses server-sent events where supported. Chat Completions ends with
`[DONE]`; Messages uses `message_stop`; Responses has a completed, incomplete or
failed terminal event. Decisions is JSON. A finish reason, successful HTTP
status, EOF, keepalive or a decoded ordinary event does not by itself prove a
successful protocol end. An upstream/transport error after HTTP headers cannot
change the already committed HTTP status. An implementation must not present a
truncated or mismatched stream as successful completion, and must not synthesize
a successful terminal from a transport error. The application may send its Chat
error-then-DONE trailer; that trailer is not proof of success.

Same-protocol transport preserves supported extension fields and absent/null
distinctions where represented by the public wire types. Cross-protocol
conversions can be lossy or unsupported; client applications requiring exact
fidelity should use a compatible native protocol and verify required features.
A client must tolerate unknown response fields, but must not interpret an
unknown terminal event as success. The SDK does not automatically replay a request after uncertain delivery;
the gateway may apply configured upstream retry or fallback policy. No
provider-side exactly-once execution is promised, and a disconnect may leave
the upstream outcome unknown.

## Shared gateway profile

Shared mode is explicitly enabled and uses independently published routing and
credential authority. It does not inherit local provider configuration. Its
inference transport and service readiness, authentication, resource bounds,
HTTP/1 and optional HTTP/2 behavior are described in the
[shared gateway operator guide](../../skills/bitrouter/references/shared-gateway.md).
A lost owner lease or connection is not evidence that an upstream invocation
stopped. A client must treat uncertain work as unknown rather than retrying it
as a fresh execution. The independently authenticated administration transport
is the [shared operator API](../specs/service-api/README.md#shared-operator-api). The
[shared request identity profile](../specs/service-api/README.md#shared-request-identity) specifies headers,
canonicalization, recovery and a compatibility vector.

## Owned tenant service

The opt-in [tenant service](tenant-service.md) uses an exclusively owned local
SQLite database and a protected control listener. It supports immutable resource
publication, revision-bound SDK dispatch and durable execution receipts with
separate native usage evidence. Its ownership and recovery model is distinct
from shared gateway mode; the two profiles cannot be enabled together.
[Tenant routing](tenant-routing.md) describes the actual-send limits and health
state inside the authorized retained candidate set.

## Accepted M0 Policy and Eval boundary

These boundaries apply independently of the compiled feature set.
Gateway model Policy uses the same public typed source as files, defaults, CLI,
SDK and tenant resources. Shared publication carries model_policy under format2;
ACL policies stay independent. Runtime selection changes model/effort and keeps
complete admitted context. A private compiled view never becomes an editable
format or a source-reconstruction bridge. Exact revision/literal-name bindings,
full dependency qualification and live authority are checked before dispatch.

Tenant/shared import, configuration and execution reject experimental context,
core_choice, paid assessment and ACP block even on Policy-free routes with all
features enabled. Retained experiments have an explicit BRO local harness owner.
Ordinary provider continuation and explicitly enabled server tools retain their
own permissions; selecting a Policy cannot start tool execution.

Model Policy and tenant Eval exchange belong to the minimum gateway profile.
Evaluation executes externally; the service admits/queries/freezes facts against
verified closed identities. It adds no Eval runner or implicit learner. Results,
freeze and draft source do not change active routing. Source publication and
configuration activation are distinct authorized/CAS operations.

Shared bootstrap/join/restart validate the current baseline, authenticated head
and all affected encrypted publications before instance registration, heartbeat
or recovery writes. A Policy-free head does not permit old retained content.
Refusal preserves old data; current shared reconciliation, idempotency, output,
usage/Unknown and cancellation obligations remain required. Format selection is
owned by the [service contract](../specs/service-api/README.md#resource-and-evidence-migration-and-rollback).

## Adapter validation

Authentication, routes and model availability are deployment-specific. Unknown
fields and validation are governed by each protocol and its selected adapter;
this adapter guide does not promise that an arbitrary unknown request field is
forwarded. The [adapter contracts](../../crates/bitrouter-ai/src/protocol)
and their consuming tests define the supported conversion shapes.

## Provider model-list check

`bro providers check <provider>` reads one provider already present in
configuration and requests its model catalog. Local HTTP and online HTTPS use
that same check. It resolves `api_base`, an account's endpoint override,
`protocol_endpoints`, and `${VAR}` credential references the way config load
does. An unresolved endpoint or credential reference fails that connection and
does not open a socket. A resolved empty credential is allowed; a local
endpoint may omit a key.

The request is a GET of the selected protocol's model list. Chat Completions,
Responses, and Messages use `{api_base}/models` and accept `{ "data": [{ "id" }] }`.
Authentication, static provider headers, and upstream timeouts are the ones the
gateway uses for that provider. The check does not render a prompt, does not
enter scheduling, and does not change `active` or write `bitrouter.yaml`.
Model ids it sees are reported only. `auto_discover` still runs only at startup
and reload.

`answered` means the endpoint returned that protocol's list. It does not prove
that every listed model can generate. A provider with accounts is checked once
per account (`provider/label`); `--account` selects one. The command is local:
it dials from the operator's machine.

## MCP adaptation

When configured, the gateway accepts JSON-RPC requests at `POST /mcp/{server}`
and an aggregate path (normally `/mcp`, configurable). Direct requests select
one named upstream; the aggregate merges eligible upstreams and namespaces
items. The gateway is not an arbitrary JSON-RPC tunnel: it relays supported
standard methods and an explicit extension allowlist. Unsupported methods fail
rather than being forwarded. Downstream connection identity and request context
are not forwarded as the upstream's own identity.

When `Accept: text/event-stream` is requested, gateway frames contain JSON-RPC
messages in SSE `data:` events. The stream ends after its final result or first
error. Without streaming, the response is JSON. Protocol failures use JSON-RPC
error objects, and HTTP status can also reflect a transport or pipeline failure.
A client must handle both layers and must not infer execution success solely
from an HTTP status.

## Negotiation and skills

The upstream configuration `mcp.upstream_protocol` selects lifecycle:
`auto` (default) and `2026-07-28` use modern discovery with classified legacy
fallback; `latest` selects the legacy initialize path. Discovery timeout or a
correlated legacy-classified JSON-RPC error can trigger fallback. Transport,
malformed or uncorrelated replies and modern security/metadata rejections do
not authorize fallback. Upstream tasks or input-required tool results that the
gateway cannot represent are errors, not synthesized successful tool output.

The supported skills extension methods are `skills/list`, `skills/get` and,
when advertised, `resources/directory/read`. On aggregate routes `skill://`
URIs are namespaced by upstream and reversed on dispatch. Unsupported URI
schemes are not silently converted into a different upstream's identity.
Unrecognized extension metadata is preserved where represented by the skills
wire types. Source permission and per-skill consent are independent of the
existence of cached content; listing a skill does not authorize execution.
Upstream connections use configured server credentials, not a downstream user's
inference credential. Connection sharing does not confer tenant isolation.

Schemas and bounded skills representations are in the [wire types](../../crates/bitrouter-ai/src/protocol/mcp_skills.rs).
The protocol is evolving; callers must use discovered capabilities, tolerate
unknown optional response fields, reject unsupported required behavior and
handle errors.

## MCP connection ownership

The gateway retains configured upstream clients, aggregation, server-side tool
execution and skill relay; it does not ship a separate first-party local
control-origin MCP server. Shell-capable local agents use structured CLI and the
BitRouter skill. The upstream connection pool is keyed by configured server
name and uses daemon-scoped credentials. Deriving per-request headers alone
cannot isolate callers sharing that connection: a future per-caller credential
model would need an appropriate pool identity and authorization design.

A native harness uses its own explicit source grants and execution ownership.
Gateway cache membership, upstream metadata and downloaded skill bytes do not
confer native consent or workspace authority. Keep those boundaries with
[harness resource loading](harness-resources.md).
