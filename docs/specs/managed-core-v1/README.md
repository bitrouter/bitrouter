# Managed Core interoperability profile v1

Status: **beta, specification revision 1.0.0-beta.1**; core wire version `1`.
This draft describes the implementation at `80550c9b`; beta revisions may change
incompatibly. It is separate from ACP and the Thread HTTP API. MUST, MUST NOT,
SHOULD and MAY express requirements on peers.

An active virtual service key authenticates `GET /v1/orchestrator/capabilities`
even if ordinary inference is configured to skip authentication. Availability
depends on the host's `core` feature and enabled service. The response identifies
the core instance, supported operations/transports and limits. A client MUST
honor the advertised capabilities rather than assume the endpoint's presence
implies every operation is enabled.

`GET /v1/orchestrator/channel` upgrades to WebSocket only with
`Bitrouter-Beta: orchestrator_core=v1`. Managed `POST /v1/responses` uses the
same beta header; the ordinary Responses profile remains separate. The caller
must first bind or restore a session over the core channel within the host's
initial-message deadline. The harness owns durable checkpoint persistence and
must explicitly acknowledge the checkpoint only after persistence; HTTP/SSE
observation does not acknowledge it. DTOs and message variants are defined by
[core protocol](../../../crates/bitrouter-ai/src/core/protocol.rs),
[checkpoint](../../../crates/bitrouter-ai/src/core/checkpoint.rs) and
[managed Responses](../../../crates/bitrouter-ai/src/core/responses.rs).

A channel disconnect or dropped HTTP stream does not itself complete or cancel
a run. No automatic replay or exactly-once result after uncertain delivery is
promised. A client MUST use the existing operation inspection/recovery semantics
before deciding on a new operation. `CoreError` carries `code`, `message` and
`commit_status` (`not_committed`, `committed`, `unknown`); `unknown` MUST NOT be
interpreted as rollback. Strict input structures reject unknown fields where
specified by the DTO. Unknown required capabilities or future message variants
MUST NOT be guessed. See [conformance](conformance.md).
