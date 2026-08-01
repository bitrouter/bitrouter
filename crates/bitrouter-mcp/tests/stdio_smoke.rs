//! Lifecycle conformance for the stdio origin server, across both MCP
//! lifecycles it has to serve.
//!
//! MCP `2026-07-28` removed the `initialize` / `notifications/initialized`
//! handshake (SEP-2575): a client on that version opens a stdio connection with
//! `server/discover`, or simply sends a request carrying its protocol version,
//! identity, and capabilities in `_meta`. Older clients still handshake. These
//! tests pin both, because serving only one is a silent interop failure — the
//! server either rejects every modern client or every legacy one.
//!
//! Needs no running daemon: it lists tools and discovers, never calls a tool.

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// `_meta` a conformant `2026-07-28` client puts on every request (SEP-2575).
/// Protocol version and client capabilities are mandatory on an inline-lifecycle
/// request; client info is optional.
fn draft_meta() -> serde_json::Value {
    serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": { "name": "t", "version": "0" },
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

/// A spawned stdio server you can drive request-by-request.
///
/// Requests are written and awaited one at a time rather than pipelined.
/// That models how clients actually behave, and it keeps responses
/// unambiguous: since the server no longer queues everything behind a
/// handshake, concurrently-issued requests may complete out of order.
struct Server {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    out: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl Server {
    fn spawn() -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-stdio-local"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let stdin = child.stdin.take().expect("stdin");
        let out = BufReader::new(child.stdout.take().expect("stdout")).lines();
        Self { child, stdin, out }
    }

    async fn notify(&mut self, notification: serde_json::Value) {
        self.stdin
            .write_all(format!("{notification}\n").as_bytes())
            .await
            .expect("write notification");
    }

    /// Send one request and read its response. Returns the whole JSON-RPC
    /// envelope so error-path tests can read `error` as well as `result`.
    async fn request(&mut self, request: serde_json::Value) -> serde_json::Value {
        self.notify(request).await;
        let line = self
            .out
            .next_line()
            .await
            .expect("read response")
            .expect("response line");
        serde_json::from_str(&line).expect("json-rpc response")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// One-shot: spawn, send a single request, return its envelope.
async fn oneshot(request: serde_json::Value) -> serde_json::Value {
    Server::spawn().request(request).await
}

/// `initialize` → `notifications/initialized` → `tools/list`, the pre-2026-07-28
/// lifecycle, awaiting the handshake response before proceeding as a real
/// client does.
async fn legacy_handshake_and_list(version: &str) -> (serde_json::Value, serde_json::Value) {
    let mut server = Server::spawn();
    let init = server
        .request(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": version,
                "capabilities": {},
                "clientInfo": { "name": "t", "version": "0" },
            },
        }))
        .await;
    server
        .notify(serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .await;
    let list = server
        .request(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {},
        }))
        .await;
    (init["result"].clone(), list["result"].clone())
}

