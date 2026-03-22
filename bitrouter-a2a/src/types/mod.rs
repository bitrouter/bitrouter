//! A2A v0.3.0 protocol types.
//!
//! Defines the complete A2A v0.3.0 type schema including Agent Cards,
//! Messages, Tasks, streaming events, JSON-RPC wire format, security
//! schemes, and request/response types.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ── Agent Card ───────────────────────────────────────────────────

/// An Agent Card — the self-describing manifest for an A2A agent.
///
/// Published at `/.well-known/agent-card.json` for discovery. Contains the
/// agent's identity, capabilities, skills, security requirements, and
/// preferred endpoint URL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    /// Human-readable agent name.
    pub name: String,

    /// Purpose description for users and other agents.
    pub description: String,

    /// Agent version (e.g., `"1.0.0"`).
    pub version: String,

    /// A2A protocol version (e.g., `"0.3.0"`).
    #[serde(default = "default_protocol_version")]
    pub protocol_version: String,

    /// Preferred endpoint URL for this agent.
    pub url: String,

    /// Service provider information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProvider>,

    /// Preferred transport binding (e.g., `"JSONRPC"`, `"GRPC"`, `"HTTP+JSON"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_transport: Option<String>,

    /// Additional interfaces the agent supports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_interfaces: Option<Vec<AgentInterface>>,

    /// Supported A2A capabilities.
    pub capabilities: AgentCapabilities,

    /// Named authentication scheme definitions.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub security_schemes: HashMap<String, SecurityScheme>,

    /// Security requirements for accessing this agent.
    /// Each entry maps a scheme name to required scopes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub security: Vec<HashMap<String, Vec<String>>>,

    /// Whether the agent supports an authenticated extended card endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_authenticated_extended_card: Option<bool>,

    /// Supported input media types across all skills.
    pub default_input_modes: Vec<String>,

    /// Supported output media types across all skills.
    pub default_output_modes: Vec<String>,

    /// Agent abilities and functions.
    pub skills: Vec<AgentSkill>,

    /// JWS signatures for card integrity verification.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signatures: Vec<AgentCardSignature>,

    /// URL to the agent's icon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,

    /// URL for additional documentation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,
}

fn default_protocol_version() -> String {
    "0.3.0".to_string()
}

/// Service provider of an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentProvider {
    /// Organization name.
    pub organization: String,
    /// Provider website or documentation URL.
    pub url: String,
}

/// An additional interface the agent supports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentInterface {
    /// Absolute URL where the interface is available.
    pub url: String,

    /// Transport type (e.g., `"JSONRPC"`, `"GRPC"`, `"HTTP+JSON"`).
    pub transport: String,
}

/// Optional capabilities supported by an agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    /// Supports streaming responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,

    /// Supports push notifications for async updates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_notifications: Option<bool>,

    /// Supports state transition history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_transition_history: Option<bool>,

    /// Supported protocol extensions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<AgentExtension>,
}

/// A protocol extension declaration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentExtension {
    /// Unique URI identifying the extension.
    pub uri: String,

    /// How the agent uses the extension.
    pub description: String,

    /// Whether the client must understand this extension.
    pub required: bool,

    /// Extension-specific configuration parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A distinct capability or function an agent performs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    /// Unique skill identifier.
    pub id: String,

    /// Human-readable skill name.
    pub name: String,

    /// Detailed capability description.
    pub description: String,

    /// Keywords describing capabilities.
    pub tags: Vec<String>,

    /// Example prompts or scenarios.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,

    /// Supported input media types (overrides agent defaults).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modes: Vec<String>,

    /// Supported output media types (overrides agent defaults).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modes: Vec<String>,

    /// Security requirements specific to this skill.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub security: Vec<HashMap<String, Vec<String>>>,
}

/// JWS signature of an Agent Card (RFC 7515 JSON Serialization format).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentCardSignature {
    /// Base64url-encoded JWS protected header.
    pub protected: String,

    /// Base64url-encoded computed signature.
    pub signature: String,

    /// Unprotected JWS header values.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<serde_json::Value>,
}

