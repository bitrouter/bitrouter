//! Native host metadata and byte admission against an independent MCP process.
//! Resource admission and explicit consent through the native Thread/model path.

use std::path::{Path, PathBuf};

use base64::Engine;
use bitrouter::config::mcp::{McpServerConfig, McpTransport};
use bitrouter::config::skills::McpSkillSourceGrant;
use bitrouter::gateway::caller::CallerContext;
use bitrouter::harness::resources::HarnessConfig;
use bitrouter::harness::resources::managed::NativeResources;
use bitrouter::skills::mcp::SkillRead;
use bitrouter_ai::harness::agent::ToolMode;
use rmcp::model::ProtocolVersion;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

#[path = "support/mcp_fixture.rs"]
mod mcp_fixture;

#[path = "support/example_binary.rs"]
mod example_binary;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const ROOT: &str = "skill://review/SKILL.md";
const SUPPORT: &str = "skill://review/references/check.md";

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

fn entry(body: &str) -> Value {
    json!({
        "uri":ROOT,
        "frontmatter":{"name":"review","description":"review workflow","metadata":{"custom":"preserved"},"allowed-tools":"shell"},
        "resources":[
            {"uri":ROOT,"digest":digest(body.as_bytes()),"size":body.len()},
            {"uri":SUPPORT,"digest":digest(b"support bytes\n"),"size":14}
        ],
        "unknownTopLevel":{"retained":true}
    })
}

fn body(marker: &str) -> String {
    format!(
        "---\nname: review\ndescription: review workflow\nmetadata:\n  custom: preserved\nallowed-tools: shell\n---\n{marker}\n"
    )
}

struct Fixture {
    _root: tempfile::TempDir,
    config: PathBuf,
    journal: PathBuf,
    server: McpServerConfig,
    body: String,
}

impl Fixture {
    fn new(name: &str, marker: &str) -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let config = root.path().join("server.json");
        let journal = root.path().join("requests.jsonl");
        let server =
            McpServerConfig::with_defaults(name, mcp_fixture::transport(&config, &journal)?);
        let body = body(marker);
        let fixture = Self {
            _root: root,
            config,
            journal,
            server,
            body,
        };
        let entry = entry(&fixture.body);
        fixture.write(json!({
            "capabilities":{"resources":{},"extensions":{"io.modelcontextprotocol/skills":{}}},
            "pages":{"first":{"resultType":"complete","skills":[entry.clone()],"ttlMs":0,"cacheScope":"private"}},
            "get":{ROOT:{"resultType":"complete","skill":entry,"ttlMs":0,"cacheScope":"private"}},
            "files":{
                ROOT:{"mimeType":"text/markdown","text":fixture.body},
                SUPPORT:{"mimeType":"text/markdown","text":"support bytes\n"}
            }
        }))?;
        Ok(fixture)
    }

    fn read(&self) -> TestResult<Value> {
        Ok(serde_json::from_slice(&std::fs::read(&self.config)?)?)
    }

    fn write(&self, config: Value) -> TestResult {
        std::fs::write(&self.config, serde_json::to_vec(&config)?)?;
        Ok(())
    }

    fn count(&self, method: &str) -> TestResult<usize> {
        let requests = match mcp_fixture::requests(&self.journal) {
            Ok(requests) => requests,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        Ok(requests
            .iter()
            .filter(|request| request["method"] == method)
            .count())
    }

    fn configuration(&self, workspace: &Path) -> HarnessConfig {
        let mut config = HarnessConfig {
            servers: vec![self.server.clone()],
            protocol: ProtocolVersion::V_2026_07_28,
            ..HarnessConfig::default()
        };
        config.skills.mcp_sources.insert(
            self.server.name.clone(),
            McpSkillSourceGrant {
                workspaces: vec![workspace.into()],
                local: true,
                api_key_ids: vec!["key-a".into(), "key-b".into()],
                user_ids: vec![],
            },
        );
        config
    }
}

async fn discover(
    workspace: &Path,
    config: &HarnessConfig,
    caller: &CallerContext,
) -> TestResult<NativeResources> {
    Ok(NativeResources::discover_for_caller(
        workspace,
        ToolMode::ReadOnly,
        config,
        caller,
        &CancellationToken::new(),
    )
    .await?)
}

fn read<'a>(reference: &'a str, cached_bytes: Option<&'a [u8]>) -> SkillRead<'a> {
    SkillRead {
        reference,
        max_bytes: 64 * 1024,
        cached_bytes,
    }
}

struct CapturePrompts(std::sync::Arc<tokio::sync::Mutex<Vec<bitrouter_ai::types::Prompt>>>);

#[async_trait::async_trait]
impl bitrouter::gateway::pipeline::hooks::ObserveHook for CapturePrompts {
    async fn after_phase(
        &self,
        _: bitrouter::gateway::pipeline::hooks::Phase,
        _: &bitrouter::gateway::pipeline::context::PipelineContext,
    ) {
    }
    async fn on_stream_part(
        &self,
        _: &bitrouter::gateway::pipeline::context::StreamContext,
        _: &bitrouter_ai::types::StreamPart,
    ) {
    }
    async fn on_request_end(
        &self,
        _: &bitrouter::gateway::pipeline::context::PipelineContext,
        _: &bitrouter::gateway::pipeline::hooks::RequestOutcome,
    ) {
    }
    async fn on_hop_start(
        &self,
        ctx: &bitrouter::gateway::pipeline::context::PipelineContext,
        _: &bitrouter::gateway::pipeline::types::RoutingTarget,
    ) {
        if let Some(prompt) = ctx.input().generation_prompt() {
            self.0.lock().await.push(prompt.clone());
        }
    }
}

