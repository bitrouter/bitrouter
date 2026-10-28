//! Real gateway executor and HTTP ingress against an independent stdio peer.
#![cfg(feature = "mcp")]

use std::path::PathBuf;
use std::sync::Arc;

use bitrouter::config::mcp::McpTransport;
use bitrouter::gateway::caller::CallerContext;
use bitrouter::gateway::mcp::{
    AggregateMember, Executor, McpRequest, McpTarget, aggregating_executor::AggregatingExecutor,
    rmcp_executor::RmcpExecutor,
};
use rmcp::model::ProtocolVersion;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[path = "support/mcp_fixture.rs"]
mod mcp_fixture;

#[path = "support/example_binary.rs"]
mod example_binary;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn digest(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

struct Fixture {
    _root: tempfile::TempDir,
    config: PathBuf,
    journal: PathBuf,
    transport: McpTransport,
}

fn entry(name: &str) -> (Value, String) {
    let body = format!(
        "---\nname: {name}\ndescription: bounded fixture skill\nmetadata:\n  custom: preserved\nallowed-tools: shell\n---\n\nExact instructions for {name}.\n"
    );
    (
        json!({"uri":format!("skill://{name}/SKILL.md"),
        "frontmatter":{"name":name,"description":"bounded fixture skill","metadata":{"custom":"preserved"},"allowed-tools":"shell"},
        "resources":[{"uri":format!("skill://{name}/SKILL.md"),"digest":digest(body.as_bytes()),"size":body.len()}],
        "customTopLevel":{"preserved":true}}),
        body,
    )
}

fn page(skills: Vec<Value>, ttl: u64, scope: &str) -> Value {
    json!({"resultType":"complete","skills":skills,"ttlMs":ttl,"cacheScope":scope})
}

impl Fixture {
    fn new(capabilities: Value) -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let config = root.path().join("config.json");
        let journal = root.path().join("requests.jsonl");
        let transport = mcp_fixture::transport(&config, &journal)?;
        let fixture = Self {
            _root: root,
            config,
            journal,
            transport,
        };
        let (skill, body) = entry("review");
        fixture.write(json!({"capabilities":capabilities,
            "pages":{"first":page(vec![skill.clone()],2000,"public")},
            "get":{"skill://review/SKILL.md":{"resultType":"complete","skill":skill,"ttlMs":2000,"cacheScope":"public"}},
            "files":{"skill://review/SKILL.md":{"mimeType":"text/markdown","text":body}}}))?;
        Ok(fixture)
    }

    fn write(&self, config: Value) -> TestResult {
        std::fs::write(&self.config, serde_json::to_vec(&config)?)?;
        Ok(())
    }

    fn configuration(&self) -> TestResult<Value> {
        Ok(serde_json::from_slice(&std::fs::read(&self.config)?)?)
    }

    fn requests(&self) -> TestResult<Vec<Value>> {
        Ok(mcp_fixture::requests(&self.journal)?)
    }

    fn target(&self, label: &str) -> McpTarget {
        McpTarget::Direct {
            server_name: label.into(),
            transport: self.transport.clone(),
        }
    }
}

fn request(method: &str, params: Value) -> McpRequest {
    McpRequest::direct("origin", method, params, CallerContext::local())
}

fn capabilities(directory: bool) -> Value {
    json!({"resources":{},"extensions":{"io.modelcontextprotocol/skills":{"directoryRead":directory}}})
}

