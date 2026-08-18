//! Tier 1 — the span schema, as a declaration rather than as call sites.
//!
//! What BitRouter owns as an interop surface is the *schema*: the span names,
//! the `bitrouter.*` attribute vocabulary, and the invariants that fail
//! silently and expensively when a deployment re-derives them differently.
//! Until now that schema existed only as ~90 `KeyValue::new` call sites spread
//! across `exporter.rs`, `acp.rs` and `metrics.rs`, so the only way to answer
//! "what does BitRouter emit?" was to read the exporter and re-derive it. This
//! module is the answer in declarative form.
//!
//! Three consumers, all of them checked rather than advisory:
//!
//! 1. `crates/bitrouter-sdk/span-schema.json` is rendered from
//!    [`render_json`] and committed, and an ordinary test fails when it goes
//!    stale — so the declaration cannot drift from the tree without a visible
//!    diff. It ships with the crate: it is the file a deployment implements.
//! 2. A conformance test in `exporter.rs` drives a full request lifecycle and
//!    asserts that every exported span is declared here, that every attribute
//!    key it carries is declared on that span, and that every attribute marked
//!    required is present. `acp.rs` does the same for the agent spans.
//! 3. `is_reserved_attribute_key` is the enforcement point for the extension
//!    region — the bounded hatch a deployment may add its own attributes
//!    through, applied at the `SpanAttributes` stamping site in `exporter.rs`.
//!
//! **This module names no `opentelemetry` type, deliberately.** It is plain
//! data plus `serde`. That is what makes it the *schema* tier rather than the
//! *emission* tier: it is readable by a deployment that renders BitRouter's
//! telemetry through something that is not this crate's OTLP exporter, and it
//! could move out from behind the `otel` feature gate without taking a
//! dependency with it. It sits inside `otel` because that is where a reader
//! looks for it, and it stays there: the wider module split that would have
//! relocated it was evaluated and withdrawn — the schema artifact was the part
//! worth having. See [`docs/OTEL_TIERING_SPEC.md`].
//!
//! [`docs/OTEL_TIERING_SPEC.md`]: https://github.com/bitrouter/bitrouter/blob/main/docs/OTEL_TIERING_SPEC.md

use serde::Serialize;

/// Version of *this declaration*, bumped when the emitted schema changes in a
/// way a consumer would have to react to.
///
/// Deliberately not `CARGO_PKG_VERSION`: the committed artifact is diffed in
/// CI, and stamping the crate version into it would rewrite the file on every
/// release bump, turning a guard that should be silent into routine noise. The
/// crate version reaches the wire through the instrumentation scope instead
/// (see [`SpanSchema::scope_version`]).
const SCHEMA_VERSION: &str = "1";

/// Attribute-key prefixes this schema owns outright. See [`ExtensionRegion`].
const RESERVED_PREFIXES: &[&str] = &["bitrouter.", "gen_ai."];

/// The type of an attribute value on the wire.
///
/// Only the shapes this schema actually declares. An OTel attribute can also
/// be a bool or a numeric array; the extension region accepts a bool from a
/// deployment (see [`ExtensionRegion::value_types`]), but no *declared*
/// attribute is one, so there is no variant for it.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttrType {
    /// UTF-8 string.
    String,
    /// 64-bit signed integer.
    Int,
    /// 64-bit float.
    Double,
    /// Array of UTF-8 strings.
    StringArray,
}

/// Whether an attribute is always present on a span closed through the normal
/// lifecycle, or only under a stated condition.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Requirement {
    /// Present on every exported instance of this span.
    ///
    /// "Every" means every span closed through the normal lifecycle. A span
    /// abandoned mid-request and reaped by the exporter's GC exports with
    /// whatever it had at creation time, which is a subset.
    Required,
    /// Present only when the `note` on the attribute holds.
    Conditional,
}

/// Span kind, in OTel's taxonomy but not OTel's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SpanKind {
    /// Inbound request handler.
    Server,
    /// Outbound call to an upstream.
    Client,
    /// Internal unit of work.
    Internal,
}

/// One attribute on a span, a span event, or a metric point.
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct AttrDef {
    /// Wire key, verbatim.
    pub(crate) key: &'static str,
    #[serde(rename = "type")]
    pub(crate) ty: AttrType,
    pub(crate) requirement: Requirement,
    /// What the attribute carries, and — for a conditional — when it appears.
    pub(crate) note: &'static str,
}

/// A span event, in OTel's sense: a timestamped annotation on a span.
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct EventDef {
    /// Event name, verbatim.
    pub(crate) name: &'static str,
    pub(crate) note: &'static str,
    pub(crate) attributes: &'static [AttrDef],
}

