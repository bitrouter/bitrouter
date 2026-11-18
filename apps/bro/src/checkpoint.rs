//! The foreground worker checkpoints before dispatch and after each confirmed result.
//! A lost worker releases its OS lock. Only a checkpoint with a known next
//! operation authorizes continuation; releasing the lock says nothing about effects.
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use bitrouter_ai::harness::agent::ToolMode;
use bitrouter_ai::harness::runtime::EffectStatus;
use bitrouter_ai::harness::turn::{InputRequest, TurnStatus};
use bitrouter_ai::protocol::chat_completions::{ChatMessage, ChatUsage};
use bitrouter_ai::types::{ReasoningEffort, ToolResultOutput};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
enum Format {
    #[serde(rename = "bro-local/1")]
    Local,
}

#[derive(Serialize, Deserialize)]
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
    pub endpoint: String,
    pub model: String,
    pub effort: Option<ReasoningEffort>,
    pub max_output_tokens: Option<u32>,
    pub tool_mode: ToolMode,
    pub turns: Vec<Turn>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Turn {
    pub turn_id: Uuid,
    pub prompt: String,
    /// Workspace instructions observed on admission, retained across restarts.
    pub startup_instructions: Option<String>,
    pub request_id: Option<String>,
    /// Complete, already-settled Chat history verified by the M0 offline reader.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sealed_messages: Vec<ChatMessage>,
    pub steps: Vec<Step>,
    pub outcome: Option<Outcome>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Outcome {
    pub status: TurnStatus,
    pub detail: String,
    pub final_answer: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Step {
    pub step_id: Uuid,
    pub request_id: Option<String>,
    pub execution_id: Option<String>,
    /// Incomplete display text is never used to build inference history.
    pub partial: Option<String>,
    pub state: ModelState,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub(crate) enum ModelState {
    ModelRequest,
    GatewayDenied,
    ModelResponse { response: Box<Assistant> },
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Assistant {
    pub completion_id: String,
    pub model: String,
    pub message: ChatMessage,
    pub finish_reason: String,
    pub usage: Option<ChatUsage>,
    pub calls: Vec<Invocation>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Invocation {
    pub item_id: Uuid,
    pub approval: Option<Approval>,
    pub state: ToolState,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Approval {
    pub request: InputRequest,
    pub approved: Option<bool>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub(crate) enum ToolState {
    Proposed,
    ToolIntent,
    ToolResult {
        output: ToolResultOutput,
        effect: EffectStatus,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    Model,
    Tool(usize),
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
    ) -> Self {
        Self {
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
            turns: Vec::new(),
        }
    }

    pub fn current(&self) -> Result<&Turn> {
        self.turns.last().context("thread has no turn")
    }

    pub fn current_mut(&mut self) -> Result<&mut Turn> {
        self.turns.last_mut().context("thread has no turn")
    }

    pub fn start(&mut self, prompt: String, request_id: Option<String>) -> Result<()> {
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
                ToolState::Proposed => return Next::Tool(index),
            }
        }
        Next::Model
    }

    pub fn pending(&self) -> Option<&InputRequest> {
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
            ensure!(
                other.turns.iter().all(Turn::released)
                    || !overlapping(&other.workspace, &thread.workspace)?,
                "workspace is owned by active or unresolved thread {id}"
            );
        }
        Ok(())
    }

    pub fn save(&self, thread: &Thread) -> Result<()> {
        crate::durable::write_file(
            &self.path(thread.thread_id),
            atomicwrites::AllowOverwrite,
            |file| serde_json::to_writer(file, thread).map_err(std::io::Error::from),
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
        crate::skills::validate(&thread.skills)?;
        for (index, turn) in thread.turns.iter().enumerate() {
            ensure!(
                index + 1 == thread.turns.len() || turn.released() || turn.legacy_denial(),
                "later turn follows unresolved work"
            );
            ensure!(
                turn.sealed_messages.is_empty() || (turn.steps.is_empty() && turn.released()),
                "unsettled imported history"
            );
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
        Ok(thread)
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
