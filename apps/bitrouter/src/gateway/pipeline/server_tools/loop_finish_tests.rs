//! A legacy bridge can legally project native partial calls as compatible JSON.
//! Completion, not valid arguments or ownership, authorizes automatic effects.
use super::*;
use crate::gateway::{
    caller::CallerContext,
    model::protocol::{OutboundAdapter, chat_completions::ChatCompletionsAdapter},
    pipeline::server_tools::{approval::AllowAll, toolset::RouterToolset},
};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Counter(Arc<AtomicUsize>);
#[async_trait]
impl RouterToolset for Counter {
    async fn list_tools(&self, _: &ToolContext) -> Result<Vec<Tool>> {
        Ok(vec![Tool::Function {
            name: "search".into(),
            description: None,
            parameters: json!({"type":"object"}),
            strict: None,
            provider_metadata: Default::default(),
        }])
    }
    async fn call_tool(&self, _: &str, _: &str, _: &ToolContext) -> Result<ToolResultOutput> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResultOutput::Text {
            value: "ran".into(),
        })
    }
    fn owns(&self, name: &str) -> bool {
        name == "search"
    }
}

struct Source {
    turns: AtomicUsize,
    queue: Mutex<VecDeque<ExecutionResult>>,
}
#[async_trait]
impl UpstreamTurn for Source {
    async fn run(&self, _: &Prompt) -> Result<ExecutionResult> {
        self.turns.fetch_add(1, Ordering::SeqCst);
        self.queue
            .lock()
            .map_err(|_| BitrouterError::UpstreamUnavailable)?
            .pop_front()
            .ok_or(BitrouterError::UpstreamUnavailable)
    }
}

fn response(
    finish: Value,
    tool: Option<&str>,
    input: u64,
    output: u64,
) -> anyhow::Result<ExecutionResult> {
    let mut message = json!({"role":"assistant","content":"provider text"});
    if let Some(name) = tool {
        message["tool_calls"] = json!([{"id":format!("call-{input}"),"type":"function","function":{"name":name,"arguments":"{}"}}]);
    }
    let mut choice = json!({"index":0,"message":message});
    if !finish.is_null() {
        choice["finish_reason"] = finish;
    }
    let generation = ChatCompletionsAdapter.parse_response(json!({
        "id":format!("bridge-{input}"),"object":"chat.completion","model":"gemini",
        "choices":[choice],"usage":{"prompt_tokens":input,"completion_tokens":output,"total_tokens":input+output}
    }))?;
    Ok(ExecutionResult {
        provider_id: "private-compatible-bridge".into(),
        model_id: "gemini".into(),
        account_label: None,
        result: generation.into(),
        request_duration_ms: 0,
        upstream_duration_ms: None,
        server_tool_calls: Vec::new(),
    })
}

async fn execute(
    responses: Vec<ExecutionResult>,
) -> anyhow::Result<(ExecutionResult, usize, usize, bool)> {
    let calls = Arc::new(AtomicUsize::new(0));
    let loop_ = ServerToolLoop::new(
        ToolsetRegistry::new(vec![Arc::new(Counter(calls.clone()))]),
        ServerToolLoopConfig::default(),
        Arc::new(AllowAll),
    );
    let source = Source {
        turns: AtomicUsize::new(0),
        queue: Mutex::new(responses.into()),
    };
    let base = Prompt {
        model: "m".into(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "hello")],
        tools: Vec::new(),
        params: Default::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        loop_.run_with_provenance(
            &base,
            &ToolContext::new(CallerContext::local(), Default::default()),
            &source,
        ),
    )
    .await??;
    Ok((
        outcome.result,
        calls.load(Ordering::SeqCst),
        source.turns.load(Ordering::SeqCst),
        outcome.provider_terminal_exposed,
    ))
}

#[tokio::test]
async fn incomplete_compatible_json_never_executes_owned_tool_and_retains_provider_result()
-> anyhow::Result<()> {
    for reason in [
        json!("length"),
        json!("content_filter"),
        Value::Null,
        json!("provider_other"),
    ] {
        let first = response(reason, Some("search"), 10, 3)?;
        let expected = serde_json::to_value(first.result.generation().context("generation")?)?;
        let (result, calls, turns, provider) =
            execute(vec![first, response(json!("stop"), None, 40, 7)?]).await?;
        ensure!(
            calls == 0 && turns == 1,
            "incomplete finish ran {calls} tools over {turns} turns"
        );
        ensure!(provider && result.server_tool_calls.is_empty());
        ensure!(
            serde_json::to_value(result.result.generation().context("generation")?)? == expected,
            "partial content, finish or native usage changed"
        );
    }
    Ok(())
}

#[tokio::test]
async fn complete_compatible_json_still_executes_once_and_accumulates_usage() -> anyhow::Result<()>
{
    for reason in ["stop", "tool_calls"] {
        let (result, calls, turns, provider) = execute(vec![
            response(json!(reason), Some("search"), 10, 3)?,
            response(json!("stop"), None, 40, 7)?,
        ])
        .await?;
        ensure!(calls == 1 && turns == 2 && provider && result.server_tool_calls.len() == 1);
        let generation = result.result.generation().context("generation")?;
        let usage = generation.usage.as_ref().context("usage")?;
        ensure!(
            generation.finish_reason == Some(FinishReason::Stop)
                && usage.prompt_tokens == 50
                && usage.completion_tokens == 10
        );
        ensure!(usage.origin == UsageOrigin::ProviderReported);
        ensure!(
            usage.raw.as_ref().context("raw usage")?["provider_usage_fragments"]
                .as_array()
                .context("fragments")?
                .len()
                == 2
        );
    }
    Ok(())
}

#[tokio::test]
async fn later_incomplete_turn_keeps_prior_effect_and_total_without_another_effect()
-> anyhow::Result<()> {
    let (result, calls, turns, provider) = execute(vec![
        response(json!("tool_calls"), Some("search"), 10, 3)?,
        response(json!("length"), Some("search"), 40, 7)?,
        response(json!("stop"), None, 90, 9)?,
    ])
    .await?;
    ensure!(calls == 1 && turns == 2 && provider && result.server_tool_calls.len() == 1);
    let generation = result.result.generation().context("generation")?;
    let usage = generation.usage.as_ref().context("usage")?;
    ensure!(
        generation.finish_reason == Some(FinishReason::Length)
            && generation
                .content
                .iter()
                .any(|part| matches!(part,Content::ToolCall{id,..} if id=="call-40"))
    );
    ensure!(
        usage.prompt_tokens == 50
            && usage.completion_tokens == 10
            && usage.origin == UsageOrigin::ProviderReported
    );
    ensure!(
        usage.raw.as_ref().context("raw usage")?["provider_usage_fragments"]
            .as_array()
            .context("fragments")?
            .len()
            == 2
    );
    Ok(())
}

#[tokio::test]
async fn client_only_partial_compatible_json_is_unchanged() -> anyhow::Result<()> {
    for reason in [json!("length"), json!("content_filter"), Value::Null] {
        let first = response(reason, Some("client_function"), 10, 3)?;
        let expected = serde_json::to_value(first.result.generation().context("generation")?)?;
        let (result, calls, turns, provider) = execute(vec![first]).await?;
        ensure!(calls == 0 && turns == 1 && provider && result.server_tool_calls.is_empty());
        ensure!(
            serde_json::to_value(result.result.generation().context("generation")?)? == expected
        );
    }
    Ok(())
}
