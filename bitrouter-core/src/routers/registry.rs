//! Discovery registry types and traits for models and skills.
//!
//! These are the core abstractions powering public discovery endpoints
//! (`GET /v1/models`). Each entry type is protocol-agnostic — conversion
//! from protocol-specific types happens in the respective crates.
//!
//! Tool registry types live in [`crate::tools::registry`].

use std::future::Future;

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
/// Implemented by `bitrouter-providers::agentskills`, consumed by `bitrouter-api` filters.
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
