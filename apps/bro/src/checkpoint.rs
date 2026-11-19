//! The foreground worker checkpoints before dispatch and after each confirmed result.
//! A lost worker releases its OS lock. Only a checkpoint with a known next
//! operation authorizes continuation; releasing the lock says nothing about effects.
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use bitrouter_ai::harness::agent::ToolMode;
use bitrouter_ai::harness::runtime::EffectStatus;
use bitrouter_ai::harness::turn::{InputRequest, TurnStatus};
use bitrouter_ai::protocol::chat_completions::{ChatMessage, ChatUsage};
use bitrouter_ai::types::{ReasoningEffort, ToolResultOutput};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
enum Format {
    #[serde(rename = "bro-local/1")]
    Local,
    #[serde(rename = "bro-local/2")]
    Managed,
    // Older workers must refuse checkpoints whose completion requires a check.
    #[serde(rename = "bro-local/3")]
    Checked,
    #[serde(rename = "bro-local/3-managed")]
    ManagedChecked,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Thread {
    format: Format,
    pub thread_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub m0_instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<crate::skills::Selection>,
    pub workspace: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<crate::workspace::WorkspaceId>,
    pub endpoint: String,
    pub model: String,
    pub effort: Option<ReasoningEffort>,
    pub max_output_tokens: Option<u32>,
    pub tool_mode: ToolMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_command: Option<String>,
    pub turns: Vec<Turn>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Turn {
    pub turn_id: Uuid,
    pub prompt: String,
    /// Workspace instructions observed on admission, retained across restarts.
    pub startup_instructions: Option<String>,
    pub request_id: Option<String>,
    /// Complete, already-settled Chat history verified by the M0 offline reader.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sealed_messages: Vec<ChatMessage>,
    /// Confirmed effects and original JSON outputs rendered as Chat strings.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub sealed_tool_results: std::collections::BTreeMap<String, SealedToolResult>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub children: std::collections::BTreeMap<Uuid, Thread>,
    pub steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<crate::verification::Check>,
    pub outcome: Option<Outcome>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Outcome {
    pub status: TurnStatus,
    pub detail: String,
    pub final_answer: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Step {
    pub step_id: Uuid,
    /// Caller identity committed before the request. The response IDs below are independent evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_request_id: Option<Uuid>,
    pub request_id: Option<String>,
    pub execution_id: Option<String>,
    /// Incomplete display text is never used to build inference history.
    pub partial: Option<String>,
    pub state: ModelState,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub(crate) enum ModelState {
    ModelRequest,
    GatewayDenied,
    ModelResponse { response: Box<Assistant> },
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Assistant {
    pub completion_id: String,
    pub model: String,
    pub message: ChatMessage,
    pub finish_reason: String,
    pub usage: Option<ChatUsage>,
    pub calls: Vec<Invocation>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Invocation {
    pub item_id: Uuid,
    pub approval: Option<Approval>,
    pub state: ToolState,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Approval {
    pub request: InputRequest,
    pub approved: Option<bool>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub(crate) enum ToolState {
    Proposed,
    ToolIntent,
    Child,
    ToolResult {
        output: ToolResultOutput,
        effect: EffectStatus,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SealedToolResult {
    pub output: ToolResultOutput,
    pub effect: EffectStatus,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    Model,
    Tool(usize),
    Verify,
    Waiting,
    Complete,
    Blocked,
    Settled,
}

impl Step {
    fn confirmed(&self) -> bool {
        match &self.state {
            ModelState::ModelRequest => false,
            ModelState::GatewayDenied => true,
            ModelState::ModelResponse { response } => response.calls.iter().all(|call| {
                matches!(
                    call.state,
                    ToolState::ToolResult {
                        effect: EffectStatus::NotExecuted | EffectStatus::Completed,
                        ..
                    }
                )
            }),
        }
    }
}

impl Thread {
    pub fn new(
        workspace: PathBuf,
        endpoint: String,
        model: String,
        effort: Option<ReasoningEffort>,
        max_output_tokens: Option<u32>,
        tool_mode: ToolMode,
    ) -> Result<Self> {
        let workspace_id = Some(crate::workspace::WorkspaceId::read(&workspace)?);
        Ok(Self {
            workspace_id,
            format: Format::Local,
            thread_id: Uuid::new_v4(),
            parent_id: None,
            m0_instructions: None,
            skills: Vec::new(),
            workspace,
            endpoint,
            model,
            effort,
            max_output_tokens,
            tool_mode,
            verification_command: None,
            turns: Vec::new(),
        })
    }

    pub fn verify_workspace(&self) -> Result<()> {
        let expected = self
            .workspace_id
            .as_ref()
            .context("saved workspace grant has no filesystem identity; a new grant is required")?;
        ensure!(
            self.workspace.canonicalize()? == self.workspace
                && &crate::workspace::WorkspaceId::read(&self.workspace)? == expected,
            "workspace directory changed since admission"
        );
        Ok(())
    }

    pub fn managed(&self) -> bool {
        matches!(self.format, Format::Managed | Format::ManagedChecked)
    }

    pub fn enable_managed(&mut self) {
        self.format = if self.verification_command.is_some() {
            Format::ManagedChecked
        } else {
            Format::Managed
        };
    }

    pub fn configure_check(&mut self, command: String) {
        self.format = if self.managed() {
            Format::ManagedChecked
        } else {
            Format::Checked
        };
        self.verification_command = Some(command);
    }

    pub fn current(&self) -> Result<&Turn> {
        self.turns.last().context("thread has no turn")
    }

    pub fn current_mut(&mut self) -> Result<&mut Turn> {
        self.turns.last_mut().context("thread has no turn")
    }

    pub fn accepted_request(&self, key: &str, prompt: &str) -> Result<Option<Uuid>> {
        ensure!(
            !key.is_empty() && key.len() <= 128 && !key.chars().any(char::is_control),
            "invalid request ID"
        );
        let prior = self
            .turns
            .iter()
            .find(|turn| turn.request_id.as_deref() == Some(key));
        if let Some(turn) = prior {
            ensure!(
                turn.prompt == prompt,
                "request ID already accepted with different input"
            );
        }
        Ok(prior.map(|turn| turn.turn_id))
    }

    pub fn start(&mut self, prompt: String, request_id: Option<String>) -> Result<()> {
        self.verify_workspace()?;
        ensure!(!prompt.trim().is_empty(), "prompt is empty");
        ensure!(prompt.len() <= 512 * 1024, "prompt exceeds context limit");
        ensure!(
            self.turns.iter().all(Turn::released),
            "thread already has an active or unresolved turn"
        );
        self.turns.push(Turn {
            turn_id: Uuid::new_v4(),
            prompt,
            startup_instructions: crate::gateway::workspace_instructions(&self.workspace)?,
            request_id,
            sealed_messages: vec![],
            sealed_tool_results: Default::default(),
            children: Default::default(),
            check: self
                .verification_command
                .as_deref()
                .map(crate::verification::Check::new)
                .transpose()?,
            steps: vec![],
            outcome: None,
        });
        Ok(())
    }

    pub fn step_mut(&mut self) -> Result<&mut Step> {
        self.current_mut()?
            .steps
            .last_mut()
            .context("missing model intent")
    }

    pub fn response_mut(&mut self) -> Result<&mut Assistant> {
        match self.current_mut()?.steps.last_mut().map(|s| &mut s.state) {
            Some(ModelState::ModelResponse { response }) => Ok(response),
            _ => bail!("checkpoint has no complete model response"),
        }
    }
}

impl Turn {
    fn validate_sealed_tools(&self) -> Result<()> {
        let mut matched = std::collections::BTreeSet::new();
        for (index, message) in self.sealed_messages.iter().enumerate() {
            for call in &message.tool_calls {
                if !matches!(call.function.name.as_str(), "write" | "edit" | "shell") {
                    continue;
                }
                let result = self
                    .sealed_tool_results
                    .get(&call.id)
                    .context("missing sealed mutation result")?;
                let ToolResultOutput::Json { value } = &result.output else {
                    bail!("invalid sealed result type")
                };
                let next = self
                    .sealed_messages
                    .get(index + 1)
                    .context("missing sealed tool message")?;
                ensure!(
                    result.effect == EffectStatus::Completed
                        && message.role == "assistant"
                        && message.tool_calls.len() == 1
                        && next.role == "tool"
                        && next.tool_call_id.as_ref() == Some(&call.id)
                        && next
                            .content
                            .as_ref()
                            .and_then(|value| value.as_str())
                            .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                            .as_ref()
                            == Some(value)
                        && matched.insert(&call.id),
                    "sealed mutation differs from Chat history"
                );
            }
        }
        ensure!(
            matched.len() == self.sealed_tool_results.len(),
            "unmatched sealed mutation result"
        );
        Ok(())
    }

    pub fn final_answer(&self) -> Option<String> {
        let response = self.response()?;
        if !response.calls.is_empty() {
            return None;
        }
        response
            .message
            .content
            .as_ref()?
            .as_str()
            .map(str::to_owned)
    }

    pub fn response(&self) -> Option<&Assistant> {
        match &self.steps.last()?.state {
            ModelState::ModelResponse { response } => Some(response),
            ModelState::ModelRequest | ModelState::GatewayDenied => None,
        }
    }

    pub fn next(&self) -> Next {
        if self.outcome.is_some() {
            return Next::Settled;
        }
        if self.steps.is_empty() {
            return Next::Model;
        }
        let Some(response) = self.response() else {
            return Next::Blocked;
        };
        if response.calls.is_empty() {
            if response.finish_reason == "stop"
                && let Some(check) = &self.check
            {
                return check.next();
            }
            return Next::Complete;
        }
        for (index, call) in response.calls.iter().enumerate() {
            match call.state {
                ToolState::ToolIntent
                | ToolState::ToolResult {
                    effect: EffectStatus::Unknown,
                    ..
                } => return Next::Blocked,
                ToolState::ToolResult { .. } => continue,
                ToolState::Proposed
                    if call.approval.as_ref().is_some_and(|a| a.approved.is_none()) =>
                {
                    return Next::Waiting;
                }
                ToolState::Child | ToolState::Proposed => return Next::Tool(index),
            }
        }
        Next::Model
    }

    pub fn tool_calls(&self) -> usize {
        self.steps
            .iter()
            .filter_map(|step| match &step.state {
                ModelState::ModelResponse { response } => Some(response.calls.len()),
                _ => None,
            })
            .sum()
    }

    pub fn pending(&self) -> Option<&InputRequest> {
        if self.outcome.is_none()
            && self
                .response()
                .is_some_and(|r| r.finish_reason == "stop" && r.calls.is_empty())
            && let Some(check) = &self.check
            && check.next() == Next::Waiting
        {
            return Some(&check.approval.request);
        }
        self.response()?
            .calls
            .iter()
            .find_map(|c| {
                c.approval
                    .as_ref()
                    .filter(|a| matches!(c.state, ToolState::Proposed) && a.approved.is_none())
                    .map(|a| &a.request)
            })
            .filter(|_| self.outcome.is_none())
    }

    pub fn status(&self, running: bool) -> TurnStatus {
        if let Some(outcome) = &self.outcome {
            return outcome.status;
        }
        if !running && self.children.values().any(crate::tree::unresolved) {
            return TurnStatus::RecoveryRequired;
        }
        match self.next() {
            Next::Blocked if !running => TurnStatus::RecoveryRequired,
            Next::Waiting => TurnStatus::WaitingForInput,
            _ => TurnStatus::Running,
        }
    }

    fn legacy_denial(&self) -> bool {
        self.steps
            .last()
            .is_some_and(|step| matches!(step.state, ModelState::GatewayDenied))
    }

    fn released(&self) -> bool {
        self.outcome.as_ref().is_some_and(|o| o.status.terminal())
    }
}

pub(crate) struct Home {
    root: PathBuf,
    dir: PathBuf,
}

impl Home {
    pub fn open(root: &Path) -> Result<Self> {
        crate::migrated::reject_gateway_home(root)?;
        if !root.join("store.sqlite").try_exists()? {
            crate::home_owner::claim(root)?;
        }
        let dir = root.join("threads");
        crate::durable::create_dirs(&dir, true)?;
        Ok(Self {
            root: root.canonicalize()?,
            dir: dir.canonicalize()?,
        })
    }

    fn path(&self, thread: Uuid) -> PathBuf {
        self.dir.join(format!("{thread}.json"))
    }

    pub fn cancel_path(&self, turn: Uuid) -> PathBuf {
        self.dir.join(format!("{turn}.cancel"))
    }

    pub fn lock_file(&self, thread: Uuid) -> Result<File> {
        open_lock(&self.dir.join(format!("{thread}.lock")))
    }

    /// A contended worker can still be observed through its atomic checkpoint.
    pub fn observe_worker(&self, thread: Uuid) -> Result<(File, bool)> {
        let file = self.lock_file(thread)?;
        let running = match file.try_lock() {
            Ok(()) => false,
            Err(TryLockError::WouldBlock) => true,
            Err(TryLockError::Error(error)) => return Err(error.into()),
        };
        Ok((file, running))
    }

    pub fn worker(&self, thread: Uuid) -> Result<File> {
        let lock = self.lock_file(thread)?;
        lock.try_lock()
            .context("another process owns this thread")?;
        Ok(lock)
    }

    pub fn ids(&self) -> Result<Vec<Uuid>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                ids.push(
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .context("invalid checkpoint name")?
                        .parse()?,
                );
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Serialize admission so a second thread cannot bypass an unresolved
    /// workspace owner by starting in another process using the same home.
    pub fn admit(&self, thread: &Thread) -> Result<()> {
        let guard = open_lock(&self.dir.join("admission.lock"))?;
        guard.lock()?;
        self.check_owner(thread)?;
        self.save(thread)
    }

    pub fn check_owner(&self, thread: &Thread) -> Result<()> {
        thread.verify_workspace()?;
        ensure!(
            !overlapping(&self.root, &thread.workspace)?
                && !overlapping(&self.dir, &thread.workspace)?,
            "BRO data directory must be outside the tool workspace"
        );
        ensure!(
            thread.turns.iter().rev().skip(1).all(Turn::released),
            "earlier turn has an unresolved dispatch"
        );
        for id in self.ids()? {
            if id == thread.thread_id {
                continue;
            }
            let other = self.load(id)?;
            if other.turns.iter().all(Turn::released) {
                continue;
            }
            // If its directory moved, the remaining path cannot establish disjoint ownership.
            other
                .verify_workspace()
                .with_context(|| format!("unresolved workspace grant of thread {id}"))?;
            ensure!(
                !overlapping(&other.workspace, &thread.workspace)?,
                "workspace is owned by active or unresolved thread {id}"
            );
        }
        Ok(())
    }

    pub fn save(&self, thread: &Thread) -> Result<()> {
        crate::durable::write_file(
            &self.path(thread.thread_id),
            atomicwrites::AllowOverwrite,
            |file| {
                let mut writer = BufWriter::new(file);
                serde_json::to_writer(&mut writer, thread)?;
                writer.flush()
            },
        )?;
        Ok(())
    }

    pub fn load(&self, id: Uuid) -> Result<Thread> {
        let file = File::open(self.path(id)).with_context(|| format!("opening thread {id}"))?;
        let mut thread: Thread = serde_json::from_reader(std::io::BufReader::new(file))
            .context("invalid or unsupported local checkpoint")?;
        ensure!(
            thread.thread_id == id,
            "checkpoint thread identity mismatch"
        );
        Self::validate(&mut thread)?;
        crate::tree::validate(&thread)?;
        Ok(thread)
    }

    fn validate(thread: &mut Thread) -> Result<()> {
        ensure!(
            matches!(thread.format, Format::Checked | Format::ManagedChecked)
                == thread.verification_command.is_some(),
            "verification binding differs from checkpoint format"
        );
        crate::skills::validate(&thread.skills)?;
        for (index, turn) in thread.turns.iter().enumerate() {
            crate::verification::validate(thread, turn)?;
            ensure!(
                index + 1 == thread.turns.len() || turn.released() || turn.legacy_denial(),
                "later turn follows unresolved work"
            );
            ensure!(
                turn.sealed_messages.is_empty() || (turn.steps.is_empty() && turn.released()),
                "unsettled imported history"
            );
            turn.validate_sealed_tools()?;
            for (step_index, step) in turn.steps.iter().enumerate() {
                if step_index + 1 < turn.steps.len() || turn.released() {
                    ensure!(
                        step.confirmed(),
                        "checkpoint advances past an unresolved dispatch"
                    );
                }
                if let ModelState::ModelResponse { response } = &step.state {
                    ensure!(
                        response.calls.len() == response.message.tool_calls.len(),
                        "incomplete tool ledger"
                    );
                    for (invocation, call) in
                        response.calls.iter().zip(&response.message.tool_calls)
                    {
                        if let Some(approval) = &invocation.approval {
                            ensure!(
                                approval.request.tool_id == invocation.item_id.to_string()
                                    && approval.request.tool_name == call.function.name
                                    && approval.request.arguments == call.function.arguments,
                                "checkpoint approval mismatch"
                            );
                        }
                    }
                }
            }
        }
        // Older workers treated HTTP 401/403 as proof of no dispatch. Retain
        // their ledger (including later history), but never release that grant.
        for turn in &mut thread.turns {
            if turn.legacy_denial() {
                let outcome = turn.outcome.get_or_insert_with(|| Outcome {
                    status: TurnStatus::Failed,
                    detail: "legacy gateway denial".into(),
                    final_answer: None,
                });
                if outcome.status != TurnStatus::RecoveryRequired {
                    outcome.status = TurnStatus::RecoveryRequired;
                    outcome
                        .detail
                        .push_str("; gateway dispatch was not ruled out");
                }
            }
        }
        for turn in &mut thread.turns {
            for child in turn.children.values_mut() {
                Self::validate(child)?;
            }
        }
        Ok(())
    }
}

// Canonical path spelling can differ for the same directory on case-insensitive
// filesystems. Compare directory identities as well as saved path components.
fn overlapping(left: &Path, right: &Path) -> std::io::Result<bool> {
    if left.starts_with(right) || right.starts_with(left) {
        return Ok(true);
    }
    for (root, candidate) in [(left, right), (right, left)] {
        let handle = match same_file::Handle::from_path(root) {
            Ok(handle) => handle,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for ancestor in candidate.ancestors() {
            match same_file::Handle::from_path(ancestor) {
                Ok(other) if other == handle => return Ok(true),
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
        }
    }
    Ok(false)
}

fn open_lock(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?)
}
