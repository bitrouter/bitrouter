//! Native durable entities and bounded live text projected into each ACP version.

use std::collections::HashMap;

use agent_client_protocol::{Client, ConnectionTo, Error};
use bitrouter_sdk::language_model::{Content, Message};
use serde_json::{Value, json};

use super::flow::Flow;
use super::wire::{Wire, invalid};
use crate::thread::{ThreadChange, ThreadEvent, ThreadStatus, ThreadView};
use crate::turn::{TurnEventPayload, TurnStatus};

pub(super) struct Projector {
    wire: Wire,
    session: String,
    prefixes: HashMap<String, String>,
    state: Option<Value>,
    text_limit: usize,
}

impl Projector {
    pub(super) fn new(wire: Wire, session: String, text_limit: usize) -> Self {
        Self {
            wire,
            session,
            prefixes: HashMap::new(),
            state: None,
            text_limit,
        }
    }

    async fn send(
        &self,
        cx: &ConnectionTo<Client>,
        flow: &Flow,
        value: Value,
    ) -> Result<(), Error> {
        self.wire.update(cx, flow, &self.session, value).await
    }

    async fn text(
        &mut self,
        cx: &ConnectionTo<Client>,
        flow: &Flow,
        id: &str,
        text: &str,
        user: bool,
        complete: bool,
    ) -> Result<(), Error> {
        let kind = if user {
            "user_message"
        } else {
            "agent_message"
        };
        if self.wire == Wire::V2 && complete {
            self.send(cx, flow, json!({"sessionUpdate":kind,"messageId":id,"content":[{"type":"text","text":text}]})).await?;
            self.prefixes.remove(id);
            return Ok(());
        }
        if !complete {
            if self.prefixes.len() >= 256 && !self.prefixes.contains_key(id) {
                return Err(invalid("too many live ACP messages"));
            }
            let bytes = self.prefixes.values().map(String::len).sum::<usize>();
            if bytes.saturating_add(text.len()) > self.text_limit {
                return Err(invalid("live ACP text exceeds context bound"));
            }
            if self.wire == Wire::V2 && !self.prefixes.contains_key(id) {
                self.send(
                    cx,
                    flow,
                    json!({"sessionUpdate":kind,"messageId":id,"content":[]}),
                )
                .await?;
            }
        }
        let prefix = self.prefixes.get(id).map_or("", String::as_str);
        let suffix = if complete {
            text.strip_prefix(prefix).ok_or_else(|| {
                invalid("assistant text cannot be repaired by appending; reload required")
            })?
        } else {
            text
        };
        if !suffix.is_empty() {
            let mut update = json!({"sessionUpdate":format!("{kind}_chunk"),"content":{"type":"text","text":suffix},"messageId":id});
            if self.wire == Wire::V1 {
                update["_meta"] = json!({"bitrouter":{"itemId":id}});
            }
            self.send(cx, flow, update).await?;
        }
        if complete {
            self.prefixes.remove(id);
        } else {
            let prefix = self.prefixes.entry(id.into()).or_default();
            prefix.push_str(text);
        }
        Ok(())
    }