/// Build a minimal Agent Card with required fields only.
///
/// Sets reasonable defaults: empty skills, empty security, `text/plain`
/// input/output modes.
pub fn minimal_card(name: &str, description: &str, version: &str, url: &str) -> AgentCard {
    AgentCard {
        name: name.to_string(),
        description: description.to_string(),
        version: version.to_string(),
        protocol_version: "0.3.0".to_string(),
        url: url.to_string(),
        provider: None,
        preferred_transport: None,
        additional_interfaces: None,
        capabilities: AgentCapabilities::default(),
        security_schemes: HashMap::new(),
        security: Vec::new(),
        supports_authenticated_extended_card: None,
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["text/plain".to_string()],
        skills: Vec::new(),
        signatures: Vec::new(),
        icon_url: None,
        documentation_url: None,
    }
}

// ── Core conversions ───────────────────────────────────────────────

impl From<AgentSkill> for bitrouter_core::routers::registry::AgentSkillEntry {
    fn from(s: AgentSkill) -> Self {
        Self {
            id: s.id,
            name: s.name,
            description: Some(s.description),
            tags: s.tags,
            examples: s.examples,
        }
    }
}

impl From<AgentCard> for bitrouter_core::routers::registry::AgentEntry {
    fn from(card: AgentCard) -> Self {
        let provider = card
            .provider
            .as_ref()
            .map(|p| p.organization.clone())
            .unwrap_or_default();
        Self {
            id: card.name.clone(),
            name: Some(card.name),
            provider,
            description: Some(card.description),
            version: Some(card.version),
            skills: card.skills.into_iter().map(Into::into).collect(),
            input_modes: card.default_input_modes,
            output_modes: card.default_output_modes,
            streaming: card.capabilities.streaming,
            icon_url: card.icon_url,
            documentation_url: card.documentation_url,
        }
    }
}

// ── Message and Artifact ─────────────────────────────────────────

/// Role of the message sender.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MessageRole {
    /// User-initiated message.
    #[serde(rename = "user")]
    User,
    /// Agent-generated message.
    #[serde(rename = "agent")]
    Agent,
}

/// A single communication turn between client and agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    /// Object kind — always `"message"`.
    #[serde(default = "default_message_kind")]
    pub kind: String,

    /// Sender role.
    pub role: MessageRole,

    /// Content parts.
    pub parts: Vec<Part>,

    /// Unique message identifier.
    pub message_id: String,

    /// Logical conversation grouping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,

    /// Associated task identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,

    /// IDs of tasks this message references.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reference_task_ids: Vec<String>,

    /// Extension metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

fn default_message_kind() -> String {
    "message".to_string()
}

/// File content within a file Part.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileContent {
    /// Base64-encoded file bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<String>,

    /// URI reference to the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,

    /// File name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// MIME type (e.g., `"image/png"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Smallest unit of content within a Message or Artifact.
///
/// A2A v0.3.0 uses a tagged enum with `kind` discriminator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum Part {
    /// Plain text content.
    #[serde(rename = "text")]
    Text {
        /// The text content.
        text: String,
        /// Extension metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
    /// File content (inline bytes or URI reference).
    #[serde(rename = "file")]
    File {
        /// The file content.
        file: FileContent,
        /// Extension metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
    /// Structured JSON data.
    #[serde(rename = "data")]
    Data {
        /// The structured data.
        data: serde_json::Value,
        /// Extension metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
}

impl Part {
    /// Create a text part.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            metadata: None,
        }
    }

    /// Create a structured data part.
    pub fn data(data: serde_json::Value) -> Self {
        Self::Data {
            data,
            metadata: None,
        }
    }

    /// Create a file part with inline bytes and optional name/mime type.
    pub fn file_bytes(
        bytes: impl Into<String>,
        name: Option<String>,
        mime_type: Option<String>,
    ) -> Self {
        Self::File {
            file: FileContent {
                bytes: Some(bytes.into()),
                uri: None,
                name,
                mime_type,
            },
            metadata: None,
        }
    }

    /// Create a file part with a URI reference and optional name.
    pub fn file_uri(uri: impl Into<String>, name: Option<String>) -> Self {
        Self::File {
            file: FileContent {
                bytes: None,
                uri: Some(uri.into()),
                name,
                mime_type: None,
            },
            metadata: None,
        }
    }
}

/// An output deliverable produced by an agent task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    /// Unique artifact identifier.
    pub artifact_id: String,

    /// Human-readable artifact name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Human-readable artifact description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Content parts composing this artifact.
    pub parts: Vec<Part>,

    /// Extension metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