#[tokio::test]
async fn native_thread_consent_precedes_real_mcp_reads_and_model_data_in_both_permission_profiles()
-> TestResult {
    use bitrouter::gateway::model::target::AuthScheme;
    use bitrouter::gateway::pipeline::executor::MockExecutor;
    use bitrouter::gateway::pipeline::routing::StaticRoutingTable;
    use bitrouter::gateway::pipeline::types::RoutingTarget;
    use bitrouter::harness::agent::AgentConfigExt;
    use bitrouter::harness::service::ThreadService;
    use bitrouter::harness::thread::{ThreadRequest, ThreadTarget};
    use bitrouter::harness::turn::{ApprovalAnswer, TurnRequest};
    use bitrouter_ai::harness::agent::AgentConfig;
    use bitrouter_ai::harness::thread::PermissionProfile;
    use bitrouter_ai::harness::turn::TurnStatus;
    use bitrouter_ai::types::{ApiProtocol, Role};
    use std::sync::Arc;
    use std::time::Duration;

    for profile in [PermissionProfile::ReadOnly, PermissionProfile::AllowEffects] {
        let fixture = Fixture::new("consent-host", "MCP_BODY_AFTER_EXPLICIT_CONSENT")?;
        let workspace = tempfile::tempdir()?;
        let prompts = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let routing = StaticRoutingTable::new();
        routing.insert(
            "fixture-model",
            vec![RoutingTarget {
                provider_name: "fixture".into(),
                service_id: "fixture-model".into(),
                api_base: "https://example.invalid".into(),
                api_key: "fixture-key".into(),
                api_protocol: ApiProtocol::ChatCompletions,
                chat_token_limit_field: None,
                chat_supports_store: None,
                chat_supports_stream_options: None,
                chat_google_extensions: false,
                reasoning_effort: None,
                model_constraints: Default::default(),
                registry: None,
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                auth_scheme: AuthScheme::Bearer,
                headers: vec![],
            }],
        );
        let app = Arc::new(
            bitrouter::gateway::assembly::App::builder()
                .language_model(|builder| {
                    builder
                        .routing_table(Arc::new(routing))
                        .executor(Arc::new(MockExecutor::new(vec![
                            bitrouter::gateway::pipeline::executor::MockResponse::Stream(vec![
                                bitrouter_ai::types::StreamPart::TextDelta {
                                    text: "done".into(),
                                },
                                bitrouter_ai::types::StreamPart::Finish {
                                    reason: bitrouter_ai::types::FinishReason::Stop,
                                },
                            ]),
                        ])))
                        .observe_hook(CapturePrompts(prompts.clone()));
                })
                .build()?,
        );
        let service = ThreadService::with_workspace_grants(
            app,
            &[bitrouter::harness::thread::WorkspaceGrant {
                workspace: workspace.path().into(),
                permission_profiles: vec![profile],
            }],
            Arc::new(bitrouter::harness::storage::MemoryExecutionStore::default()),
        )?
        .with_resources(fixture.configuration(workspace.path()))?;
        let caller = CallerContext::local();
        let config = AgentConfig::fixed("fixture-model", None).with_skill_constraints(Some(
            bitrouter_ai::core::skills::SkillConstraints {
                include: vec![bitrouter_ai::core::skills::SkillSelector::Mcp {
                    server: fixture.server.name.clone(),
                    uri: ROOT.into(),
                }],
                exclude: vec![],
            },
        ));
        let created = service
            .create_thread(
                &service.capabilities().server_instance_id,
                ThreadRequest {
                    caller: caller.clone(),
                    workspace: workspace.path().into(),
                    config: if profile == PermissionProfile::ReadOnly {
                        config.read_only()
                    } else {
                        config
                    },
                    permission_profile: profile,
                    verification_command: None,
                    idempotency_key: "create".into(),
                },
            )
            .await?;
        let target = ThreadTarget {
            thread_id: created.thread_id,
            server_instance_id: created.server_instance_id,
        };
        let accepted = service
            .start_turn(
                &target,
                &caller,
                TurnRequest {
                    prompt: "Review this workflow".into(),
                    idempotency_key: "review".into(),
                },
            )
            .await?;
        let waiting = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = service.read_turn(&target, &caller, &accepted.turn_id)?;
                if snapshot.status == TurnStatus::WaitingForInput || snapshot.status.terminal() {
                    return Ok::<_, bitrouter::harness::service::ServiceError>(snapshot);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await??;
        assert_eq!(
            waiting.status,
            TurnStatus::WaitingForInput,
            "{:?}",
            waiting.detail
        );
        let pending = waiting.pending_input.ok_or("skill consent input")?;
        assert_eq!(pending.tool_name, "mcp_skill.activate");
        let consent: Value = serde_json::from_str(&pending.arguments)?;
        assert_eq!(
            consent["request"]["source"]["entry"]["unknownTopLevel"],
            json!({"retained":true})
        );
        assert!(
            !pending
                .arguments
                .contains("MCP_BODY_AFTER_EXPLICIT_CONSENT")
        );
        assert_eq!(fixture.count("resources/read")?, 0);
        assert!(prompts.lock().await.is_empty());
        service
            .answer_thread_input(
                &target,
                &caller,
                ApprovalAnswer {
                    turn_id: accepted.turn_id.clone(),
                    request_id: pending.request_id,
                    approved: true,
                    idempotency_key: "explicit-consent".into(),
                },
            )
            .await?;
        let completed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = service.read_turn(&target, &caller, &accepted.turn_id)?;
                if snapshot.status.terminal() {
                    return Ok::<_, bitrouter::harness::service::ServiceError>(snapshot);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await??;
        assert_eq!(
            completed.status,
            TurnStatus::Completed,
            "{:?}",
            completed.detail
        );
        assert_eq!(fixture.count("resources/read")?, 1);
        let captured = prompts.lock().await;
        assert_eq!(captured.len(), 1);
        let prompt = captured.first().ok_or("model prompt")?;
        let user = prompt
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");
        let system = prompt
            .messages
            .iter()
            .filter(|message| message.role == Role::System)
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");
        assert!(user.contains("MCP_BODY_AFTER_EXPLICIT_CONSENT"));
        assert!(user.contains(ROOT));
        assert!(user.contains(SUPPORT));
        assert!(!system.contains("MCP_BODY_AFTER_EXPLICIT_CONSENT"));
        if profile == PermissionProfile::ReadOnly {
            assert!(!serde_json::to_string(&prompt.tools)?.contains("\"shell\""));
        }
        drop(captured);
        service.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn explicit_source_caller_and_workspace_grants_precede_metadata_and_direct_lookup()
-> TestResult {
    let fixture = Fixture::new("host-label", "EXACT_BODY")?;
    let workspace = tempfile::tempdir()?;
    let other = tempfile::tempdir()?;
    let mut config = fixture.configuration(workspace.path());
    let grant = config
        .skills
        .mcp_sources
        .remove("host-label")
        .ok_or("grant")?;
    let resources = discover(workspace.path(), &config, &CallerContext::local()).await?;
    assert!(resources.mcp_skill_catalog().skills.is_empty());
    assert!(
        resources
            .mcp_skill("host-label", ROOT, &CancellationToken::new())
            .await
            .is_err()
    );
    resources.shutdown().await?;
    assert_eq!(fixture.count("server/discover")?, 0);
    config.skills.mcp_sources.insert("host-label".into(), grant);
    for (path, caller) in [
        (workspace.path(), CallerContext::anonymous()),
        (
            workspace.path(),
            CallerContext::new("unlisted-key", "unlisted-user"),
        ),
        (other.path(), CallerContext::local()),
    ] {
        let resources = discover(path, &config, &caller).await?;
        assert!(resources.mcp_skill_catalog().skills.is_empty());
        assert!(
            resources
                .mcp_skill("host-label", ROOT, &CancellationToken::new())
                .await
                .is_err()
        );
        resources.shutdown().await?;
    }
    assert_eq!(fixture.count("server/discover")?, 0);
    let mut data = fixture.read()?;
    data["pages"]["first"]["skills"] = json!([]);
    fixture.write(data)?;
    let resources = discover(workspace.path(), &config, &CallerContext::local()).await?;
    assert!(resources.mcp_skill_catalog().skills.is_empty());
    let held = resources
        .mcp_skill("host-label", ROOT, &CancellationToken::new())
        .await?;
    assert_eq!(held.origin.server, "host-label");
    assert_eq!(
        held.entry.extra["unknownTopLevel"],
        json!({"retained":true})
    );
    assert_eq!(held.entry.frontmatter["allowed-tools"], "shell");
    assert!(
        resources
            .mcp_skill(
                "host-label",
                "skill://unknown/SKILL.md",
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(fixture.count("skills/list")?, 1);
    assert_eq!(fixture.count("skills/get")?, 2);
    assert_eq!(fixture.count("resources/read")?, 0);
    assert_eq!(
        fixture.count("tools/list")?,
        0,
        "read-only skill source grants expose no MCP tools"
    );
    resources.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn every_network_and_cached_use_checks_bytes_and_complete_frontmatter() -> TestResult {
    let fixture = Fixture::new("source", "EXACT_BODY")?;
    let workspace = tempfile::tempdir()?;
    let resources = discover(
        workspace.path(),
        &fixture.configuration(workspace.path()),
        &CallerContext::local(),
    )
    .await?;
    let held = resources
        .mcp_skill_catalog()
        .skills
        .first()
        .ok_or("held skill")?;
    assert_eq!(fixture.count("resources/read")?, 0);
    let loaded = resources
        .mcp_skill_file(held, read("SKILL.md", None), &CancellationToken::new())
        .await?;
    assert_eq!(loaded.text()?, fixture.body);
    assert_eq!(fixture.count("resources/read")?, 1);
    let cached = resources
        .mcp_skill_file(
            held,
            read("SKILL.md", Some(&loaded.bytes)),
            &CancellationToken::new(),
        )
        .await?;
    assert_eq!(cached.bytes, loaded.bytes);
    let mut tampered = loaded.bytes.clone();
    let last = tampered.last_mut().ok_or("cache bytes")?;
    *last ^= 1;
    assert!(
        resources
            .mcp_skill_file(
                held,
                read("SKILL.md", Some(&tampered)),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(
        resources
            .mcp_skill_file(
                held,
                read("SKILL.md", Some(b"short")),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(
        fixture.count("resources/read")?,
        1,
        "cache hits are verified without a network refresh"
    );
    let support = resources
        .mcp_skill_file(
            held,
            read("references/check.md", None),
            &CancellationToken::new(),
        )
        .await?;
    assert_eq!(support.text()?, "support bytes\n");
    let mut data = fixture.read()?;
    data["files"][SUPPORT]["text"] = "support bytes!".into();
    fixture.write(data.clone())?;
    assert!(
        resources
            .mcp_skill_file(
                held,
                read("references/check.md", None),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    data["get"][ROOT]["skill"]["frontmatter"]["metadata"]["custom"] = "changed".into();
    fixture.write(data)?;
    let different = resources
        .mcp_skill("source", ROOT, &CancellationToken::new())
        .await?;
    assert_ne!(held.entry_digest, different.entry_digest);
    let error = resources
        .mcp_skill_file(
            &different,
            read("SKILL.md", None),
            &CancellationToken::new(),
        )
        .await
        .err()
        .ok_or("expected mismatch")?;
    assert!(error.contains("complete frontmatter mismatch"));
    resources.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn source_identity_scope_and_manifest_bound_reads_cannot_be_replaced() -> TestResult {
    let alpha = Fixture::new("alpha", "ALPHA_BODY")?;
    let beta = Fixture::new("beta", "BETA_BODY")?;
    let workspace = tempfile::tempdir()?;
    let mut config = alpha.configuration(workspace.path());
    config.servers.push(beta.server.clone());
    config
        .skills
        .mcp_sources
        .extend(beta.configuration(workspace.path()).skills.mcp_sources);
    let resources = discover(
        workspace.path(),
        &config,
        &CallerContext::new("key-a", "user"),
    )
    .await?;
    assert_eq!(resources.mcp_skill_catalog().skills.len(), 2);
    for held in &resources.mcp_skill_catalog().skills {
        let content = resources
            .mcp_skill_file(held, read("SKILL.md", None), &CancellationToken::new())
            .await?;
        assert_eq!(
            content.text()?,
            if held.origin.server == "alpha" {
                &alpha.body
            } else {
                &beta.body
            }
        );
        for reference in [
            "../outside",
            "skill://review/unlisted.md",
            "skill://review-evil/SKILL.md",
            "references/../../outside",
        ] {
            assert!(
                resources
                    .mcp_skill_file(held, read(reference, None), &CancellationToken::new())
                    .await
                    .is_err()
            );
        }
    }
    assert_eq!(alpha.count("resources/read")?, 1);
    assert_eq!(beta.count("resources/read")?, 1);
    let held = resources
        .mcp_skill_catalog()
        .skills
        .iter()
        .find(|held| held.origin.server == "alpha")
        .ok_or("alpha")?
        .clone();
    resources.shutdown().await?;
    let other = discover(
        workspace.path(),
        &config,
        &CallerContext::new("key-b", "user"),
    )
    .await?;
    assert!(
        other
            .mcp_skill_file(
                &held,
                read("SKILL.md", Some(alpha.body.as_bytes())),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    other.shutdown().await?;
    let mut revoked = config.clone();
    revoked.skills.mcp_sources.clear();
    let other = discover(
        workspace.path(),
        &revoked,
        &CallerContext::new("key-a", "user"),
    )
    .await?;
    assert!(
        other
            .mcp_skill_file(
                &held,
                read("SKILL.md", Some(alpha.body.as_bytes())),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    other.shutdown().await?;
    assert_eq!(alpha.count("resources/read")?, 1);
    let mut data = alpha.read()?;
    for resources_value in [
        json!([]),
        json!([{"uri":ROOT,"digest":digest(b"changed"),"size":7}]),
    ] {
        data["get"][ROOT]["skill"]["resources"] = resources_value;
        alpha.write(data.clone())?;
        let next = discover(
            workspace.path(),
            &config,
            &CallerContext::new("key-a", "user"),
        )
        .await?;
        let result = next
            .mcp_skill("alpha", ROOT, &CancellationToken::new())
            .await;
        if let Ok(changed) = result {
            assert_ne!(held.entry_digest, changed.entry_digest);
            assert!(
                next.mcp_skill_file(
                    &changed,
                    read("SKILL.md", Some(alpha.body.as_bytes())),
                    &CancellationToken::new()
                )
                .await
                .is_err()
            );
        }
        next.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn dynamic_and_invalid_structure_are_excluded_without_reading_content() -> TestResult {
    let fixture = Fixture::new("source", "EXACT_BODY")?;
    let workspace = tempfile::tempdir()?;
    for (field, value, diagnostic) in [
        ("resources", json!("dynamic"), "dynamic"),
        (
            "uri",
            json!("skill://different/SKILL.md"),
            "parent directory",
        ),
        (
            "uri",
            json!("github://owner/repo/review/OTHER.md"),
            "/SKILL.md",
        ),
        (
            "frontmatter",
            json!({"name":"review","description":"review workflow","metadata":"invalid-map"}),
            "invalid skill frontmatter metadata",
        ),
    ] {
        let mut data = fixture.read()?;
        let mut skill = entry(&fixture.body);
        skill[field] = value;
        if field == "uri" {
            skill["resources"][0]["uri"] = skill["uri"].clone();
        }
        data["pages"]["first"]["skills"] = json!([skill.clone()]);
        data["get"][ROOT]["skill"] = skill;
        fixture.write(data)?;
        let resources = discover(
            workspace.path(),
            &fixture.configuration(workspace.path()),
            &CallerContext::local(),
        )
        .await?;
        assert!(resources.mcp_skill_catalog().skills.is_empty());
        assert!(
            resources
                .mcp_skill_catalog()
                .excluded
                .iter()
                .any(|problem| problem.reason.contains(diagnostic))
        );
        assert!(
            resources
                .mcp_skill("source", ROOT, &CancellationToken::new())
                .await
                .is_err()
        );
        resources.shutdown().await?;
    }
    assert_eq!(fixture.count("resources/read")?, 0);
    Ok(())
}

#[tokio::test]
async fn full_baseline_manifest_and_binary_files_have_independent_read_budgets() -> TestResult {
    let fixture = Fixture::new("source", "EXACT_BODY")?;
    let workspace = tempfile::tempdir()?;
    let mut data = fixture.read()?;
    let mut skill = entry(&fixture.body);
    let mut files = vec![skill["resources"][0].clone()];
    let mut remaining = 16 * 1024 * 1024 - fixture.body.len();
    for index in 0..511 {
        let count = if index == 510 { remaining } else { 32768 };
        let bytes = vec![0xff; count];
        remaining -= count;
        let uri = format!("skill://review/assets/{index}.bin");
        files.push(json!({"uri":uri,"digest":digest(&bytes),"size":count}));
        data["files"][&uri] = json!({"mimeType":"application/octet-stream","blob":base64::engine::general_purpose::STANDARD.encode(bytes)});
    }
    assert_eq!(remaining, 0);
    skill["resources"] = json!(files);
    data["pages"]["first"]["skills"] = json!([skill.clone()]);
    data["get"][ROOT]["skill"] = skill;
    fixture.write(data)?;
    let resources = discover(
        workspace.path(),
        &fixture.configuration(workspace.path()),
        &CallerContext::local(),
    )
    .await?;
    let held = resources
        .mcp_skill_catalog()
        .skills
        .first()
        .ok_or("baseline held entry")?;
    assert_eq!(held.entry.resources.entries().ok_or("manifest")?.len(), 512);
    assert_eq!(fixture.count("resources/read")?, 0);
    let error = resources
        .mcp_skill_file(
            held,
            SkillRead {
                reference: "assets/0.bin",
                max_bytes: 32767,
                cached_bytes: None,
            },
            &CancellationToken::new(),
        )
        .await
        .err()
        .ok_or("expected budget error")?;
    assert!(error.contains("budget"));
    assert_eq!(fixture.count("resources/read")?, 0);
    let binary = resources
        .mcp_skill_file(held, read("assets/0.bin", None), &CancellationToken::new())
        .await?;
    assert_eq!(binary.bytes, vec![0xff; 32768]);
    assert!(binary.text().is_err());
    assert_eq!(fixture.count("resources/read")?, 1);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(
        resources
            .mcp_skill_file(held, read("SKILL.md", None), &cancelled)
            .await
            .is_err()
    );
    assert_eq!(fixture.count("resources/read")?, 1);
    resources.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn another_uri_scheme_and_nested_data_keep_the_original_source_and_entry() -> TestResult {
    let fixture = Fixture::new("host-label", "EXACT_BODY")?;
    let workspace = tempfile::tempdir()?;
    let mut data = fixture.read()?;
    let uri = "github://owner/repository/review/SKILL.md";
    let support = "github://owner/repository/review/references/check.md";
    let nested = "github://owner/repository/review/nested/SKILL.md";
    let nested_body = "---\nname: nested\ndescription: needs separate consent\n---\nNESTED_DATA\n";
    let mut skill = entry(&fixture.body);
    skill["uri"] = uri.into();
    skill["resources"][0]["uri"] = uri.into();
    skill["resources"][1]["uri"] = support.into();
    skill["resources"]
        .as_array_mut()
        .ok_or("manifest")?
        .push(json!({
            "uri":nested,"digest":digest(nested_body.as_bytes()),"size":nested_body.len()
        }));
    data["get"] = json!({uri:{"resultType":"complete","skill":skill.clone(),"ttlMs":0,"cacheScope":"private"}});
    data["pages"]["first"]["skills"] = json!([skill]);
    data["files"] = json!({
        uri:{"text":fixture.body}, support:{"text":"support bytes\n"}, nested:{"text":nested_body}
    });
    fixture.write(data)?;
    let resources = discover(
        workspace.path(),
        &fixture.configuration(workspace.path()),
        &CallerContext::local(),
    )
    .await?;
    let held = resources
        .mcp_skill("host-label", uri, &CancellationToken::new())
        .await?;
    let root = resources
        .mcp_skill_file(&held, read("SKILL.md", None), &CancellationToken::new())
        .await?;
    assert_eq!(root.text()?, fixture.body);
    let nested_data = resources
        .mcp_skill_file(
            &held,
            read("nested/SKILL.md", None),
            &CancellationToken::new(),
        )
        .await?;
    assert_eq!(nested_data.text()?, nested_body);
    assert_eq!(resources.mcp_skill_catalog().skills.len(), 1);
    assert!(
        resources
            .mcp_skill("host-label", nested, &CancellationToken::new())
            .await
            .is_err(),
        "reading a nested file does not publish/activate a new skill"
    );
    for reference in [
        "../../other/file",
        "github://another/repository/review/SKILL.md",
    ] {
        assert!(
            resources
                .mcp_skill_file(&held, read(reference, None), &CancellationToken::new())
                .await
                .is_err()
        );
    }
    assert_eq!(fixture.count("resources/read")?, 2);
    assert_eq!(fixture.count("resources/directory/read")?, 0);
    resources.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn a_new_connection_configuration_cannot_reuse_an_old_binding_or_cached_bytes() -> TestResult
{
    let fixture = Fixture::new("source", "EXACT_BODY")?;
    let workspace = tempfile::tempdir()?;
    let mut config = fixture.configuration(workspace.path());
    let resources = discover(workspace.path(), &config, &CallerContext::local()).await?;
    let held = resources
        .mcp_skill_catalog()
        .skills
        .first()
        .ok_or("held skill")?
        .clone();
    resources.shutdown().await?;
    let transport = &mut config.servers.first_mut().ok_or("server")?.transport;
    if let McpTransport::Stdio { env, .. } = transport {
        env.insert("FIXTURE_BINDING_REVISION".into(), "second".into());
    }
    let resources = discover(workspace.path(), &config, &CallerContext::local()).await?;
    let current = resources
        .mcp_skill_catalog()
        .skills
        .first()
        .ok_or("current skill")?;
    assert_ne!(
        current.origin.configuration_digest,
        held.origin.configuration_digest
    );
    assert!(
        resources
            .mcp_skill_file(
                &held,
                read("SKILL.md", Some(fixture.body.as_bytes())),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(fixture.count("resources/read")?, 0);
    resources.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn unicode_name_and_percent_encoded_parent_survive_native_metadata_and_verified_reads()
-> TestResult {
    let fixture = Fixture::new("source", "UNICODE_VERIFIED_BODY")?;
    let workspace = tempfile::tempdir()?;
    let config = fixture.configuration(workspace.path());
    let uri = "skill://%E5%AE%A1%E9%98%85/SKILL.md";
    let support = "skill://%E5%AE%A1%E9%98%85/references/check.md";
    let body = fixture.body.replace("name: review", "name: 审阅");
    let mut skill = entry(&body);
    skill["uri"] = uri.into();
    skill["frontmatter"]["name"] = "审阅".into();
    skill["resources"][0]["uri"] = uri.into();
    skill["resources"][1]["uri"] = support.into();
    let mut data = fixture.read()?;
    data["pages"]["first"]["skills"] = json!([skill.clone()]);
    data["get"] =
        json!({uri:{"resultType":"complete","skill":skill,"ttlMs":0,"cacheScope":"private"}});
    data["files"] = json!({uri:{"text":body,"mimeType":"text/markdown"},support:{"text":"support bytes\n","mimeType":"text/markdown"}});
    fixture.write(data)?;
    let resources = discover(workspace.path(), &config, &CallerContext::local()).await?;
    assert!(resources.mcp_skill_catalog().excluded.is_empty());
    let held = resources
        .mcp_skill_catalog()
        .skills
        .first()
        .ok_or("unicode held entry")?;
    assert_eq!(held.entry.uri, uri);
    assert_eq!(held.entry.frontmatter["name"], "审阅");
    assert_eq!(fixture.count("resources/read")?, 0);
    let loaded = resources
        .mcp_skill_file(held, read("SKILL.md", None), &CancellationToken::new())
        .await?;
    assert_eq!(loaded.text()?, body);
    let reference = resources
        .mcp_skill_file(
            held,
            read("references/check.md", None),
            &CancellationToken::new(),
        )
        .await?;
    assert_eq!(reference.uri, support);
    assert_eq!(reference.text()?, "support bytes\n");
    assert_eq!(fixture.count("resources/read")?, 2);
    resources.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn an_offline_source_uses_only_held_verified_bytes_without_discovering_cached_skills()
-> TestResult {
    let fixture = Fixture::new("source", "DURABLE_ORIGINAL_BYTES")?;
    let workspace = tempfile::tempdir()?;
    let config = fixture.configuration(workspace.path());
    let resources = discover(workspace.path(), &config, &CallerContext::local()).await?;
    let held = resources
        .mcp_skill_catalog()
        .skills
        .first()
        .ok_or("held skill")?
        .clone();
    let loaded = resources
        .mcp_skill_file(&held, read("SKILL.md", None), &CancellationToken::new())
        .await?;
    resources.shutdown().await?;

    // The same configured process now fails initialization. No configuration
    // identity, source grant or held entry changes to manufacture recovery.
    std::fs::remove_file(&fixture.config)?;
    let offline = discover(workspace.path(), &config, &CallerContext::local()).await?;
    assert!(offline.mcp_skill_catalog().skills.is_empty());
    assert_eq!(offline.mcp_skill_catalog().excluded.len(), 1);
    assert!(
        offline
            .mcp_skill("source", ROOT, &CancellationToken::new())
            .await
            .is_err()
    );
    assert!(
        offline
            .mcp_skill_file(&held, read("SKILL.md", None), &CancellationToken::new())
            .await
            .is_err()
    );
    let cached = offline
        .mcp_skill_file(
            &held,
            read("SKILL.md", Some(&loaded.bytes)),
            &CancellationToken::new(),
        )
        .await?;
    assert_eq!(cached.bytes, loaded.bytes);
    assert_eq!(held.origin.server, "source");
    assert!(
        offline.mcp_skill_catalog().skills.is_empty(),
        "cache never becomes a new candidate"
    );
    assert_eq!(fixture.count("skills/list")?, 1);
    assert_eq!(fixture.count("resources/read")?, 1);

    let mut corrupt = loaded.bytes.clone();
    *corrupt.last_mut().ok_or("cached byte")? = b'!';
    assert!(
        offline
            .mcp_skill_file(
                &held,
                read("SKILL.md", Some(&corrupt)),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(
        offline
            .mcp_skill_file(
                &held,
                read("references/check.md", Some(&loaded.bytes)),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(
        offline
            .mcp_skill_file(&held, read("SKILL.md", Some(&loaded.bytes)), &cancelled)
            .await
            .is_err()
    );
    offline.shutdown().await?;

    let mut removed = config.clone();
    removed.servers.clear();
    let resources = discover(workspace.path(), &removed, &CallerContext::local()).await?;
    assert!(
        resources
            .mcp_skill_file(
                &held,
                read("SKILL.md", Some(&loaded.bytes)),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    resources.shutdown().await?;
    let mut revoked = config;
    revoked.skills.mcp_sources.clear();
    let resources = discover(workspace.path(), &revoked, &CallerContext::local()).await?;
    assert!(
        resources
            .mcp_skill_file(
                &held,
                read("SKILL.md", Some(&loaded.bytes)),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    resources.shutdown().await?;
    assert_eq!(fixture.count("resources/read")?, 1);
    Ok(())
}

#[tokio::test]
async fn a_live_resource_only_peer_can_close_after_a_read_without_invalidating_its_cache()
-> TestResult {
    let fixture = Fixture::new("source", "CACHE_AFTER_DISCONNECT")?;
    let workspace = tempfile::tempdir()?;
    let config = fixture.configuration(workspace.path());
    let mut data = fixture.read()?;
    data["closeAfter"] = "resources/read".into();
    fixture.write(data)?;
    let resources = discover(workspace.path(), &config, &CallerContext::local()).await?;
    let held = resources
        .mcp_skill_catalog()
        .skills
        .first()
        .ok_or("held skill")?
        .clone();
    let loaded = resources
        .mcp_skill_file(&held, read("SKILL.md", None), &CancellationToken::new())
        .await?;
    let cached = resources
        .mcp_skill_file(
            &held,
            read("SKILL.md", Some(&loaded.bytes)),
            &CancellationToken::new(),
        )
        .await?;
    assert_eq!(cached.bytes, loaded.bytes);
    assert_eq!(fixture.count("resources/read")?, 1);
    resources.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn an_unavailable_execution_source_is_not_silently_dropped_for_cached_skill_recovery()
-> TestResult {
    let fixture = Fixture::new("source", "ORIGINAL_BYTES")?;
    let workspace = tempfile::tempdir()?;
    let config = fixture.configuration(workspace.path());
    std::fs::remove_file(&fixture.config)?;
    let error = NativeResources::discover_for_caller(
        workspace.path(),
        ToolMode::Coding,
        &config,
        &CallerContext::local(),
        &CancellationToken::new(),
    )
    .await
    .err()
    .ok_or("unavailable execution source must reject discovery")?;
    assert!(!error.cleanup_unknown, "failed source process was joined");
    assert_eq!(fixture.count("resources/read")?, 0);
    Ok(())
}

#[test]
fn configuration_admission_rejects_relative_workspace_grants_without_restricting_mcp_removal()
-> TestResult {
    let mut config = bitrouter::config::Config::default();
    assert!(config.skills.mcp_sources.is_empty());
    config.skills.mcp_sources.insert(
        "missing".into(),
        McpSkillSourceGrant {
            workspaces: vec![PathBuf::from("relative")],
            local: true,
            ..Default::default()
        },
    );
    assert!(config.validate_router_config().is_err());
    let workspace = tempfile::tempdir()?;
    config
        .skills
        .mcp_sources
        .get_mut("missing")
        .ok_or("grant")?
        .workspaces = vec![workspace.path().into()];
    assert!(config.validate_router_config().is_ok());
    assert!(
        config.mcp_servers.is_empty(),
        "a saved source grant cannot create a missing MCP connection or prevent removing one"
    );
    assert!(
        serde_json::from_value::<bitrouter::config::skills::SkillsConfig>(json!({"unknown":true}))
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn native_loading_rechecks_connection_capabilities_and_rejects_undeclared_methods()
-> TestResult {
    let fixture = Fixture::new("source", "EXACT_BODY")?;
    let workspace = tempfile::tempdir()?;
    let config = fixture.configuration(workspace.path());
    let resources = discover(workspace.path(), &config, &CallerContext::local()).await?;
    let held = resources
        .mcp_skill_catalog()
        .skills
        .first()
        .ok_or("held skill")?
        .clone();
    resources.shutdown().await?;
    assert_eq!(fixture.count("skills/list")?, 1);
    for capabilities in [
        json!({}),
        json!({"resources":{}}),
        json!({"extensions":{"io.modelcontextprotocol/skills":{}}}),
    ] {
        let mut data = fixture.read()?;
        data["capabilities"] = capabilities;
        fixture.write(data)?;
        let resources = discover(workspace.path(), &config, &CallerContext::local()).await?;
        assert!(resources.mcp_skill_catalog().skills.is_empty());
        assert!(
            resources
                .mcp_skill_catalog()
                .excluded
                .iter()
                .any(|problem| problem
                    .reason
                    .contains("did not declare Resources and Skills"))
        );
        assert!(
            resources
                .mcp_skill("source", ROOT, &CancellationToken::new())
                .await
                .is_err()
        );
        assert!(
            resources
                .mcp_skill_file(
                    &held,
                    read("SKILL.md", Some(fixture.body.as_bytes())),
                    &CancellationToken::new()
                )
                .await
                .is_err()
        );
        resources.shutdown().await?;
    }
    assert_eq!(fixture.count("skills/list")?, 1);
    assert_eq!(fixture.count("skills/get")?, 0);
    assert_eq!(fixture.count("resources/read")?, 0);
    Ok(())
}

#[tokio::test]
async fn metadata_budget_failures_do_not_fetch_content_or_truncate_entries() -> TestResult {
    let fixture = Fixture::new("source", "EXACT_BODY")?;
    let workspace = tempfile::tempdir()?;
    let mut data = fixture.read()?;
    for bytes in [300 * 1024, 2 * 1024 * 1024] {
        let mut skill = entry(&fixture.body);
        skill["unknownMetadata"] = "x".repeat(bytes).into();
        data["pages"]["first"]["skills"] = json!([skill.clone()]);
        data["get"][ROOT]["skill"] = skill;
        fixture.write(data.clone())?;
        let result = discover(
            workspace.path(),
            &fixture.configuration(workspace.path()),
            &CallerContext::local(),
        )
        .await;
        if bytes == 300 * 1024 {
            let error = result.err().ok_or("expected inventory budget refusal")?;
            assert!(
                error
                    .to_string()
                    .contains("harness inventory exceeds 256 KiB")
            );
        } else {
            let resources = result?;
            assert!(resources.mcp_skill_catalog().skills.is_empty());
            assert!(
                resources
                    .mcp_skill_catalog()
                    .excluded
                    .iter()
                    .any(|problem| problem.reason.contains("2 MiB metadata budget"))
            );
            assert!(
                resources
                    .mcp_skill("source", ROOT, &CancellationToken::new())
                    .await
                    .is_err()
            );
            resources.shutdown().await?;
        }
    }
    assert_eq!(fixture.count("resources/read")?, 0);
    Ok(())
}

#[tokio::test]
async fn a_coding_host_does_not_require_tools_from_a_resource_only_skill_source() -> TestResult {
    let fixture = Fixture::new("source", "EXACT_BODY")?;
    let workspace = tempfile::tempdir()?;
    let resources = NativeResources::discover_for_caller(
        workspace.path(),
        ToolMode::Coding,
        &fixture.configuration(workspace.path()),
        &CallerContext::local(),
        &CancellationToken::new(),
    )
    .await?;
    assert_eq!(resources.mcp_skill_catalog().skills.len(), 1);
    assert_eq!(
        fixture.count("tools/list")?,
        0,
        "a Skills source need only declare Resources, not Tools"
    );
    assert_eq!(fixture.count("resources/read")?, 0);
    assert!(
        !resources
            .manifest()
            .tools
            .iter()
            .any(|tool| tool.name.starts_with("source__"))
    );
    resources.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn rootless_and_authority_uri_paths_resolve_support_as_files_without_rewriting_source_identity()
-> TestResult {
    for (uri, root) in [
        ("urn:review/SKILL.md", "urn:review"),
        ("urn:acme/review/SKILL.md", "urn:acme/review"),
        ("github:/owner/review/SKILL.md", "github:/owner/review"),
        ("skill:review/SKILL.md", "skill:review"),
        ("https:review/SKILL.md", "https:review"),
        (
            "https://Example.invalid/review/SKILL.md",
            "https://Example.invalid/review",
        ),
        ("urn:review/SKILL.md?revision=1#instructions", "urn:review"),
        (
            "skill://review/SKILL.md?revision=1#instructions",
            "skill://review",
        ),
    ] {
        let fixture = Fixture::new("host-label", "ORIGINAL_URI_BYTES")?;
        let workspace = tempfile::tempdir()?;
        let mut data = fixture.read()?;
        let support =
            format!("{root}/references/%E6%8C%87%E5%8D%97.md?revision=2&tag=../other#section");
        let mut skill = entry(&fixture.body);
        skill["uri"] = uri.into();
        skill["resources"][0]["uri"] = uri.into();
        skill["resources"][1]["uri"] = support.clone().into();
        data["get"] = json!({uri:{"resultType":"complete","skill":skill.clone(),"ttlMs":0,"cacheScope":"private"}});
        data["pages"]["first"]["skills"] = json!([skill]);
        data["files"] =
            json!({uri:{"text":fixture.body}, support.clone():{"text":"support bytes\n"}});
        fixture.write(data)?;
        let resources = discover(
            workspace.path(),
            &fixture.configuration(workspace.path()),
            &CallerContext::local(),
        )
        .await?;
        let held = resources
            .mcp_skill("host-label", uri, &CancellationToken::new())
            .await?;
        assert_eq!(held.entry.uri, uri);
        assert_eq!(fixture.count("resources/read")?, 0);
        let body = resources
            .mcp_skill_file(&held, read("SKILL.md", None), &CancellationToken::new())
            .await?;
        assert_eq!(body.uri, uri);
        assert_eq!(body.text()?, fixture.body);
        for reference in [
            "references/指南.md",
            "./references/指南.md",
            "references/../references/%E6%8C%87%E5%8D%97.md",
        ] {
            let file = resources
                .mcp_skill_file(&held, read(reference, None), &CancellationToken::new())
                .await?;
            assert_eq!(file.uri, support);
            assert_eq!(file.text()?, "support bytes\n");
        }
        let before = fixture.count("resources/read")?;
        for reference in [
            "../outside.md",
            "//another/review/SKILL.md",
            "%2e%2e/outside.md",
            "references/%2Foutside.md",
        ] {
            assert!(
                resources
                    .mcp_skill_file(&held, read(reference, None), &CancellationToken::new())
                    .await
                    .is_err()
            );
        }
        assert_eq!(fixture.count("resources/read")?, before);
        resources.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn ambiguous_filesystem_paths_require_an_exact_held_uri_without_guessing_or_reading()
-> TestResult {
    let fixture = Fixture::new("source", "ORIGINAL_ROOT")?;
    let workspace = tempfile::tempdir()?;
    let mut data = fixture.read()?;
    let mut skill = entry(&fixture.body);
    let alias = "skill://review/references/%63heck.md";
    let versioned = "skill://review/references/check.md?revision=old#section";
    skill["resources"]
        .as_array_mut()
        .ok_or("manifest")?
        .push(json!({
            "uri":alias,"digest":digest(b"support bytes\n"),"size":14
        }));
    data["pages"]["first"]["skills"] = json!([skill]);
    data["pages"]["first"]["skills"][0]["resources"]
        .as_array_mut()
        .ok_or("manifest")?
        .push(json!({"uri":versioned,"digest":digest(b"support bytes\n"),"size":14}));
    data["files"][alias] = json!({"text":"support bytes\n"});
    data["files"][versioned] = json!({"text":"support bytes\n"});
    fixture.write(data)?;
    let resources = discover(
        workspace.path(),
        &fixture.configuration(workspace.path()),
        &CallerContext::local(),
    )
    .await?;
    let held = resources
        .mcp_skill_catalog()
        .skills
        .first()
        .ok_or("held skill")?;
    let error = resources
        .mcp_skill_file(
            held,
            read("references/check.md", None),
            &CancellationToken::new(),
        )
        .await
        .err()
        .ok_or("ambiguous path was guessed")?;
    assert!(error.contains("multiple held URIs"));
    assert_eq!(fixture.count("resources/read")?, 0);
    let exact = resources
        .mcp_skill_file(held, read(SUPPORT, None), &CancellationToken::new())
        .await?;
    assert_eq!(exact.uri, SUPPORT);
    assert_eq!(exact.text()?, "support bytes\n");
    assert_eq!(fixture.count("resources/read")?, 1);
    let exact_version = resources
        .mcp_skill_file(held, read(versioned, None), &CancellationToken::new())
        .await?;
    assert_eq!(exact_version.uri, versioned);
    assert_eq!(exact_version.text()?, "support bytes\n");
    assert_eq!(fixture.count("resources/read")?, 2);
    let relative_version = resources
        .mcp_skill_file(
            held,
            read("references/check.md?revision=old#section", None),
            &CancellationToken::new(),
        )
        .await?;
    assert_eq!(relative_version.uri, versioned);
    assert_eq!(fixture.count("resources/read")?, 3);
    resources.shutdown().await?;
    Ok(())
}
