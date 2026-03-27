//! Discovery registry types and traits for models, tools, and agents.
//!
//! These are the core abstractions powering public discovery endpoints
//! (`GET /v1/models`, `GET /v1/tools`, `GET /v1/agents`). Each entry
//! type is protocol-agnostic — conversion from protocol-specific types
//! (MCP tools, A2A agent cards) happens in the respective crates.

use std::future::Future;

use serde_json::Value;

use super::routing_table::ModelPricing;

// ── Model ──────────────────────────────────────────────────────────

/// A single model available through a provider, with its metadata.
#[derive(Debug, Clone)]
pub struct ModelEntry {
    /// The upstream model ID (e.g. "gpt-4o", "claude-sonnet-4-20250514").
    pub id: String,
    /// The providers that offer this model.
    pub providers: Vec<String>,
    /// Human-readable display name.
    pub name: Option<String>,
    /// Brief description of the model's capabilities.
    pub description: Option<String>,
    /// Maximum input context window in tokens.
    pub max_input_tokens: Option<u64>,
    /// Maximum number of output tokens the model can produce.
    pub max_output_tokens: Option<u64>,
    /// Input modalities the model accepts (e.g. "text", "image").
    pub input_modalities: Vec<String>,
    /// Output modalities the model can produce.
    pub output_modalities: Vec<String>,
    /// Token pricing per million tokens.
    pub pricing: Option<ModelPricing>,
}

/// Read-only registry for discovering models available across all configured providers.
///
/// Parallel to [`RoutingTable`](super::routing_table::RoutingTable) which handles
/// request routing, this trait handles model discovery — listing what external
/// capabilities BitRouter knows about.
pub trait ModelRegistry {
    /// Lists all models available across all configured providers.
    fn list_models(&self) -> Vec<ModelEntry> {
        Vec::new()
    }
}

// ── Tool ───────────────────────────────────────────────────────────

/// A single tool available through the router, regardless of origin protocol.
///
/// Unifies MCP tools (structured, schema-driven) and A2A skills
/// (unstructured, tag-driven) into a common discovery type.
#[derive(Debug, Clone)]
pub struct ToolEntry {
    /// Machine-readable tool identifier (e.g. `"github/search"`).
    pub id: String,
    /// Human-readable display name.
    pub name: Option<String>,
    /// The server or agent that provides this tool.
    pub provider: String,
    /// Description of what the tool does.
    pub description: Option<String>,
    /// JSON Schema describing input parameters.
    pub input_schema: Option<Value>,
}

/// Read-only registry for discovering tools available across all sources.
pub trait ToolRegistry: Send + Sync {
    /// Lists all tools available through the router.
    fn list_tools(&self) -> impl Future<Output = Vec<ToolEntry>> + Send;
}

impl<T: ToolRegistry> ToolRegistry for std::sync::Arc<T> {
    async fn list_tools(&self) -> Vec<ToolEntry> {
        (**self).list_tools().await
    }
}

/// Combines two [`ToolRegistry`] implementations into one.
///
/// `list_tools()` returns entries from both registries (primary first).
pub struct CompositeToolRegistry<A, B> {
    primary: A,
    secondary: B,
}

impl<A, B> CompositeToolRegistry<A, B> {
    pub fn new(primary: A, secondary: B) -> Self {
        Self { primary, secondary }
    }
}

impl<A: ToolRegistry, B: ToolRegistry> ToolRegistry for CompositeToolRegistry<A, B> {
    async fn list_tools(&self) -> Vec<ToolEntry> {
        let mut tools = self.primary.list_tools().await;
        tools.extend(self.secondary.list_tools().await);
        tools
    }
}

// ── Skill ─────────────────────────────────────────────────────────