#[tokio::test]
async fn current_connection_declarations_gate_methods_without_upstream_requests() -> TestResult {
    for declaration in [
        json!({}),
        json!({"resources":{}}),
        json!({"extensions":{"io.modelcontextprotocol/skills":{}}}),
        json!({"resources":{},"extensions":{"io.modelcontextprotocol/skills":false}}),
    ] {
        let fixture = Fixture::new(declaration)?;
        let executor = RmcpExecutor::new().with_protocol_version(ProtocolVersion::V_2026_07_28);
        for method in ["skills/list", "skills/get", "resources/directory/read"] {
            assert!(
                executor
                    .execute(
                        &fixture.target("origin"),
                        &request(method, json!({"uri":"skill://review/SKILL.md"}))
                    )
                    .await
                    .is_err()
            );
        }
        let requests = fixture.requests()?;
        assert!(
            !requests.iter().any(|r| matches!(
                r["method"].as_str(),
                Some("skills/list" | "skills/get" | "resources/directory/read")
            )),
            "{requests:?}"
        );
    }
    let fixture = Fixture::new(capabilities(false))?;
    let executor = RmcpExecutor::new().with_protocol_version(ProtocolVersion::V_2026_07_28);
    executor
        .execute(
            &fixture.target("origin"),
            &request("skills/list", json!({})),
        )
        .await?;
    executor
        .execute(
            &fixture.target("origin"),
            &request("skills/get", json!({"uri":"skill://review/SKILL.md"})),
        )
        .await?;
    assert!(
        executor
            .execute(
                &fixture.target("origin"),
                &request("resources/directory/read", json!({"uri":"skill://review"}))
            )
            .await
            .is_err()
    );
    let events = fixture.requests()?;
    for event in events.iter().filter(|r| r["method"] != "server/discover") {
        assert_eq!(
            event["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
            "2026-07-28"
        );
        assert!(event["params"]["_meta"]["io.modelcontextprotocol/clientInfo"].is_object());
        assert!(event["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"].is_object());
    }
    assert_eq!(
        events
            .iter()
            .filter(|r| r["method"] == "skills/list")
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn reconnect_uses_new_declaration_and_resources_stay_independent() -> TestResult {
    let fixture = Fixture::new(capabilities(true))?;
    let executor = RmcpExecutor::new().with_protocol_version(ProtocolVersion::V_2026_07_28);
    let target = fixture.target("origin");
    executor
        .execute(
            &target,
            &request("resources/directory/read", json!({"uri":"skill://review"})),
        )
        .await?;
    let mut config = fixture.configuration()?;
    config["capabilities"] = json!({"resources":{}});
    fixture.write(config)?;
    assert!(executor.evict("origin").await);
    let prior = fixture.requests()?.len();
    assert!(
        executor
            .execute(&target, &request("skills/list", json!({})))
            .await
            .is_err()
    );
    let resource = executor
        .execute(
            &target,
            &request("resources/read", json!({"uri":"skill://review/SKILL.md"})),
        )
        .await?;
    assert!(
        resource.result["contents"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Exact instructions"))
    );
    let requests = fixture.requests()?;
    assert!(
        !requests[prior..]
            .iter()
            .any(|r| r["method"] == "skills/list")
    );
    assert!(
        requests[prior..]
            .iter()
            .any(|r| r["method"] == "server/discover")
    );
    Ok(())
}

#[tokio::test]
async fn real_pages_and_colliding_origins_preserve_entries_and_route_reads() -> TestResult {
    let first = Fixture::new(capabilities(false))?;
    let second = Fixture::new(capabilities(false))?;
    let mut other_config = second.configuration()?;
    let second_body = entry("review").1 + "Second origin bytes.\n";
    let mut second_entry = entry("review").0;
    second_entry["resources"][0]["digest"] = digest(second_body.as_bytes()).into();
    second_entry["resources"][0]["size"] = second_body.len().into();
    other_config["pages"]["first"]["skills"][0] = second_entry.clone();
    other_config["get"]["skill://review/SKILL.md"]["skill"] = second_entry;
    other_config["files"]["skill://review/SKILL.md"]["text"] = second_body.clone().into();
    second.write(other_config)?;
    for (fixture, support) in [
        (&first, "alpha support bytes"),
        (&second, "beta support bytes"),
    ] {
        let mut config = fixture.configuration()?;
        let mut skill = config["get"]["skill://review/SKILL.md"]["skill"].clone();
        let uri = "skill://review/references/check.md";
        skill["resources"]
            .as_array_mut()
            .ok_or("manifest array")?
            .push(json!({"uri":uri,"digest":digest(support.as_bytes()),"size":support.len()}));
        config["pages"]["first"]["skills"][0] = skill.clone();
        config["get"]["skill://review/SKILL.md"]["skill"] = skill;
        config["files"][uri] = json!({"mimeType":"text/plain","text":support});
        fixture.write(config)?;
    }
    let mut config = first.configuration()?;
    let (hidden, body) = entry("hidden");
    config["pages"]["first"]["nextCursor"] = "second".into();
    config["pages"]["second"] = page(vec![hidden.clone()], 1000, "private");
    config["get"]["skill://hidden/SKILL.md"] =
        json!({"resultType":"complete","skill":hidden,"ttlMs":1000,"cacheScope":"private"});
    config["files"]["skill://hidden/SKILL.md"] = json!({"mimeType":"text/markdown","text":body});
    first.write(config)?;
    let upstream =
        Arc::new(RmcpExecutor::new().with_protocol_version(ProtocolVersion::V_2026_07_28));
    let cached = bitrouter::gateway::mcp::caching_executor::CachingExecutor::new(
        upstream.clone(),
        bitrouter::gateway::mcp::caching_executor::CacheTtls::default(),
    )
    .with_invalidation(upstream.invalidation_receiver());
    let executor = AggregatingExecutor::new(Arc::new(cached));
    let target = McpTarget::Aggregate {
        members: vec![
            AggregateMember {
                server_name: "alpha".into(),
                tool_prefix: "alpha__".into(),
                transport: first.transport.clone(),
            },
            AggregateMember {
                server_name: "beta".into(),
                tool_prefix: "beta__".into(),
                transport: second.transport.clone(),
            },
        ],
    };
    let listed = executor
        .execute(&target, &request("skills/list", json!({})))
        .await?
        .result;
    assert_eq!(listed["resultType"], "complete");
    assert_eq!(listed["cacheScope"], "private");
    assert!(listed["ttlMs"].as_u64().is_some_and(|ttl| ttl <= 1000));
    let skills = listed["skills"].as_array().ok_or("missing skills")?;
    assert_eq!(skills.len(), 3);
    for label in ["alpha", "beta"] {
        let uri = format!("skill://{label}/review/SKILL.md");
        let entry = skills
            .iter()
            .find(|s| s["uri"] == uri)
            .ok_or("missing origin")?;
        assert_eq!(entry["frontmatter"], self::entry("review").0["frontmatter"]);
        assert_eq!(entry["customTopLevel"], json!({"preserved":true}));
        let got = executor
            .execute(&target, &request("skills/get", json!({"uri":uri})))
            .await?
            .result;
        assert_eq!(got["skill"], *entry);
        let read = executor
            .execute(&target, &request("resources/read", json!({"uri":uri})))
            .await?
            .result;
        let expected = if label == "beta" {
            second_body.clone()
        } else {
            self::entry("review").1
        };
        assert_eq!(read["contents"][0]["text"], expected);
        let uri = format!("skill://{label}/review/references/check.md");
        let support = executor
            .execute(&target, &request("resources/read", json!({"uri":uri})))
            .await?
            .result;
        assert_eq!(support["contents"][0]["uri"], uri);
        assert_eq!(
            support["contents"][0]["text"],
            format!("{label} support bytes")
        );
    }
    assert_eq!(
        first
            .requests()?
            .iter()
            .filter(|r| r["method"] == "skills/list")
            .count(),
        2
    );
    assert_eq!(
        second
            .requests()?
            .iter()
            .filter(|r| r["method"] == "skills/list")
            .count(),
        1
    );
    Ok(())
}

struct GatewayTable(McpTarget);

#[async_trait::async_trait]
impl bitrouter::gateway::mcp::RoutingTable for GatewayTable {
    async fn resolve(
        &self,
        _: &bitrouter::gateway::mcp::ServerSelector,
        _: &CallerContext,
    ) -> bitrouter::gateway::error::Result<McpTarget> {
        Ok(self.0.clone())
    }
}

struct NoModel;

#[async_trait::async_trait]
impl bitrouter::gateway::pipeline::executor::Executor for NoModel {
    async fn execute(
        &self,
        _: &bitrouter::gateway::pipeline::types::RoutingTarget,
        _: &bitrouter_ai::types::Prompt,
        _: &bitrouter::gateway::pipeline::context::PipelineContext,
    ) -> bitrouter::gateway::error::Result<bitrouter::gateway::pipeline::types::ExecutionResult>
    {
        Err(bitrouter::gateway::error::BitrouterError::internal(
            "MCP fixture must not generate",
        ))
    }
    async fn execute_stream(
        &self,
        _: &bitrouter::gateway::pipeline::types::RoutingTarget,
        _: &bitrouter_ai::types::Prompt,
        _: &bitrouter::gateway::pipeline::context::PipelineContext,
    ) -> bitrouter::gateway::error::Result<bitrouter::gateway::pipeline::executor::StreamPartStream>
    {
        Err(bitrouter::gateway::error::BitrouterError::internal(
            "MCP fixture must not generate",
        ))
    }
}

fn gateway(target: McpTarget, executor: Arc<dyn Executor>) -> TestResult<axum::Router> {
    let app = bitrouter::gateway::assembly::App::builder()
        .language_model(|model| {
            model
                .routing_table(Arc::new(
                    bitrouter::gateway::pipeline::routing::StaticRoutingTable::new(),
                ))
                .executor(Arc::new(NoModel));
        })
        .mcp(|mcp| {
            mcp.routing_table(Arc::new(GatewayTable(target)))
                .executor(executor);
        })
        .skip_auth(true)
        .build()?;
    Ok(bitrouter::gateway::http::build_router(
        bitrouter::gateway::http::AppState {
            language_model: app
                .language_model()
                .ok_or("missing model pipeline")?
                .clone(),
            mcp: app.mcp().cloned(),
            skip_auth: true,
            metrics_renderer: None,
            prompt_transforms: vec![],
        },
    ))
}

async fn rpc(router: &axum::Router, method: &str, mut params: Value) -> TestResult<Value> {
    use tower::ServiceExt;
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientInfo":{"name":"G09-fixture","version":"1"},
        "io.modelcontextprotocol/clientCapabilities":{},
    });
    let mut request = http::Request::builder()
        .method("POST")
        .uri("/mcp/origin")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", method);
    if method == "resources/read"
        && let Some(uri) = params["uri"].as_str()
    {
        request = request.header("mcp-name", uri);
    }
    let request = request.body(axum::body::Body::from(serde_json::to_vec(
        &json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}),
    )?))?;
    let response = router.clone().oneshot(request).await?;
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024).await?;
    if let Ok(value) = serde_json::from_slice(&body) {
        return Ok(value);
    }
    let text = std::str::from_utf8(&body)?;
    for data in text.lines().filter_map(|line| line.strip_prefix("data:")) {
        if let Ok(value) = serde_json::from_str::<Value>(data.trim())
            && value["id"] == 7
            && (value.get("result").is_some() || value.get("error").is_some())
        {
            return Ok(value);
        }
    }
    Err(std::io::Error::other(format!(
        "HTTP MCP {method} returned no JSON-RPC result: {text}"
    ))
    .into())
}

