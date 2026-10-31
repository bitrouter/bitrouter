# MCP gateway interoperability profile

Status: **beta, specification revision 1.0.0-beta.1**. This profile describes
BitRouter behavior at `80550c9b`; its revision is not an MCP wire version.
Features and configuration determine availability; incompatible beta revisions
are possible. MUST, MUST NOT, SHOULD and MAY express requirements on peers.
The base protocol is [MCP](https://modelcontextprotocol.io/specification/).

## Downstream and upstream

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
A client MUST handle both layers and MUST NOT infer execution success solely
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

Schemas and bounded skills representations are in the [wire types](../../../crates/bitrouter-ai/src/protocol/mcp_skills.rs).
The protocol is evolving; callers MUST use discovered capabilities, tolerate
unknown optional response fields, reject unsupported required behavior and
handle errors. See [conformance](conformance.md).
