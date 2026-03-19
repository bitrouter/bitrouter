//! `bitrouter a2a` subcommand — A2A agent management and protocol client.

use std::fs;
use std::path::Path;

use bitrouter_a2a::card::{AgentCard, AgentProvider, minimal_card};
use bitrouter_a2a::client::{A2aClient, SendMessageResult};
use bitrouter_a2a::file_registry::FileAgentCardRegistry;
use bitrouter_a2a::message::Part;
use bitrouter_a2a::registry::{AgentCardRegistry, AgentRegistration};
use bitrouter_a2a::task::TaskState;

// ── Local agent card management ────────────────────────────────

/// Options for `bitrouter a2a register`.
pub struct RegisterOpts {
    /// Agent name (required unless --card is used).
    pub name: Option<String>,
    /// Import a full Agent Card from a JSON file.
    pub card: Option<String>,
    /// Agent description.
    pub description: Option<String>,
    /// Agent version.
    pub version: String,
    /// Provider organization name.
    pub provider_org: Option<String>,
    /// Bind to JWT iss claim (CAIP-10 address).
    pub iss: Option<String>,
    /// Base URL for the agent interface.
    pub url: Option<String>,
}

/// Register a new agent card.
pub fn run_register(agents_dir: &Path, opts: RegisterOpts) -> Result<(), String> {
    let registry =
        FileAgentCardRegistry::new(agents_dir).map_err(|e| format!("registry error: {e}"))?;

    let (card, iss) = if let Some(ref card_path) = opts.card {
        let contents = fs::read_to_string(card_path)
            .map_err(|e| format!("failed to read {card_path}: {e}"))?;
        let card: AgentCard = serde_json::from_str(&contents)
            .map_err(|e| format!("failed to parse agent card: {e}"))?;
        (card, opts.iss)
    } else {
        let name = opts
            .name
            .ok_or_else(|| "either --name or --card is required".to_string())?;
        let description = opts.description.unwrap_or_else(|| format!("{name} agent"));
        let url = opts
            .url
            .unwrap_or_else(|| "http://localhost:8787".to_string());

        let mut card = minimal_card(&name, &description, &opts.version, &url);
        if let Some(org) = opts.provider_org {
            card.provider = Some(AgentProvider {
                organization: org,
                url: url.clone(),
            });
        }
        (card, opts.iss)
    };

    let registration = AgentRegistration { card, iss };
    let name = registration.card.name.clone();
    registry
        .register(registration)
        .map_err(|e| format!("{e}"))?;

    println!("Registered agent: {name}");
    Ok(())
}

/// List all registered agents.
pub fn run_list(agents_dir: &Path) -> Result<(), String> {
    let registry =
        FileAgentCardRegistry::new(agents_dir).map_err(|e| format!("registry error: {e}"))?;
    let registrations = registry.list().map_err(|e| format!("{e}"))?;

    if registrations.is_empty() {
        println!("No agents registered.");
        println!("Run `bitrouter a2a register --name <name>` to register one.");
        return Ok(());
    }

    for reg in &registrations {
        let iss_info = reg
            .iss
            .as_deref()
            .map(|i| format!("  iss={i}"))
            .unwrap_or_default();
        println!("  {}  v{}{}", reg.card.name, reg.card.version, iss_info);
        println!("       {}", reg.card.description);
    }

    Ok(())
}

/// Show a specific agent's card as pretty-printed JSON.
pub fn run_show(agents_dir: &Path, name: &str) -> Result<(), String> {
    let registry =
        FileAgentCardRegistry::new(agents_dir).map_err(|e| format!("registry error: {e}"))?;
    let reg = registry
        .get(name)
        .map_err(|e| format!("{e}"))?
        .ok_or_else(|| format!("agent not found: {name}"))?;

    let pretty =
        serde_json::to_string_pretty(&reg.card).map_err(|e| format!("failed to format: {e}"))?;
    println!("{pretty}");

    if let Some(ref iss) = reg.iss {
        println!();
        println!("Bound to JWT iss: {iss}");
    }

    Ok(())
}

/// Remove a registered agent.
pub fn run_rm(agents_dir: &Path, name: &str) -> Result<(), String> {
    let registry =
        FileAgentCardRegistry::new(agents_dir).map_err(|e| format!("registry error: {e}"))?;
    registry.remove(name).map_err(|e| format!("{e}"))?;
    println!("Removed agent: {name}");
    Ok(())
}

// ── Remote A2A client operations ───────────────────────────────