#[tokio::test]
async fn stdio_lists_three_tools() {
    let (_, list) = legacy_handshake_and_list("2025-11-25").await;
    let names: Vec<&str> = list["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    // Sorted: SEP-2575 asks servers to return a deterministic order so clients
    // can cache the list, which is also what makes our `ttlMs` hint honest.
    assert_eq!(names, ["complete", "list_models", "status"]);
}

/// A `2025-11-25` peer must not see the SEP-2549 hints: rmcp strips only
/// `resultType` for legacy peers, so the version gate in `list_tools` is the
/// only thing keeping draft-only fields off this response.
#[tokio::test]
async fn stable_peer_gets_no_cache_hints_on_tools_list() {
    let (init, list) = legacy_handshake_and_list("2025-11-25").await;
    assert_eq!(init["protocolVersion"], "2025-11-25");
    assert!(list.get("ttlMs").is_none(), "got: {list}");
    assert!(list.get("cacheScope").is_none(), "got: {list}");
}

/// The legacy handshake still reaches `2026-07-28` if a client asks for it that
/// way — rmcp negotiates any version in `KNOWN_VERSIONS`.
#[tokio::test]
async fn draft_peer_via_legacy_handshake_gets_cache_hints() {
    let (init, list) = legacy_handshake_and_list("2026-07-28").await;
    assert_eq!(init["protocolVersion"], "2026-07-28");
    assert_eq!(list["ttlMs"], 5 * 60 * 1000);
    assert_eq!(list["cacheScope"], "public");
}

/// An unsupported legacy version is negotiated down to the server fallback,
/// and that negotiated version — not the client's raw request — governs every
/// later metadata-free request on the connection.
#[tokio::test]
async fn legacy_future_version_uses_fallback_for_following_requests() {
    let (init, list) = legacy_handshake_and_list("2099-01-01").await;
    assert_eq!(init["protocolVersion"], "2025-11-25");
    assert!(list.get("resultType").is_none(), "got: {list}");
    assert!(list.get("ttlMs").is_none(), "got: {list}");
    assert!(list.get("cacheScope").is_none(), "got: {list}");
}

/// SEP-2575: servers MUST implement `server/discover`. This is how a
/// `2026-07-28` client opens a stdio connection, so a `-32601` here means no
/// conformant client can talk to us at all.
#[tokio::test]
async fn server_discover_advertises_versions_and_bitrouter_identity() {
    let response = oneshot(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "server/discover",
        "params": { "_meta": draft_meta() },
    }))
    .await;
    assert!(
        response.get("error").is_none(),
        "discover failed: {response}"
    );
    let result = &response["result"];

    let versions = result["supportedVersions"]
        .as_array()
        .expect("supportedVersions");
    assert!(
        versions.iter().any(|v| v == "2026-07-28"),
        "got: {versions:?}"
    );

    // Identity must be ours, not the SDK's. `InitializeResult::new` defaults to
    // `Implementation::from_build_env()`, which resolves inside rmcp and would
    // otherwise report this server as "rmcp".
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "bitrouter"
    );
    assert!(result.get("serverInfo").is_none(), "got: {result}");
    assert_eq!(result["capabilities"]["tools"], serde_json::json!({}));
}

/// Once `server/discover` selects the inline lifecycle, every later request on
/// that connection must remain self-contained. Missing metadata is invalid;
/// it must not silently switch the connection back to legacy semantics.
#[tokio::test]
async fn discover_requires_metadata_on_following_requests() {
    let mut server = Server::spawn();
    let discover = server
        .request(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "server/discover",
            "params": { "_meta": draft_meta() },
        }))
        .await;
    assert!(discover.get("error").is_none(), "got: {discover}");

    let response = server
        .request(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {},
        }))
        .await;
    assert_eq!(response["error"]["code"], -32602, "got: {response}");
}

/// The real `2026-07-28` shape: no handshake at all, every request
/// self-contained. This is the path a conformant client actually takes, so it
/// is the one that has to carry the cache hints.
#[tokio::test]
async fn stateless_draft_tools_list_needs_no_handshake_and_carries_hints() {
    let response = oneshot(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list",
        "params": { "_meta": draft_meta() },
    }))
    .await;
    assert!(
        response.get("error").is_none(),
        "stateless tools/list failed: {response}"
    );
    let result = &response["result"];
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["ttlMs"], 5 * 60 * 1000);
    assert_eq!(result["cacheScope"], "public");
    assert_eq!(
        result["tools"].as_array().expect("tools array").len(),
        3,
        "got: {result}"
    );
}

/// An inline-lifecycle request must be self-contained; a half-populated `_meta`
/// is a client bug and gets `invalid_params` rather than being served on
/// guessed defaults.
#[tokio::test]
async fn stateless_request_missing_required_meta_is_rejected() {
    let response = oneshot(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list",
        "params": { "_meta": { "io.modelcontextprotocol/protocolVersion": "2026-07-28" } },
    }))
    .await;
    let error = &response["error"];
    assert_eq!(error["code"], -32602, "got: {response}");
    let message = error["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("io.modelcontextprotocol/clientCapabilities"),
        "got: {message}"
    );
}

/// `clientInfo` is optional for a self-contained `2026-07-28` request; the
/// protocol version and client capabilities establish the required context.
#[tokio::test]
async fn stateless_request_accepts_missing_optional_client_info() {
    let response = oneshot(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list",
        "params": { "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        } },
    }))
    .await;
    assert!(
        response.get("error").is_none(),
        "stateless tools/list failed: {response}"
    );
    let result = &response["result"];
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["ttlMs"], 5 * 60 * 1000);
    assert_eq!(result["cacheScope"], "public");
}
