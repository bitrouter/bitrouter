use bitrouter_sdk::mcp::transport::{McpServerConfig, McpTransport};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

pub async fn server(repeated_cursor: bool) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/mcp"))
        .respond_with(move |request: &Request| {
            let Ok(body) = serde_json::from_slice::<Value>(&request.body) else { return ResponseTemplate::new(400); };
            let Some(id) = body.get("id") else { return ResponseTemplate::new(202); };
            let mut result = match body["method"].as_str() {
                Some("server/discover") => json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"instructions":"Use echo with text.","ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"fixture","version":"1"}}}),
                Some("initialize") => json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"},"instructions":"Use echo with text."}),
                Some("tools/list") if body["params"]["cursor"].is_null() => json!({"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}],"nextCursor":"second"}),
                Some("tools/list") if repeated_cursor => json!({"tools":[],"nextCursor":"second"}),
                Some("tools/list") => json!({"tools":[{"name":"other","inputSchema":{"type":"object"}}]}),
                Some("tools/call") => json!({"content":[{"type":"text","text":body["params"]["arguments"]["text"]}],"isError":false}),
                _ => return ResponseTemplate::new(400),
            };
            if body["method"] != "initialize" {
                result["resultType"] = json!("complete");
                result["ttlMs"] = json!(0);
                result["cacheScope"] = json!("private");
            }
            ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":id,"result":result}))
        }).mount(&server).await;
    server
}

pub fn configuration(server: &MockServer) -> McpServerConfig {
    McpServerConfig::with_defaults(
        "fixture",
        McpTransport::Http {
            url: format!("{}/mcp", server.uri()),
            headers: Default::default(),
        },
    )
}