/// Discover a remote agent by fetching its Agent Card.
pub async fn run_discover(url: &str) -> Result<(), String> {
    let client = A2aClient::new();
    let card = client.discover(url).await.map_err(|e| format!("{e}"))?;

    println!("Agent: {}", card.name);
    println!("Description: {}", card.description);
    println!("Version: {}", card.version);

    if let Some(ref provider) = card.provider {
        println!("Provider: {} ({})", provider.organization, provider.url);
    }

    if !card.supported_interfaces.is_empty() {
        println!("Interfaces:");
        for iface in &card.supported_interfaces {
            println!(
                "  {} ({} v{})",
                iface.url, iface.protocol_binding, iface.protocol_version
            );
        }
    }

    if !card.skills.is_empty() {
        println!("Skills:");
        for skill in &card.skills {
            println!("  {} — {}", skill.name, skill.description);
            if !skill.tags.is_empty() {
                println!("    tags: {}", skill.tags.join(", "));
            }
        }
    }

    let caps = &card.capabilities;
    let mut cap_list = Vec::new();
    if caps.streaming == Some(true) {
        cap_list.push("streaming");
    }
    if caps.push_notifications == Some(true) {
        cap_list.push("push-notifications");
    }
    if caps.extended_agent_card == Some(true) {
        cap_list.push("extended-card");
    }
    if !cap_list.is_empty() {
        println!("Capabilities: {}", cap_list.join(", "));
    }

    Ok(())
}

/// Send a task to a remote agent.
pub async fn run_send(url: &str, message: &str) -> Result<(), String> {
    let client = A2aClient::new();

    // Discover the agent to resolve its endpoint.
    let card = client
        .discover(url)
        .await
        .map_err(|e| format!("discovery failed: {e}"))?;

    let endpoint = A2aClient::resolve_endpoint(&card)
        .ok_or_else(|| "agent has no supported interfaces".to_string())?;

    let msg = A2aClient::text_message(message);

    println!("Sending task to {} ({})...", card.name, endpoint);

    let result = client
        .send_message(endpoint, msg)
        .await
        .map_err(|e| format!("{e}"))?;

    match result {
        SendMessageResult::Task(task) => print_task(&task),
        SendMessageResult::Message(msg) => {
            println!();
            println!("--- Agent Message ---");
            print_parts(&msg.parts);
            println!("---");
        }
    }

    Ok(())
}

/// Get the status of a task.
pub async fn run_status(url: &str, task_id: &str) -> Result<(), String> {
    let client = A2aClient::new();

    let card = client
        .discover(url)
        .await
        .map_err(|e| format!("discovery failed: {e}"))?;

    let endpoint = A2aClient::resolve_endpoint(&card)
        .ok_or_else(|| "agent has no supported interfaces".to_string())?;

    let task = client
        .get_task(endpoint, task_id)
        .await
        .map_err(|e| format!("{e}"))?;

    print_task(&task);

    Ok(())
}

/// Cancel a running task.
pub async fn run_cancel(url: &str, task_id: &str) -> Result<(), String> {
    let client = A2aClient::new();

    let card = client
        .discover(url)
        .await
        .map_err(|e| format!("discovery failed: {e}"))?;

    let endpoint = A2aClient::resolve_endpoint(&card)
        .ok_or_else(|| "agent has no supported interfaces".to_string())?;

    let task = client
        .cancel_task(endpoint, task_id)
        .await
        .map_err(|e| format!("{e}"))?;

    println!("Task {} canceled.", task.id);
    print_task(&task);

    Ok(())
}

// ── Output helpers ─────────────────────────────────────────────

fn print_task(task: &bitrouter_a2a::task::Task) {
    println!("Task: {}", task.id);
    if let Some(ref ctx) = task.context_id {
        println!("Context: {ctx}");
    }
    println!(
        "Status: {} ({})",
        state_label(&task.status.state),
        task.status.timestamp
    );

    if let Some(ref msg) = task.status.message {
        println!();
        println!("--- Agent Message ---");
        print_parts(&msg.parts);
        println!("---");
    }

    if !task.artifacts.is_empty() {
        println!();
        for artifact in &task.artifacts {
            let name = artifact.name.as_deref().unwrap_or(&artifact.artifact_id);
            println!("--- Artifact: {name} ---");
            print_parts(&artifact.parts);
            println!("---");
        }
    }
}

fn print_parts(parts: &[Part]) {
    for part in parts {
        if let Some(ref text) = part.text {
            println!("{text}");
        } else if let Some(ref url) = part.url {
            let name = part.filename.as_deref().unwrap_or("(unnamed)");
            println!("[url: {name}] {url}");
        } else if part.raw.is_some() {
            let name = part.filename.as_deref().unwrap_or("(unnamed file)");
            let mime = part.media_type.as_deref().unwrap_or("unknown");
            println!(
                "[file: {name} ({mime})] (inline, {} bytes)",
                part.raw.as_ref().map_or(0, |b| b.len())
            );
        } else if let Some(ref data) = part.data {
            let pretty = serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string());
            println!("{pretty}");
        }
    }
}

fn state_label(state: &TaskState) -> &'static str {
    match state {
        TaskState::Submitted => "submitted",
        TaskState::Working => "working",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Canceled => "canceled",
        TaskState::Rejected => "rejected",
        TaskState::InputRequired => "input-required",
        TaskState::AuthRequired => "auth-required",
    }
}
