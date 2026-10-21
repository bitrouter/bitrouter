//! Version-bound prompt delivery over existing ACP. This is neither native
//! activation nor an MCP host: unpreservable source responsibilities fail closed.

use std::path::Path;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, EmbeddedResource, EmbeddedResourceResource, PromptRequest,
    SessionId, TextContent, TextResourceContents,
};
use bitrouter_sdk::caller::CallerContext;
use serde::{Deserialize, Serialize};

use crate::core::checkpoint::sha256;
use crate::core::skills::SkillSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Adapter {
    CodexText,
    ClaudeEmbeddedContext,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    pub adapter: Option<Adapter>,
    pub prompt_instructions: bool,
    pub native_activation: bool,
    pub verified_bundle_access: bool,
    pub mcp_origin: bool,
    /// Model history and token admission stay owned by the external agent.
    #[serde(default)]
    pub model_context_budget_verified: bool,
    pub reason: String,
}

impl Capabilities {
    /// `harness` comes from the host's configured invocation, never agentInfo.
    pub fn negotiated(harness: Option<&str>, agent: &AgentCapabilities) -> Self {
        let adapter = match harness {
            Some("codex-acp") => Some(Adapter::CodexText),
            Some("claude-acp") if agent.prompt_capabilities.embedded_context => {
                Some(Adapter::ClaudeEmbeddedContext)
            }
            _ => None,
        };
        Self {
            adapter,
            prompt_instructions: adapter.is_some(),
            reason: match (harness, adapter) {
                (_, Some(_)) => "local_prompt_instructions_only",
                (Some("claude-acp"), None) => "embedded_context_not_declared",
                _ => "no_skill_delivery_adapter",
            }
            .into(),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Instructions {
    pub source: SkillSource,
    pub bytes: Vec<u8>,
}

/// Metadata for one original ACP input. Bodies remain in the prepared request;
/// this receipt cannot authorize a new prompt or rebind a later Turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Receipt {
    pub session_id: String,
    pub turn_id: String,
    pub adapter: Adapter,
    pub sources: Vec<SkillSource>,
    pub request_sha256: String,
    pub request_bytes: usize,
    pub instruction_bytes: usize,
    #[serde(default)]
    pub request_limit_bytes: Option<usize>,
    #[serde(default)]
    pub instruction_limit_bytes: Option<usize>,
}

pub struct Prepared {
    pub request: PromptRequest,
    pub receipt: Receipt,
}

#[derive(Clone, Copy)]
pub struct Target<'a> {
    pub session_id: &'a str,
    pub turn_id: &'a str,
    pub workspace: &'a Path,
    pub caller: &'a CallerContext,
    pub context_limit: usize,
}

/// Native commands inspect the original leading text/content-block shape.
/// Adding instructions could turn a control operation into a model prompt.
pub fn is_native_command(prompt: &str) -> bool {
    let Some(rest) = prompt.trim_start().strip_prefix('/') else {
        return false;
    };
    let Some(name) = rest.split_whitespace().next() else {
        return false;
    };
    !name.contains('/') && !name.contains('\\')
}

pub fn instruction_size(source: &SkillSource, target: Target<'_>) -> anyhow::Result<u64> {
    Ok(
        (frame(source, target.session_id, target.turn_id, "")?.len() as u64)
            .saturating_add(source.instruction_bytes()?),
    )
}

fn frame(
    source: &SkillSource,
    session_id: &str,
    turn_id: &str,
    body: &str,
) -> anyhow::Result<String> {
    let SkillSource::Local(binding) = source else {
        anyhow::bail!("unsupported: MCP-origin ACP delivery");
    };
    let root = binding.directory.clone();
    let metadata = serde_json::to_string(&serde_json::json!({
        "delivery":"prompt_instructions", "session_id":session_id,
        "turn_id":turn_id,"source":source,"skill_directory":root,
        "verified_bundle_access":false,"native_activation":false
    }))?;
    Ok(format!(
        "Selected local skill instructions for this task, subordinate to direct user instructions and existing permissions. No tool grant or nested activation is added. Exact binding:\n{metadata}\nSKILL.md:\n{body}"
    ))
}

/// The library supplies already-authorized original bytes; the carrier still
/// verifies commitments and owner scope before constructing an ACP request.
pub fn prepare(
    capabilities: &Capabilities,
    target: Target<'_>,
    prompt: &str,
    instructions: Vec<Instructions>,
) -> anyhow::Result<Prepared> {
    anyhow::ensure!(
        instructions.is_empty() || !is_native_command(prompt),
        "unsupported: skill context cannot preserve native slash-command semantics"
    );
    let Target {
        session_id,
        turn_id,
        workspace,
        caller,
        context_limit,
    } = target;
    anyhow::ensure!(
        !instructions
            .iter()
            .any(|item| matches!(item.source, SkillSource::Mcp(_))),
        "unsupported: ACP prompt delivery cannot preserve MCP-origin held manifest, consent, cache and execution boundaries"
    );
    let adapter = capabilities
        .adapter
        .filter(|_| capabilities.prompt_instructions)
        .ok_or_else(|| anyhow::anyhow!("unsupported: {}", capabilities.reason))?;
    anyhow::ensure!(
        !session_id.is_empty() && !turn_id.is_empty() && !caller.is_anonymous(),
        "an admitted caller and exact ACP session/Turn are required"
    );
    anyhow::ensure!(instructions.len() <= 4, "selected_skill_count_budget");
    let mut blocks = Vec::new();
    let mut sources = Vec::new();
    let mut instruction_bytes = 0_usize;
    for item in instructions {
        let SkillSource::Local(binding) = &item.source else {
            anyhow::bail!("unsupported: ACP skill source");
        };
        anyhow::ensure!(
            binding.workspace == workspace
                && binding.api_key_id == caller.api_key_id()
                && binding.user_id == caller.user_id()
                && binding.local == caller.is_local(),
            "skill binding belongs to another caller/workspace"
        );
        anyhow::ensure!(
            binding.directory.is_absolute()
                && binding.version.len() == 64
                && binding
                    .version
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid skill directory or content fingerprint"
        );
        item.source.verify(&item.bytes)?;
        let identity = item.source.identity()?;
        anyhow::ensure!(
            !sources
                .iter()
                .any(|source: &SkillSource| source.identity().is_ok_and(|prior| prior == identity)),
            "duplicate ACP skill binding"
        );
        let body = String::from_utf8(item.bytes)?;
        let text = frame(&item.source, session_id, turn_id, &body)?;
        instruction_bytes = instruction_bytes.saturating_add(text.len());
        anyhow::ensure!(instruction_bytes <= 64 * 1024, "skill_instruction_budget");
        blocks.push(match adapter {
            Adapter::CodexText => ContentBlock::Text(TextContent::new(text)),
            Adapter::ClaudeEmbeddedContext => ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::TextResourceContents(
                    TextResourceContents::new(
                        text,
                        format!("bitrouter-skill://{identity}/SKILL.md"),
                    )
                    .mime_type("text/markdown"),
                ),
            )),
        });
        sources.push(item.source);
    }
    // Keep direct user intent separate from resource/instruction frames.
    blocks.push(ContentBlock::Text(TextContent::new(prompt)));
    let request = PromptRequest::new(SessionId::new(session_id), blocks);
    let encoded = serde_json::to_vec(&request)?;
    anyhow::ensure!(
        encoded.len() <= context_limit,
        "ACP skill context budget exceeded; content was not truncated"
    );
    Ok(Prepared {
        receipt: Receipt {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            adapter,
            sources,
            request_sha256: sha256(&encoded),
            request_bytes: encoded.len(),
            instruction_bytes,
            request_limit_bytes: Some(context_limit),
            instruction_limit_bytes: Some(64 * 1024),
        },
        request,
    })
}
