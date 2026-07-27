//! Smoke test: spawn the stdio MCP server, perform the `initialize` handshake,
//! then `tools/list`, and assert the three BitRouter tools are advertised.
//! Does not require a running daemon — it only lists tools, never calls them.
//!
//! Also pins the protocol version the handshake actually settles on, and the
//! SEP-2549 cache hints that ride on `tools/list` only for draft-version peers.

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Drive `initialize` → `notifications/initialized` → `tools/list` against a
/// freshly spawned stdio server, asking for `protocol_version`.
///
/// Returns the parsed `initialize` and `tools/list` results.
async fn handshake_and_list(protocol_version: &str) -> (serde_json::Value, serde_json::Value) {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-stdio-local"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut out = BufReader::new(child.stdout.take().expect("stdout")).lines();

    let init = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": protocol_version,
            "capabilities": {},
            "clientInfo": { "name": "t", "version": "0" },
        },
    });
    stdin
        .write_all(format!("{init}\n").as_bytes())
        .await
        .expect("write init");
    let init_line = out
        .next_line()
        .await
        .expect("read init")
        .expect("init line");

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
    let list_line = out
        .next_line()
        .await
        .expect("read list")
        .expect("list line");
    let _ = child.kill().await;

    let parse = |line: String| -> serde_json::Value {
        let v: serde_json::Value = serde_json::from_str(&line).expect("json-rpc response");
        v["result"].clone()
    };
    (parse(init_line), parse(list_line))
}

#[tokio::test]
async fn stdio_lists_three_tools() {
    let (_, list) = handshake_and_list("2025-11-25").await;
    let names: Vec<&str> = list["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in ["complete", "list_models", "status"] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }
}

/// A `2025-11-25` peer must not see the SEP-2549 hints: rmcp strips only
/// `resultType` for legacy peers, so the version gate in `list_tools` is the
/// only thing keeping draft-only fields off this response.
#[tokio::test]
async fn stable_peer_gets_no_cache_hints_on_tools_list() {
    let (init, list) = handshake_and_list("2025-11-25").await;
    assert_eq!(init["protocolVersion"], "2025-11-25");
    assert!(list.get("ttlMs").is_none(), "got: {list}");
    assert!(list.get("cacheScope").is_none(), "got: {list}");
}

/// The draft peer gets the hints — and note the server agrees to `2026-07-28`
/// without any opt-in on our side, because rmcp negotiates any version in
/// `ProtocolVersion::KNOWN_VERSIONS`. That is exactly why the bump matters:
/// before it, we accepted this version while implementing none of it.
#[tokio::test]
async fn draft_peer_gets_public_cache_hints_on_tools_list() {
    let (init, list) = handshake_and_list("2026-07-28").await;
    assert_eq!(init["protocolVersion"], "2026-07-28");
    assert_eq!(list["ttlMs"], 5 * 60 * 1000);
    assert_eq!(list["cacheScope"], "public");
}
