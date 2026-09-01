//! Authenticated ACP/native request-session normalization.

use std::sync::Arc;

use async_trait::async_trait;
use bitrouter_sdk::PipelineEvent;
use bitrouter_sdk::language_model::{ApiProtocol, HookDecision, PipelineContext, PreRequestHook};
use serde::Serialize;

use crate::acp_runtime::AcpRuntime;
use crate::auth::events::ControllerAuthenticated;
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

/// Whether request identity came from the legacy model API path or a
/// credential-bound ACP controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestOrigin {
    /// No authenticated controller credential was present.
    PureModelApi,
    /// The daemon authenticated a controller-scoped credential.
    AuthenticatedAcpController,
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
    /// Whether this evidence may participate in authenticated lease lookup.
    pub trusted_for_route: bool,
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
    /// Trust origin.
    pub origin: RequestOrigin,
    /// Credential-bound controller identity.
    pub authenticated_controller_instance_id: Option<String>,
    /// Untrusted static controller header, retained separately.
    pub claimed_controller_instance_id: Option<String>,
    /// Dynamic ACP session header only after a trusted controller binding.
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
    /// Trust origin.
    pub origin: RequestOrigin,
    /// Recognized harness.
    pub harness: Option<String>,
    /// Credential-bound controller identity.
    pub authenticated_controller_instance_id: Option<String>,
    /// Untrusted claimed controller identity.
    pub claimed_controller_instance_id: Option<String>,
    /// Trusted ACP session binding.
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
}

impl PipelineEvent for SessionIdentityObserved {
    fn event_name(&self) -> &'static str {
        "session.identity_observed"
    }
}

/// Post-auth hook that normalizes identity and applies an authenticated route
/// lease without changing legacy pure API behavior.
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
        let authenticated_controller = ctx
            .get_event::<ControllerAuthenticated>()
            .map(|event| event.controller_instance_id.clone());
        let origin = if authenticated_controller.is_some() {
            RequestOrigin::AuthenticatedAcpController
        } else {
            RequestOrigin::PureModelApi
        };
        let claimed_controller = header_value(ctx, "x-bitrouter-controller-id");
        let dynamic_acp_session = header_value(ctx, "x-bitrouter-acp-session-id");
        let mut conflicts = Vec::new();
        let controller_claim_matches = match (
            authenticated_controller.as_deref(),
            claimed_controller.as_deref(),
        ) {
            (Some(authenticated), Some(claimed)) if authenticated == claimed => true,
            (Some(authenticated), Some(claimed)) => {
                conflicts.push(IdentityConflict {
                    field: "header.x-bitrouter-controller-id".to_string(),
                    expected: Some(authenticated.to_string()),
                    observed: Some(claimed.to_string()),
                    resolution: "credential_binding_wins".to_string(),
                });
                false
            }
            _ => false,
        };
        let acp_session_id = dynamic_acp_session
            .clone()
            .filter(|_| controller_claim_matches);
        if dynamic_acp_session.is_some() && acp_session_id.is_none() {
            conflicts.push(IdentityConflict {
                field: "header.x-bitrouter-acp-session-id".to_string(),
                expected: authenticated_controller.clone(),
                observed: dynamic_acp_session.clone(),
                resolution: "ignored_without_matching_controller_binding".to_string(),
            });
        }

        let mut evidence = header_evidence(
            ctx,
            authenticated_controller.is_some(),
            acp_session_id.is_some(),
        );
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
                trusted_for_route: false,
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
        if authenticated_controller.is_none() {
            // Native evidence remains observable for pure API callers, but no
            // field can be promoted to controller route authority.
            for item in &mut evidence {
                item.trusted_for_route = false;
            }
        }
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

        let route_lease = authenticated_controller.as_deref().and_then(|controller| {
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
            let candidate_refs = candidates.iter().map(String::as_str).collect::<Vec<_>>();
            self.runtime
                .resolve_route(controller, &candidate_refs)
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
            authenticated_controller_instance_id: authenticated_controller,
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
        harness: context.native.harness.clone(),
        authenticated_controller_instance_id: context.authenticated_controller_instance_id.clone(),
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
    }
}

