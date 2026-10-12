//! Stdio MCP fixture for workspace binding, cancellation and process cleanup tests.

use std::error::Error;
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "descendant-child") {
        std::thread::sleep(Duration::from_secs(60));
        return Ok(());
    }
    std::fs::write("mcp.pid", std::process::id().to_string())?;
    let _descendant = if args.iter().any(|arg| arg == "descendant") {
        let child = Command::new(std::env::current_exe()?)
            .arg("descendant-child")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        std::fs::write("mcp-child.pid", child.id().to_string())?;
        Some(child)
    } else {
        None
    };
    if args.iter().any(|arg| arg == "hang") {
        std::thread::sleep(Duration::from_secs(60));
    }
    let mut output = std::io::stdout().lock();
    // MCP lifecycle and tool notifications: https://modelcontextprotocol.io/specification/2025-11-25
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        let Some(id) = request.get("id") else {
            continue;
        };
        let method = request["method"].as_str();
        let result = match method {
            Some("initialize") => json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "stdio-fixture", "version": "1"}
            }),
            Some("tools/list") => {
                json!({"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]})
            }
            Some("tools/call") => {
                json!({"content": [{"type": "text", "text": std::env::current_dir()?}]})
            }
            _ => continue,
        };
        writeln!(
            output,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "result": result})
        )?;
        if method == Some("tools/call") && args.iter().any(|arg| arg == "notify") {
            writeln!(
                output,
                "{}",
                json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
            )?;
        }
        output.flush()?;
    }
    Ok(())
}
