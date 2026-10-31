# Public protocol adapters for Axum

## Ownership contract

`bitrouter-ai` owns public protocol DTOs, bounded wire serialization and an
optional `axum` binding. Its default features remain empty. The binding supplies
request extraction and JSON/SSE responses; consumers write ordinary handlers and
middleware. It owns no listener, runtime, SDK client, provider conversion,
authentication, routing, task lifecycle, database, hooks or application builder.

`bitrouter-sdk` owns HTTP transport, validated response metadata and stream
decoding. It does not depend on Axum in production. SDK streams and server
adapters use the same public event contracts. Existing event-only methods remain
available; relay methods explicitly retain protocol termination. Model requests
are never automatically retried or reconnected.

The application owns protocol conversion, execution and receipt delivery gates.
It reuses bounded serialization and response construction without exporting its
`DeliveryPermit`, receipt writer or detached execution lifecycle. JSON encoding
must precede success authorization. The last successful SSE terminal remains
behind the existing delivery gate.
All terminal bytes must be encoded before authorization; after authorization,
the last chunk is yielded without an intervening await. Encoding failure and
cancellation tests must preserve this boundary.

## Public behavior

1. Reuse the existing Chat Completions, Messages, Responses and Decisions DTOs.
   Preserve extension fields and missing/null distinctions. Decisions is JSON.
2. Offer optional Axum JSON extraction with bounded input and redacted structured
   rejection, bounded JSON output and pull-driven SSE output. Application limits
   remain explicit; no unbounded queue or background task is introduced.
3. Separate data events from explicit protocol ends: Chat `[DONE]`, Messages
   `message_stop`, Responses completed/incomplete/failed. A protocol end is not
   proof of durable execution or downstream delivery. An ordinary finish reason,
   HTTP 200, EOF or decoded `Ok` event is not sufficient evidence of success.
4. An adapter retains a terminal data event until its matching end is observed.
   Missing/mismatched termination, transport/decode failure or a caller error
   cannot generate a successful terminal. Stream errors are terminal and public
   presentation is selected by the caller. No frames follow a protocol end.
   Finish/usage chunks are not protocol ends: preserve their original order.
   The application retains its existing Chat error-then-DONE trailer; the new
   relay stops on an error without synthesizing DONE. Neither signifies success.
5. Preserve SSE event names and IDs with bounded, injection-safe framing. Keep
   unknown protocol fields. Comments/keepalives carry no completion authority;
   timers, cancellation policy and retries remain with the host.
   Programmatically constructed flattened fields must not override fields used
   to classify termination. Public error inputs are bounded before retention;
   error serialization borrows those fields and applies the output byte limit.
6. SDK reply metadata is bounded and allowlisted. Public response headers are
   selected by the consumer; internal authentication, Location and arbitrary
   upstream headers are not automatically forwarded.
7. Pre-stream HTTP failures retain an error result before committing a stream.
   After commitment an error is encoded in the selected protocol or terminates
   the body; the HTTP status cannot be rewritten.
   New relay methods reject 202 existing-execution responses and successful HTTP
   responses with the wrong content type before returning any stream.
8. The Axum dependency disables default features. Its optional adapter may add
   HTTP/body/stream primitives, but no HTTP client, async runtime, database,
   telemetry exporter or service implementation. Other framework integrations
   can use the same contracts without an Axum dependency.

## Delivery and acceptance

- Public contracts, bounded wire codec, optional Axum extractors/responses.
- SDK relay methods with explicit end and metadata, preserving existing methods.
- Application reuse with receipt/lifecycle behavior unchanged.
- A runnable independent Axum + SDK gateway example, without application imports.
- Tests for all three SSE protocols and Decisions JSON; extension/null fidelity;
  malformed/oversized input and output; CR/LF injection; stream error/truncation;
  terminal mismatch; backpressure and drop; real HTTP SDK-to-adapter forwarding.
- Existing SDK and application receipt/stream regressions remain passing.
- Preserve the SDK's existing `ServerEvent` type path as a compatibility alias
  for the common AI-owned event; no duplicate DTO or service re-export is added.
- Isolated dependency and public API gates cover default and adapter features.
- Formatting, workspace checks, tests, independent review and a pull request.

Performance measurement and hosted service migration are separate work. This
change does not imply a new tenant dispatch API or alter financial semantics.

## References

- [Axum extractors](https://docs.rs/axum/0.8.9/axum/extract/index.html)
- [Axum responses](https://docs.rs/axum/0.8.9/axum/response/index.html)
- [SSE framing](https://html.spec.whatwg.org/multipage/server-sent-events.html#parsing-an-event-stream)
- [OpenAI streaming](https://platform.openai.com/docs/api-reference/streaming)
- [Anthropic streaming](https://platform.claude.com/docs/en/build-with-claude/streaming)