// ── Task ─────────────────────────────────────────────────────────

/// Task lifecycle states per A2A v0.3.0.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TaskState {
    /// Task accepted, awaiting processing.
    #[serde(rename = "submitted")]
    Submitted,
    /// Task actively executing.
    #[serde(rename = "working")]
    Working,
    /// Task completed successfully.
    #[serde(rename = "completed")]
    Completed,
    /// Task execution failed.
    #[serde(rename = "failed")]
    Failed,
    /// Task canceled by client.
    #[serde(rename = "canceled")]
    Canceled,
    /// Agent declined the task.
    #[serde(rename = "rejected")]
    Rejected,
    /// Waiting for additional client input.
    #[serde(rename = "input-required")]
    InputRequired,
    /// Authentication needed to proceed.
    #[serde(rename = "auth-required")]
    AuthRequired,
    /// Unknown or unrecognized state.
    #[serde(rename = "unknown")]
    Unknown,
}

/// Current status of a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskStatus {
    /// Current lifecycle state.
    pub state: TaskState,

    /// ISO 8601 timestamp of the status change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,

    /// Optional agent message accompanying the status change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
}

/// Request parameters for the `tasks/get` JSON-RPC method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskRequest {
    /// Task ID to retrieve.
    pub id: String,

    /// Maximum number of history messages to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,
}

/// Request parameters for the `tasks/list` JSON-RPC method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ListTasksRequest {
    /// Filter by context ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,

    /// Filter by task state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskState>,

    /// Filter tasks with status timestamp after this ISO 8601 value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_timestamp_after: Option<String>,

    /// Maximum number of tasks per page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_size: Option<u32>,

    /// Cursor for pagination.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_token: Option<String>,

    /// Maximum number of history messages per task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,

    /// Whether to include artifacts in the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_artifacts: Option<bool>,
}

/// Response for the `tasks/list` JSON-RPC method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ListTasksResponse {
    /// Tasks matching the query.
    pub tasks: Vec<Task>,

    /// Cursor for the next page, if more results exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_page_token: Option<String>,

    /// Number of tasks in this page.
    pub page_size: u32,

    /// Total number of tasks matching the query.
    pub total_size: u32,
}

/// A stateful unit of work in the A2A protocol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// Object kind — always `"task"`.
    #[serde(default = "default_task_kind")]
    pub kind: String,

    /// Unique task identifier.
    pub id: String,

    /// Logical conversation grouping across related tasks.
    pub context_id: String,

    /// Current task status.
    pub status: TaskStatus,

    /// Output artifacts produced by the task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,

    /// Interaction history (messages exchanged during the task).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Message>,

    /// Extension metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

fn default_task_kind() -> String {
    "task".to_string()
}

// ── Streaming ────────────────────────────────────────────────────

/// A streaming response event from the server.
///
/// Serializes with an internally tagged `kind` field:
/// `"task"`, `"message"`, `"status-update"`, or `"artifact-update"`,
/// matching the A2A v0.3.0 `StreamResponse` wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum StreamResponse {
    /// Complete task snapshot.
    #[serde(rename = "task")]
    Task(Task),
    /// Direct message response.
    #[serde(rename = "message")]
    Message(Message),
    /// Task status change notification.
    #[serde(rename = "status-update")]
    StatusUpdate(TaskStatusUpdateEvent),
    /// Artifact data chunk or complete artifact.
    #[serde(rename = "artifact-update")]
    ArtifactUpdate(TaskArtifactUpdateEvent),
}

/// Notification of a task status change during streaming.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusUpdateEvent {
    /// Task ID this event pertains to.
    pub task_id: String,

    /// Context ID for the task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,

    /// New task status.
    pub status: TaskStatus,

    /// Whether this is the final event for the stream.
    #[serde(rename = "final")]
    pub is_final: bool,

    /// Extension metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Notification of an artifact update during streaming.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskArtifactUpdateEvent {
    /// Task ID this event pertains to.
    pub task_id: String,

    /// Context ID for the task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,

    /// The artifact being produced or updated.
    pub artifact: Artifact,

    /// Whether this chunk should be appended to a previous artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub append: Option<bool>,

    /// Whether this is the final chunk for this artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_chunk: Option<bool>,

    /// Extension metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