/// One span in the tree.
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct SpanDef {
    /// Span name. `{...}` marks an interpolated attribute value, so
    /// `chat {gen_ai.request.model}` names a span whose literal prefix is
    /// `chat ` — matching follows the GenAI semconv's
    /// `{gen_ai.operation.name} {model}` convention.
    pub(crate) name: &'static str,
    pub(crate) kind: SpanKind,
    /// What this span parents on.
    pub(crate) parent: &'static str,
    pub(crate) note: &'static str,
    pub(crate) attributes: &'static [AttrDef],
    pub(crate) events: &'static [EventDef],
}

/// Metric instrument kind.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Instrument {
    /// Monotonic counter.
    Counter,
    /// Value distribution.
    Histogram,
}

/// One metric instrument and the dimensions it is recorded with.
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct MetricDef {
    /// Instrument name, verbatim.
    pub(crate) name: &'static str,
    pub(crate) instrument: Instrument,
    pub(crate) value_type: AttrType,
    /// UCUM unit, as passed to the instrument builder.
    pub(crate) unit: &'static str,
    pub(crate) note: &'static str,
    pub(crate) dimensions: &'static [AttrDef],
}

/// The open region of the attribute namespace: what a deployment may add, and
/// what it may not.
///
/// `SpanAttributes` exists so a deployment can carry attributes the SDK does
/// not know about — request cost, namespace, routing profile — onto the root
/// `chat` span without the SDK learning a backend concept. Its keys are used
/// verbatim, which is the point and also the hole: nothing stopped a
/// deployment writing `bitrouter.retry_count`, a key inside the vocabulary
/// this schema claims to own, at which point "the schema is identical across
/// deployments" stops being true.
///
/// The resolution is an explicitly bounded region rather than a validated key
/// list: everything outside the reserved prefixes and the declared keys is
/// open, and everything inside them is dropped at the stamping site (see
/// [`is_reserved_attribute_key`]). A validated list would have to grow for
/// every deployment-specific attribute, which is exactly the coupling
/// `SpanAttributes` exists to avoid.
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct ExtensionRegion {
    /// The pipeline event that carries extension attributes.
    pub(crate) carrier: &'static str,
    /// The span extension attributes are stamped onto.
    pub(crate) target_span: &'static str,
    /// Key prefixes owned by this schema.
    pub(crate) reserved_prefixes: &'static [&'static str],
    pub(crate) rule: &'static str,
    pub(crate) value_types: &'static str,
    /// What is emitted when a deployment writes into the reserved region.
    pub(crate) diagnostic: &'static str,
}

/// A rule that a re-implementation can break without any error surfacing —
/// the class this whole declaration exists for.
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct Invariant {
    /// Stable slug, so a review comment can name one.
    pub(crate) id: &'static str,
    pub(crate) rule: &'static str,
    /// What goes wrong, and why nothing reports it.
    pub(crate) failure: &'static str,
}

/// The whole declaration.
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct SpanSchema {
    pub(crate) schema_version: &'static str,
    /// Instrumentation scope name stamped on every exported span. Wire
    /// contract: dashboards and collector routing rules select on it.
    pub(crate) scope_name: &'static str,
    /// Instrumentation scope version. A placeholder rather than a literal so
    /// this artifact does not churn on every release; the emitted value is
    /// `bitrouter-sdk`'s own crate version.
    pub(crate) scope_version: &'static str,
    pub(crate) resource_attributes: &'static [AttrDef],
    pub(crate) spans: &'static [SpanDef],
    /// Meter name for every instrument below. Wire contract, same as
    /// `scope_name`.
    pub(crate) meter_name: &'static str,
    pub(crate) metrics: &'static [MetricDef],
    pub(crate) extension_region: ExtensionRegion,
    pub(crate) invariants: &'static [Invariant],
}

/// The declaration itself.
pub(crate) const SCHEMA: SpanSchema = SpanSchema {
    schema_version: SCHEMA_VERSION,
    scope_name: "io.bitrouter.observe",
    scope_version: "{bitrouter-sdk crate version}",
    resource_attributes: RESOURCE_ATTRIBUTES,
    spans: SPANS,
    meter_name: "bitrouter",
    metrics: METRICS,
    extension_region: EXTENSION_REGION,
    invariants: INVARIANTS,
};

const RESOURCE_ATTRIBUTES: &[AttrDef] = &[
    AttrDef {
        key: "service.name",
        ty: AttrType::String,
        requirement: Requirement::Required,
        note: "Configured service name; defaults to the deployment's own name.",
    },
    AttrDef {
        key: "service.version",
        ty: AttrType::String,
        requirement: Requirement::Required,
        note: "The emitting crate's version.",
    },
];

const SPANS: &[SpanDef] = &[
    SERVER_SPAN,
    ROOT_CHAT_SPAN,
    ROUTE_SPAN,
    HOP_CHAT_SPAN,
    SETTLE_SPAN,
    INVOKE_AGENT_SPAN,
    EXECUTE_TOOL_SPAN,
];

