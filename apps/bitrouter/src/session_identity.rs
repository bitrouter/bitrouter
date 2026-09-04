//! ACP/native request-session normalization.

use std::sync::Arc;

use async_trait::async_trait;
use bitrouter_sdk::PipelineEvent;
use bitrouter_sdk::language_model::{ApiProtocol, HookDecision, PipelineContext, PreRequestHook};
use serde::Serialize;

use crate::acp_runtime::AcpRuntime;
use crate::auth::events::ApiPrincipalEstablished;
use crate::workflow_state::extractors::{ExtractorInput, parse_compatibility_harness};
use crate::workflow_state::ir::ProtocolKind;
use crate::workflow_state::session::resolve_session_signal;

const EVIDENCE_HEADERS: &[&str] = &[
    "x-bitrouter-request-id",
    "x-bitrouter-controller-id",
    "x-bitrouter-acp-session-id",
    "x-bitrouter-workflow-session",
    "x-bitrouter-parent-session-id",
    "x-bitrouter-agent-session-id",
    "x-bitrouter-agent-role",
    "x-bitrouter-context-epoch",
    "x-bitrouter-context-transition",
    "x-bitrouter-session-fingerprint",
    "x-claude-code-session-id",
    "x-claude-code-agent-id",
    "x-claude-code-parent-agent-id",
    "session-id",
    "thread-id",
    "x-codex-turn-metadata",
    "x-session-id",
    "anthropic-beta",
    "user-agent",
    "x-bitrouter-harness",
    "x-bitrouter-inbound-protocol",
];

/// Whether request identity came from the legacy model API path or declared
/// ACP harness correlation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestOrigin {
    /// No controller claim was present.
    PureModelApi,
    /// The request declared an ACP controller namespace.
    AcpHarnessRequest,
}

/// Source-independent native agent identity observed on the model request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NativeSessionIdentity {
    /// Recognized harness (`claude_code` or `codex`).
    pub harness: Option<String>,
    /// Root conversation/session identity.
    pub root_session_id: Option<String>,
    /// Exact child/thread identity.
    pub agent_thread_id: Option<String>,
    /// Native parent lineage.
    pub parent_agent_thread_id: Option<String>,
    /// Native turn identity.
    pub turn_id: Option<String>,
}

/// One privacy-reviewed identity signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentityEvidence {
    /// `header`, `body`, or `derived`.
    pub transport: String,
    /// Exact transport field or parsed semantic sub-field.
    pub field: String,
    /// Signal family (`bitrouter`, `claude_code`, `codex`, or `legacy`).
    pub source: String,
    /// Whether this exact evidence value participated in route lookup.
    pub used_for_route_match: bool,
    /// `raw`, `stable_digest`, or `presence_only`.
    pub value_representation: String,
    /// Value when the reviewed representation permits it.
    pub value: Option<String>,
}

/// A disagreement retained for diagnostics instead of erasing evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentityConflict {
    /// Field whose claim lost precedence.
    pub field: String,
    /// Higher-authority value.
    pub expected: Option<String>,
    /// Conflicting observed value.
    pub observed: Option<String>,
    /// Resolution applied.
    pub resolution: String,
}

/// Route-lease lookup and precedence decision for one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteLeaseObservation {
    /// Opaque runtime lease id.
    pub lease_id: String,
    /// Candidate that matched the lease.
    pub matched_session_id: String,
    /// Selected BitRouter route vocabulary value.
    pub route: String,
    /// Whether the route changed this request's effective model.
    pub applied: bool,
    /// Applied/skipped reason code.
    pub reason: String,
}

/// Full request-scoped normalized session context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestSessionContext {
    /// Attribution origin.
    pub origin: RequestOrigin,
    /// Non-secret public API-key identity, or `local` under `skip_auth`.
    pub api_principal_id: String,
    /// Caller-declared static controller header.
    pub claimed_controller_instance_id: Option<String>,
    /// Caller-declared dynamic ACP session header.
    pub acp_session_id: Option<String>,
    /// Native Claude/Codex identity.
    pub native: NativeSessionIdentity,
    /// Existing pure model API compatibility projection.
    pub legacy_workflow_session_id: Option<String>,
    /// Provider-side Responses continuation identity.
    pub api_continuation_id: Option<String>,
    /// Complete reviewed evidence inventory observed on this request.
    pub evidence: Vec<IdentityEvidence>,
    /// Structured conflicts.
    pub conflicts: Vec<IdentityConflict>,
    /// Matching route lease and precedence result, when any.
    pub route_lease: Option<RouteLeaseObservation>,
}

