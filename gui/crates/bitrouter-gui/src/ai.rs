//! [`IntentClient`] trait + [`GenAiIntentClient`] real impl + [`parse_intent`] helper.
//!
//! The trait seam lets tests inject a fake without touching the network.
//! The real impl talks to BitRouter's OpenAI-compatible `/v1/` endpoint via
//! genai 0.6's `ServiceTargetResolver` mechanism.

use anyhow::Context as _;
use bitrouter_gui_core::protocol::Command;
use genai::{
    adapter::AdapterKind,
    chat::{ChatMessage, ChatRequest},
    resolver::{AuthData, Endpoint, ServiceTargetResolver},
    Client, ModelIden, ServiceTarget,
};
use tokio::runtime::Runtime;

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Abstraction over the LLM call so tests can inject a fake implementation.
pub trait IntentClient {
    /// Map a natural-language instruction to zero or more [`Command`]s.
    fn parse(&self, text: &str) -> anyhow::Result<Vec<Command>>;
}

// ── Public helper ─────────────────────────────────────────────────────────────

/// Parse a natural-language `text` into [`Command`]s via `client`.
///
/// This thin wrapper validates that `text` is not empty, then delegates to the
/// client. Callers that want to inject a custom `IntentClient` in tests pass
/// their fake here.
pub fn parse_intent(client: &dyn IntentClient, text: &str) -> anyhow::Result<Vec<Command>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    client.parse(trimmed)
}

// ── Real implementation ────────────────────────────────────────────────────────

/// System prompt shown to the model.
///
/// Instructs it to reply with a JSON array of Command objects. Two concrete
/// examples are embedded so the model understands the schema.
const SYSTEM_PROMPT: &str = r#"You are a command dispatcher for BitRouter, a multi-agent IDE shell.
Translate the user's natural-language instruction into a JSON array of commands.
Reply with ONLY the JSON array — no prose, no markdown fences.

The Command enum is tagged with "command" in snake_case. Examples:

[{"command":"spawn_agent","agent_id":"worker","model":"claude/claude-sonnet-4-5","tab":"main","worktree":null,"prompt":null}]
[{"command":"stop_agent","target":{"target":"session","id":"worker"}}]
[{"command":"send_prompt","target":{"target":"session","id":"worker"},"text":"summarise the diff"}]
[{"command":"resolve_pending","target":{"target":"session","id":"worker"},"request_id":null,"outcome":"allow_once"}]

If the instruction maps to no known command, return an empty array: []
"#;

/// Live [`IntentClient`] that calls BitRouter's local API via genai.
///
/// Creates one `tokio::runtime::Runtime` on construction so the async genai
/// call can be driven synchronously from `parse()`, which is called off the
/// UI thread (from a `std::thread`).
pub struct GenAiIntentClient {
    /// Model name in `provider/model` slash form accepted by BitRouter.
    model: String,
    /// BitRouter OpenAI-compatible base URL.
    endpoint: String,
    /// Tokio runtime used to drive the async genai call.
    runtime: Runtime,
}

impl GenAiIntentClient {
    /// Create a client targeting BitRouter at `http://localhost:4356/v1/`.
    ///
    /// `model` should be a `provider/model` string that BitRouter understands,
    /// e.g. `"claude/claude-sonnet-4-5"`.
    pub fn new(model: impl Into<String>) -> anyhow::Result<Self> {
        let runtime = Runtime::new().context("failed to create tokio runtime")?;
        Ok(Self {
            model: model.into(),
            endpoint: "http://localhost:4356/v1/".into(),
            runtime,
        })
    }
}

impl IntentClient for GenAiIntentClient {
    fn parse(&self, text: &str) -> anyhow::Result<Vec<Command>> {
        let model_name = self.model.clone();
        let endpoint_url = self.endpoint.clone();
        let user_text = text.to_owned();

        self.runtime.block_on(async move {
            // Route every call through BitRouter's local OpenAI-compatible endpoint.
            // We use AdapterKind::OpenAI and override endpoint + auth via
            // ServiceTargetResolver so genai formats the request as an OpenAI chat
            // completion — BitRouter speaks that wire format.
            let target_resolver =
                ServiceTargetResolver::from_resolver_fn(move |st: ServiceTarget| {
                    let endpoint = Endpoint::from_owned(endpoint_url.clone());
                    // BitRouter acts as a transparent proxy — no bearer token needed
                    // for local connections, but the field must be provided; use None.
                    let auth = AuthData::None;
                    let model = ModelIden::new(AdapterKind::OpenAI, st.model.model_name);
                    Ok(ServiceTarget {
                        endpoint,
                        auth,
                        model,
                    })
                });

            let client = Client::builder()
                .with_service_target_resolver(target_resolver)
                .build();

            let chat_req = ChatRequest::default()
                .with_system(SYSTEM_PROMPT)
                .append_message(ChatMessage::user(user_text));

            let response = client
                .exec_chat(model_name.as_str(), chat_req, None)
                .await
                .context("genai exec_chat failed")?;

            let raw = response
                .first_text()
                .context("model returned no text content")?;

            // Accept both a JSON array and a single object.
            let commands: Vec<Command> = if raw.trim_start().starts_with('[') {
                serde_json::from_str(raw).context("failed to parse JSON array of commands")?
            } else {
                let single: Command =
                    serde_json::from_str(raw).context("failed to parse single Command JSON")?;
                vec![single]
            };

            Ok(commands)
        })
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{parse_intent, IntentClient};
    use bitrouter_gui_core::protocol::{Command, TabId};

    // -- Fake client that returns a SpawnAgent command when the text contains "spawn"

    struct FakeSpawnClient;

    impl IntentClient for FakeSpawnClient {
        fn parse(&self, text: &str) -> anyhow::Result<Vec<Command>> {
            if text.contains("spawn") {
                Ok(vec![Command::SpawnAgent {
                    agent_id: "worker".into(),
                    model: "claude/claude-sonnet-4-5".into(),
                    worktree: None,
                    tab: TabId("main".into()),
                    prompt: None,
                }])
            } else {
                Ok(Vec::new())
            }
        }
    }

    // -- Fake client that always errors

    struct FakeErrorClient;

    impl IntentClient for FakeErrorClient {
        fn parse(&self, _text: &str) -> anyhow::Result<Vec<Command>> {
            Err(anyhow::anyhow!("simulated network failure"))
        }
    }

    #[test]
    fn parse_intent_spawn_keyword_returns_spawn_agent() -> anyhow::Result<()> {
        let client = FakeSpawnClient;
        let cmds = parse_intent(&client, "spawn a worker agent")?;
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            Command::SpawnAgent { agent_id, .. } => {
                assert_eq!(agent_id, "worker");
            }
            other => anyhow::bail!("unexpected command: {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn parse_intent_no_spawn_returns_empty() -> anyhow::Result<()> {
        let client = FakeSpawnClient;
        let cmds = parse_intent(&client, "list all agents")?;
        assert!(cmds.is_empty());
        Ok(())
    }

    #[test]
    fn parse_intent_empty_string_returns_empty_without_calling_client() -> anyhow::Result<()> {
        // FakeErrorClient would return Err — but empty input is short-circuited before
        // the client is called, so we must get Ok([]).
        let client = FakeErrorClient;
        let cmds = parse_intent(&client, "   ")?;
        assert!(cmds.is_empty());
        Ok(())
    }

    #[test]
    fn parse_intent_propagates_client_error() {
        let client = FakeErrorClient;
        let result = parse_intent(&client, "do something");
        assert!(result.is_err());
    }
}
