//! `bitrouter a2a` subcommand — A2A protocol client.

use bitrouter_a2a::client::a2a_client::{A2aClient, SendMessageResult};
use bitrouter_a2a::message::Part;
use bitrouter_a2a::request::{CancelTaskRequest, SendMessageRequest};
use bitrouter_a2a::task::{GetTaskRequest, ListTasksRequest, TaskState};

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

    let request = SendMessageRequest {
        tenant: None,
        message: A2aClient::text_message(message),
        configuration: None,
        metadata: None,
    };

    println!("Sending task to {} ({})...", card.name, endpoint);

    let result = client
        .send_message(endpoint, request)
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

    let request = GetTaskRequest {
        id: task_id.to_string(),
        history_length: None,
        tenant: None,
    };

    let task = client
        .get_task(endpoint, request)
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

    let request = CancelTaskRequest {
        id: task_id.to_string(),
        tenant: None,
    };

    let task = client
        .cancel_task(endpoint, request)
        .await
        .map_err(|e| format!("{e}"))?;

    println!("Task {} canceled.", task.id);
    print_task(&task);

    Ok(())
}

/// List tasks from a remote agent.
pub async fn run_list_tasks(url: &str) -> Result<(), String> {
    let client = A2aClient::new();

    let card = client
        .discover(url)
        .await
        .map_err(|e| format!("discovery failed: {e}"))?;

    let endpoint = A2aClient::resolve_endpoint(&card)
        .ok_or_else(|| "agent has no supported interfaces".to_string())?;

    let request = ListTasksRequest {
        context_id: None,
        status: None,
        status_timestamp_after: None,
        page_size: None,
        page_token: None,
        history_length: Some(0),
        include_artifacts: Some(false),
        tenant: None,
    };

    let response = client
        .list_tasks(endpoint, request)
        .await
        .map_err(|e| format!("{e}"))?;

    if response.tasks.is_empty() {
        println!("No tasks found.");
        return Ok(());
    }

    println!("Tasks ({} total):", response.total_size);
    for task in &response.tasks {
        println!(
            "  {} — {} ({})",
            task.id,
            state_label(&task.status.state),
            task.status.timestamp
        );
    }

    if let Some(ref token) = response.next_page_token {
        println!("  ... more results (next page token: {token})");
    }

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
