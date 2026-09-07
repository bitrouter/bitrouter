use super::*;
use crate::session_evidence::{claude_sdk, types::Harness};

impl Extractor<'_> {
    pub(super) fn claude_cli(&mut self) -> Result<()> {
        let raw = &self.record.input.raw;
        ensure!(
            self.source.harness == Harness::ClaudeCode,
            "foreign native CLI source"
        );
        let process = text(raw, "process_id")?;
        let parsed = uuid::Uuid::parse_str(&process)?;
        ensure!(
            parsed.to_string() == process,
            "noncanonical native process id"
        );
        let path = std::path::Path::new(
            self.source
                .locator
                .strip_prefix("spool:")
                .context("CLI spool locator missing")?,
        );
        ensure!(
            path.file_name().and_then(|name| name.to_str())
                == Some(format!("cli-{process}.jsonl").as_str()),
            "native process/source mismatch"
        );
        ensure!(
            raw.get("sequence").and_then(Value::as_u64) == Some(self.record.input.sequence),
            "native process sequence mismatch"
        );
        self.process_id = Some(process);
        if raw.get("scope_valid") != Some(&Value::Bool(true))
            || raw.get("namespace").and_then(Value::as_str) != Some(self.source.namespace.as_str())
        {
            return self.push(
                None,
                None,
                FactKind::Gap {
                    reason: "native_process_scope_unresolved".into(),
                },
            );
        }
        match raw.get("method").and_then(Value::as_str) {
            Some(method @ ("runtime/started" | "runtime/stopped" | "runtime/failed")) => {
                let clean = raw
                    .get("clean")
                    .map(|value| value.as_bool().context("native exit clean flag"))
                    .transpose()?;
                let exit_code = raw
                    .get("exit_code")
                    .filter(|value| !value.is_null())
                    .map(|value| {
                        value
                            .as_i64()
                            .and_then(|code| i32::try_from(code).ok())
                            .context("native exit code")
                    })
                    .transpose()?;
                self.push(
                    None,
                    None,
                    FactKind::ProcessLifecycle {
                        state: method.trim_start_matches("runtime/").into(),
                        clean,
                        exit_code,
                    },
                )
            }
            Some("runtime/message") => {
                let message = raw
                    .get("payload")
                    .context("CLI lifecycle payload missing")?;
                let node = self.node(&text(message, "session_id")?, None)?;
                self.claude_message(node, message)
            }
            Some("runtime/input") => {
                let payload = raw.get("payload").context("native input missing")?;
                let node = optional_text(payload, "session_id")?
                    .filter(|id| !id.is_empty())
                    .map(|id| self.node(&id, None))
                    .transpose()?;
                self.push(
                    node,
                    None,
                    FactKind::Activity {
                        activity: "native_user_input".into(),
                        native_id: optional_text(payload, "uuid")?,
                    },
                )
            }
            Some("runtime/gap") => self.push(
                None,
                None,
                FactKind::Gap {
                    reason: text(raw, "reason")?,
                },
            ),
            _ => anyhow::bail!("unknown native process record"),
        }
    }

    /// The adapter's original session envelope establishes scope. Native task
    /// ids identify SDK tasks (including shell jobs), not agent transcript ids.
    /// Result delivery, command completion, session idle and background sets
    /// remain separate observations for the settlement reducer.
    /// <https://github.com/agentclientprotocol/claude-agent-acp/blob/main/src/acp-agent.ts>
    /// <https://code.claude.com/docs/en/agent-sdk/typescript>
    pub(super) fn claude_sdk(&mut self) -> Result<()> {
        let raw = &self.record.input.raw;
        if self.source.harness != Harness::ClaudeCode
            || raw.get("method").and_then(Value::as_str) != Some(claude_sdk::METHOD)
            || raw.get("phase").and_then(Value::as_str) != Some("notification")
        {
            return Ok(());
        }
        // Early notifications and uncertain Query reuse must not inherit the
        // controller's default profile. Keep their raw envelope for rebinding.
        if !matches!(
            raw.get("native_scope").and_then(Value::as_str),
            Some("session" | "operation")
        ) {
            return Ok(());
        }
        let payload = raw.get("payload").context("native SDK envelope missing")?;
        let node = self.node(&text(payload, "sessionId")?, None)?;
        let message = payload
            .get("message")
            .context("native SDK message missing")?;
        self.claude_message(node, message)
    }

    fn claude_message(&mut self, node: NodeKey, message: &Value) -> Result<()> {
        ensure!(
            message.get("bitrouter_capture_invalid") != Some(&Value::Bool(true)),
            "invalid native lifecycle field shape"
        );
        if let Some(id) = optional_text(message, "session_id")? {
            ensure!(id == node.native_id, "native SDK session id mismatch");
        }
        let event = match (
            message.get("type").and_then(Value::as_str),
            message.get("subtype").and_then(Value::as_str),
        ) {
            (Some("command_lifecycle"), _) => {
                let state = text(message, "state")?;
                ensure!(
                    matches!(state.as_str(), "queued" | "started" | "completed" | "cancelled" | "discarded")
                        // CLI 2.1.238+ declines some peer messages before they
                        // enter the command lane; no result need follow.
                        || (self.source.format == SourceFormat::ClaudeCli && state == "refused"),
                    "unknown native command state"
                );
                FactKind::NativeCommand {
                    command_id: text(message, "command_uuid")?,
                    state,
                }
            }
            (Some("system"), Some("init")) => {
                let capabilities = match message.get("capabilities") {
                    Some(value) => {
                        let values = value
                            .as_array()
                            .context("native capabilities must be an array")?;
                        ensure!(values.len() <= MAX_GRAPH_ITEMS, "native capability limit");
                        values
                            .iter()
                            .map(|value| {
                                let value = value
                                    .as_str()
                                    .context("native capability must be a string")?;
                                identifier(value)?;
                                Ok(value.to_owned())
                            })
                            .collect::<Result<BTreeSet<_>>>()?
                    }
                    None => BTreeSet::new(),
                };
                FactKind::Runtime {
                    version: text(message, "claude_code_version")?,
                    capabilities,
                }
            }
            (Some("system"), Some("session_state_changed")) => {
                let state = text(message, "state")?;
                ensure!(
                    matches!(state.as_str(), "idle" | "running" | "requires_action"),
                    "unknown native session state"
                );
                FactKind::SessionState { state }
            }
            (Some("system"), Some("background_tasks_changed")) => {
                // REPLACE semantics within one native process, not an edge
                // stream. Ordering relative to task notifications is unspecified.
                let tasks = message
                    .get("tasks")
                    .and_then(Value::as_array)
                    .context("native task set missing")?;
                ensure!(tasks.len() <= MAX_GRAPH_ITEMS, "native task set limit");
                let mut task_ids = BTreeSet::new();
                for task in tasks {
                    ensure!(
                        task_ids.insert(text(task, "task_id")?),
                        "duplicate native task id"
                    );
                }
                FactKind::BackgroundTasks { task_ids }
            }
            (
                Some("system"),
                Some(
                    subtype @ ("task_started" | "task_progress" | "task_updated"
                    | "task_notification"),
                ),
            ) => {
                let patch = message.get("patch").unwrap_or(&Value::Null);
                let status = match subtype {
                    "task_started" => Some("started".into()),
                    "task_progress" => Some("progress".into()),
                    "task_notification" => {
                        let status = text(message, "status")?;
                        ensure!(
                            matches!(status.as_str(), "completed" | "failed" | "stopped"),
                            "unknown native task terminal status"
                        );
                        Some(status)
                    }
                    _ => {
                        let status = optional_text(patch, "status")?;
                        if let Some(status) = &status {
                            ensure!(
                                matches!(
                                    status.as_str(),
                                    "pending"
                                        | "running"
                                        | "completed"
                                        | "failed"
                                        | "killed"
                                        | "paused"
                                ),
                                "unknown native task status"
                            );
                        }
                        status
                    }
                };
                FactKind::NativeTask {
                    task_id: text(message, "task_id")?,
                    tool_use_id: optional_text(message, "tool_use_id")?,
                    task_type: optional_text(message, "task_type")?,
                    status,
                    background: patch
                        .get("is_backgrounded")
                        .map(|value| {
                            value
                                .as_bool()
                                .context("native background state must be a boolean")
                        })
                        .transpose()?,
                }
            }
            (Some("result"), Some(subtype)) => {
                ensure!(
                    matches!(
                        subtype,
                        "success"
                            | "error_during_execution"
                            | "error_max_turns"
                            | "error_max_budget_usd"
                            | "error_max_structured_output_retries"
                    ),
                    "unknown native result status"
                );
                FactKind::NativeResult {
                    result_id: text(message, "uuid")?,
                    command_id: optional_text(message, "user_message_uuid")?,
                    status: subtype.into(),
                    is_error: message
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .context("native result error flag missing")?,
                }
            }
            (Some("conversation_reset"), _) => FactKind::ConversationReset {
                new_conversation_id: text(message, "new_conversation_id")?,
            },
            (Some("system"), Some("compact_boundary")) => FactKind::Activity {
                activity: "compact_boundary".into(),
                native_id: optional_text(message, "uuid")?,
            },
            _ => return Ok(()),
        };
        self.push(Some(node), None, event)
    }
}

fn optional_text(value: &Value, key: &str) -> Result<Option<String>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => text(value, key).map(Some),
    }
}
