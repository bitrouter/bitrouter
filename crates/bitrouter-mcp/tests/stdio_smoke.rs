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
        Self::spawn_scenario(None)
    }

    fn spawn_scenario(scenario: Option<&str>) -> Self {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-stdio-local"));
        if let Some(scenario) = scenario {
            command.arg(scenario);
        }
        let mut child = command
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

async fn skills_oneshot(request: serde_json::Value) -> serde_json::Value {
    Server::spawn_scenario(Some("skills"))
        .request(request)
        .await
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
async fn stdio_lists_the_introspection_tools() {
    let (_, list) = legacy_handshake_and_list("2025-11-25").await;
    let names: Vec<&str> = list["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    // Sorted: SEP-2575 asks servers to return a deterministic order so clients
    // can cache the list, which is also what makes our `ttlMs` hint honest.
    assert_eq!(names, ["list_models", "status"]);
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

/// The modern version has no legacy `initialize` lifecycle. rmcp 3.3 therefore
/// negotiates that incoherent request down to the stable fallback; a modern
/// client must use `server/discover` or self-contained request metadata.
#[tokio::test]
async fn draft_version_via_legacy_handshake_uses_stable_fallback() {
    let (init, list) = legacy_handshake_and_list("2026-07-28").await;
    assert_eq!(init["protocolVersion"], "2025-11-25");
    assert!(list.get("resultType").is_none(), "got: {list}");
    assert!(list.get("ttlMs").is_none(), "got: {list}");
    assert!(list.get("cacheScope").is_none(), "got: {list}");
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

/// Draft-namespaced metadata is legal extension data on a legacy initialize;
/// one key alone must not make the request an inline-lifecycle opener.
#[tokio::test]
async fn legacy_initialize_with_partial_draft_meta_is_accepted() {
    let mut server = Server::spawn();
    let init = server
        .request(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "t", "version": "0" },
                "_meta": { "io.modelcontextprotocol/clientCapabilities": {} },
            },
        }))
        .await;
    assert!(init.get("error").is_none(), "initialize failed: {init}");
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");

    server
        .notify(serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .await;
    let list = server
        .request(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {},
        }))
        .await;
    assert!(list.get("error").is_none(), "tools/list failed: {list}");
    let result = &list["result"];
    assert!(result.get("resultType").is_none(), "got: {result}");
    assert!(result.get("ttlMs").is_none(), "got: {result}");
    assert!(result.get("cacheScope").is_none(), "got: {result}");
}

/// A pre-initialize ping is lifecycle-neutral even when extension metadata is
/// only partially populated. After it, the connection must still initialize.
#[tokio::test]
async fn pre_init_ping_with_partial_draft_meta_then_initializes() {
    let mut server = Server::spawn();
    let ping = server
        .request(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "ping",
            "params": { "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            } },
        }))
        .await;
    assert!(ping.get("error").is_none(), "ping failed: {ping}");
    assert_eq!(ping["result"], serde_json::json!({}));

    let init = server
        .request(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "t", "version": "0" },
            },
        }))
        .await;
    assert!(init.get("error").is_none(), "initialize failed: {init}");
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");

    server
        .notify(serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .await;
    let list = server
        .request(serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {},
        }))
        .await;
    assert!(list.get("error").is_none(), "tools/list failed: {list}");
    let result = &list["result"];
    assert!(result.get("resultType").is_none(), "got: {result}");
    assert!(result.get("ttlMs").is_none(), "got: {result}");
    assert!(result.get("cacheScope").is_none(), "got: {result}");
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
        2,
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

#[tokio::test]
async fn stateless_skills_methods_emit_the_stable_extension_envelope() {
    let list = skills_oneshot(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "skills/list",
        "params": { "_meta": draft_meta() },
    }))
    .await;
    let result = &list["result"];
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["ttlMs"], 60_000);
    assert_eq!(result["cacheScope"], "public");

    let get = skills_oneshot(serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "skills/get",
        "params": {
            "uri": "skill://git-workflow/SKILL.md",
            "_meta": draft_meta(),
        },
    }))
    .await;
    let result = &get["result"];
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["ttlMs"], 0);
    assert_eq!(result["cacheScope"], "public");

    // These are extension fields, not base-protocol additions. A partial
    // Skills client that used a legacy handshake still receives the complete
    // stable extension shape and may ignore additive fields it does not know.
    let mut legacy = Server::spawn_scenario(Some("skills"));
    legacy
        .request(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "t", "version": "0" },
            },
        }))
        .await;
    legacy
        .notify(serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }))
        .await;
    let list = legacy
        .request(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "skills/list", "params": {}
        }))
        .await;
    assert_eq!(list["result"]["resultType"], "complete");
    assert_eq!(list["result"]["ttlMs"], 60_000);
    assert_eq!(list["result"]["cacheScope"], "public");
}

#[tokio::test]
async fn skill_resources_use_frontmatter_metadata_and_versioned_cache_hints() {
    let modern = skills_oneshot(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "resources/list",
        "params": { "_meta": draft_meta() },
    }))
    .await;
    let result = &modern["result"];
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["ttlMs"], 60_000);
    assert_eq!(result["cacheScope"], "public");
    let entrypoint = result["resources"]
        .as_array()
        .and_then(|resources| {
            resources
                .iter()
                .find(|resource| resource["uri"] == "skill://git-workflow/SKILL.md")
        })
        .expect("SKILL.md resource");
    assert_eq!(entrypoint["name"], "git-workflow");
    assert_eq!(
        entrypoint["description"],
        "Follow the team's Git conventions"
    );
    assert_eq!(entrypoint["mimeType"], "text/markdown");

    let mut legacy = Server::spawn_scenario(Some("skills"));
    let init = legacy
        .request(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "t", "version": "0" },
            },
        }))
        .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    legacy
        .notify(serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }))
        .await;
    let list = legacy
        .request(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "resources/list", "params": {}
        }))
        .await;
    let result = &list["result"];
    assert!(result.get("resultType").is_none(), "got: {result}");
    assert!(result.get("ttlMs").is_none(), "got: {result}");
    assert!(result.get("cacheScope").is_none(), "got: {result}");
}

#[tokio::test]
async fn stateless_skill_resource_read_is_complete_and_immediately_stale() {
    let response = skills_oneshot(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "resources/read",
        "params": {
            "uri": "skill://git-workflow/SKILL.md",
            "_meta": draft_meta(),
        },
    }))
    .await;
    let result = &response["result"];
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["ttlMs"], 0);
    assert_eq!(result["cacheScope"], "public");
    assert_eq!(result["contents"][0]["text"], "# Git workflow");
}
