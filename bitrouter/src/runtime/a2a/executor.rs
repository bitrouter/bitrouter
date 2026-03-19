//! LLM-backed A2A agent executor.
//!
//! Converts incoming A2A messages into LLM calls through BitRouter's
//! routing system and returns A2A-typed results.

use std::sync::Arc;

use bitrouter_a2a::error::A2aError;
use bitrouter_a2a::message::{Message, MessageRole, Part};
use bitrouter_a2a::server::{AgentExecutor, ExecuteResult, ExecutorContext};
use bitrouter_a2a::task::{Task, TaskState, TaskStatus};
use bitrouter_core::models::language::call_options::LanguageModelCallOptions;
use bitrouter_core::models::language::content::LanguageModelContent;
use bitrouter_core::models::language::data_content::LanguageModelDataContent;
use bitrouter_core::models::language::language_model::LanguageModel;
use bitrouter_core::models::language::prompt::{
    LanguageModelMessage, LanguageModelPrompt, LanguageModelUserContent,
};
use bitrouter_core::routers::model_router::LanguageModelRouter;
use bitrouter_core::routers::routing_table::RoutingTable;

/// Agent executor that converts A2A messages to LLM calls.
pub struct LlmAgentExecutor<T, R> {
    table: Arc<T>,
    router: Arc<R>,
    /// The model route name to use (references a route in config).
    model: String,
    /// Optional system prompt prepended to all conversations.
    system_prompt: Option<String>,
}

impl<T, R> LlmAgentExecutor<T, R>
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    pub fn new(
        table: Arc<T>,
        router: Arc<R>,
        model: String,
        system_prompt: Option<String>,
    ) -> Self {
        Self {
            table,
            router,
            model,
            system_prompt,
        }
    }
}

impl<T, R> AgentExecutor for LlmAgentExecutor<T, R>
where
    T: RoutingTable + Send + Sync + 'static,
    R: LanguageModelRouter + Send + Sync + 'static,
{
    async fn execute(&self, ctx: &ExecutorContext) -> Result<ExecuteResult, A2aError> {
        // Convert A2A message to LLM prompt.
        let prompt = build_prompt(&ctx.message, self.system_prompt.as_deref());

        // Route to a model.
        let target = self
            .table
            .route(&self.model)
            .await
            .map_err(|e| A2aError::Execution(format!("routing failed: {e}")))?;

        let model = self
            .router
            .route_model(target)
            .await
            .map_err(|e| A2aError::Execution(format!("model resolution failed: {e}")))?;

        // Call the LLM.
        let options = LanguageModelCallOptions {
            prompt,
            stream: Some(false),
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            presence_penalty: None,
            frequency_penalty: None,
            response_format: None,
            seed: None,
            tools: None,
            tool_choice: None,
            include_raw_chunks: None,
            abort_signal: None,
            headers: None,
            provider_options: None,
        };

        let result = model
            .generate(options)
            .await
            .map_err(|e| A2aError::Execution(format!("generation failed: {e}")))?;

        // Convert LLM content back to A2A parts.
        let parts = content_to_parts(&result.content);

        // Build response message.
        let response_msg = Message {
            role: MessageRole::Agent,
            parts,
            message_id: format!("{}-resp", ctx.task_id),
            context_id: Some(ctx.context_id.clone()),
            task_id: Some(ctx.task_id.clone()),
            reference_task_ids: Vec::new(),
            metadata: None,
            extensions: Vec::new(),
        };

        // Build completed task.
        let task = Task {
            id: ctx.task_id.clone(),
            context_id: Some(ctx.context_id.clone()),
            status: TaskStatus {
                state: TaskState::Completed,
                timestamp: now_utc(),
                message: Some(response_msg),
            },
            artifacts: Vec::new(),
            history: Vec::new(),
            metadata: None,
        };

        Ok(ExecuteResult::Task(task))
    }

    async fn cancel(&self, task_id: &str) -> Result<Task, A2aError> {
        // Synchronous execution model — tasks complete immediately,
        // so there's nothing to cancel. Return a canceled task.
        Ok(Task {
            id: task_id.to_string(),
            context_id: None,
            status: TaskStatus {
                state: TaskState::Canceled,
                timestamp: now_utc(),
                message: None,
            },
            artifacts: Vec::new(),
            history: Vec::new(),
            metadata: None,
        })
    }
}

