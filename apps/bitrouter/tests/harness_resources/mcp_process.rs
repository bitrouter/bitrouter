//! MCP-only process coverage uses the existing Cargo-built Rust stdio fixture.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use bitrouter::config::mcp::{McpServerConfig, McpTransport};
use bitrouter::gateway::mcp::client::BoundedMcpClient;
use rmcp::model::{CallToolRequestParams, ProtocolVersion};
use tokio_util::sync::CancellationToken;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[path = "../support/prebuilt_example.rs"]
mod prebuilt_example;

fn fixture_binary() -> Result<&'static Path, String> {
    static BINARY: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    BINARY
        .get_or_init(build_fixture)
        .as_deref()
        .map_err(Clone::clone)
}

fn build_fixture() -> Result<PathBuf, String> {
    if let Some(path) = prebuilt_example::from_env("harness_mcp_fixture")? {
        return Ok(path);
    }
    eprintln!(
        "building Cargo MCP fixture; CI can set BITROUTER_TEST_EXAMPLES_DIR to prebuilt examples"
    );
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let profile = executable
        .parent()
        .and_then(|deps| deps.parent())
        .ok_or("test executable has no Cargo profile directory")?;
    let target = profile.parent().ok_or("Cargo target directory missing")?;
    let profile_name = profile
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("Cargo profile name missing")?;
    let mut command = std::process::Command::new(env!("CARGO"));
    command
        .args([
            "build",
            "--offline",
            "--no-default-features",
            "--features",
            "mcp",
            "--jobs=2",
            "--message-format=json-render-diagnostics",
            "--package",
            "bitrouter",
            "--example",
            "harness_mcp_fixture",
            "--manifest-path",
        ])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .arg("--target-dir")
        .arg(target);
    if profile_name != "debug" {
        command.args(["--profile", profile_name]);
    }
    let output = command.output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "building MCP fixture: {}\n{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        ));
    }
    // Cargo artifact messages: https://doc.rust-lang.org/cargo/reference/external-tools.html#json-messages
    for line in output.stdout.split(|byte| *byte == b'\n') {
        let Ok(artifact) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if artifact["reason"] == "compiler-artifact"
            && artifact["target"]["name"] == "harness_mcp_fixture"
            && let Some(path) = artifact["executable"].as_str()
        {
            return Ok(PathBuf::from(path));
        }
    }
    Err("Cargo did not return the MCP fixture executable".into())
}

fn server(args: &[&str]) -> Result<McpServerConfig, String> {
    Ok(McpServerConfig::with_defaults(
        "stdio",
        McpTransport::Stdio {
            command: fixture_binary()?.to_string_lossy().into_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            env: Default::default(),
        },
    ))
}

fn assert_processes_stopped(workspace: &Path) -> TestResult {
    for file in ["mcp.pid", "mcp-child.pid"] {
        let raw = std::fs::read_to_string(workspace.join(file))?.parse::<i32>()?;
        let pid = rustix::process::Pid::from_raw(raw).ok_or("invalid fixture PID")?;
        assert_eq!(
            rustix::process::test_kill_process(pid),
            Err(rustix::io::Errno::SRCH),
            "{file} is still alive"
        );
    }
    Ok(())
}

#[tokio::test]
async fn shared_stdio_shutdown_confirms_child_and_descendant_exit() -> TestResult {
    let workspace = tempfile::TempDir::new()?;
    let mut client = BoundedMcpClient::connect(
        workspace.path(),
        &[server(&["descendant", "notify"])?],
        ProtocolVersion::LATEST,
    )
    .await?;
    assert_eq!(client.tools()[0].name, "stdio__echo");
    let (name, peer) = client.tool_peer("stdio__echo")?;
    let result = peer
        .call_tool(CallToolRequestParams::new(name.to_owned()))
        .await?;
    assert!(
        serde_json::to_string(&result)?
            .contains(&workspace.path().canonicalize()?.display().to_string())
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while client.validate_catalog().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert!(client.tool_peer("stdio__echo").is_err());
    client.shutdown().await?;
    assert_processes_stopped(workspace.path())?;
    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn cancelled_shared_handshake_confirms_child_and_descendant_exit() -> TestResult {
    let workspace = tempfile::TempDir::new()?;
    let servers = [server(&["hang", "descendant"])?];
    let cancel = CancellationToken::new();
    let connect = BoundedMcpClient::connect_cancellable(
        workspace.path(),
        &servers,
        ProtocolVersion::LATEST,
        &cancel,
        Duration::from_secs(10),
    );
    let cancellation = async {
        let ready = tokio::time::timeout(Duration::from_secs(5), async {
            while !workspace.path().join("mcp-child.pid").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        cancel.cancel();
        ready
    };
    let (result, ready) = tokio::join!(connect, cancellation);
    ready?;
    let error = match result {
        Err(error) => error,
        Ok(mut client) => {
            client.shutdown().await?;
            return Err("cancelled handshake unexpectedly succeeded".into());
        }
    };
    assert!(error.message.contains("cancelled"), "{error}");
    assert!(!error.cleanup_unknown, "{error}");
    assert_processes_stopped(workspace.path())
}

#[tokio::test]
async fn timed_out_shared_handshake_confirms_child_and_descendant_exit() -> TestResult {
    let workspace = tempfile::TempDir::new()?;
    let result = BoundedMcpClient::connect_cancellable(
        workspace.path(),
        &[server(&["hang", "descendant"])?],
        ProtocolVersion::LATEST,
        &CancellationToken::new(),
        Duration::from_secs(2),
    )
    .await;
    let error = match result {
        Err(error) => error,
        Ok(mut client) => {
            client.shutdown().await?;
            return Err("timed out handshake unexpectedly succeeded".into());
        }
    };
    assert_eq!(
        error.message,
        "MCP discovery exceeded the remaining Turn time"
    );
    assert!(!error.cleanup_unknown, "{error}");
    assert_processes_stopped(workspace.path())
}
