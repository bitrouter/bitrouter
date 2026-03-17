# bitrouter-a2a

A2A (Agent-to-Agent) protocol implementation for BitRouter, targeting the [A2A v1.0 specification](https://a2a-protocol.org/latest/).

## What is A2A?

[A2A](https://github.com/a2aproject/A2A) is an open protocol by Google for agent-to-agent communication. Its core primitive is the **Agent Card** — a self-describing JSON manifest that declares an agent's identity, capabilities, skills, and security requirements. Agent Cards enable machine-readable discovery: any A2A-aware system can fetch a card and understand what an agent does, how to authenticate, and which protocols it speaks.

## Why A2A in BitRouter?

BitRouter routes LLM requests on behalf of agents. But until now, agents were anonymous — the proxy only knew the JWT identity (`iss` claim), not *what* the agent is, what it does, or how to describe it to other systems.

`bitrouter-a2a` solves this in two ways:

1. **Identity & Discovery** — every agent behind the proxy gets a first-class identity via A2A Agent Cards, served at `/.well-known/agent-card.json` for standard A2A discovery.
2. **A2A Client** — agents using BitRouter can discover and communicate with *any* remote A2A-compliant agent via the `bitrouter a2a` CLI, using the standard JSON-RPC 2.0 wire protocol.

This means a rich agent runtime (Claude Code, OpenClaw, Cursor) becomes A2A-enabled just by using BitRouter — it can be discovered by others, and it can discover and send tasks to remote agents through CLI commands.

## Design Decisions

### Standalone crate, zero internal dependencies

`bitrouter-a2a` depends only on `serde`, `serde_json`, `thiserror`, `tracing`, `warp`, and `reqwest`. It has **no dependency on `bitrouter-core`** or any other BitRouter crate. This is intentional:

- The A2A types are pure data — they don't need BitRouter's error types, traits, or models.
- The crate can be used independently as a general-purpose A2A library.
- It sits at the same layer as provider crates in BitRouter's dependency graph.

### Agent Card as identity replacement

This crate replaces the `agent_slug` / `agent_label` JWT claims proposed in [issue #127](https://github.com/bitrouter/bitrouter/issues/127). Instead of baking agent metadata into the JWT (which conflates authentication with identity), agent identity lives in the Agent Card — a separate, mutable, spec-compliant document. The JWT remains focused on authorization (who signed, what models, what budget), while the Agent Card answers "who is this agent?"

The binding between the two is stored in the registry as an `AgentRegistration`, which pairs an `AgentCard` with an optional `iss` (CAIP-10 address from the JWT).

### CLI-first A2A client for rich agent runtimes

Rather than requiring agent runtimes to integrate an SDK or implement an MCP server, `bitrouter-a2a` exposes A2A capabilities through the `bitrouter a2a` CLI. Any agent runtime with shell access (Claude Code, OpenClaw, Cursor) can discover remote agents, send tasks, and check results using standard CLI commands — no code changes or protocol integration needed.

The CLI speaks the A2A JSON-RPC 2.0 wire protocol to any compliant server, making BitRouter agents interoperable with the broader A2A ecosystem (LiteLLM, Google agents, LangGraph deployments, etc.).

### File-based registry

Agent registrations are stored as JSON files in `~/.bitrouter/agents/`:

```
~/.bitrouter/agents/
├── claude-code.json
├── cursor.json
└── my-custom-agent.json
```

Each file contains an `AgentRegistration` — the Agent Card plus an optional `iss` binding:

```json
{
  "card": {
    "name": "claude-code",
    "description": "Claude Code agent for software engineering",
    "version": "1.0.0",
    "supported_interfaces": [
      {
        "url": "http://localhost:8787",
        "protocol_binding": "http-rest",
        "protocol_version": "1.0"
      }
    ],
    "capabilities": {},
    "default_input_modes": ["text/plain"],
    "default_output_modes": ["text/plain"],
    "skills": []
  },
  "iss": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:DRpb..."
}
```

File-based storage was chosen over a database because:

- Agent cards change infrequently — they're managed by operators, not hot paths.
- JSON files are inspectable, diffable, and version-controllable.
- No migration overhead — just read/write files.
- Sufficient for the expected scale (<100 agents per deployment).

### Agent name validation

Names follow DNS label rules: `^[a-z0-9][a-z0-9-]*$`, max 63 characters. This ensures names are safe for use as filenames, URL path segments, and metric labels without escaping.

## Module Structure

```
src/
├── card.rs          # AgentCard, AgentSkill, AgentProvider, AgentInterface,
│                    # AgentCapabilities, AgentExtension, AgentCardSignature,
│                    # minimal_card() builder
├── security.rs      # SecurityScheme (ApiKey, Http, OAuth2, OpenIdConnect, MutualTls),
│                    # OAuthFlows (5 flow types), SecurityRequirement, StringList
├── message.rs       # Message, MessageRole, Part (text/file/data), FileContent, Artifact
├── task.rs          # Task, TaskStatus, TaskState (8 lifecycle states)
├── jsonrpc.rs       # JsonRpcRequest, JsonRpcResponse, JsonRpcError (JSON-RPC 2.0 wire format)
├── client.rs        # A2aClient — discover, send_message, get_task, cancel_task
├── error.rs         # A2aError enum
├── registry.rs      # AgentCardRegistry trait, AgentRegistration, validate_name()
├── file_registry.rs # FileAgentCardRegistry (file-backed implementation)
├── filters.rs       # Warp HTTP filters for discovery endpoints
└── lib.rs
```

## Usage

### A2A Client (CLI)

Discover and communicate with any remote A2A-compliant agent:

```bash
# Discover a remote agent
bitrouter a2a discover https://remote-agent.example.com
# → displays name, description, skills, capabilities, interfaces

# Send a task to a remote agent
bitrouter a2a send https://remote-agent.example.com --message "Review this PR"
# → sends message/send JSON-RPC, waits for completion, prints result

# Check task status
bitrouter a2a status https://remote-agent.example.com --task task-abc123
# → polls tasks/get, displays status and artifacts

# Cancel a running task
bitrouter a2a cancel https://remote-agent.example.com --task task-abc123
```

The CLI resolves the agent's endpoint URL from its Agent Card's `supported_interfaces` and speaks JSON-RPC 2.0 — the standard A2A wire protocol.

### Agent Registration (CLI)

```bash
# Register an agent with inline fields
bitrouter agent register \
  --name claude-code \
  --description "Claude Code agent for software engineering" \
  --version 1.0.0 \
  --provider-org Anthropic \
  --iss "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:DRpb..."

# Register from a full A2A Agent Card JSON file
bitrouter agent register --card ./my-agent-card.json --iss "solana:..."

# List registered agents
bitrouter agent list

# Show an agent's card
bitrouter agent show claude-code

# Remove an agent
bitrouter agent rm claude-code
```

### HTTP Discovery

When the server is running, agents are discoverable via standard A2A endpoints:

```bash
# Get the default agent card (A2A well-known endpoint)
curl http://localhost:8787/.well-known/agent-card.json

# Get a specific agent's card
curl http://localhost:8787/.well-known/agent-card.json?name=claude-code

# List all registered agents
curl http://localhost:8787/a2a/agents
```

The well-known endpoint includes `Cache-Control: max-age=3600` and `ETag` headers per the A2A v1.0 discovery specification.

### As a library

```rust
use bitrouter_a2a::client::A2aClient;
use bitrouter_a2a::card::{AgentCard, minimal_card};
use bitrouter_a2a::file_registry::FileAgentCardRegistry;
use bitrouter_a2a::registry::{AgentCardRegistry, AgentRegistration};

// ── Discovery & Communication ──────────────────────────────
let client = A2aClient::new();

// Discover a remote agent
let card = client.discover("https://remote-agent.example.com").await?;

// Send a task
let endpoint = A2aClient::resolve_endpoint(&card).expect("has interface");
let message = A2aClient::text_message("Review this code");
let task = client.send_message(endpoint, message).await?;

// Check task status
let task = client.get_task(endpoint, &task.id).await?;

// ── Local Registry ─────────────────────────────────────────
let registry = FileAgentCardRegistry::new("./agents")?;

// Register an agent
let card = minimal_card("my-agent", "My custom agent", "1.0.0", "http://localhost:8787");
registry.register(AgentRegistration { card, iss: None })?;

// Resolve agent name from JWT identity
let agent_name = registry.resolve_by_iss("solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:DRpb...")?;
```

## A2A v1.0 Spec Coverage

### Agent Card types (complete)

All required and optional fields from the [A2A v1.0 definitions](https://a2a-protocol.org/latest/definitions/) are implemented with full serde round-trip support:

- `AgentCard` — name, description, version, provider, supported_interfaces, capabilities, security_schemes, security_requirements, default_input_modes, default_output_modes, skills, signatures, icon_url, documentation_url
- `AgentProvider` — organization, url
- `AgentInterface` — url, protocol_binding (`json-rpc`, `grpc`, `http-rest`), protocol_version (`1.0`), tenant
- `AgentCapabilities` — streaming, push_notifications, extended_agent_card, extensions
- `AgentExtension` — uri, description, required, params
- `AgentSkill` — id, name, description, tags, examples, input_modes, output_modes, security_requirements
- `AgentCardSignature` — protected, signature, header (JWS structure, no signing/verification yet)

### Security scheme types (complete)

All five OpenAPI 3.2-aligned security scheme variants:

- `ApiKey` — name, location (query/header/cookie), description
- `Http` — scheme (Bearer/Basic/etc.), bearer_format, description
- `OAuth2` — flows (authorization_code with PKCE, client_credentials, device_code, implicit, password), oauth2_metadata_url
- `OpenIdConnect` — open_id_connect_url, description
- `MutualTls` — description

### Task protocol types (complete)

Full A2A v1.0 task lifecycle types:

- `Task` — id, context_id, status, artifacts, history
- `TaskStatus` — state, timestamp, message
- `TaskState` — submitted, working, completed, failed, canceled, rejected, input_required, auth_required
- `Message` — role (user/agent), parts, message_id, context_id, task_id, reference_task_ids, metadata
- `Part` — text, file (inline bytes or URI), data (structured JSON)
- `FileContent` — name, mime_type, bytes (base64), uri
- `Artifact` — artifact_id, name, parts, metadata

### A2A Client (implemented)

JSON-RPC 2.0 client for communicating with any A2A-compliant server:

- `discover()` — fetch Agent Card from `/.well-known/agent-card.json`
- `send_message()` — `message/send` JSON-RPC method (blocking, waits for task completion)
- `get_task()` — `tasks/get` JSON-RPC method (poll task status)
- `cancel_task()` — `tasks/cancel` JSON-RPC method

### What's not yet implemented

- **Streaming** — `message/sendStreaming` via SSE for real-time task updates.
- **Push notifications** — webhook-based async task notifications.
- **Authentication for remote agents** — the client does not yet handle `security_schemes` from Agent Cards (OAuth2, API keys). Connections to unauthenticated agents work; authenticated agents require manual header configuration.
- **JWS signing and verification** — `AgentCardSignature` is a data structure only; no cryptographic operations.
- **`GetExtendedAgentCard` RPC** — requires authenticated vs. public card variants.
- **A2A server (inbound tasks)** — BitRouter can send tasks to remote agents but does not yet receive inbound A2A tasks. This is the next phase.
- **Agent self-registration API** — agents cannot POST their own cards; operator-managed only.

## Architecture: A2A Client for Rich Agent Runtimes

The design philosophy is that rich agent runtimes (Claude Code, OpenClaw, Cursor) become A2A-enabled by using BitRouter as their LLM proxy — without integrating an SDK, implementing an MCP server, or changing any code.

```
Your Agent (Claude Code)
    │
    ├── LLM calls ──→ BitRouter proxy (localhost:8787) ──→ OpenAI / Anthropic / Google
    │
    └── A2A tasks ──→ bitrouter a2a send ... ──→ Remote A2A Agent
                      bitrouter a2a status ...     (any compliant server)
                      bitrouter a2a discover ...
```

The agent runtime uses two channels to BitRouter:

1. **LLM proxy** (existing) — routes model calls to providers.
2. **CLI** (new) — discovers remote agents and sends/receives A2A tasks via shell commands.

A Claude Code skill (`.claude/skills/a2a.md`) teaches the LLM how to use these CLI commands, so the agent can autonomously discover and communicate with other agents.