/// A skill tracked in the bitrouter skills registry.
///
/// Mirrors the Anthropic Skills API object shape while adding
/// bitrouter-specific fields (source, required_apis).
#[derive(Debug, Clone)]
pub struct SkillEntry {
    /// Unique skill identifier (UUID string).
    pub id: String,
    /// Skill name (agentskills.io format: 1–64 chars, lowercase + hyphens).
    pub name: String,
    /// What the skill does and when to use it.
    pub description: String,
    /// "config" or "manual".
    pub source: String,
    /// Provider names this skill depends on for paid API access.
    pub required_apis: Vec<String>,
    /// ISO 8601 timestamp.
    pub created_at: String,
    /// ISO 8601 timestamp.
    pub updated_at: String,
}

/// CRUD service for the skills registry.
///
/// Implemented by `bitrouter-skills`, consumed by `bitrouter-api` filters.
pub trait SkillService: Send + Sync {
    /// Register a new skill. Returns the assigned ID.
    fn create(
        &self,
        name: String,
        description: String,
        source: Option<String>,
        required_apis: Vec<String>,
    ) -> impl Future<Output = Result<SkillEntry, String>> + Send;

    /// List all registered skills.
    fn list(&self) -> impl Future<Output = Result<Vec<SkillEntry>, String>> + Send;

    /// Retrieve a single skill by name.
    fn get(&self, name: &str) -> impl Future<Output = Result<Option<SkillEntry>, String>> + Send;

    /// Delete a skill by name. Returns true if it existed.
    fn delete(&self, name: &str) -> impl Future<Output = Result<bool, String>> + Send;
}

impl<T: SkillService> SkillService for std::sync::Arc<T> {
    async fn create(
        &self,
        name: String,
        description: String,
        source: Option<String>,
        required_apis: Vec<String>,
    ) -> Result<SkillEntry, String> {
        (**self)
            .create(name, description, source, required_apis)
            .await
    }

    async fn list(&self) -> Result<Vec<SkillEntry>, String> {
        (**self).list().await
    }

    async fn get(&self, name: &str) -> Result<Option<SkillEntry>, String> {
        (**self).get(name).await
    }

    async fn delete(&self, name: &str) -> Result<bool, String> {
        (**self).delete(name).await
    }
}

// ── Agent ──────────────────────────────────────────────────────────

/// A single agent available through the router, with its metadata.
///
/// Protocol-agnostic summary of an agent's identity and capabilities.
/// Conversion from A2A `AgentCard` happens in [`crate::api::a2a::types`].
#[derive(Debug, Clone)]
pub struct AgentEntry {
    /// Machine-readable agent identifier.
    pub id: String,
    /// Human-readable display name.
    pub name: Option<String>,
    /// The source that provides this agent.
    pub provider: String,
    /// Description of the agent's capabilities.
    pub description: Option<String>,
    /// Agent version string.
    pub version: Option<String>,
    /// Skills or capabilities the agent advertises.
    pub skills: Vec<AgentSkillEntry>,
    /// Input content types the agent accepts (MIME types).
    pub input_modes: Vec<String>,
    /// Output content types the agent produces (MIME types).
    pub output_modes: Vec<String>,
    /// Whether the agent supports streaming responses.
    pub streaming: Option<bool>,
    /// Icon URL for display.
    pub icon_url: Option<String>,
    /// Documentation URL.
    pub documentation_url: Option<String>,
}

/// A skill advertised by an agent.
#[derive(Debug, Clone)]
pub struct AgentSkillEntry {
    /// Unique skill identifier.
    pub id: String,
    /// Human-readable skill name.
    pub name: String,
    /// Description of the skill.
    pub description: Option<String>,
    /// Keywords for discovery.
    pub tags: Vec<String>,
    /// Example prompts or scenarios.
    pub examples: Vec<String>,
}

/// Read-only registry for discovering agents available across all sources.
pub trait AgentRegistry: Send + Sync {
    /// Lists all agents available through the router.
    fn list_agents(&self) -> impl Future<Output = Vec<AgentEntry>> + Send;
}

impl<T: AgentRegistry> AgentRegistry for std::sync::Arc<T> {
    async fn list_agents(&self) -> Vec<AgentEntry> {
        (**self).list_agents().await
    }
}