/// HTTP ingress. Emitted by the host's inbound layer, not by the exporter, so
/// a deployment that fronts BitRouter with its own server owns this span — it
/// is declared here because the root `chat` span's parentage depends on it.
const SERVER_SPAN: SpanDef = SpanDef {
    name: "{http.request.method} {http.route}",
    kind: SpanKind::Server,
    parent: "The inbound W3C `traceparent` when the caller sent one; otherwise a trace root.",
    note: "One per inbound HTTP request. Every span below is a descendant of it.",
    attributes: &[
        AttrDef {
            key: "http.request.method",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "HTTP method.",
        },
        AttrDef {
            key: "http.route",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Request path.",
        },
        AttrDef {
            key: "url.path",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Request path, as the URL semconv spells it.",
        },
        AttrDef {
            key: "http.response.status_code",
            ty: AttrType::Int,
            requirement: Requirement::Conditional,
            note: "Stamped when the response head is produced. Absent only if no head was \
                   produced at all. Note the span itself outlives this: it ends when the \
                   response *body* is drained or dropped, so its duration covers a streamed \
                   generation and a mid-stream disconnect still exports the status the head \
                   carried.",
        },
    ],
    events: &[],
};

/// The one gen_ai *generation* per request. Every `gen_ai.*` attribute in the
/// tree lives here — see the `single-generation` invariant.
const ROOT_CHAT_SPAN: SpanDef = SpanDef {
    name: "chat {gen_ai.request.model}",
    kind: SpanKind::Internal,
    parent: "The ingress SERVER span when one exists; otherwise the inbound W3C trace context; \
             otherwise a trace root.",
    note: "Full request lifetime, named for the model the caller asked for — not the model that \
           ultimately served it.",
    attributes: &[
        AttrDef {
            key: "bitrouter.request_id",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Per-request id; the join key to logs and settlement records.",
        },
        AttrDef {
            key: "bitrouter.api_key_id",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Calling key's id. Raw on the span — cardinality caps apply to metrics only.",
        },
        AttrDef {
            key: "bitrouter.user_id",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Calling user's id. Raw on the span, as above.",
        },
        AttrDef {
            key: "gen_ai.operation.name",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Always `chat` on this span.",
        },
        AttrDef {
            key: "gen_ai.request.model",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Model the caller requested.",
        },
        AttrDef {
            key: "gen_ai.request.temperature",
            ty: AttrType::Double,
            requirement: Requirement::Conditional,
            note: "When the request set it.",
        },
        AttrDef {
            key: "gen_ai.request.top_p",
            ty: AttrType::Double,
            requirement: Requirement::Conditional,
            note: "When the request set it.",
        },
        AttrDef {
            key: "gen_ai.request.max_tokens",
            ty: AttrType::Int,
            requirement: Requirement::Conditional,
            note: "When the request set it.",
        },
        AttrDef {
            key: "gen_ai.request.seed",
            ty: AttrType::Int,
            requirement: Requirement::Conditional,
            note: "Read opportunistically from the request's untyped `extra` map.",
        },
        AttrDef {
            key: "gen_ai.request.frequency_penalty",
            ty: AttrType::Double,
            requirement: Requirement::Conditional,
            note: "Read opportunistically from the request's untyped `extra` map.",
        },
        AttrDef {
            key: "gen_ai.request.presence_penalty",
            ty: AttrType::Double,
            requirement: Requirement::Conditional,
            note: "Read opportunistically from the request's untyped `extra` map.",
        },
        AttrDef {
            key: "gen_ai.request.stop_sequences",
            ty: AttrType::StringArray,
            requirement: Requirement::Conditional,
            note: "Read from the request's `stop` extra; omitted when it holds no strings.",
        },
        AttrDef {
            key: "$screen_name",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Vendor-specific: PostHog's \"URL / Screen\" column reads this key. Set once at \
                   request end to `<provider>/<model>`, or to the requested model when no \
                   provider was reached.",
        },
        AttrDef {
            key: "bitrouter.request_duration_ms",
            ty: AttrType::Int,
            requirement: Requirement::Required,
            note: "End-to-end request latency.",
        },
        AttrDef {
            key: "bitrouter.outcome",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "One of `completed`, `failed`, `disconnected`.",
        },
        AttrDef {
            key: "bitrouter.provider_id",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "Provider that served the request; absent when none was reached.",
        },
        AttrDef {
            key: "bitrouter.model_id",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "Model that served the request; absent when none was reached.",
        },
        AttrDef {
            key: "bitrouter.upstream_duration_ms",
            ty: AttrType::Int,
            requirement: Requirement::Conditional,
            note: "Time spent in the upstream call, when the executor reported it.",
        },
        AttrDef {
            key: "bitrouter.account_label",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "Label of the credential used, when the target carried one.",
        },
        AttrDef {
            key: "bitrouter.ttft_ms",
            ty: AttrType::Int,
            requirement: Requirement::Conditional,
            note: "Time to first token, for a streamed response.",
        },
        AttrDef {
            key: "bitrouter.first_token_kind",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "Which delta arrived first — reasoning, text or tool call.",
        },
        AttrDef {
            key: "bitrouter.generation_duration_ms",
            ty: AttrType::Int,
            requirement: Requirement::Conditional,
            note: "First-token-to-last-token span of a streamed generation.",
        },
        AttrDef {
            key: "gen_ai.provider.name",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "Serving provider. The semconv's current key — it replaced `gen_ai.system`.",
        },
        AttrDef {
            key: "gen_ai.response.model",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "Model that actually served the request, which may differ from the requested one.",
        },
        AttrDef {
            key: "gen_ai.response.id",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "Gateway-observable response id — see the `gateway-observable-ids` invariant.",
        },
        AttrDef {
            key: "gen_ai.response.finish_reasons",
            ty: AttrType::StringArray,
            requirement: Requirement::Conditional,
            note: "Single-element array; the semconv types this as an array.",
        },
        AttrDef {
            key: "gen_ai.response.time_to_first_chunk",
            ty: AttrType::Double,
            requirement: Requirement::Conditional,
            note: "Time to first token in *seconds*, as the semconv requires. Measured from the \
                   first content delta, never from response headers.",
        },
        AttrDef {
            key: "gen_ai.usage.input_tokens",
            ty: AttrType::Int,
            requirement: Requirement::Conditional,
            note: "When the upstream reported usage.",
        },
        AttrDef {
            key: "gen_ai.usage.output_tokens",
            ty: AttrType::Int,
            requirement: Requirement::Conditional,
            note: "When the upstream reported usage.",
        },
        AttrDef {
            key: "gen_ai.usage.reasoning_tokens",
            ty: AttrType::Int,
            requirement: Requirement::Conditional,
            note: "Only when non-zero.",
        },
        AttrDef {
            key: "gen_ai.input.messages",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "JSON-encoded prompt messages. Only under full content capture, which is \
                   off by default; truncated at the configured byte cap on a char boundary.",
        },
        AttrDef {
            key: "gen_ai.output.messages",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "JSON-encoded response content blocks, from the IR for a non-streamed \
                   response and reassembled from deltas for a streamed one. Full content \
                   capture only.",
        },
        AttrDef {
            key: "error.type",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "Error class, kept low-cardinality so metrics can dimension on it. The \
                   human-readable message rides the `exception` event instead.",
        },
    ],
    events: &[EXCEPTION_EVENT, TOOL_CALL_STARTED_EVENT],
};