// ── Conversion helpers ──────────────────────────────────────────

fn build_prompt(message: &Message, system_prompt: Option<&str>) -> LanguageModelPrompt {
    let mut prompt: LanguageModelPrompt = Vec::new();

    // Prepend system prompt if configured.
    if let Some(sys) = system_prompt {
        prompt.push(LanguageModelMessage::System {
            content: sys.to_string(),
            provider_options: None,
        });
    }

    // Convert A2A parts to LLM user content.
    let user_content: Vec<LanguageModelUserContent> = message
        .parts
        .iter()
        .filter_map(part_to_user_content)
        .collect();

    if !user_content.is_empty() {
        prompt.push(LanguageModelMessage::User {
            content: user_content,
            provider_options: None,
        });
    }

    prompt
}

fn part_to_user_content(part: &Part) -> Option<LanguageModelUserContent> {
    if let Some(ref text) = part.text {
        return Some(LanguageModelUserContent::Text {
            text: text.clone(),
            provider_options: None,
        });
    }

    if let Some(ref raw) = part.raw {
        let media_type = part
            .media_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_string());
        return Some(LanguageModelUserContent::File {
            filename: part.filename.clone(),
            data: LanguageModelDataContent::String(raw.clone()),
            media_type,
            provider_options: None,
        });
    }

    if let Some(ref url) = part.url {
        return Some(LanguageModelUserContent::File {
            filename: part.filename.clone(),
            data: LanguageModelDataContent::Url(url.clone()),
            media_type: part
                .media_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            provider_options: None,
        });
    }

    if let Some(ref data) = part.data {
        // Structured JSON data — serialize as text for the LLM.
        return Some(LanguageModelUserContent::Text {
            text: data.to_string(),
            provider_options: None,
        });
    }

    None
}

fn content_to_parts(content: &LanguageModelContent) -> Vec<Part> {
    match content {
        LanguageModelContent::Text { text, .. } => vec![Part::text(text)],
        LanguageModelContent::Reasoning { text, .. } => vec![Part::text(text)],
        LanguageModelContent::File {
            data, media_type, ..
        } => {
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(data);
            vec![Part::raw(encoded, None, Some(media_type.clone()))]
        }
        // Tool calls and other content types are represented as structured data.
        LanguageModelContent::ToolCall {
            tool_call_id,
            tool_name,
            tool_input,
            ..
        } => vec![Part::data(serde_json::json!({
            "type": "tool_call",
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
            "tool_input": tool_input,
        }))],
        LanguageModelContent::ToolResult {
            tool_call_id,
            tool_name,
            result,
            ..
        } => vec![Part::data(serde_json::json!({
            "type": "tool_result",
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
            "result": result,
        }))],
        // Other variants mapped to text summaries.
        LanguageModelContent::UrlSource { url, title, .. } => {
            let label = title.as_deref().unwrap_or(url);
            vec![Part::text(format!("[source: {label}] {url}"))]
        }
        LanguageModelContent::DocumentSource { title, .. } => {
            vec![Part::text(format!("[document: {title}]"))]
        }
        LanguageModelContent::ToolApprovalRequest { .. } => Vec::new(),
    }
}

/// Generate a simple UTC ISO 8601 timestamp from system time.
fn now_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Civil date from days since Unix epoch (Hinnant's algorithm).
    let days = secs.div_euclid(86400);
    let time_of_day = secs.rem_euclid(86400);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    let h = time_of_day / 3600;
    let min = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;

    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}
