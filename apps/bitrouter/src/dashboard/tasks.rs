//! Nonblocking application task controls for the Code conversation.

use bitrouter_sdk::acp::client::{
    AcpClient,
    tasks::{TaskControlError, TaskMethod},
};
use bitrouter_sdk::acp::controller::tasks::{
    TaskSelectRequest, TaskSelectionMode, TaskStatusResponse,
};
use bitrouter_tui::dashboard::TaskView;

type Outcome = Result<TaskStatusResponse, TaskControlError>;
const CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

struct Pending {
    selection: bool,
    handle: tokio::task::JoinHandle<Outcome>,
}

#[derive(Default)]
pub(crate) struct TaskDriver {
    confirmed: Option<TaskStatusResponse>,
    verified: bool,
    pending: Option<Pending>,
    // An uncertain result retains the whole intent. A later status with no
    // reservation cannot prove this request was never applied and consumed.
    selection: Option<TaskSelectRequest>,
    action_error: Option<String>,
    refresh_error: Option<String>,
}

impl Drop for TaskDriver {
    fn drop(&mut self) {
        if let Some(pending) = &self.pending {
            pending.handle.abort();
        }
    }
}

impl TaskDriver {
    pub(crate) fn refresh(&mut self, client: &AcpClient, session: &str) {
        if self.pending.is_some()
            || self.selection.is_some()
            || !client.task_control().allows(TaskMethod::Status)
        {
            return;
        }
        let client = client.clone();
        let session = session.to_string();
        self.pending = Some(Pending {
            selection: false,
            handle: tokio::spawn(async move {
                tokio::time::timeout(CONTROL_TIMEOUT, client.task_status(&session))
                    .await
                    .map_err(|error| TaskControlError::Unknown(error.into()))?
            }),
        });
    }

    pub(crate) fn select(
        &mut self,
        client: &AcpClient,
        session: &str,
        mode: TaskSelectionMode,
        working: bool,
    ) -> Result<(), String> {
        if !client.task_control().allows(TaskMethod::Status)
            || !client.task_control().allows(TaskMethod::Select)
        {
            return Err("This controller does not offer task selection.".into());
        }
        if working
            || self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.selection)
        {
            return Err("Wait for the current operation before selecting a task.".into());
        }
        let request = if let Some(original) = &self.selection {
            if original.mode != mode || original.session_id != session {
                return Err(
                    "Resolve the previous selection by retrying the same action first.".into(),
                );
            }
            original.clone()
        } else {
            if !self.verified {
                return Err("Wait for task state to refresh before selecting.".into());
            }
            let status = self
                .confirmed
                .as_ref()
                .ok_or("Task state is not available.")?;
            if status.pending.is_some() {
                return Err("A selection is already reserved for the next message.".into());
            }
            TaskSelectRequest {
                session_id: session.into(),
                request_id: uuid::Uuid::new_v4().to_string(),
                expected: status
                    .current
                    .clone()
                    .ok_or("Send the first message to start a task.")?,
                mode,
            }
        };
        // A status query started before selection must never overwrite the
        // selection result; adding a reservation does not advance its cursor.
        if let Some(pending) = self.pending.take() {
            pending.handle.abort();
        }
        self.selection = Some(request.clone());
        self.action_error = None;
        let client = client.clone();
        self.pending = Some(Pending {
            selection: true,
            handle: tokio::spawn(async move {
                tokio::time::timeout(CONTROL_TIMEOUT, client.task_select(request))
                    .await
                    .map_err(|error| TaskControlError::Unknown(error.into()))?
            }),
        });
        Ok(())
    }

    pub(crate) fn can_prompt(&self, client: &AcpClient) -> bool {
        !client.task_control().allows(TaskMethod::Status)
            || (self.verified && self.selection.is_none())
    }

    /// Invalidate reads started before a prompt boundary. Old replies may still
    /// reach the ACP client but cannot publish into this driver's new state.
    pub(crate) fn invalidate(&mut self) {
        if self.selection.is_none() {
            if let Some(pending) = self.pending.take() {
                pending.handle.abort();
            }
            self.verified = false;
        }
    }

    pub(crate) async fn result(&mut self) -> Outcome {
        match &mut self.pending {
            Some(pending) => (&mut pending.handle)
                .await
                .map_err(|error| TaskControlError::Unknown(error.into()))?,
            None => std::future::pending().await,
        }
    }

    pub(crate) fn complete(&mut self, outcome: Outcome) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        match outcome {
            Ok(status) => {
                self.confirmed = Some(status);
                self.verified = true;
                self.refresh_error = None;
                self.action_error = None;
                if pending.selection {
                    self.selection = None;
                }
            }
            Err(error) => {
                self.verified = false;
                if pending.selection {
                    if matches!(
                        error,
                        TaskControlError::NotApplied(_) | TaskControlError::Unavailable(_)
                    ) {
                        self.selection = None;
                    }
                    self.action_error = Some(error.to_string());
                } else {
                    self.refresh_error = Some(error.to_string());
                }
            }
        }
    }

    pub(crate) fn view(&self, client: &AcpClient) -> Option<TaskView> {
        if !client.task_control().allows(TaskMethod::Status) {
            return None;
        }
        let current = self
            .confirmed
            .as_ref()
            .and_then(|status| status.current.as_ref());
        let next = self
            .confirmed
            .as_ref()
            .and_then(|status| status.pending.as_ref())
            .map(|pending| {
                match pending.mode {
                    TaskSelectionMode::NewTask => "Next message starts a new task.",
                    TaskSelectionMode::Retry => "Next message starts another attempt of this task.",
                }
                .into()
            });
        let status = if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.selection)
        {
            "Saving task selection…".into()
        } else if self.selection.is_some() {
            "Selection unconfirmed; retry the same action before sending a message.".into()
        } else if let Some(error) = self.action_error.as_ref().or(self.refresh_error.as_ref()) {
            error.clone()
        } else if !self.verified {
            "Refreshing task state…".into()
        } else if current.is_none() {
            "The first message starts your task.".into()
        } else {
            match self
                .confirmed
                .as_ref()
                .and_then(|status| status.phase.as_deref())
            {
                Some("collecting") => "Collecting task evidence",
                Some("settling") => "Reconciling task evidence; evaluation pending",
                Some("ready") => "Task evidence ready",
                Some("partial") => "Task evidence incomplete",
                _ => "Task evidence state unknown",
            }
            .into()
        };
        let hint = if let Some(request) = &self.selection {
            match request.mode {
                TaskSelectionMode::NewTask => "F2 Retry selection",
                TaskSelectionMode::Retry => "F3 Retry selection",
            }
            .into()
        } else if client.task_control().allows(TaskMethod::Select) {
            "F2 New task · F3 Retry task · F4 Refresh task".into()
        } else {
            "F4 Refresh task".into()
        };
        Some(TaskView {
            task_id: current.map(|current| current.task_id.clone()),
            attempt_id: current.map(|current| current.attempt_id.clone()),
            status,
            next_prompt: next,
            hint,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests;