    pub(super) async fn event(
        &mut self,
        cx: &ConnectionTo<Client>,
        flow: &Flow,
        event: &ThreadEvent,
    ) -> Result<(), Error> {
        for change in &event.changes {
            match change {
                ThreadChange::TurnQueued {
                    user_item_id,
                    prompt,
                    ..
                } => {
                    self.text(cx, flow, user_item_id, prompt, true, true)
                        .await?
                }
                ThreadChange::AssistantResponse {
                    item_id, message, ..
                } => {
                    self.text(cx, flow, item_id, &message_text(message), false, true)
                        .await?
                }
                ThreadChange::AssistantInterrupted {
                    item_id, partial, ..
                } => {
                    self.text(cx, flow, item_id, &message_text(partial), false, true)
                        .await?
                }
                ThreadChange::ToolIntent { call, .. } => {
                    let tag = if self.wire == Wire::V1 {
                        "tool_call"
                    } else {
                        "tool_call_update"
                    };
                    self.send(cx, flow, json!({"sessionUpdate":tag,"toolCallId":call.item_id,"title":call.name,"kind":tool_kind(&call.name),"status":"pending","rawInput":serde_json::from_str::<Value>(&call.arguments).unwrap_or(Value::String(call.arguments.clone()))})).await?;
                }
                ThreadChange::ToolResult {
                    item_id,
                    message,
                    effect,
                    ..
                } => {
                    let text = message_text(message);
                    let status = if *effect == crate::store::EffectStatus::Completed {
                        "completed"
                    } else {
                        "failed"
                    };
                    self.send(cx, flow, json!({"sessionUpdate":"tool_call_update","toolCallId":item_id,"status":status,"content":[{"type":"content","content":{"type":"text","text":text}}]})).await?;
                }
                ThreadChange::VerificationResult {
                    call,
                    evidence,
                    effect,
                    ..
                } => {
                    let status = if *effect == crate::store::EffectStatus::Completed
                        && evidence.exit_status == Some(0)
                    {
                        "completed"
                    } else {
                        "failed"
                    };
                    self.send(cx, flow, json!({"sessionUpdate":"tool_call_update","toolCallId":call.item_id,"status":status,"rawOutput":evidence})).await?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(super) async fn live(
        &mut self,
        cx: &ConnectionTo<Client>,
        flow: &Flow,
        event: &crate::turn::TurnEvent,
    ) -> Result<(), Error> {
        if let TurnEventPayload::AssistantDelta { item_id, text, .. } = &event.payload {
            self.text(cx, flow, item_id, text, false, false).await?;
        } else if let TurnEventPayload::ToolOutputDelta { id, source, text } = &event.payload {
            self.send(cx, flow, json!({"sessionUpdate":"tool_call_update","toolCallId":id,"status":"in_progress","_meta":{"bitrouter":{"outputDelta":{"source":source,"text":text}}}})).await?;
        }
        Ok(())
    }

    pub(super) async fn view(
        &mut self,
        cx: &ConnectionTo<Client>,
        flow: &Flow,
        view: &ThreadView,
        settled: bool,
    ) -> Result<(), Error> {
        if self.wire != Wire::V2 {
            return Ok(());
        }
        let turn = view.latest_turn.as_ref();
        let ready = view.thread.active_turn_id.is_none()
            && view.thread.queued.is_empty()
            && matches!(
                view.thread.status,
                ThreadStatus::Idle | ThreadStatus::Paused
            );
        let state = if turn.is_some_and(|t| t.status == TurnStatus::WaitingForInput)
            || matches!(
                view.thread.status,
                ThreadStatus::RecoveryRequired | ThreadStatus::Closing
            )
            || (view.thread.status == ThreadStatus::Paused && !ready)
        {
            "requires_action"
        } else if ready && settled {
            "idle"
        } else {
            "running"
        };
        let meta = json!({"bitrouter":{"version":1,"threadStatus":view.thread.status,"queued":view.thread.queued,"pauseReason":view.thread.pause_reason,"cursor":view.thread.cursor}});
        let mut update = json!({"sessionUpdate":"state_update","state":state,"_meta":meta});
        if state == "idle"
            && let Some(turn) = turn
        {
            update["stopReason"] = json!(match turn.status {
                TurnStatus::Cancelled => "cancelled",
                TurnStatus::Completed => "end_turn",
                _ => "_bitrouter_failed",
            });
        }
        if self.state.as_ref() != Some(&update) {
            self.send(cx, flow, update.clone()).await?;
            self.state = Some(update);
        }
        Ok(())
    }

    pub(super) async fn cancelled(
        &mut self,
        cx: &ConnectionTo<Client>,
        flow: &Flow,
    ) -> Result<(), Error> {
        if self.wire == Wire::V2 {
            self.send(
                cx,
                flow,
                json!({"sessionUpdate":"state_update","state":"idle","stopReason":"cancelled"}),
            )
            .await?;
            self.state = None;
        }
        Ok(())
    }
}

fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "glob" | "grep" => "read",
        "write" | "edit" => "edit",
        "shell" => "execute",
        _ => "other",
    }
}

fn message_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            Content::Text { text, .. } => Some(text.clone()),
            Content::ToolResult { output, .. } => Some(
                serde_json::to_string(output).unwrap_or_else(|_| "unavailable tool output".into()),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