/// Routing decision.
const ROUTE_SPAN: SpanDef = SpanDef {
    name: "route",
    kind: SpanKind::Internal,
    parent: "The root `chat` span.",
    note: "Brief span recording what the router decided. Carries no `gen_ai.*` attribute.",
    attributes: &[
        AttrDef {
            key: "bitrouter.route_chain_length",
            ty: AttrType::Int,
            requirement: Requirement::Conditional,
            note: "Number of candidates in the resolved chain; absent when routing produced none.",
        },
        AttrDef {
            key: "bitrouter.route_head_provider",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "Provider of the first candidate.",
        },
        AttrDef {
            key: "bitrouter.route_head_model",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "Model of the first candidate.",
        },
    ],
    events: &[],
};

/// One upstream attempt. A failover request emits several.
const HOP_CHAT_SPAN: SpanDef = SpanDef {
    name: "chat {bitrouter.model_id}",
    kind: SpanKind::Client,
    parent: "The root `chat` span.",
    note: "A plain HTTP client span for one upstream attempt — *not* a gen_ai generation. See \
           the `single-generation` invariant. Distinguished from the root `chat` span by kind: \
           this one is CLIENT, the root is INTERNAL.",
    attributes: &[
        AttrDef {
            key: "bitrouter.provider_id",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Provider this attempt targeted.",
        },
        AttrDef {
            key: "bitrouter.model_id",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Upstream model id this attempt targeted.",
        },
        AttrDef {
            key: "bitrouter.account_label",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "Label of the credential used, when the target carried one.",
        },
        AttrDef {
            key: "bitrouter.upstream_duration_ms",
            ty: AttrType::Int,
            requirement: Requirement::Conditional,
            note: "Time in the upstream call, when the executor reported it.",
        },
        AttrDef {
            key: "server.address",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "Upstream host, parsed from the effective API base.",
        },
        AttrDef {
            key: "server.port",
            ty: AttrType::Int,
            requirement: Requirement::Conditional,
            note: "Upstream port, when the API base spelled one.",
        },
        AttrDef {
            key: "error.type",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "Error class for a failed attempt, or `stream_dropped` / `client_disconnected` \
                   when the response body ended abnormally.",
        },
    ],
    events: &[EXCEPTION_EVENT],
};

