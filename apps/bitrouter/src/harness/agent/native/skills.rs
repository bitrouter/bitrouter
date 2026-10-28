//! Bridge one explicit skill consent into the existing Thread pending input.

use std::sync::Arc;

use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::core::session::CoreSession;
use crate::harness::agent::ApprovalRequest;
use bitrouter_ai::core::skills::SkillConsentRequest;

use super::port::Port;

pub(super) async fn consent(
    core: CoreSession,
    port: Arc<Port>,
    request: SkillConsentRequest,
    approvals: Option<mpsc::Sender<ApprovalRequest>>,
    gate: Arc<Mutex<()>>,
    cancel: CancellationToken,
) -> Result<(), String> {
    let approved = if let Some(approvals) = approvals {
        let guard = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            () = port.stopped.cancelled() => return Ok(()),
            () = core.skill_consent_expired(&request) => return Ok(()),
            guard = gate.lock() => guard,
        };
        if !core.skill_consent_pending(&request).await {
            return Ok(());
        }
        let (response, receive) = oneshot::channel();
        let input = ApprovalRequest {
            id: request.consent.request_id.clone(), tool_id: request.consent.skill_id.clone(),
            tool_name: "mcp_skill.activate".into(),
            arguments: serde_json::to_string(&serde_json::json!({
                "notice":"Approve this exact MCP skill's instructions, including any local commands they may induce. Existing caller/tool permissions and ordinary command approvals still apply. Nested skills require their own consent.",
                "request":request,
            })).map_err(|error| error.to_string())?, response,
        };
        let answer = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            () = port.stopped.cancelled() => return Ok(()),
            () = core.skill_consent_expired(&request) => return Ok(()),
            result = async { approvals.send(input).await.map_err(|_| ())?; receive.await.map_err(|_| ()) } => result.unwrap_or(false),
        };
        drop(guard);
        answer
    } else {
        false
    };
    if !core.skill_consent_pending(&request).await {
        return Ok(());
    }
    if let Err(error) = core
        .answer_skill_consent(
            &format!("skill_answer_{}", request.consent.request_id),
            request.answer(approved),
        )
        .await
        && (!matches!(
            error.code,
            bitrouter_ai::core::protocol::ErrorCode::StaleRevision
                | bitrouter_ai::core::protocol::ErrorCode::StaleEpoch
                | bitrouter_ai::core::protocol::ErrorCode::UnauthorizedScope
        ) || core.skill_consent_pending(&request).await)
    {
        return Err(error.message);
    }
    Ok(())
}