/// Settlement-safe identity event emitted exactly once for every model request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionIdentityObserved {
    /// Router request id joining trace, route, and metering artifacts.
    pub router_request_id: String,
    /// Attribution origin.
    pub origin: RequestOrigin,
    /// Non-secret public API-key identity, or `local` under `skip_auth`.
    pub api_principal_id: String,
    /// Recognized harness.
    pub harness: Option<String>,
    /// Caller-declared controller identity.
    pub claimed_controller_instance_id: Option<String>,
    /// Caller-declared ACP session identity.
    pub acp_session_id: Option<String>,
    /// Native root session.
    pub native_root_session_id: Option<String>,
    /// Native exact agent/thread.
    pub native_agent_thread_id: Option<String>,
    /// Native parent lineage.
    pub native_parent_agent_thread_id: Option<String>,
    /// Native turn.
    pub native_turn_id: Option<String>,
    /// Legacy workflow projection.
    pub legacy_workflow_session_id: Option<String>,
    /// Responses continuation.
    pub api_continuation_id: Option<String>,
    /// Reviewed evidence.
    pub evidence: Vec<IdentityEvidence>,
    /// Conflicts.
    pub conflicts: Vec<IdentityConflict>,
    /// Whether any session/continuation projection was available.
    pub attributed: bool,
    /// `session` only when a lease was applied, otherwise `default`.
    pub route_scope: String,
    /// Matching route lease id, if any.
    pub route_lease_id: Option<String>,
    /// Matching lease and its applied/skipped precedence outcome, if any.
    pub route_lease_outcome: Option<RouteLeaseObservation>,
}

impl PipelineEvent for SessionIdentityObserved {
    fn event_name(&self) -> &'static str {
        "session.identity_observed"
    }
}

/// Post-auth hook that normalizes identity and applies an API-principal-scoped
/// route lease without changing legacy pure API behavior.
pub struct SessionContextHook {
    runtime: Arc<AcpRuntime>,
}

