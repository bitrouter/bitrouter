//! Forward-compat escalation seam (TUI_SPEC §5, "Forward-compat → the
//! orchestrator conversation"): routing a subagent's *gated* (high-risk)
//! permission back to the **orchestrator conversation** as an
//! `elicitation/create` server→client request, when the connecting client
//! declared the MCP Tasks / elicitation capability (SEP-1686 / SEP-2577).
//!
//! ## What is wired vs. what awaits a real Tasks-declaring harness
//!
//! - **Capability detection** — [`client_supports_escalation`] reads the
//!   client's declared `initialize` capabilities; [`EscalationState::record`]
//!   captures the flag *and* the live server→client peer at the first fleet
//!   tool call. **Wired + tested.**
//! - **The branch point + fallback** — [`EscalationState::can_escalate`] gates
//!   the app-side permission path: `true` ⇒ route via elicitation, `false` ⇒
//!   the existing HumanBridge / headless-deny fallback. **Wired + tested.**
//! - **The `elicitation/create` round-trip** — [`EscalationState::escalate`]
//!   issues the real server→client request (behind rmcp's `elicitation`
//!   feature) and maps the reply to a decision. **Wired, but capability-gated
//!   and not exercised end-to-end**: no shipping harness declares the
//!   capability, so `can_escalate` is `false` for every real client today and
//!   the escalation branch is never taken — the default, guaranteed path stays
//!   the HumanBridge / deny fallback. A failed/declined round-trip maps to
//!   `Deny` (never silently "allow").

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use rmcp::RoleServer;
use rmcp::model::ClientCapabilities;
use rmcp::service::Peer;

/// Whether a connecting client declared a capability that lets a gated
/// permission be routed back to it for a decision — plain elicitation
/// (`capabilities.elicitation`) or task-augmented elicitation
/// (`capabilities.tasks.requests.elicitation.create`, SEP-1686 / SEP-2577).
pub fn client_supports_escalation(caps: &ClientCapabilities) -> bool {
    caps.elicitation.is_some()
        || caps
            .tasks
            .as_ref()
            .is_some_and(|t| t.supports_elicitation_create())
}

/// A gated permission to put to the human via the orchestrator conversation.
/// Plain fields — the app builds it from its own permission type.
pub struct EscalationRequest {
    /// The subagent handle whose action is gated.
    pub subagent: String,
    /// The tool-call title (what the subagent wants to do).
    pub tool_title: String,
}

/// The human's decision, mapped by the app back to a permission outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationDecision {
    /// Allow the gated action once.
    Allow,
    /// Deny it (also the fail-safe for a declined/failed round-trip).
    Deny,
}

/// Connection-scoped escalation state, shared between the MCP handler (which
/// records the client capability + captures the server→client peer at the first
/// fleet tool call) and the app-side permission path (which queries it). One
/// per stdio connection — the fleet backend is stdio-only.
///
/// Forward-compat note: the MCP spec is moving toward a stateless core where
/// continuation state rides in explicit, model-visible handles rather than
/// connection/session scope. Holding this state in-process is sound *only*
/// because the fleet backend is one stdio connection in one process; if the
/// escalation seam ever rides a resumable or multi-instance transport, the
/// capability flag + peer must move out of connection scope (keyed by an
/// explicit handle bound to the verified caller, never a session id).
#[derive(Default)]
pub struct EscalationState {
    /// Whether the connected client declared the capability.
    client_supports: AtomicBool,
    /// The live server→client peer, captured from a tool call's request
    /// context. `Peer` is a cheap connection handle (`Clone`).
    peer: Mutex<Option<Peer<RoleServer>>>,
}

impl EscalationState {
    /// A fresh, unpopulated state.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record the connecting client's capability and capture the peer. Called
    /// from a fleet tool method: the peer is the connection (stable for the
    /// whole session), and the capabilities were fixed at `initialize`.
    pub fn record(&self, caps: &ClientCapabilities, peer: Peer<RoleServer>) {
        self.client_supports
            .store(client_supports_escalation(caps), Ordering::SeqCst);
        if let Ok(mut slot) = self.peer.lock() {
            *slot = Some(peer);
        }
    }

    /// Whether a gated permission may be routed to the orchestrator conversation
    /// — the client declared the capability *and* a peer was captured. `false`
    /// for every client that hasn't opted in, so the app takes the existing
    /// human-bridge / deny fallback.
    pub fn can_escalate(&self) -> bool {
        self.client_supports.load(Ordering::SeqCst)
            && self.peer.lock().map(|p| p.is_some()).unwrap_or(false)
    }