// ── Request types ────────────────────────────────────────────────

/// Request parameters for `message/send` / `message/stream`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageRequest {
    /// The user message to send.
    pub message: Message,

    /// Client-side configuration for the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<SendMessageConfiguration>,

    /// Extension metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Client configuration for a `message/send` request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageConfiguration {
    /// Accepted output media types.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_output_modes: Option<Vec<String>>,

    /// Push notification configuration for async updates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_notification_config: Option<PushNotificationConfig>,

    /// Maximum number of history messages to include in the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,

    /// Whether the call should block until completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocking: Option<bool>,
}

/// Request parameters for `tasks/cancel`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CancelTaskRequest {
    /// Task ID to cancel.
    pub id: String,
}

/// Request parameters for `tasks/resubscribe`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeToTaskRequest {
    /// Task ID to subscribe to.
    pub task_id: String,
}

/// Push notification configuration associated with a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskPushNotificationConfig {
    /// Task ID this config applies to.
    pub task_id: String,

    /// The push notification configuration.
    pub push_notification_config: PushNotificationConfig,
}

/// Push notification endpoint configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PushNotificationConfig {
    /// Config ID (generated if not provided).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Webhook URL to receive push notifications.
    pub url: String,

    /// Bearer token for the webhook.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Authentication info for the webhook.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authentication: Option<PushNotificationAuthenticationInfo>,
}

/// Authentication credentials for push notification webhooks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PushNotificationAuthenticationInfo {
    /// Supported authentication schemes.
    pub schemes: Vec<String>,

    /// Credentials value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
}

/// Request parameters for `tasks/pushNotificationConfig/get`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskPushNotificationConfigRequest {
    /// Task ID.
    pub id: String,

    /// Optional push notification config ID to get a specific config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_notification_config_id: Option<String>,
}

/// Request parameters for `tasks/pushNotificationConfig/list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ListTaskPushNotificationConfigsRequest {
    /// Task ID.
    pub id: String,
}

/// Request parameters for `tasks/pushNotificationConfig/delete`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeleteTaskPushNotificationConfigRequest {
    /// Task ID.
    pub id: String,

    /// Push notification config ID to delete.
    pub push_notification_config_id: String,
}

/// Result of a `message/send` call — may be a full Task or a direct Message.
#[derive(Debug, Clone)]
pub enum SendMessageResult {
    /// Server returned a full task with status and lifecycle.
    Task(Box<Task>),
    /// Server returned a direct message response (no task lifecycle).
    Message(Box<Message>),
}

// ── Security ─────────────────────────────────────────────────────

/// A security scheme declared in an Agent Card.
///
/// Mirrors the A2A v0.3.0 `SecurityScheme` oneof, serialized with a `type`
/// discriminator for JSON round-tripping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SecurityScheme {
    /// API key passed via header, query, or cookie.
    ApiKey(ApiKeySecurityScheme),
    /// HTTP authentication (Bearer, Basic, etc.).
    Http(HttpAuthSecurityScheme),
    /// OAuth 2.0 authentication.
    #[serde(rename = "oauth2")]
    OAuth2(Box<OAuth2SecurityScheme>),
    /// OpenID Connect authentication.
    OpenIdConnect(OpenIdConnectSecurityScheme),
    /// Mutual TLS authentication.
    MutualTls(MutualTlsSecurityScheme),
}

/// API key-based authentication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiKeySecurityScheme {
    /// Header, query, or cookie parameter name.
    pub name: String,
    /// Where the key is sent.
    #[serde(rename = "in")]
    pub location: ApiKeyLocation,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Location of an API key in the request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ApiKeyLocation {
    Query,
    Header,
    Cookie,
}

/// HTTP authentication scheme (RFC 7235).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpAuthSecurityScheme {
    /// HTTP authentication scheme name (e.g., `"Bearer"`, `"Basic"`).
    pub scheme: String,
    /// Bearer token format hint (e.g., `"JWT"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bearer_format: Option<String>,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// OAuth 2.0 authentication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OAuth2SecurityScheme {
    /// Supported OAuth 2.0 flow configurations.
    pub flows: OAuthFlows,
    /// RFC 8414 authorization server metadata URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth2_metadata_url: Option<String>,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// OAuth 2.0 flow configurations.
