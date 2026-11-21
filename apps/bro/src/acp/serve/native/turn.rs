use super::{Connection, wire};
use crate::checkpoint::{ModelState, Thread};
use crate::harness::RunEvent;
use crate::runner::{self, Journal};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Manager<'a> {
    connection: &'a Connection,
    version: u64,
    message: Cell<Option<Uuid>>,
    committed_message: Cell<Option<Uuid>>,
    tools: RefCell<HashMap<Uuid, &'static str>>,
}

impl Journal for Manager<'_> {
    fn save(&self, thread: &Thread) -> Result<()> {
        self.connection.home.save(thread)?;
        let Some(step) = thread.current()?.steps.last() else {
            return Ok(());
        };
        if let ModelState::ModelResponse { response } = &step.state {
            if self.version == 2
                && self.committed_message.replace(Some(step.step_id)) != Some(step.step_id)
                && let Some(text) = response.message.content.as_ref().and_then(Value::as_str)
            {
                self.connection.update(
                    thread.thread_id,
                    wire::message(2, "agent", step.step_id, text, true),
                );
            }
            let mut tools = self.tools.borrow_mut();
            for (call, definition) in response.calls.iter().zip(&response.message.tool_calls) {
                let status = wire::tool_status(call);
                let previous = tools.insert(call.item_id, status);
                if previous != Some(status) {
                    self.connection.update(
                        thread.thread_id,
                        wire::tool(self.version, call, &definition.function, previous.is_none()),
                    );
                }
            }
        }
        Ok(())
    }
    fn live(&self, session: Uuid, _turn: Uuid, event: RunEvent) -> Result<()> {
        match event {
            RunEvent::AssistantDelta { item_id, text } => {
                let id = Uuid::parse_str(&item_id)?;
                if self.version == 2 && self.message.replace(Some(id)) != Some(id) {
                    let head = json!({"sessionUpdate":"agent_message","messageId":id,"content":[]});
                    self.connection.update(session, head);
                }
                self.connection.update(
                    session,
                    wire::message(self.version, "agent", id, &text, false),
                );
            }
            RunEvent::ToolOutputDelta { id, source, text } => {
                let update = json!({"sessionUpdate":"tool_call_update","toolCallId":id,"status":"in_progress",
                    "_meta":{"bitrouter":{"outputDelta":{"source":source,"text":text}}}});
                self.connection.update(session, update);
            }
        }
        Ok(())
    }
}

pub(super) async fn drive(
    c: &Connection,
    v: u64,
    thread: &mut Thread,
    cancel: &CancellationToken,
) -> Result<()> {
    let turn = thread.current()?.turn_id;
    let path = c.home.cancel_path(turn);
    if path.try_exists()? {
        cancel.cancel();
    }
    let watcher = tokio::spawn(runner::cancellation(path, cancel.clone()));
    let result = execute(c, v, thread, cancel).await;
    watcher.abort();
    result
}

async fn execute(
    c: &Connection,
    v: u64,
    thread: &mut Thread,
    cancel: &CancellationToken,
) -> Result<()> {
    let manager = Manager {
        connection: c,
        version: v,
        message: Cell::new(None),
        committed_message: Cell::new(thread.current()?.steps.last().map(|s| s.step_id)),
        tools: Default::default(),
    };
    let session = thread.thread_id;
    loop {
        c.home.check_owner(thread)?;
        runner::drive(&manager, thread, &c.client, cancel, None, 0).await?;
        let turn = thread.current()?;
        if v == 2 {
            c.update(session, wire::state(turn));
        }
        let Some(input) = turn.pending() else {
            return Ok(());
        };
        if c.disconnected.is_cancelled() {
            return Ok(());
        }
        let callback = Uuid::new_v4().to_string();
        let message = wire::permission(v, session, input, &callback)?;
        let (answer, recv) = oneshot::channel();
        c.permissions.borrow_mut().insert(callback.clone(), answer);
        let result = async {
            c.frame(message)?;
            tokio::select! {
                biased;
                _=cancel.cancelled()=>Ok(None),
                _=c.disconnected.cancelled()=>Ok(None),
                answer=recv=>Ok::<_,anyhow::Error>(Some(answer.context("permission reply lost")?)),
            }
        }
        .await;
        c.permissions.borrow_mut().remove(&callback);
        match result? {
            Some(approved) => {
                let request = input.request_id.clone();
                runner::answer(&c.home, thread, &request, approved)?;
            }
            None if cancel.is_cancelled() => {
                runner::cancel_turn(&manager, thread)?;
                if v == 2 {
                    c.update(session, wire::state(thread.current()?));
                }
                return Ok(());
            }
            None => return Ok(()),
        }
    }
}