/// Settlement summary.
const SETTLE_SPAN: SpanDef = SpanDef {
    name: "settle",
    kind: SpanKind::Internal,
    parent: "The root `chat` span.",
    note: "Brief span recording the resolved provider / model / account. Carries no `gen_ai.*` \
           attribute — see the `single-generation` invariant.",
    attributes: &[
        AttrDef {
            key: "bitrouter.provider_id",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Provider that served the request.",
        },
        AttrDef {
            key: "bitrouter.model_id",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Model that served the request.",
        },
        AttrDef {
            key: "bitrouter.account_label",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "Label of the credential used, when one was recorded.",
        },
    ],
    events: &[],
};

/// One completed agent turn on the ACP path.
const INVOKE_AGENT_SPAN: SpanDef = SpanDef {
    name: "invoke_agent {gen_ai.agent.name}",
    kind: SpanKind::Internal,
    parent: "A trace root. ACP session updates carry no causal parent ids.",
    note: "Emitted once per completed prompt turn, back-dated by the turn latency so its \
           duration is real.",
    attributes: &[
        AttrDef {
            key: "gen_ai.operation.name",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Always `invoke_agent` on this span.",
        },
        AttrDef {
            key: "gen_ai.agent.name",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Configured agent id.",
        },
        AttrDef {
            key: "gen_ai.conversation.id",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "The session's stable record id. Also the join key to the HTTP plane: when the \
                   agent routes its model calls through BitRouter, one backend sees both the \
                   agent turns and the `chat` generations they caused.",
        },
        AttrDef {
            key: "bitrouter.stop_reason",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Turn stop reason, rendered as a string.",
        },
        AttrDef {
            key: "bitrouter.context.used",
            ty: AttrType::Int,
            requirement: Requirement::Conditional,
            note: "Context-window occupancy, when the upstream reported usage. See the \
                   `occupancy-is-not-usage` invariant.",
        },
        AttrDef {
            key: "bitrouter.context.size",
            ty: AttrType::Int,
            requirement: Requirement::Conditional,
            note: "Context-window size, when the upstream reported it.",
        },
    ],
    events: &[],
};

/// One terminally-reported tool call on the ACP path.
const EXECUTE_TOOL_SPAN: SpanDef = SpanDef {
    name: "execute_tool {gen_ai.tool.name}",
    kind: SpanKind::Internal,
    parent: "A trace root — a *sibling* of the turn span, not a child. ACP session updates carry \
             no causal parent ids, so correlation runs through `gen_ai.conversation.id`.",
    note: "Emitted when a tool call reports a terminal status. A tool that never finishes emits \
           no span.",
    attributes: &[
        AttrDef {
            key: "gen_ai.operation.name",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Always `execute_tool` on this span.",
        },
        AttrDef {
            key: "gen_ai.tool.name",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Tool title when the update carried one, else the tool call id.",
        },
        AttrDef {
            key: "gen_ai.conversation.id",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Correlates the tool call with its turn.",
        },
        AttrDef {
            key: "gen_ai.agent.name",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Configured agent id.",
        },
        AttrDef {
            key: "error.type",
            ty: AttrType::String,
            requirement: Requirement::Conditional,
            note: "`tool_failed` when the tool reported failure.",
        },
    ],
    events: &[],
};

const EXCEPTION_EVENT: EventDef = EventDef {
    name: "exception",
    note: "OTel exception semconv. Carries the human-readable message that deliberately stays \
           off `error.type`, which is kept low-cardinality for metric dimensions.",
    attributes: &[
        AttrDef {
            key: "exception.type",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Error class — the same value as `error.type`.",
        },
        AttrDef {
            key: "exception.message",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Rendered error.",
        },
    ],
};

const TOOL_CALL_STARTED_EVENT: EventDef = EventDef {
    name: "tool_call.started",
    note: "Emitted per streamed tool-call delta that names a tool.",
    attributes: &[AttrDef {
        key: "tool.name",
        ty: AttrType::String,
        requirement: Requirement::Required,
        note: "Tool being called.",
    }],
};

/// Dimensions carried by every request-scoped metric point.
const BASE_METRIC_DIMENSIONS: &[AttrDef] = &[
    AttrDef {
        key: "api_key_id",
        ty: AttrType::String,
        requirement: Requirement::Required,
        note: "Calling key's id, replaced by an overflow sentinel once the configured cardinality \
               cap is reached. Unprefixed for historical reasons — frozen, not exemplary.",
    },
    AttrDef {
        key: "user_id",
        ty: AttrType::String,
        requirement: Requirement::Required,
        note: "Calling user's id, capped the same way.",
    },
    AttrDef {
        key: "outcome",
        ty: AttrType::String,
        requirement: Requirement::Required,
        note: "One of `completed`, `failed`, `disconnected`.",
    },
    AttrDef {
        key: "gen_ai.provider.name",
        ty: AttrType::String,
        requirement: Requirement::Conditional,
        note: "When a provider served the request.",
    },
    AttrDef {
        key: "gen_ai.response.model",
        ty: AttrType::String,
        requirement: Requirement::Conditional,
        note: "When a provider served the request.",
    },
    AttrDef {
        key: "bitrouter.account_label",
        ty: AttrType::String,
        requirement: Requirement::Conditional,
        note: "When the serving target carried a credential label.",
    },
];