///
/// At least one flow should be specified.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OAuthFlows {
    /// Authorization Code flow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_code: Option<AuthorizationCodeOAuthFlow>,
    /// Client Credentials flow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_credentials: Option<ClientCredentialsOAuthFlow>,
    /// Device Code flow (RFC 8628).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_code: Option<DeviceCodeOAuthFlow>,
    /// Implicit flow (deprecated in OAuth 2.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implicit: Option<ImplicitOAuthFlow>,
    /// Resource Owner Password flow (deprecated in OAuth 2.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<PasswordOAuthFlow>,
}

/// OAuth 2.0 Authorization Code flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuthorizationCodeOAuthFlow {
    /// Authorization endpoint URL.
    pub authorization_url: String,
    /// Token endpoint URL.
    pub token_url: String,
    /// Refresh token endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    /// Available scopes (scope name → description).
    pub scopes: HashMap<String, String>,
    /// Whether PKCE (RFC 7636) is required.
    pub pkce_required: bool,
}

/// OAuth 2.0 Client Credentials flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientCredentialsOAuthFlow {
    /// Token endpoint URL.
    pub token_url: String,
    /// Refresh token endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    /// Available scopes (scope name → description).
    pub scopes: HashMap<String, String>,
}

/// OAuth 2.0 Device Code flow (RFC 8628).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceCodeOAuthFlow {
    /// Device authorization endpoint URL.
    pub device_authorization_url: String,
    /// Token endpoint URL.
    pub token_url: String,
    /// Refresh token endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    /// Available scopes (scope name → description).
    pub scopes: HashMap<String, String>,
}

/// OAuth 2.0 Implicit flow (deprecated).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImplicitOAuthFlow {
    /// Authorization endpoint URL.
    pub authorization_url: String,
    /// Refresh token endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    /// Available scopes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<HashMap<String, String>>,
}

/// OAuth 2.0 Resource Owner Password flow (deprecated).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PasswordOAuthFlow {
    /// Token endpoint URL.
    pub token_url: String,
    /// Refresh token endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    /// Available scopes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<HashMap<String, String>>,
}

/// OpenID Connect authentication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenIdConnectSecurityScheme {
    /// OpenID Connect Discovery URL.
    pub open_id_connect_url: String,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Mutual TLS authentication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MutualTlsSecurityScheme {
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// ── JSON-RPC ─────────────────────────────────────────────────────

/// A JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcRequest {
    /// Protocol version — always `"2.0"`.
    pub jsonrpc: String,

    /// Request identifier.
    pub id: String,

    /// Method name (e.g., `"message/send"`, `"tasks/get"`).
    pub method: String,

    /// Method parameters.
    pub params: serde_json::Value,
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcResponse {
    /// Protocol version — always `"2.0"`.
    pub jsonrpc: String,

    /// Request identifier (matches the request).
    pub id: String,

    /// Successful result (mutually exclusive with `error`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,

    /// Error result (mutually exclusive with `result`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcError {
    /// Error code.
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
    /// Additional error data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    /// Create a new JSON-RPC 2.0 request.
    pub fn new(id: &str, method: &str, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: id.to_string(),
            method: method.to_string(),
            params,
        }
    }
}