#[tokio::test]
async fn http_gateway_direct_lookup_is_independent_of_listing_and_preserves_errors() -> TestResult {
    let fixture = Fixture::new(capabilities(false))?;
    let mut config = fixture.configuration()?;
    config["pages"]["first"] = page(vec![], 2000, "public");
    fixture.write(config)?;
    let executor =
        Arc::new(RmcpExecutor::new().with_protocol_version(ProtocolVersion::V_2026_07_28));
    let router = gateway(fixture.target("origin"), executor)?;
    let discovered = rpc(&router, "server/discover", json!({})).await?;
    assert!(
        discovered["result"]["capabilities"]["resources"].is_object(),
        "{discovered}"
    );
    let extension =
        &discovered["result"]["capabilities"]["extensions"]["io.modelcontextprotocol/skills"];
    assert!(extension.is_object());
    assert_ne!(extension["directoryRead"], true);
    assert!(
        !fixture.journal.exists(),
        "gateway discovery connected an upstream"
    );
    let got = rpc(
        &router,
        "skills/get",
        json!({"uri":"skill://review/SKILL.md"}),
    )
    .await?;
    assert_eq!(got["result"]["skill"], entry("review").0);
    assert_eq!(got["result"]["resultType"], "complete");
    let requests = fixture.requests()?;
    assert!(
        !requests
            .iter()
            .any(|r| matches!(r["method"].as_str(), Some("skills/list" | "resources/read")))
    );
    let empty = rpc(&router, "skills/list", json!({})).await?;
    assert!(
        empty["result"]["skills"]
            .as_array()
            .ok_or("missing empty listing")?
            .is_empty()
    );
    let missing = rpc(
        &router,
        "skills/get",
        json!({"uri":"skill://missing/SKILL.md"}),
    )
    .await?;
    assert_eq!(missing["error"]["code"], -32602, "{missing}");
    let directory = rpc(
        &router,
        "resources/directory/read",
        json!({"uri":"skill://review"}),
    )
    .await?;
    assert!(directory["error"].is_object());
    let ordinary = rpc(
        &router,
        "resources/read",
        json!({"uri":"skill://review/SKILL.md"}),
    )
    .await?;
    assert_eq!(ordinary["result"]["contents"][0]["text"], entry("review").1);
    let unknown = rpc(&router, "skills/execute", json!({})).await?;
    assert_eq!(unknown["error"]["code"], -32601);
    assert!(
        !fixture
            .requests()?
            .iter()
            .any(|r| r["method"] == "skills/execute")
    );
    Ok(())
}