fn header_evidence(
    ctx: &PipelineContext,
    authenticated: bool,
    dynamic_acp_trusted: bool,
) -> Vec<IdentityEvidence> {
    EVIDENCE_HEADERS
        .iter()
        .filter_map(|name| {
            let value = header_value(ctx, name)?;
            let (source, trusted_for_route) = match *name {
                "x-bitrouter-acp-session-id" => ("bitrouter", dynamic_acp_trusted),
                "x-claude-code-session-id" => ("claude_code", authenticated),
                "session-id" | "thread-id" => ("codex", authenticated),
                name if name.starts_with("x-claude-code-") => ("claude_code", false),
                "x-codex-turn-metadata" => ("codex", false),
                _ => ("bitrouter", false),
            };
            let compound = *name == "x-codex-turn-metadata";
            Some(IdentityEvidence {
                transport: "header".to_string(),
                field: (*name).to_string(),
                source: source.to_string(),
                trusted_for_route,
                value_representation: if compound { "presence_only" } else { "raw" }.to_string(),
                value: (!compound).then_some(value),
            })
        })
        .collect()
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
    trusted_for_route: bool,
) -> IdentityEvidence {
    IdentityEvidence {
        transport: "body".to_string(),
        field: field.to_string(),
        source: source.to_string(),
        trusted_for_route,
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
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::time::Duration;

    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::language_model::{
        ApiProtocol, GenerationParams, HookDecision, Message, PipelineContext, PipelineRequest,
        PreRequestHook, Prompt, Role,
    };

    use super::{
        RequestOrigin, RequestSessionContext, SessionContextHook, SessionIdentityObserved,
    };
    use crate::acp_runtime::AcpRuntime;
    use crate::auth::events::ControllerAuthenticated;

    fn context(
        model: &str,
        protocol: ApiProtocol,
        headers: &[(&str, &str)],
        extra: serde_json::Map<String, serde_json::Value>,
        authenticated_controller: Option<&str>,
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
        let mut request = PipelineRequest::new(model, CallerContext::local(), prompt);
        request.request_id = "router-request-1".to_string();
        request.inbound_protocol = Some(protocol);
        for (name, value) in headers {
            request.headers.insert(
                http::HeaderName::from_bytes(name.as_bytes()).expect("valid test header name"),
                value.parse().expect("valid test header value"),
            );
        }
        let mut context = PipelineContext::new(request);
        if let Some(controller_instance_id) = authenticated_controller {
            context.emit(ControllerAuthenticated {
                controller_instance_id: controller_instance_id.to_string(),
                expires_at: "2026-09-02T12:00:00Z".to_string(),
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
        assert!(normalized.authenticated_controller_instance_id.is_none());
        assert!(normalized.acp_session_id.is_none());
        assert_eq!(context.model(), "gpt-5");
    }

    #[tokio::test]
    async fn authenticated_claude_identity_applies_the_matching_session_lease() {
        let runtime = Arc::new(AcpRuntime::new());
        let _grant = runtime
            .issue_controller("brc_claude", Duration::from_secs(60))
            .expect("controller");
        runtime
            .set_route("brc_claude", "claude-root", "anthropic:claude-opus")
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
            Some("brc_claude"),
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
    }

    #[tokio::test]
    async fn codex_exact_thread_lease_wins_before_root_fallback_and_parses_turn_lineage() {
        let runtime = Arc::new(AcpRuntime::new());
        let _grant = runtime
            .issue_controller("brc_codex", Duration::from_secs(60))
            .expect("controller");
        runtime
            .set_route("brc_codex", "codex-root", "openai:gpt-5")
            .expect("root route");
        runtime
            .set_route("brc_codex", "codex-child", "openai:gpt-5.5")
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
            Some("brc_codex"),
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
    async fn mismatched_claimed_controller_cannot_authorize_dynamic_session_header() {
        let runtime = Arc::new(AcpRuntime::new());
        let _grant = runtime
            .issue_controller("brc_real", Duration::from_secs(60))
            .expect("controller");
        runtime
            .set_route("brc_real", "forged-session", "attacker:model")
            .expect("forged candidate route");
        runtime
            .set_route("brc_real", "native-session", "safe:model")
            .expect("native route");
        let mut context = context(
            "logical-model",
            ApiProtocol::Messages,
            &[
                ("x-bitrouter-controller-id", "brc_other"),
                ("x-bitrouter-acp-session-id", "forged-session"),
                ("x-claude-code-session-id", "native-session"),
            ],
            serde_json::Map::new(),
            Some("brc_real"),
        );

        let normalized = observe(runtime, &mut context).await;

        assert!(normalized.acp_session_id.is_none());
        assert_eq!(context.model(), "safe:model");
        assert!(
            normalized
                .conflicts
                .iter()
                .any(|conflict| { conflict.field == "header.x-bitrouter-controller-id" })
        );
    }

    #[tokio::test]
    async fn explicit_caller_routes_and_continuations_are_stronger_than_a_lease() {
        let runtime = Arc::new(AcpRuntime::new());
        let _grant = runtime
            .issue_controller("brc_precedence", Duration::from_secs(60))
            .expect("controller");
        runtime
            .set_route("brc_precedence", "root", "lease:model")
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
                Some("brc_precedence"),
            );
            let normalized = observe(Arc::clone(&runtime), &mut context).await;
            assert_eq!(context.model(), model);
            assert_eq!(
                normalized.route_lease.as_ref().map(|lease| lease.applied),
                Some(false)
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
            Some("brc_precedence"),
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