impl JsonRpcResponse {
    /// Returns the result value, or an error if the response is an error.
    pub fn into_result(self) -> Result<serde_json::Value, JsonRpcError> {
        if let Some(err) = self.error {
            return Err(err);
        }
        self.result.ok_or_else(|| JsonRpcError {
            code: -32603,
            message: "response contains neither result nor error".to_string(),
            data: None,
        })
    }
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod card_tests {
    use super::*;

    #[test]
    fn agent_card_round_trip() {
        let card = minimal_card(
            "test-agent",
            "A test agent",
            "1.0.0",
            "https://agent.example.com",
        );

        let json = serde_json::to_string_pretty(&card).expect("serialize");
        let parsed: AgentCard = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(card, parsed);
    }

    #[test]
    fn full_agent_card_round_trip() {
        let card = AgentCard {
            name: "smart-assistant".to_string(),
            description: "A smart assistant agent".to_string(),
            version: "2.1.0".to_string(),
            protocol_version: "0.3.0".to_string(),
            url: "https://agent.acme.example.com/a2a".to_string(),
            provider: Some(AgentProvider {
                organization: "Acme Corp".to_string(),
                url: "https://acme.example.com".to_string(),
            }),
            preferred_transport: Some("JSONRPC".to_string()),
            additional_interfaces: Some(vec![AgentInterface {
                url: "https://agent.acme.example.com/rest".to_string(),
                transport: "HTTP+JSON".to_string(),
            }]),
            capabilities: AgentCapabilities {
                streaming: Some(true),
                push_notifications: Some(false),
                state_transition_history: None,
                extensions: vec![AgentExtension {
                    uri: "https://a2a.example.com/ext/logging".to_string(),
                    description: "Structured logging extension".to_string(),
                    required: false,
                    params: Some(serde_json::json!({"level": "info"})),
                }],
            },
            security_schemes: HashMap::from([(
                "bearer".to_string(),
                SecurityScheme::Http(HttpAuthSecurityScheme {
                    scheme: "Bearer".to_string(),
                    bearer_format: Some("JWT".to_string()),
                    description: None,
                }),
            )]),
            security: vec![HashMap::from([(
                "bearer".to_string(),
                vec!["agent:invoke".to_string()],
            )])],
            supports_authenticated_extended_card: Some(true),
            default_input_modes: vec!["text/plain".to_string(), "application/json".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
            skills: vec![AgentSkill {
                id: "text-gen".to_string(),
                name: "Text Generation".to_string(),
                description: "Generate text from prompts".to_string(),
                tags: vec!["llm".to_string(), "text".to_string()],
                examples: vec!["Write a poem about Rust".to_string()],
                input_modes: Vec::new(),
                output_modes: Vec::new(),
                security: Vec::new(),
            }],
            signatures: vec![AgentCardSignature {
                protected: "eyJhbGciOiJFZERTQSJ9".to_string(),
                signature: "abc123".to_string(),
                header: None,
            }],
            icon_url: Some("https://acme.example.com/icon.png".to_string()),
            documentation_url: Some("https://docs.acme.example.com".to_string()),
        };

        let json = serde_json::to_string_pretty(&card).expect("serialize");
        let parsed: AgentCard = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(card, parsed);
    }

    #[test]
    fn minimal_card_has_defaults() {
        let card = minimal_card("test", "desc", "0.1.0", "http://localhost:8787");
        assert!(card.skills.is_empty());
        assert!(card.security_schemes.is_empty());
        assert_eq!(card.protocol_version, "0.3.0");
        assert_eq!(card.url, "http://localhost:8787");
    }
}

#[cfg(test)]
mod message_tests {
    use super::*;

    #[test]
    fn text_part_round_trip() {
        let part = Part::text("hello world");
        let json = serde_json::to_string(&part).expect("serialize");
        let parsed: Part = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(part, parsed);
        assert!(json.contains("\"text\":\"hello world\""));
        // v0.3.0: "kind" tag is present
        assert!(json.contains("\"kind\":\"text\""));
    }

    #[test]
    fn file_bytes_part_round_trip() {
        let part = Part::file_bytes(
            "aGVsbG8=",
            Some("test.png".to_string()),
            Some("image/png".to_string()),
        );
        let json = serde_json::to_string(&part).expect("serialize");
        let parsed: Part = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(part, parsed);
        assert!(json.contains("\"kind\":\"file\""));
        assert!(json.contains("\"bytes\""));
        assert!(json.contains("\"mimeType\""));
    }

    #[test]
    fn file_uri_part_round_trip() {
        let part = Part::file_uri("https://example.com/file.png", Some("file.png".to_string()));
        let json = serde_json::to_string(&part).expect("serialize");
        let parsed: Part = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(part, parsed);
    }

    #[test]
    fn data_part_round_trip() {
        let part = Part::data(serde_json::json!({"key": "value"}));
        let json = serde_json::to_string(&part).expect("serialize");
        let parsed: Part = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(part, parsed);
    }

    #[test]
    fn message_round_trip() {
        let msg = Message {
            kind: "message".to_string(),
            role: MessageRole::User,
            parts: vec![Part::text("Review this code")],
            message_id: "msg-001".to_string(),
            context_id: Some("ctx-abc".to_string()),
            task_id: None,
            reference_task_ids: Vec::new(),
            metadata: None,
        };
        let json = serde_json::to_string_pretty(&msg).expect("serialize");
        let parsed: Message = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, parsed);
        assert!(json.contains("\"user\""));
    }

    #[test]
    fn message_role_v03_format() {
        let json = serde_json::to_string(&MessageRole::User).expect("serialize");
        assert_eq!(json, "\"user\"");

        let json = serde_json::to_string(&MessageRole::Agent).expect("serialize");
        assert_eq!(json, "\"agent\"");

        let parsed: MessageRole = serde_json::from_str("\"agent\"").expect("deserialize");
        assert_eq!(parsed, MessageRole::Agent);
    }

    #[test]
    fn artifact_round_trip() {
        let artifact = Artifact {
            artifact_id: "art-001".to_string(),
            name: Some("review-result".to_string()),
            description: None,
            parts: vec![Part::text("Looks good!")],
            metadata: None,
        };
        let json = serde_json::to_string_pretty(&artifact).expect("serialize");
        let parsed: Artifact = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(artifact, parsed);
    }
}

#[cfg(test)]
mod task_tests {
    use super::*;

