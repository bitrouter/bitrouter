//! `bitrouter a2a` subcommand — A2A protocol client.

use bitrouter_a2a::client::a2a_client::{A2aClient, SendMessageResult};
use bitrouter_a2a::message::Part;
use bitrouter_a2a::request::{CancelTaskRequest, SendMessageRequest};
use bitrouter_a2a::task::{GetTaskRequest, ListTasksRequest};

// ── Remote A2A client operations ───────────────────────────────

/// Discover a remote agent by fetching its Agent Card.
pub async fn run_discover(url: &str) -> Result<(), String> {
    let client = A2aClient::new();
    let card = client.discover(url).await.map_err(|e| format!("{e}"))?;

    println!("Agent: {}", card.name);
    println!("Description: {}", card.description);
    println!("Version: {}", card.version);
    println!("Protocol: {}", card.protocol_version);
    println!("URL: {}", card.url);

    if let Some(ref provider) = card.provider {
        println!("Provider: {} ({})", provider.organization, provider.url);
    }

    if let Some(ref transport) = card.preferred_transport {
        println!("Preferred transport: {transport}");
    }

    if let Some(interfaces) = card
        .additional_interfaces
        .as_ref()
        .filter(|i| !i.is_empty())
    {
        println!("Additional interfaces:");
        for iface in interfaces {
            println!("  {} ({})", iface.url, iface.transport);
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
    if caps.state_transition_history == Some(true) {
        cap_list.push("state-transition-history");
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

    let endpoint = A2aClient::resolve_endpoint(&card);

    let request = SendMessageRequest {
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

    let endpoint = A2aClient::resolve_endpoint(&card);

    let request = GetTaskRequest {
        id: task_id.to_string(),
        history_length: None,
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

    let endpoint = A2aClient::resolve_endpoint(&card);

    let request = CancelTaskRequest {
        id: task_id.to_string(),
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

    let endpoint = A2aClient::resolve_endpoint(&card);

    let request = ListTasksRequest {
        context_id: None,
        status: None,
        status_timestamp_after: None,
        page_size: None,
        page_token: None,
        history_length: Some(0),
        include_artifacts: Some(false),
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
        let ts = task.status.timestamp.as_deref().unwrap_or("no timestamp");
        println!(
            "  {} — {} ({})",
            task.id,
            state_label(&task.status.state),
            ts
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
    println!("Context: {}", task.context_id);
    let ts = task.status.timestamp.as_deref().unwrap_or("no timestamp");
    println!("Status: {} ({})", state_label(&task.status.state), ts);

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
        match part {
            Part::Text { text, .. } => {
                println!("{text}");
            }
            Part::File { file, .. } => {
                let name = file.name.as_deref().unwrap_or("(unnamed)");
                if let Some(ref uri) = file.uri {
                    println!("[file: {name}] {uri}");
                } else {
                    let mime = file.mime_type.as_deref().unwrap_or("unknown");
                    let size = file.bytes.as_ref().map_or(0, |b| b.len());
                    println!("[file: {name} ({mime})] (inline, {size} bytes)");
                }
            }
            Part::Data { data, .. } => {
                let pretty =
                    serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string());
                println!("{pretty}");
            }
        }
    }
}

fn state_label(state: &bitrouter_a2a::task::TaskState) -> &'static str {
    match state {
        bitrouter_a2a::task::TaskState::Submitted => "submitted",
        bitrouter_a2a::task::TaskState::Working => "working",
        bitrouter_a2a::task::TaskState::Completed => "completed",
        bitrouter_a2a::task::TaskState::Failed => "failed",
        bitrouter_a2a::task::TaskState::Canceled => "canceled",
        bitrouter_a2a::task::TaskState::Rejected => "rejected",
        bitrouter_a2a::task::TaskState::InputRequired => "input-required",
        bitrouter_a2a::task::TaskState::AuthRequired => "auth-required",
        bitrouter_a2a::task::TaskState::Unknown => "unknown",
    }
}