const METRICS: &[MetricDef] = &[
    MetricDef {
        name: "bitrouter.requests",
        instrument: Instrument::Counter,
        value_type: AttrType::Int,
        unit: "1",
        note: "Total requests processed.",
        dimensions: BASE_METRIC_DIMENSIONS,
    },
    MetricDef {
        name: "bitrouter.errors",
        instrument: Instrument::Counter,
        value_type: AttrType::Int,
        unit: "1",
        note: "Requests that ended in a failure. A client disconnect is not an error.",
        dimensions: BASE_METRIC_DIMENSIONS,
    },
    MetricDef {
        name: "gen_ai.client.operation.duration",
        instrument: Instrument::Histogram,
        value_type: AttrType::Double,
        unit: "s",
        note: "Request latency in seconds, per the GenAI semconv. Recorded even when an early \
               failure produced no execution result.",
        dimensions: BASE_METRIC_DIMENSIONS,
    },
    MetricDef {
        name: "gen_ai.client.token.usage",
        instrument: Instrument::Histogram,
        value_type: AttrType::Int,
        unit: "{token}",
        note: "Token usage. One instrument for both directions — see the \
               `one-token-histogram` invariant.",
        dimensions: TOKEN_USAGE_DIMENSIONS,
    },
    MetricDef {
        name: "bitrouter.stream_parts",
        instrument: Instrument::Counter,
        value_type: AttrType::Int,
        unit: "1",
        note: "Stream parts observed. Not request-scoped, so it carries none of the base \
               dimensions.",
        dimensions: &[AttrDef {
            key: "part_type",
            ty: AttrType::String,
            requirement: Requirement::Required,
            note: "Stream part discriminant, e.g. `text_delta`, `usage`, `response_completed`.",
        }],
    },
];

const TOKEN_USAGE_DIMENSIONS: &[AttrDef] = &[
    AttrDef {
        key: "api_key_id",
        ty: AttrType::String,
        requirement: Requirement::Required,
        note: "As on the base dimensions.",
    },
    AttrDef {
        key: "user_id",
        ty: AttrType::String,
        requirement: Requirement::Required,
        note: "As on the base dimensions.",
    },
    AttrDef {
        key: "outcome",
        ty: AttrType::String,
        requirement: Requirement::Required,
        note: "As on the base dimensions.",
    },
    AttrDef {
        key: "gen_ai.provider.name",
        ty: AttrType::String,
        requirement: Requirement::Required,
        note: "Token usage is only recorded when a provider served the request.",
    },
    AttrDef {
        key: "gen_ai.response.model",
        ty: AttrType::String,
        requirement: Requirement::Required,
        note: "Token usage is only recorded when a provider served the request.",
    },
    AttrDef {
        key: "bitrouter.account_label",
        ty: AttrType::String,
        requirement: Requirement::Conditional,
        note: "When the serving target carried a credential label.",
    },
    AttrDef {
        key: "gen_ai.token.type",
        ty: AttrType::String,
        requirement: Requirement::Required,
        note: "`input` or `output`. This dimension is what makes one histogram sufficient.",
    },
];

const EXTENSION_REGION: ExtensionRegion = ExtensionRegion {
    carrier: "observe.span_attributes",
    target_span: "chat {gen_ai.request.model}",
    reserved_prefixes: RESERVED_PREFIXES,
    rule: "A deployment may stamp any attribute onto the root `chat` span except keys under a \
           reserved prefix and keys this schema already declares on any span. Reserved keys are \
           dropped, not stamped: a deployment that redefined `bitrouter.*` would make the schema \
           deployment-dependent, which is the one thing it exists not to be.",
    value_types: "Scalars only — string, bool, integer, float. Null and nested JSON are skipped, \
                  because an OTel attribute value is scalar.",
    diagnostic: "One DEBUG line per dropped key on the `bitrouter::observe::span_attributes` \
                 target.",
};