    #[test]
    fn task_state_serializes_v03_format() {
        let json = serde_json::to_string(&TaskState::InputRequired).expect("serialize");
        assert_eq!(json, "\"input-required\"");

        let parsed: TaskState = serde_json::from_str("\"auth-required\"").expect("deserialize");
        assert_eq!(parsed, TaskState::AuthRequired);

        let json = serde_json::to_string(&TaskState::Submitted).expect("serialize");
        assert_eq!(json, "\"submitted\"");

        let json = serde_json::to_string(&TaskState::Unknown).expect("serialize");
        assert_eq!(json, "\"unknown\"");
    }

    #[test]
    fn task_round_trip() {
        let task = Task {
            kind: "task".to_string(),
            id: "task-001".to_string(),
            context_id: "ctx-abc".to_string(),
            status: TaskStatus {
                state: TaskState::Completed,
                timestamp: Some("2026-03-17T10:30:00Z".to_string()),
                message: Some(Message {
                    kind: "message".to_string(),
                    role: MessageRole::Agent,
                    parts: vec![Part::text("Done reviewing")],
                    message_id: "msg-resp".to_string(),
                    context_id: None,
                    task_id: Some("task-001".to_string()),
                    reference_task_ids: Vec::new(),
                    metadata: None,
                }),
            },
            artifacts: vec![Artifact {
                artifact_id: "art-001".to_string(),
                name: Some("review".to_string()),
                description: None,
                parts: vec![Part::text("LGTM")],
                metadata: None,
            }],
            history: Vec::new(),
            metadata: None,
        };

        let json = serde_json::to_string_pretty(&task).expect("serialize");
        let parsed: Task = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(task, parsed);
    }

    #[test]
    fn minimal_task_round_trip() {
        let task = Task {
            kind: "task".to_string(),
            id: "task-002".to_string(),
            context_id: "ctx-default".to_string(),
            status: TaskStatus {
                state: TaskState::Submitted,
                timestamp: Some("2026-03-17T10:00:00Z".to_string()),
                message: None,
            },
            artifacts: Vec::new(),
            history: Vec::new(),
            metadata: None,
        };

        let json = serde_json::to_string(&task).expect("serialize");
        let parsed: Task = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(task, parsed);
        // Verify empty vecs are omitted.
        assert!(!json.contains("artifacts"));
        assert!(!json.contains("history"));
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;

    #[test]
    fn status_update_event_round_trip() {
        let event = TaskStatusUpdateEvent {
            task_id: "task-1".to_string(),
            context_id: Some("ctx-1".to_string()),
            status: TaskStatus {
                state: TaskState::Working,
                timestamp: Some("2026-03-19T00:00:00Z".to_string()),
                message: None,
            },
            is_final: false,
            metadata: None,
        };

        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: TaskStatusUpdateEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.task_id, "task-1");
        assert!(!parsed.is_final);
    }

    #[test]
    fn stream_response_tagged_serialization() {
        let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t-1".to_string(),
            context_id: None,
            status: TaskStatus {
                state: TaskState::Completed,
                timestamp: Some("2026-03-19T00:00:00Z".to_string()),
                message: None,
            },
            is_final: true,
            metadata: None,
        });

