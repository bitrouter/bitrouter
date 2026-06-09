//! Smoke test: spawn the stdio MCP server, perform the `initialize` handshake,
//! then `tools/list`, and assert the three BitRouter tools are advertised.
//! Does not require a running daemon — it only lists tools, never calls them.

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn stdio_lists_three_tools() {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-stdio-local"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut out = BufReader::new(child.stdout.take().expect("stdout")).lines();

    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;
    stdin
        .write_all(format!("{init}\n").as_bytes())
        .await
        .expect("write init");
    let _ = out.next_line().await.expect("read init"); // init result

    // The server expects the client to confirm initialization before it will
    // service further requests.
    let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    stdin
        .write_all(format!("{initialized}\n").as_bytes())
        .await
        .expect("write initialized");

    let listed = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
    stdin
        .write_all(format!("{listed}\n").as_bytes())
        .await
        .expect("write list");
    let line = out.next_line().await.expect("read list").expect("line");
    assert!(
        line.contains("complete") && line.contains("list_models") && line.contains("status"),
        "got: {line}"
    );
    let _ = child.kill().await;
}