const INVARIANTS: &[Invariant] = &[
    Invariant {
        id: "single-generation",
        rule: "Only the root `chat` INTERNAL span carries `gen_ai.*` attributes. The auxiliary \
               spans — `route`, the per-hop CLIENT spans, `settle` — carry `bitrouter.*` and \
               `server.*` only.",
        failure: "A gen_ai-aware backend renders any span carrying `gen_ai.*` as its own \
                  generation. Stamping a hop makes it count two generations per request and \
                  double the reported cost. Nothing errors; the bill is just wrong.",
    },
    Invariant {
        id: "occupancy-is-not-usage",
        rule: "ACP agent spans never carry `gen_ai.usage.*`. Context-window occupancy rides \
               `bitrouter.context.used` / `bitrouter.context.size` instead.",
        failure: "The substrate reports occupancy, not per-turn deltas. Writing occupancy into \
                  the token attributes corrupts every token dashboard built on them, and the \
                  numbers look plausible.",
    },
    Invariant {
        id: "gateway-observable-ids",
        rule: "`gen_ai.response.id` carries the gateway-observable id. Under the Responses \
               protocol that is an encoded gateway continuation id; the native upstream id never \
               reaches an exported span.",
        failure: "A leaked native id is not resolvable through the gateway, so continuation \
                  breaks for anyone who reads it off a trace.",
    },
    Invariant {
        id: "one-token-histogram",
        rule: "`gen_ai.client.token.usage` is a single histogram; input versus output is the \
               `gen_ai.token.type` dimension.",
        failure: "Registering a second same-named instrument is a duplicate-instrument conflict: \
                  the SDK warns once and merges them anyway, so the split silently does nothing.",
    },
    Invariant {
        id: "span-cardinality-is-unbounded",
        rule: "Cardinality caps apply to metric dimensions only. Spans carry raw `api_key_id` / \
               `user_id` values.",
        failure: "Capping span attributes trades per-tenant debug fidelity for storage the \
                  tracing backend was never going to charge for. Cardinality is a metrics \
                  concern.",
    },
    Invariant {
        id: "wire-visible-names",
        rule: "The scope name, the meter name, and the instrument names are wire contract.",
        failure: "Dashboards and collector routing rules select on them. Renaming one to track \
                  the crate that happens to host the code breaks every consumer, and the export \
                  keeps succeeding.",
    },
];

/// Whether `key` falls inside the region this schema owns, and so must not be
/// stamped from a deployment's `SpanAttributes`.
///
/// Checked against the declaration rather than a hand-maintained list, so a
/// key added to any span above is reserved from the moment it is declared.
pub(crate) fn is_reserved_attribute_key(key: &str) -> bool {
    RESERVED_PREFIXES
        .iter()
        .any(|prefix| key.starts_with(prefix))
        || SPANS
            .iter()
            .flat_map(|span| span.attributes.iter())
            .any(|attr| attr.key == key)
}

/// Whether an emitted attribute value has the type this schema declares.
///
/// `observed` is the OTel value's discriminant, spelled as one of `string`,
/// `int`, `double`, `bool`, `string_array` — a string rather than the OTel
/// type, so this module keeps naming no `opentelemetry` type even in its test
/// surface. Callers do the one-line mapping.
///
/// Exists because the conformance tests originally checked *keys* and
/// requiredness but never types, leaving a third of the committed contract
/// unenforced: an attribute drifting from `Int` to `Double` would have passed
/// every gate while `span-schema.json` kept promising `int` to anyone
/// implementing against it.
#[cfg(test)]
pub(crate) fn value_type_matches(declared: AttrType, observed: &str) -> bool {
    matches!(
        (declared, observed),
        (AttrType::String, "string")
            | (AttrType::Int, "int")
            | (AttrType::Double, "double")
            | (AttrType::StringArray, "string_array")
    )
}

/// The literal part of a span name — everything before the first
/// interpolated `{...}` segment. Empty when the whole name is interpolated,
/// as it is for the ingress SERVER span.
#[cfg(test)]
fn literal_prefix(name: &'static str) -> &'static str {
    match name.find('{') {
        Some(index) => &name[..index],
        None => name,
    }
}

/// Resolve the declaration for an exported span.
///
/// Matching is on the literal name prefix *together with* the kind: `chat `
/// alone is ambiguous, because the root generation (INTERNAL) and every
/// upstream hop (CLIENT) share it, and that ambiguity is load-bearing — the
/// two spans are deliberately named alike and deliberately kinded apart. A
/// declaration whose name is entirely interpolated matches on kind alone.
///
/// Lives here rather than in either test module so `exporter.rs` and `acp.rs`
/// check their exports against the same rule.
#[cfg(test)]
pub(crate) fn span_def_for(name: &str, kind: SpanKind) -> Option<&'static SpanDef> {
    SPANS.iter().find(|span| {
        span.kind == kind && {
            let prefix = literal_prefix(span.name);
            prefix.is_empty() || name.starts_with(prefix)
        }
    })
}