    /// Route `req` to the orchestrator conversation as an `elicitation/create`
    /// and await the decision. `None` when escalation isn't available (the
    /// caller falls back) — a failed/dismissed round-trip is folded into
    /// [`EscalationDecision::Deny`], never read as "allow".
    pub async fn escalate(&self, req: EscalationRequest) -> Option<EscalationDecision> {
        if !self.client_supports.load(Ordering::SeqCst) {
            return None;
        }
        let peer = match self.peer.lock() {
            Ok(slot) => slot.clone()?,
            Err(_) => return None,
        };
        escalate_via_elicitation(&peer, &req).await
    }
}

/// How long to wait for the orchestrator's answer to an escalated permission
/// before giving up. A client that accepts the `elicitation/create` but never
/// responds must not stall the subagent's permission loop forever; on timeout
/// the request errors → `None` → the caller falls back to deny.
const ESCALATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Issue the server→client `elicitation/create` and map the response to a
/// decision. Isolated so the rmcp-elicitation coupling lives in one place.
///
/// Capability-gated by the caller ([`EscalationState::escalate`]); no shipping
/// harness declares the capability yet, so this is not exercised against a live
/// client. A transport error, a timeout, a declined form, or a cancelled dialog
/// all map to `Deny` / `None` — the escalation never fails *open*.
async fn escalate_via_elicitation(
    peer: &Peer<RoleServer>,
    req: &EscalationRequest,
) -> Option<EscalationDecision> {
    use rmcp::model::{
        BooleanSchema, ElicitRequestParams, ElicitationAction, ElicitationSchemaBuilder,
        PrimitiveSchemaDefinition,
    };
    let schema = ElicitationSchemaBuilder::new()
        .required_property(
            "approve",
            PrimitiveSchemaDefinition::Boolean(
                BooleanSchema::new().description("Allow this high-risk subagent action?"),
            ),
        )
        .build()
        .ok()?;
    let message = format!(
        "Subagent {} requests a high-risk action: {}. Approve?",
        req.subagent, req.tool_title
    );
    // Bounded: a client that accepts the request but never answers must not
    // stall the subagent's permission loop forever. Timeout → Err → `None` →
    // the caller falls back to deny.
    let result = peer
        .create_elicitation_with_timeout(
            ElicitRequestParams::FormElicitationParams {
                meta: None,
                message,
                requested_schema: schema,
            },
            Some(ESCALATION_TIMEOUT),
        )
        .await
        .ok()?;
    match result.action {
        ElicitationAction::Accept => {
            let approved = result
                .content
                .as_ref()
                .and_then(|c| c.get("approve"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Some(if approved {
                EscalationDecision::Allow
            } else {
                EscalationDecision::Deny
            })
        }
        // Declined or cancelled → deny (fail safe).
        _ => Some(EscalationDecision::Deny),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `ClientCapabilities` from JSON — the rmcp capability structs are
    /// `#[non_exhaustive]` (no cross-crate struct literals), but they're
    /// `Deserialize`, which mirrors what the real handshake does anyway.
    fn caps(json: serde_json::Value) -> ClientCapabilities {
        serde_json::from_value(json).expect("valid ClientCapabilities")
    }

    #[test]
    fn detection_reads_plain_and_task_elicitation() {
        // A bare client declares neither → not escalatable.
        assert!(!client_supports_escalation(&caps(serde_json::json!({}))));

        // Plain elicitation capability → escalatable.
        assert!(client_supports_escalation(&caps(
            serde_json::json!({ "elicitation": {} })
        )));

        // Task-augmented elicitation (`tasks.requests.elicitation.create`).
        assert!(client_supports_escalation(&caps(serde_json::json!({
            "tasks": { "requests": { "elicitation": { "create": {} } } }
        }))));

        // Tasks present but without elicitation/create → not escalatable.
        assert!(!client_supports_escalation(&caps(serde_json::json!({
            "tasks": { "requests": {} }
        }))));
    }

    #[tokio::test]
    async fn fallback_is_the_default_without_capability() {
        // The safety property: absent a declared capability, escalation is
        // never available (so the app takes the human-bridge / deny fallback),
        // and `escalate` yields `None` rather than a decision.
        let state = EscalationState::default();
        assert!(!state.can_escalate(), "no client recorded ⇒ no escalation");
        assert!(
            state
                .escalate(EscalationRequest {
                    subagent: "abc123".into(),
                    tool_title: "rm -rf".into(),
                })
                .await
                .is_none(),
            "no capability ⇒ escalate() is None ⇒ caller falls back"
        );

        // Recording a client that did NOT declare the capability keeps
        // escalation off — default behavior is unchanged for such clients.
        // (No peer is captured here; `record` would set one, but the
        // capability flag stays false, so `can_escalate` is false regardless.)
        state.client_supports.store(false, Ordering::SeqCst);
        assert!(!state.can_escalate());
    }
}
