//! Both halves of the gateway, talking to each other over a real stdio
//! transport: `bitrouter-sdk`'s `RmcpExecutor` (the client BitRouter uses to
//! reach upstream MCP servers) driving this crate's origin server.
//!
//! The server-side lifecycle tests in `stdio_smoke.rs` hand-write JSON-RPC, so
//! they prove the server answers correctly but say nothing about whether our
//! *client* speaks the same dialect. These close that loop — in particular for
//! MCP `2026-07-28`, where opting in changes the client's whole lifecycle
//! (`server/discover` instead of the removed `initialize` handshake) rather
//! than just a version string.
//!
//! Only `tools/list` is exercised: it needs no backend, so no daemon has to be
//! running.

use std::collections::HashMap;

use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::mcp::rmcp_executor::RmcpExecutor;
use bitrouter_sdk::mcp::transport::McpTransport;
use bitrouter_sdk::mcp::{Executor, McpRequest, McpTarget};

fn origin_server_target() -> McpTarget {
    McpTarget::Direct {
        server_name: "origin".into(),
        transport: McpTransport::Stdio {
            command: env!("CARGO_BIN_EXE_mcp-stdio-local").to_string(),
            args: vec![],
            env: HashMap::new(),
        },
    }
}

fn paginated_server_target(scenario: &str) -> McpTarget {
    McpTarget::Direct {
        server_name: format!("pagination-{scenario}"),
        transport: McpTransport::Stdio {
            command: env!("CARGO_BIN_EXE_mcp-stdio-local").to_string(),
            args: vec![scenario.to_string()],
            env: HashMap::new(),
        },
    }
}

fn tools_list() -> McpRequest {
    McpRequest::direct(
        "origin",
        "tools/list",
        serde_json::json!({}),
        CallerContext::new("k", "u"),
    )
}