/// Render the declaration as the committed JSON artifact.
///
/// The output is `crates/bitrouter-sdk/span-schema.json`, kept in step by the
/// test below. Ordering is declaration order throughout, so the diff is
/// stable, and a schema change shows up in review as a diff of the wire
/// surface rather than as a line inside `exporter.rs`.
///
/// Returns the serializer's error only if the declaration itself cannot be
/// serialized, which is unreachable for a tree of `&'static str` — but it is
/// returned rather than unwrapped, because this crate does not panic.
pub fn render_json() -> Result<String, serde_json::Error> {
    let mut rendered = serde_json::to_string_pretty(&SCHEMA)?;
    rendered.push('\n');
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regeneration command, quoted in the failure message so the fix is in
    /// front of whoever hits it.
    const REGENERATE: &str =
        "UPDATE_SPAN_SCHEMA=1 cargo test -p bitrouter-sdk --all-features committed_artifact";

    fn artifact_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("span-schema.json")
    }

    #[test]
    fn the_committed_artifact_matches_the_declaration() {
        // The diff-in-CI half of the guard. It rides the ordinary test job
        // rather than a bespoke one: `cargo nextest run --all-features` is
        // already what CI and CLAUDE.md run, and a schema artifact nobody
        // regenerates is worse than no artifact at all.
        //
        // Deliberately not under `dist/`: that tree is `dist-helper`'s, and
        // rendering this from `dist-helper` would put `bitrouter-sdk/otel` —
        // and the whole OpenTelemetry stack — into that helper's own
        // dependency tree, which the `feature-isolation` CI job forbids by
        // name. So the artifact lives beside `public-api-deps.txt`, the
        // crate's other generated, review-facing manifest.
        let path = artifact_path();
        let rendered = render_json().expect("a tree of &'static str serializes");

        if std::env::var_os("UPDATE_SPAN_SCHEMA").is_some() {
            std::fs::write(&path, &rendered).expect("writing the span-schema artifact");
            return;
        }

        let committed = std::fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!("reading {} ({err}) - run `{REGENERATE}`", path.display())
        });
        assert_eq!(
            committed,
            rendered,
            "{} is stale - run `{REGENERATE}` and commit the result",
            path.display()
        );
    }

    #[test]
    fn the_declaration_renders() {
        let rendered = render_json().expect("a tree of &'static str serializes");
        assert!(rendered.ends_with('\n'), "artifact ends with a newline");
        assert!(rendered.contains("io.bitrouter.observe"));
    }

    #[test]
    fn reserved_region_covers_the_vocabulary_and_leaves_the_hatch_open() {
        // Prefix-owned.
        assert!(is_reserved_attribute_key("bitrouter.retry_count"));
        assert!(is_reserved_attribute_key("gen_ai.usage.input_tokens"));
        // Declared without a reserved prefix — reserved by declaration, which
        // is why the check reads the declaration instead of a literal list.
        assert!(is_reserved_attribute_key("$screen_name"));
        assert!(is_reserved_attribute_key("error.type"));
        assert!(is_reserved_attribute_key("server.address"));
        // The open region: everything a deployment would actually add.
        assert!(!is_reserved_attribute_key("$ai_total_cost_usd"));
        assert!(!is_reserved_attribute_key("namespace"));
        assert!(!is_reserved_attribute_key("routing_profile"));
        // A near-miss on a reserved prefix is not reserved. `bitrouter_cost`
        // has no dot, so it is outside the vocabulary.
        assert!(!is_reserved_attribute_key("bitrouter_cost"));
    }

    #[test]
    fn every_declared_span_resolves_to_itself() {
        // The matcher has to be unambiguous or both conformance tests would
        // check exported spans against the wrong declaration and pass.
        for span in SCHEMA.spans {
            let sample = format!("{}sample", literal_prefix(span.name));
            let resolved = span_def_for(&sample, span.kind)
                .unwrap_or_else(|| panic!("{} does not resolve through span_def_for", span.name));
            assert_eq!(
                resolved.name, span.name,
                "{} resolves to {} — the (name prefix, kind) pair is ambiguous",
                span.name, resolved.name
            );
        }
    }

    #[test]
    fn no_span_declares_the_same_key_twice() {
        for span in SCHEMA.spans {
            for (index, attr) in span.attributes.iter().enumerate() {
                assert!(
                    !span.attributes[..index].iter().any(|a| a.key == attr.key),
                    "{} declares {} twice",
                    span.name,
                    attr.key
                );
            }
        }
    }

    #[test]
    fn auxiliary_spans_declare_no_genai_attribute() {
        // The `single-generation` invariant, asserted against the declaration
        // itself — the exporter's own conformance test asserts it against the
        // spans actually emitted.
        for span in SCHEMA.spans {
            let is_root_generation =
                span.name.starts_with("chat ") && span.kind == SpanKind::Internal;
            let is_agent_span =
                span.name.starts_with("invoke_agent ") || span.name.starts_with("execute_tool ");
            if is_root_generation || is_agent_span {
                continue;
            }
            for attr in span.attributes {
                assert!(
                    !attr.key.starts_with("gen_ai."),
                    "{} declares {} — auxiliary spans are not generations",
                    span.name,
                    attr.key
                );
            }
        }
    }
}