impl SessionContextHook {
    /// Build the hook over the daemon's shared in-memory ACP runtime.
    pub fn new(runtime: Arc<AcpRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl PreRequestHook for SessionContextHook {
    async fn check(&self, ctx: &mut PipelineContext) -> bitrouter_sdk::Result<HookDecision> {
        let api_principal_id = ctx.caller().api_key_id().to_string();
        let route_principal = if ctx.caller().is_local() {
            "local".to_string()
        } else {
            ctx.get_event::<ApiPrincipalEstablished>()
                .map(|event| event.route_scope_id.clone())
                .unwrap_or_else(|| api_principal_id.clone())
        };
        let claimed_controller = header_value(ctx, "x-bitrouter-controller-id");
        let origin = if claimed_controller.is_some() {
            RequestOrigin::AcpHarnessRequest
        } else {
            RequestOrigin::PureModelApi
        };
        let acp_session_id = header_value(ctx, "x-bitrouter-acp-session-id");
        let mut conflicts = Vec::new();
        let mut evidence = header_evidence(ctx);
        let raw_body = canonical_extra_body(ctx);
        let protocol_kind = protocol_kind(ctx.inbound_protocol());
        let harness_hint = header_value(ctx, "x-bitrouter-harness")
            .and_then(|value| parse_compatibility_harness(&value));
        let legacy = resolve_session_signal(&ExtractorInput {
            harness_hint,
            protocol_hint: protocol_kind,
            headers: ctx.headers(),
            raw_body: &raw_body,
            prompt: ctx.prompt(),
        });
        for legacy_evidence in legacy.evidence {
            evidence.push(IdentityEvidence {
                transport: "derived".to_string(),
                field: legacy_evidence.value,
                source: "legacy".to_string(),
                used_for_route_match: false,
                value_representation: "presence_only".to_string(),
                value: None,
            });
        }

        let api_continuation_id = extra_string(ctx, "previous_response_id");
        if let Some(value) = &api_continuation_id {
            evidence.push(body_evidence(
                "previous_response_id",
                "api_continuation",
                value.clone(),
                false,
            ));
        }

        let mut native = extract_native(ctx, &mut evidence, &mut conflicts);
        if let (Some(dynamic), Some(root)) =
            (acp_session_id.as_deref(), native.root_session_id.as_deref())
            && dynamic != root
        {
            conflicts.push(IdentityConflict {
                field: "native.root_session_id".to_string(),
                expected: Some(dynamic.to_string()),
                observed: Some(root.to_string()),
                resolution: "dynamic_acp_session_lookup_precedes_native".to_string(),
            });
        }

        let route_lease = claimed_controller.as_deref().and_then(|controller| {
            let mut candidates = Vec::new();
            push_unique(&mut candidates, acp_session_id.as_deref());
            match native.harness.as_deref() {
                Some("claude_code") => {
                    push_unique(&mut candidates, native.root_session_id.as_deref());
                }
                Some("codex") => {
                    push_unique(&mut candidates, native.agent_thread_id.as_deref());
                    push_unique(&mut candidates, native.root_session_id.as_deref());
                }
                _ => {}
            }
            mark_route_evidence(&mut evidence, &native, acp_session_id.as_deref());
            let candidate_refs = candidates.iter().map(String::as_str).collect::<Vec<_>>();
            self.runtime
                .resolve_route(&route_principal, controller, &candidate_refs)
                .map(|lease| {
                    let (applied, reason) = if api_continuation_id.is_some() {
                        (false, "continuation_precedence")
                    } else if explicit_caller_route(ctx.original_model()) {
                        (false, "explicit_caller_route_precedence")
                    } else {
                        ctx.set_model(lease.route());
                        (true, "applied")
                    };
                    RouteLeaseObservation {
                        lease_id: lease.lease_id().to_string(),
                        matched_session_id: lease.matched_session_id().to_string(),
                        route: lease.route().to_string(),
                        applied,
                        reason: reason.to_string(),
                    }
                })
        });

        let normalized = RequestSessionContext {
            origin,
            api_principal_id,
            claimed_controller_instance_id: claimed_controller,
            acp_session_id,
            native: std::mem::take(&mut native),
            legacy_workflow_session_id: legacy.signal.key,
            api_continuation_id,
            evidence,
            conflicts,
            route_lease,
        };
        let event = event_from_context(ctx.request_id(), &normalized);
        ctx.insert_extension(Arc::new(normalized));
        ctx.emit(event);
        Ok(HookDecision::Allow)
    }
}

fn event_from_context(
    router_request_id: &str,
    context: &RequestSessionContext,
) -> SessionIdentityObserved {
    let attributed = context.acp_session_id.is_some()
        || context.native.root_session_id.is_some()
        || context.native.agent_thread_id.is_some()
        || context.legacy_workflow_session_id.is_some()
        || context.api_continuation_id.is_some();
    SessionIdentityObserved {
        router_request_id: router_request_id.to_string(),
        origin: context.origin,
        api_principal_id: context.api_principal_id.clone(),
        harness: context.native.harness.clone(),
        claimed_controller_instance_id: context.claimed_controller_instance_id.clone(),
        acp_session_id: context.acp_session_id.clone(),
        native_root_session_id: context.native.root_session_id.clone(),
        native_agent_thread_id: context.native.agent_thread_id.clone(),
        native_parent_agent_thread_id: context.native.parent_agent_thread_id.clone(),
        native_turn_id: context.native.turn_id.clone(),
        legacy_workflow_session_id: context.legacy_workflow_session_id.clone(),
        api_continuation_id: context.api_continuation_id.clone(),
        evidence: context.evidence.clone(),
        conflicts: context.conflicts.clone(),
        attributed,
        route_scope: if context
            .route_lease
            .as_ref()
            .is_some_and(|lease| lease.applied)
        {
            "session".to_string()
        } else {
            "default".to_string()
        },
        route_lease_id: context
            .route_lease
            .as_ref()
            .map(|lease| lease.lease_id.clone()),
        route_lease_outcome: context.route_lease.clone(),
    }
}

fn header_evidence(ctx: &PipelineContext) -> Vec<IdentityEvidence> {
    EVIDENCE_HEADERS
        .iter()
        .filter_map(|name| {
            let value = header_value(ctx, name)?;
            let source = match *name {
                "x-bitrouter-acp-session-id" => "bitrouter",
                "x-claude-code-session-id" => "claude_code",
                "session-id" | "thread-id" => "codex",
                name if name.starts_with("x-claude-code-") => "claude_code",
                "x-codex-turn-metadata" => "codex",
                _ => "bitrouter",
            };
            let compound = *name == "x-codex-turn-metadata";
            Some(IdentityEvidence {
                transport: "header".to_string(),
                field: (*name).to_string(),
                source: source.to_string(),
                used_for_route_match: false,
                value_representation: if compound { "presence_only" } else { "raw" }.to_string(),
                value: (!compound).then_some(value),
            })
        })
        .collect()
}

fn mark_route_evidence(
    evidence: &mut [IdentityEvidence],
    native: &NativeSessionIdentity,
    acp_session_id: Option<&str>,
) {
    for item in evidence {
        let selected_native = match item.field.as_str() {
            "x-claude-code-session-id" | "metadata.user_id.session_id"
                if native.harness.as_deref() == Some("claude_code") =>
            {
                native.root_session_id.as_deref()
            }
            "session-id" | "client_metadata.session_id"
                if native.harness.as_deref() == Some("codex") =>
            {
                native.root_session_id.as_deref()
            }
            "thread-id" | "client_metadata.thread_id"
                if native.harness.as_deref() == Some("codex") =>
            {
                native.agent_thread_id.as_deref()
            }
            _ => None,
        };
        item.used_for_route_match = match item.field.as_str() {
            "x-bitrouter-controller-id" => {
                acp_session_id.is_some()
                    || native.root_session_id.is_some()
                    || native.agent_thread_id.is_some()
            }
            "x-bitrouter-acp-session-id" => item.value.as_deref() == acp_session_id,
            _ => selected_native.is_some() && item.value.as_deref() == selected_native,
        };
    }
}

fn extract_native(
    ctx: &PipelineContext,
    evidence: &mut Vec<IdentityEvidence>,
    conflicts: &mut Vec<IdentityConflict>,
) -> NativeSessionIdentity {
    let claude_session = header_value(ctx, "x-claude-code-session-id");
    let claude_agent = header_value(ctx, "x-claude-code-agent-id");
    let claude_parent = header_value(ctx, "x-claude-code-parent-agent-id");
    let codex_session = header_value(ctx, "session-id");
    let codex_thread = header_value(ctx, "thread-id");
    let codex_turn_metadata = header_json_object(ctx, "x-codex-turn-metadata");
    let client_metadata = extra_object(ctx, "client_metadata");
    let claude_body_session = claude_metadata_session(ctx);

    let claude_native =
        claude_session.is_some() || claude_agent.is_some() || claude_parent.is_some();
    let codex_native =
        codex_session.is_some() || codex_thread.is_some() || codex_turn_metadata.is_some();
    let selector = header_value(ctx, "x-bitrouter-harness").map(|value| value.to_ascii_lowercase());
    let claude_hint = header_contains(ctx, "anthropic-beta", "claude-code")
        || selector
            .as_deref()
            .is_some_and(|value| value.contains("claude"));
    let codex_hint = header_contains(ctx, "user-agent", "codex")
        || selector
            .as_deref()
            .is_some_and(|value| value.contains("codex"));
    let harness = match (claude_native, codex_native) {
        (true, false) => Some("claude_code"),
        (false, true) => Some("codex"),
        (true, true) => {
            conflicts.push(IdentityConflict {
                field: "native.harness".to_string(),
                expected: ctx
                    .inbound_protocol()
                    .map(|protocol| protocol.as_str().to_string()),
                observed: Some("claude_code,codex".to_string()),
                resolution: "inbound_protocol_breaks_tie".to_string(),
            });
            match ctx.inbound_protocol() {
                Some(ApiProtocol::Messages) => Some("claude_code"),
                Some(ApiProtocol::Responses) => Some("codex"),
                _ => None,
            }
        }
        (false, false) if claude_hint && !codex_hint => Some("claude_code"),
        (false, false) if codex_hint && !claude_hint => Some("codex"),
        _ => None,
    };

    match harness {
        Some("claude_code") => {
            if let Some(body_session) = &claude_body_session {
                evidence.push(body_evidence(
                    "metadata.user_id.session_id",
                    "claude_code",
                    body_session.clone(),
                    false,
                ));
            }
            compare_sources(
                conflicts,
                "body.metadata.user_id.session_id",
                claude_session.as_deref(),
                claude_body_session.as_deref(),
                "native_header_wins",
            );
            NativeSessionIdentity {
                harness: Some("claude_code".to_string()),
                root_session_id: claude_session.or(claude_body_session),
                agent_thread_id: claude_agent,
                parent_agent_thread_id: claude_parent,
                turn_id: None,
            }
        }
        Some("codex") => {
            let body_thread = object_string(client_metadata.as_ref(), &["thread_id", "threadId"]);
            let body_session =
                object_string(client_metadata.as_ref(), &["session_id", "sessionId"]);
            let header_turn = object_string(codex_turn_metadata.as_ref(), &["turn_id", "turnId"]);
            let body_turn = object_string(client_metadata.as_ref(), &["turn_id", "turnId"]);
            let header_parent = object_string(
                codex_turn_metadata.as_ref(),
                &[
                    "parent_thread_id",
                    "parentThreadId",
                    "parent_agent_thread_id",
                ],
            );
            let body_parent = object_string(
                client_metadata.as_ref(),
                &[
                    "parent_thread_id",
                    "parentThreadId",
                    "parent_agent_thread_id",
                ],
            );
            for (field, value) in [
                ("client_metadata.session_id", body_session.as_ref()),
                ("client_metadata.thread_id", body_thread.as_ref()),
                ("client_metadata.turn_id", body_turn.as_ref()),
                ("client_metadata.parent_thread_id", body_parent.as_ref()),
                ("x-codex-turn-metadata.turn_id", header_turn.as_ref()),
                (
                    "x-codex-turn-metadata.parent_thread_id",
                    header_parent.as_ref(),
                ),
            ] {
                if let Some(value) = value {
                    evidence.push(body_evidence(field, "codex", value.clone(), false));
                }
            }
            compare_sources(
                conflicts,
                "body.client_metadata.session_id",
                codex_session.as_deref(),
                body_session.as_deref(),
                "native_header_wins",
            );
            compare_sources(
                conflicts,
                "body.client_metadata.thread_id",
                codex_thread.as_deref(),
                body_thread.as_deref(),
                "native_header_wins",
            );
            compare_sources(
                conflicts,
                "body.client_metadata.turn_id",
                header_turn.as_deref(),
                body_turn.as_deref(),
                "turn_metadata_header_wins",
            );
            compare_sources(
                conflicts,
                "body.client_metadata.parent_thread_id",
                header_parent.as_deref(),
                body_parent.as_deref(),
                "turn_metadata_header_wins",
            );
            NativeSessionIdentity {
                harness: Some("codex".to_string()),
                root_session_id: codex_session.or(body_session),
                agent_thread_id: codex_thread.or(body_thread),
                parent_agent_thread_id: header_parent.or(body_parent),
                turn_id: header_turn.or(body_turn),
            }
        }
        _ => NativeSessionIdentity::default(),
    }
}

fn compare_sources(
    conflicts: &mut Vec<IdentityConflict>,
    field: &str,
    winner: Option<&str>,
    observed: Option<&str>,
    resolution: &str,
) {
    if let (Some(winner), Some(observed)) = (winner, observed)
        && winner != observed
    {
        conflicts.push(IdentityConflict {
            field: field.to_string(),
            expected: Some(winner.to_string()),
            observed: Some(observed.to_string()),
            resolution: resolution.to_string(),
        });
    }
}

fn body_evidence(
    field: &str,
    source: &str,
    value: String,
    used_for_route_match: bool,
) -> IdentityEvidence {
    IdentityEvidence {
        transport: "body".to_string(),
        field: field.to_string(),
        source: source.to_string(),
        used_for_route_match,
        value_representation: "raw".to_string(),
        value: Some(value),
    }
}

fn canonical_extra_body(ctx: &PipelineContext) -> serde_json::Value {
    serde_json::Value::Object(
        ctx.prompt()
            .params
            .extra
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
    )
}

fn protocol_kind(protocol: Option<ApiProtocol>) -> ProtocolKind {
    match protocol {
        Some(ApiProtocol::ChatCompletions) => ProtocolKind::ChatCompletions,
        Some(ApiProtocol::Messages) => ProtocolKind::Messages,
        Some(ApiProtocol::Responses) => ProtocolKind::Responses,
        _ => ProtocolKind::Unknown,
    }
}

fn header_value(ctx: &PipelineContext, name: &str) -> Option<String> {
    ctx.headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn header_contains(ctx: &PipelineContext, name: &str, needle: &str) -> bool {
    header_value(ctx, name).is_some_and(|value| {
        value
            .to_ascii_lowercase()
            .contains(&needle.to_ascii_lowercase())
    })
}

fn header_json_object(
    ctx: &PipelineContext,
    name: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    parse_object_value(&serde_json::Value::String(header_value(ctx, name)?))
}

fn extra_object(
    ctx: &PipelineContext,
    name: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    parse_object_value(ctx.prompt().params.extra.get(name)?)
}

fn parse_object_value(
    value: &serde_json::Value,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    match value {
        serde_json::Value::Object(object) => Some(object.clone()),
        serde_json::Value::String(raw) => serde_json::from_str::<serde_json::Value>(raw)
            .ok()
            .and_then(|value| value.as_object().cloned()),
        _ => None,
    }
}

fn object_string(
    object: Option<&serde_json::Map<String, serde_json::Value>>,
    names: &[&str],
) -> Option<String> {
    let object = object?;
    names.iter().find_map(|name| {
        object
            .get(*name)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

fn extra_string(ctx: &PipelineContext, name: &str) -> Option<String> {
    ctx.prompt()
        .params
        .extra
        .get(name)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn claude_metadata_session(ctx: &PipelineContext) -> Option<String> {
    let metadata = extra_object(ctx, "metadata")?;
    let user_id = metadata
        .get("user_id")
        .and_then(serde_json::Value::as_str)?;
    let user = serde_json::from_str::<serde_json::Value>(user_id).ok()?;
    user.get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn explicit_caller_route(model: &str) -> bool {
    model.starts_with('@')
        || model.starts_with("bitrouter/")
        || model
            .split_once(':')
            .is_some_and(|(provider, service)| !provider.is_empty() && !service.is_empty())
}

fn push_unique(values: &mut Vec<String>, candidate: Option<&str>) {
    if let Some(candidate) = candidate
        && !candidate.is_empty()
        && !values.iter().any(|value| value == candidate)
    {
        values.push(candidate.to_string());
    }
}

#[cfg(test)]
mod tests {
    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::language_model::{
        ApiProtocol, GenerationParams, HookDecision, Message, PipelineContext, PipelineRequest,
        PreRequestHook, Prompt, Role,
    };
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use super::{
        RequestOrigin, RequestSessionContext, SessionContextHook, SessionIdentityObserved,
    };
    use crate::acp_runtime::AcpRuntime;
    use crate::auth::events::ApiPrincipalEstablished;

    fn context(
        model: &str,
        protocol: ApiProtocol,
        headers: &[(&str, &str)],
        extra: serde_json::Map<String, serde_json::Value>,
        route_principal: Option<&str>,
    ) -> PipelineContext {
        let prompt = Prompt {
            model: model.to_string(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "hello")],
            tools: Vec::new(),
            params: GenerationParams {
                extra_protocol: Some(protocol.clone()),
                extra: extra.into_iter().collect(),
                ..Default::default()
            },
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        let caller = if route_principal.is_some() {
            CallerContext::new("test-key", "test-user")
        } else {
            CallerContext::local()
        };
        let mut request = PipelineRequest::new(model, caller, prompt);
        request.request_id = "router-request-1".to_string();
        request.inbound_protocol = Some(protocol);
        for (name, value) in headers {
            request.headers.insert(
                http::HeaderName::from_bytes(name.as_bytes()).expect("valid test header name"),
                value.parse().expect("valid test header value"),
            );
        }
        let mut context = PipelineContext::new(request);
        if let Some(route_principal) = route_principal {
            context.emit(ApiPrincipalEstablished {
                route_scope_id: route_principal.to_string(),
            });
        }
        context
    }

    async fn observe(
        runtime: Arc<AcpRuntime>,
        context: &mut PipelineContext,
    ) -> Arc<RequestSessionContext> {
        assert!(matches!(
            SessionContextHook::new(runtime)
                .check(context)
                .await
                .expect("session hook"),
            HookDecision::Allow
        ));
        context
            .extension::<RequestSessionContext>()
            .expect("normalized request context")
    }

    #[tokio::test]
    async fn pure_model_api_keeps_the_legacy_workflow_projection() {
        let runtime = Arc::new(AcpRuntime::new());
        let mut context = context(
            "gpt-5",
            ApiProtocol::Responses,
            &[
                ("x-bitrouter-workflow-session", "legacy-session"),
                ("x-session-id", "generic-must-not-win"),
            ],
            serde_json::Map::new(),
            None,
        );

        let normalized = observe(runtime, &mut context).await;

        assert_eq!(normalized.origin, RequestOrigin::PureModelApi);
        assert_eq!(
            normalized.legacy_workflow_session_id.as_deref(),
            Some("legacy-session")
        );
        assert_eq!(normalized.api_principal_id, "local");
        assert!(normalized.claimed_controller_instance_id.is_none());
        assert!(normalized.acp_session_id.is_none());
        assert_eq!(context.model(), "gpt-5");
    }

    #[tokio::test]
    async fn declared_claude_identity_applies_the_matching_session_lease() {
        let runtime = Arc::new(AcpRuntime::new());
        runtime
            .set_route(
                "principal",
                "brc_claude",
                "claude-root",
                "anthropic:claude-opus",
            )
            .expect("route");
        let mut context = context(
            "claude-sonnet",
            ApiProtocol::Messages,
            &[
                ("x-bitrouter-controller-id", "brc_claude"),
                ("x-claude-code-session-id", "claude-root"),
                ("x-claude-code-agent-id", "claude-child"),
                ("x-claude-code-parent-agent-id", "claude-parent"),
            ],
            serde_json::Map::new(),
            Some("principal"),
        );

        let normalized = observe(runtime, &mut context).await;

        assert_eq!(
            normalized.native.root_session_id.as_deref(),
            Some("claude-root")
        );
        assert_eq!(
            normalized.native.agent_thread_id.as_deref(),
            Some("claude-child")
        );
        assert_eq!(
            normalized.native.parent_agent_thread_id.as_deref(),
            Some("claude-parent")
        );
        assert_eq!(context.model(), "anthropic:claude-opus");
        assert_eq!(
            normalized.route_lease.as_ref().map(|lease| lease.applied),
            Some(true)
        );
        let event = context
            .get_event::<SessionIdentityObserved>()
            .expect("identity event");
        let outcome = event
            .route_lease_outcome
            .as_ref()
            .expect("route lease outcome");
        assert_eq!(outcome.matched_session_id, "claude-root");
        assert_eq!(outcome.route, "anthropic:claude-opus");
        assert!(outcome.applied);
        assert_eq!(outcome.reason, "applied");
    }

    #[tokio::test]
    async fn codex_exact_thread_lease_wins_before_root_fallback_and_parses_turn_lineage() {
        let runtime = Arc::new(AcpRuntime::new());
        runtime
            .set_route("principal", "brc_codex", "codex-root", "openai:gpt-5")
            .expect("root route");
        runtime
            .set_route("principal", "brc_codex", "codex-child", "openai:gpt-5.5")
            .expect("child route");
        let mut extra = serde_json::Map::new();
        extra.insert(
            "client_metadata".to_string(),
            serde_json::json!({
                "turn_id": "turn-body",
                "parent_thread_id": "parent-body",
                "thread_id": "different-body-thread"
            }),
        );
        let mut context = context(
            "gpt-5-mini",
            ApiProtocol::Responses,
            &[
                ("x-bitrouter-controller-id", "brc_codex"),
                ("session-id", "codex-root"),
                ("thread-id", "codex-child"),
                (
                    "x-codex-turn-metadata",
                    r#"{"turn_id":"turn-header","parent_thread_id":"parent-header"}"#,
                ),
            ],
            extra,
            Some("principal"),
        );

        let normalized = observe(runtime, &mut context).await;

        assert_eq!(
            normalized.native.root_session_id.as_deref(),
            Some("codex-root")
        );
        assert_eq!(
            normalized.native.agent_thread_id.as_deref(),
            Some("codex-child")
        );
        assert_eq!(normalized.native.turn_id.as_deref(), Some("turn-header"));
        assert_eq!(
            normalized.native.parent_agent_thread_id.as_deref(),
            Some("parent-header")
        );
        assert_eq!(context.model(), "openai:gpt-5.5");
        assert!(!normalized.conflicts.is_empty());
    }

    #[tokio::test]
    async fn a_distinct_api_principal_cannot_activate_the_same_declared_claims() {
        let runtime = Arc::new(AcpRuntime::new());
        runtime
            .set_route(
                "principal-owner",
                "brc_shared",
                "shared-session",
                "owner:model",
            )
            .expect("owner route");
        let mut context = context(
            "logical-model",
            ApiProtocol::Messages,
            &[
                ("x-bitrouter-controller-id", "brc_shared"),
                ("x-bitrouter-acp-session-id", "shared-session"),
            ],
            serde_json::Map::new(),
            Some("principal-other"),
        );

        let normalized = observe(runtime, &mut context).await;

        assert_eq!(normalized.origin, RequestOrigin::AcpHarnessRequest);
        assert_eq!(normalized.acp_session_id.as_deref(), Some("shared-session"));
        assert_eq!(context.model(), "logical-model");
        assert!(normalized.route_lease.is_none());
    }

    #[tokio::test]
    async fn skip_auth_uses_the_shared_local_route_principal() {
        let runtime = Arc::new(AcpRuntime::new());
        runtime
            .set_route("local", "brc_local", "local-session", "local:model")
            .expect("local route");
        let mut context = context(
            "logical-model",
            ApiProtocol::Messages,
            &[
                ("x-bitrouter-controller-id", "brc_local"),
                ("x-bitrouter-acp-session-id", "local-session"),
            ],
            serde_json::Map::new(),
            None,
        );

        let normalized = observe(runtime, &mut context).await;

        assert_eq!(normalized.api_principal_id, "local");
        assert_eq!(context.model(), "local:model");
        assert_eq!(
            normalized
                .route_lease
                .as_ref()
                .map(|lease| lease.matched_session_id.as_str()),
            Some("local-session")
        );
        for field in ["x-bitrouter-controller-id", "x-bitrouter-acp-session-id"] {
            assert!(
                normalized
                    .evidence
                    .iter()
                    .any(|evidence| { evidence.field == field && evidence.used_for_route_match })
            );
        }
    }

    #[tokio::test]
    async fn explicit_caller_routes_and_continuations_are_stronger_than_a_lease() {
        let runtime = Arc::new(AcpRuntime::new());
        runtime
            .set_route("principal", "brc_precedence", "root", "lease:model")
            .expect("route");

        for model in ["explicit:model", "@careful", "bitrouter/auto"] {
            let mut context = context(
                model,
                ApiProtocol::Responses,
                &[
                    ("x-bitrouter-controller-id", "brc_precedence"),
                    ("session-id", "root"),
                ],
                serde_json::Map::new(),
                Some("principal"),
            );
            let normalized = observe(Arc::clone(&runtime), &mut context).await;
            assert_eq!(context.model(), model);
            assert_eq!(
                normalized.route_lease.as_ref().map(|lease| lease.applied),
                Some(false)
            );
            let event = context
                .get_event::<SessionIdentityObserved>()
                .expect("identity event");
            assert_eq!(
                event
                    .route_lease_outcome
                    .as_ref()
                    .map(|outcome| outcome.reason.as_str()),
                Some("explicit_caller_route_precedence")
            );
        }

        let mut extra = serde_json::Map::new();
        extra.insert(
            "previous_response_id".to_string(),
            serde_json::json!("resp_previous"),
        );
        let mut continuation = context(
            "logical-model",
            ApiProtocol::Responses,
            &[
                ("x-bitrouter-controller-id", "brc_precedence"),
                ("session-id", "root"),
            ],
            extra,
            Some("principal"),
        );
        let normalized = observe(runtime, &mut continuation).await;
        assert_eq!(continuation.model(), "logical-model");
        assert_eq!(
            normalized.api_continuation_id.as_deref(),
            Some("resp_previous")
        );
    }

    #[tokio::test]
    async fn previous_response_id_alone_is_not_native_codex_identity() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "previous_response_id".to_string(),
            serde_json::json!("resp_legacy"),
        );
        let mut context = context("gpt-5", ApiProtocol::Responses, &[], extra, None);

        let normalized = observe(Arc::new(AcpRuntime::new()), &mut context).await;

        assert!(normalized.native.harness.is_none());
        assert!(normalized.native.root_session_id.is_none());
        assert_eq!(
            normalized.legacy_workflow_session_id.as_deref(),
            Some("resp_legacy")
        );
        assert_eq!(
            normalized.api_continuation_id.as_deref(),
            Some("resp_legacy")
        );
    }

    #[tokio::test]
    async fn observed_evidence_inventory_is_explicit_and_excludes_credentials() {
        let expected = [
            "x-bitrouter-request-id",
            "x-bitrouter-controller-id",
            "x-bitrouter-acp-session-id",
            "x-bitrouter-workflow-session",
            "x-bitrouter-parent-session-id",
            "x-bitrouter-agent-session-id",
            "x-bitrouter-agent-role",
            "x-bitrouter-context-epoch",
            "x-bitrouter-context-transition",
            "x-bitrouter-session-fingerprint",
            "x-claude-code-session-id",
            "x-claude-code-agent-id",
            "x-claude-code-parent-agent-id",
            "session-id",
            "thread-id",
            "x-codex-turn-metadata",
            "x-session-id",
            "anthropic-beta",
            "user-agent",
            "x-bitrouter-harness",
            "x-bitrouter-inbound-protocol",
        ];
        let mut request_headers = expected
            .iter()
            .map(|name| (*name, "observed"))
            .collect::<Vec<_>>();
        request_headers.push(("authorization", "Bearer must-not-appear"));
        request_headers.push(("cookie", "must-not-appear"));
        let mut context = context(
            "model",
            ApiProtocol::Responses,
            &request_headers,
            serde_json::Map::new(),
            None,
        );

        let _normalized = observe(Arc::new(AcpRuntime::new()), &mut context).await;
        let event = context
            .get_event::<SessionIdentityObserved>()
            .expect("identity event");
        let fields = event
            .evidence
            .iter()
            .map(|evidence| evidence.field.as_str())
            .collect::<BTreeSet<_>>();

        for name in expected {
            assert!(fields.contains(name), "missing evidence field {name}");
        }
        assert!(!fields.contains("authorization"));
        assert!(!fields.contains("cookie"));
    }
}