        let json = serde_json::to_string(&event).expect("serialize");
        // Internally tagged: {"kind": "status-update", ...}
        assert!(json.contains("\"kind\":\"status-update\""));
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;

    #[test]
    fn send_message_request_round_trip() {
        let req = SendMessageRequest {
            message: Message {
                kind: "message".to_string(),
                role: MessageRole::User,
                parts: vec![Part::text("hello")],
                message_id: "msg-1".to_string(),
                context_id: None,
                task_id: None,
                reference_task_ids: Vec::new(),
                metadata: None,
            },
            configuration: Some(SendMessageConfiguration {
                accepted_output_modes: Some(vec!["text/plain".to_string()]),
                push_notification_config: None,
                history_length: Some(5),
                blocking: None,
            }),
            metadata: None,
        };

        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: SendMessageRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, parsed);
    }

    #[test]
    fn cancel_task_request_round_trip() {
        let req = CancelTaskRequest {
            id: "task-1".to_string(),
        };

        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: CancelTaskRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, parsed);
    }

    #[test]
    fn push_notification_config_round_trip() {
        let config = TaskPushNotificationConfig {
            task_id: "task-1".to_string(),
            push_notification_config: PushNotificationConfig {
                id: Some("cfg-1".to_string()),
                url: "https://example.com/webhook".to_string(),
                token: None,
                authentication: Some(PushNotificationAuthenticationInfo {
                    schemes: vec!["Bearer".to_string()],
                    credentials: Some("tok-123".to_string()),
                }),
            },
        };

        let json = serde_json::to_string(&config).expect("serialize");
        let parsed: TaskPushNotificationConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(config, parsed);
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;

    #[test]
    fn security_scheme_round_trip() {
        let scheme = SecurityScheme::Http(HttpAuthSecurityScheme {
            scheme: "Bearer".to_string(),
            bearer_format: Some("JWT".to_string()),
            description: None,
        });

        let json = serde_json::to_string(&scheme).expect("serialize");
        let parsed: SecurityScheme = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(scheme, parsed);
    }

    #[test]
    fn api_key_scheme_round_trip() {
        let scheme = SecurityScheme::ApiKey(ApiKeySecurityScheme {
            name: "X-API-Key".to_string(),
            location: ApiKeyLocation::Header,
            description: Some("API key header".to_string()),
        });

        let json = serde_json::to_string(&scheme).expect("serialize");
        let parsed: SecurityScheme = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(scheme, parsed);
    }

    #[test]
    fn oauth2_flows_round_trip() {
        let flows = OAuthFlows {
            authorization_code: Some(AuthorizationCodeOAuthFlow {
                authorization_url: "https://auth.example.com/authorize".to_string(),
                token_url: "https://auth.example.com/token".to_string(),
                refresh_url: None,
                scopes: HashMap::from([("read".to_string(), "Read access".to_string())]),
                pkce_required: true,
            }),
            client_credentials: None,
            device_code: None,
            implicit: None,
            password: None,
        };

        let json = serde_json::to_string(&flows).expect("serialize");
        let parsed: OAuthFlows = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(flows, parsed);
    }
}

#[cfg(test)]
mod jsonrpc_tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let req = JsonRpcRequest::new(
            "req-1",
            "message/send",
            serde_json::json!({
                "message": {
                    "kind": "message",
                    "role": "user",
                    "parts": [{"kind": "text", "text": "hello"}],
                    "messageId": "msg-1"
                }
            }),
        );

        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: JsonRpcRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, parsed);
        assert_eq!(parsed.jsonrpc, "2.0");
    }

    #[test]
    fn success_response_into_result() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: "req-1".to_string(),
            result: Some(serde_json::json!({"id": "task-1"})),
            error: None,
        };
        let val = resp.into_result().expect("should succeed");
        assert_eq!(val["id"], "task-1");
    }

    #[test]
    fn error_response_into_result() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: "req-1".to_string(),
            result: None,
            error: Some(JsonRpcError {
                code: -32600,
                message: "Invalid Request".to_string(),
                data: None,
            }),
        };
        let err = resp.into_result().expect_err("should be error");
        assert_eq!(err.code, -32600);
    }

    #[test]
    fn empty_response_into_result() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: "req-1".to_string(),
            result: None,
            error: None,
        };
        let err = resp.into_result().expect_err("should be error");
        assert_eq!(err.code, -32603);
    }
}