#[tokio::test]
async fn unsupported_members_and_unaggregatable_schemes_keep_explicit_diagnostics() -> TestResult {
    let fixture = Fixture::new(capabilities(false))?;
    let unsupported = Fixture::new(json!({"resources":{}}))?;
    let inner = Arc::new(RmcpExecutor::new().with_protocol_version(ProtocolVersion::V_2026_07_28));
    let executor = AggregatingExecutor::new(inner.clone());
    let target = McpTarget::Aggregate {
        members: vec![
            AggregateMember {
                server_name: "allowed".into(),
                tool_prefix: "a__".into(),
                transport: fixture.transport.clone(),
            },
            AggregateMember {
                server_name: "not-allowed".into(),
                tool_prefix: "b__".into(),
                transport: unsupported.transport.clone(),
            },
        ],
    };
    let result = executor
        .execute(&target, &request("skills/list", json!({})))
        .await?
        .result;
    assert_eq!(
        result["skills"].as_array().ok_or("missing skills")?.len(),
        1
    );
    assert_eq!(result["_bitrouterErrors"][0]["server"], "not-allowed");
    assert_eq!(result["cacheScope"], "private");
    assert_eq!(result["ttlMs"], 0);
    assert!(
        !unsupported
            .requests()?
            .iter()
            .any(|r| r["method"] == "skills/list")
    );
    let mut config = fixture.configuration()?;
    let (mut custom, body) = entry("review");
    let uri = "github://owner/repo/skills/review/SKILL.md";
    custom["uri"] = uri.into();
    custom["resources"][0]["uri"] = uri.into();
    config["pages"]["first"] = page(vec![custom.clone()], 2000, "public");
    config["get"][uri] =
        json!({"resultType":"complete","skill":custom,"ttlMs":2000,"cacheScope":"public"});
    config["files"][uri] = json!({"mimeType":"text/markdown","text":body});
    fixture.write(config)?;
    let result = executor
        .execute(&target, &request("skills/list", json!({})))
        .await?
        .result;
    assert!(
        result["skills"]
            .as_array()
            .ok_or("missing skills")?
            .is_empty()
    );
    assert!(
        result["_bitrouterErrors"]
            .to_string()
            .contains("direct route")
    );
    let direct = inner
        .execute(
            &fixture.target("direct"),
            &request("skills/get", json!({"uri":uri})),
        )
        .await?
        .result;
    assert_eq!(direct["skill"], custom);
    let read = inner
        .execute(
            &fixture.target("direct"),
            &request("resources/read", json!({"uri":uri})),
        )
        .await?
        .result;
    assert_eq!(read["contents"][0]["text"], body);
    assert!(
        executor
            .execute(
                &target,
                &request(
                    "resources/directory/read",
                    json!({"uri":"skill://allowed/review"})
                )
            )
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn metadata_validation_rejects_incomplete_entries_without_inventing_manifests() -> TestResult
{
    let fixture = Fixture::new(capabilities(false))?;
    let executor = RmcpExecutor::new().with_protocol_version(ProtocolVersion::V_2026_07_28);
    let valid = entry("review").0;
    let mut missing = valid.clone();
    missing
        .as_object_mut()
        .ok_or("entry object")?
        .remove("resources");
    let mut bad_digest = valid.clone();
    bad_digest["resources"][0]["digest"] = "sha256:invalid".into();
    let mut duplicate = valid.clone();
    let resource = duplicate["resources"][0].clone();
    duplicate["resources"]
        .as_array_mut()
        .ok_or("manifest array")?
        .push(resource);
    let mut no_body = valid.clone();
    no_body["resources"][0]["uri"] = "skill://review/other.md".into();
    for malformed in [missing, bad_digest, duplicate, no_body] {
        let mut config = fixture.configuration()?;
        config["get"]["skill://review/SKILL.md"] =
            json!({"resultType":"complete","skill":malformed,"ttlMs":1000,"cacheScope":"private"});
        fixture.write(config)?;
        assert!(
            executor
                .execute(
                    &fixture.target("origin"),
                    &request("skills/get", json!({"uri":"skill://review/SKILL.md"}))
                )
                .await
                .is_err(),
            "accepted malformed entry"
        );
    }
    let mut config = fixture.configuration()?;
    config["get"]["skill://review/SKILL.md"] = json!({"skill":valid});
    fixture.write(config)?;
    let legacy = executor
        .execute(
            &fixture.target("origin"),
            &request("skills/get", json!({"uri":"skill://review/SKILL.md"})),
        )
        .await?
        .result;
    assert_eq!(legacy["resultType"], "complete");
    assert_eq!(legacy["ttlMs"], 0);
    assert_eq!(legacy["cacheScope"], "private");
    assert_eq!(legacy["skill"], valid);
    let mut config = fixture.configuration()?;
    let mut dynamic = valid.clone();
    dynamic["resources"] = "dynamic".into();
    config["get"]["skill://review/SKILL.md"] =
        json!({"resultType":"complete","skill":dynamic,"ttlMs":0,"cacheScope":"private"});
    fixture.write(config)?;
    let dynamic = executor
        .execute(
            &fixture.target("origin"),
            &request("skills/get", json!({"uri":"skill://review/SKILL.md"})),
        )
        .await?
        .result;
    assert_eq!(
        dynamic["skill"]["resources"], "dynamic",
        "gateway forwarding is independent of host activation policy"
    );
    let mut config = fixture.configuration()?;
    config["get"]["skill://review/SKILL.md"]["resultType"] = "input_required".into();
    fixture.write(config)?;
    assert!(
        executor
            .execute(
                &fixture.target("origin"),
                &request("skills/get", json!({"uri":"skill://review/SKILL.md"}))
            )
            .await
            .is_err()
    );
    assert!(
        !fixture
            .requests()?
            .iter()
            .any(|r| r["method"] == "resources/read")
    );
    Ok(())
}

#[tokio::test]
async fn baseline_manifest_is_atomic_at_512_files_and_16_mib_without_content_reads() -> TestResult {
    let fixture = Fixture::new(capabilities(false))?;
    let mut config = fixture.configuration()?;
    let (mut skill, body) = entry("review");
    let total = 16 * 1024 * 1024;
    let remaining = total - body.len();
    for index in 0..511 {
        let size = remaining / 511 + usize::from(index < remaining % 511);
        let content = "v".repeat(size);
        let uri = format!("skill://review/references/{index}.txt");
        skill["resources"]
            .as_array_mut()
            .ok_or("manifest array")?
            .push(json!({"uri":uri,"digest":digest(content.as_bytes()),"size":size}));
        config["files"][&uri] = json!({"mimeType":"text/plain","text":content});
    }
    config["pages"]["first"] = page(vec![skill.clone()], 1000, "private");
    config["get"]["skill://review/SKILL.md"] =
        json!({"resultType":"complete","skill":skill,"ttlMs":1000,"cacheScope":"private"});
    fixture.write(config)?;
    let executor = RmcpExecutor::new().with_protocol_version(ProtocolVersion::V_2026_07_28);
    let listed = executor
        .execute(
            &fixture.target("origin"),
            &request("skills/list", json!({})),
        )
        .await?
        .result;
    let files = listed["skills"][0]["resources"]
        .as_array()
        .ok_or("missing full manifest")?;
    assert_eq!(files.len(), 512);
    assert_eq!(
        files
            .iter()
            .filter_map(|file| file["size"].as_u64())
            .sum::<u64>(),
        total as u64
    );
    let got = executor
        .execute(
            &fixture.target("origin"),
            &request("skills/get", json!({"uri":"skill://review/SKILL.md"})),
        )
        .await?
        .result;
    assert_eq!(got["skill"], listed["skills"][0]);
    assert!(
        !fixture
            .requests()?
            .iter()
            .any(|r| r["method"] == "resources/read")
    );
    Ok(())
}

#[tokio::test]
async fn query_and_fragment_uris_keep_exact_identity_through_real_gateway_mapping() -> TestResult {
    let fixture = Fixture::new(capabilities(false))?;
    let mut config = fixture.configuration()?;
    let root = "skill://review/SKILL.md?revision=1#instructions";
    let support = "skill://review/references/check.md?version=2&tag=../other#section";
    let (mut skill, body) = entry("review");
    skill["uri"] = root.into();
    skill["resources"][0]["uri"] = root.into();
    skill["resources"]
        .as_array_mut()
        .ok_or("resources")?
        .push(json!({"uri":support,"digest":digest(b"exact support"),"size":13}));
    config["pages"]["first"]["skills"] = json!([skill.clone()]);
    config["get"] = json!({root:{"resultType":"complete","skill":skill.clone(),"ttlMs":2000,"cacheScope":"public"}});
    config["files"] = json!({root:{"text":body},support:{"text":"exact support"}});
    fixture.write(config)?;
    let upstream =
        Arc::new(RmcpExecutor::new().with_protocol_version(ProtocolVersion::V_2026_07_28));
    let executor = AggregatingExecutor::new(upstream);
    let target = McpTarget::Aggregate {
        members: vec![AggregateMember {
            server_name: "source".into(),
            tool_prefix: "source__".into(),
            transport: fixture.transport.clone(),
        }],
    };
    let listed = executor
        .execute(&target, &request("skills/list", json!({})))
        .await?
        .result;
    let mapped_root = "skill://source/review/SKILL.md?revision=1#instructions";
    let mapped_support = "skill://source/review/references/check.md?version=2&tag=../other#section";
    assert_eq!(listed["skills"][0]["uri"], mapped_root);
    assert_eq!(listed["skills"][0]["frontmatter"], skill["frontmatter"]);
    assert_eq!(listed["skills"][0]["resources"][1]["uri"], mapped_support);
    let got = executor
        .execute(&target, &request("skills/get", json!({"uri":mapped_root})))
        .await?
        .result;
    assert_eq!(got["skill"], listed["skills"][0]);
    for (uri, expected) in [
        (mapped_root, body.as_str()),
        (mapped_support, "exact support"),
    ] {
        let read = executor
            .execute(&target, &request("resources/read", json!({"uri":uri})))
            .await?
            .result;
        assert_eq!(read["contents"][0]["uri"], uri);
        assert_eq!(read["contents"][0]["text"], expected);
    }
    let records = fixture.requests()?;
    let reads: Vec<_> = records
        .iter()
        .filter(|record| record["method"] == "resources/read")
        .collect();
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[0]["params"]["uri"], root);
    assert_eq!(reads[1]["params"]["uri"], support);
    assert!(
        records
            .iter()
            .any(|record| record["method"] == "skills/get" && record["params"]["uri"] == root)
    );
    Ok(())
}