fn tool_names(result: &serde_json::Value) -> Vec<String> {
    result["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect()
}

async fn paginated_tools(scenario: &str) -> anyhow::Result<serde_json::Value> {
    let executor =
        RmcpExecutor::new().with_protocol_version(rmcp::model::ProtocolVersion::V_2026_07_28);
    Ok(executor
        .execute(&paginated_server_target(scenario), &tools_list())
        .await?
        .result)
}

fn assert_complete_two_page_result(result: &serde_json::Value) {
    assert_eq!(tool_names(result), ["first", "second"]);
    assert!(result.get("nextCursor").is_none(), "got: {result}");
}

/// The default path, unchanged by the `2026-07-28` work: the client dials with
/// the `initialize` handshake and the server answers it.
#[tokio::test]
async fn gateway_client_reaches_origin_server_on_latest() {
    let executor = RmcpExecutor::new();
    let result = executor
        .execute(&origin_server_target(), &tools_list())
        .await
        .expect("tools/list over the default lifecycle");
    assert_eq!(tool_names(&result.result), ["list_models", "status"]);
}

/// The opt-in path. This is the test that would have caught the original
/// mistake: setting only `ClientInfo.protocol_version` still sent an
/// `initialize`, a method `2026-07-28` does not define. Here the client must
/// open with `server/discover` and then send self-contained requests, and the
/// server must serve that lifecycle — neither half can regress without this
/// failing.
#[tokio::test]
async fn gateway_client_reaches_origin_server_on_2026_07_28() {
    let executor =
        RmcpExecutor::new().with_protocol_version(rmcp::model::ProtocolVersion::V_2026_07_28);
    let result = executor
        .execute(&origin_server_target(), &tools_list())
        .await
        .expect("tools/list over the 2026-07-28 lifecycle");
    assert_eq!(tool_names(&result.result), ["list_models", "status"]);
    // The SEP-2549 hints the server attaches for draft peers survive the client
    // round-trip, which is what lets `CachingExecutor` honour them.
    assert_eq!(result.result["ttlMs"], 5 * 60 * 1000);
    assert_eq!(result.result["cacheScope"], "public");
}

#[tokio::test]
async fn later_private_cache_scope_wins_across_pages() -> anyhow::Result<()> {
    let result = paginated_tools("private").await?;
    assert_complete_two_page_result(&result);
    assert_eq!(result["cacheScope"], "private", "got: {result}");
    Ok(())
}

#[tokio::test]
async fn later_zero_ttl_wins_across_pages() -> anyhow::Result<()> {
    let result = paginated_tools("zero-ttl").await?;
    assert_complete_two_page_result(&result);
    assert_eq!(result["ttlMs"], 0, "got: {result}");
    Ok(())
}

#[tokio::test]
async fn positive_page_ttls_merge_to_minimum() -> anyhow::Result<()> {
    let result = paginated_tools("shorter-ttl").await?;
    assert_complete_two_page_result(&result);
    assert_eq!(result["ttlMs"], 1_000, "got: {result}");
    Ok(())
}

// ── SEP-2640 skills over the same real transport ──────────────────────────
//
// This is the end-to-end check that the two halves agree: the gateway relays
// `skills/*` as an rmcp `CustomRequest` (there is no typed peer method for an
// extension), and the origin server answers it from `on_custom_request`. Unit
// tests on either side cannot catch a mismatch between them.

fn skills_target() -> McpTarget {
    McpTarget::Direct {
        server_name: "skills".into(),
        transport: McpTransport::Stdio {
            command: env!("CARGO_BIN_EXE_mcp-stdio-local").to_string(),
            args: vec!["skills".to_string()],
            env: HashMap::new(),
        },
    }
}

fn paginated_skills_target() -> McpTarget {
    McpTarget::Direct {
        server_name: "skills-pages".into(),
        transport: McpTransport::Stdio {
            command: env!("CARGO_BIN_EXE_mcp-stdio-local").to_string(),
            args: vec!["skills-pages".to_string()],
            env: HashMap::new(),
        },
    }
}

async fn skills_call(method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let executor = RmcpExecutor::new();
    let request = McpRequest::direct("skills", method, params, CallerContext::new("k", "u"));
    Ok(executor.execute(&skills_target(), &request).await?.result)
}

#[tokio::test]
async fn skills_list_survives_the_relay() -> anyhow::Result<()> {
    let result = skills_call("skills/list", serde_json::json!({})).await?;
    let skills = result["skills"].as_array().expect("skills array");
    assert_eq!(skills.len(), 1, "got: {result}");
    assert_eq!(skills[0]["uri"], "skill://git-workflow/SKILL.md");
    // Frontmatter arrives verbatim, and the SEP's name/URI invariant holds.
    assert_eq!(skills[0]["frontmatter"]["name"], "git-workflow");
    let resources = skills[0]["resources"].as_array().expect("resources");
    assert_eq!(resources.len(), 2, "complete enumeration: {result}");
    assert_eq!(
        resources[0]["digest"],
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    Ok(())
}

#[tokio::test]
async fn skills_list_exhausts_real_upstream_cursor_pages() -> anyhow::Result<()> {
    let executor = RmcpExecutor::new();
    let request = McpRequest::direct(
        "skills-pages",
        "skills/list",
        serde_json::json!({}),
        CallerContext::new("k", "u"),
    );

    let result = executor
        .execute(&paginated_skills_target(), &request)
        .await?
        .result;

    let skills = result["skills"].as_array().expect("skills");
    assert_eq!(skills.len(), 2, "got: {result}");
    assert_eq!(skills[0]["frontmatter"]["name"], "first");
    assert_eq!(skills[1]["frontmatter"]["name"], "second");
    assert!(result.get("nextCursor").is_none(), "got: {result}");
    assert_eq!(result["ttlMs"], 1_000);
    assert_eq!(result["cacheScope"], "private");
    Ok(())
}

#[tokio::test]
async fn skills_get_survives_the_relay() -> anyhow::Result<()> {
    let result = skills_call(
        "skills/get",
        serde_json::json!({ "uri": "skill://git-workflow/SKILL.md" }),
    )
    .await?;
    assert_eq!(result["skill"]["frontmatter"]["name"], "git-workflow");
    Ok(())
}

#[tokio::test]
async fn skills_get_on_an_unknown_uri_is_an_error() -> anyhow::Result<()> {
    let err = skills_call(
        "skills/get",
        serde_json::json!({ "uri": "skill://ghost/SKILL.md" }),
    )
    .await
    .expect_err("unknown skill must not resolve");
    assert!(err.to_string().contains("ghost"), "got: {err}");
    Ok(())
}

/// Skills are only useful if their files can actually be read, which is the
/// half that rides on the base Resources primitive rather than the relay.
#[tokio::test]
async fn skill_files_are_readable_through_resources_read() -> anyhow::Result<()> {
    let result = skills_call(
        "resources/read",
        serde_json::json!({ "uri": "skill://git-workflow/references/GUIDE.md" }),
    )
    .await?;
    let contents = result["contents"].as_array().expect("contents");
    assert_eq!(contents[0]["text"], "# Guide", "got: {result}");
    assert_eq!(contents[0]["mimeType"], "text/markdown");
    Ok(())
}

/// A read of a file no listing enumerates must fail: SEP-2640 makes that a
/// verification failure host-side, so the server must not serve it either.
#[tokio::test]
async fn unlisted_skill_files_are_refused() -> anyhow::Result<()> {
    let err = skills_call(
        "resources/read",
        serde_json::json!({ "uri": "skill://git-workflow/../../etc/passwd" }),
    )
    .await
    .expect_err("traversal must not resolve");
    assert!(err.to_string().contains("not a file"), "got: {err}");
    Ok(())
}

/// The relay is an allowlist: a method outside it must still be rejected
/// rather than tunnelled through to the upstream.
#[tokio::test]
async fn off_allowlist_methods_are_not_relayed() -> anyhow::Result<()> {
    let err = skills_call("logging/setLevel", serde_json::json!({ "level": "debug" }))
        .await
        .expect_err("must not be relayed");
    assert!(err.to_string().contains("not supported"), "got: {err}");
    Ok(())
}
