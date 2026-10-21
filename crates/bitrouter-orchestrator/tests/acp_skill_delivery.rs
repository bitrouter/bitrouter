//! Carrier contract evidence using independent Cargo-built upstream fixtures.
#![cfg(feature = "acp")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use agent_client_protocol::schema::v1::StopReason;
use bitrouter_orchestrator::acp::client::{AcpClient, ClientOptions};
use bitrouter_orchestrator::acp::skills::{self, Adapter, Capabilities, Instructions, Target};
use bitrouter_orchestrator::acp::up::AgentProcess;
use bitrouter_orchestrator::core::checkpoint::sha256;
use bitrouter_orchestrator::core::skills::SkillSource;
use bitrouter_orchestrator::harness::skills::library::{LibraryScope, ManagedSkill};
use bitrouter_sdk::caller::CallerContext;
use serde_json::{Value, json};

#[path = "support/example_binary.rs"]
mod example_binary;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Peer {
    root: tempfile::TempDir,
    journal: PathBuf,
    client: AcpClient,
    reaped: futures::channel::oneshot::Receiver<()>,
    session_id: String,
    capabilities: Capabilities,
}

impl Peer {
    async fn open(profile: &str, options: &[&str]) -> TestResult<Self> {
        static CODEX: OnceLock<Result<PathBuf, String>> = OnceLock::new();
        static CLAUDE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
        let binary = match profile {
            "codex-acp" => CODEX.get_or_init(|| {
                example_binary::build("bitrouter-orchestrator", "acp_codex_skill_fixture")
            }),
            _ => CLAUDE.get_or_init(|| {
                example_binary::build("bitrouter-orchestrator", "acp_claude_skill_fixture")
            }),
        }
        .as_ref()
        .map_err(|error| std::io::Error::other(error.clone()))?;
        let root = tempfile::tempdir()?;
        let journal = root.path().join("wire.jsonl");
        let mut args = vec!["--journal".into(), journal.to_string_lossy().into_owned()];
        args.extend(options.iter().map(|option| (*option).to_string()));
        let mut process = AgentProcess::new(binary.to_string_lossy(), args, HashMap::new());
        let reaped = process.reaped();
        let client = AcpClient::connect(process, ClientOptions::default()).await?;
        let capabilities = Capabilities::negotiated(Some(profile), client.agent_capabilities());
        let session_id = client
            .new_session(root.path().to_path_buf(), vec![])
            .await?
            .acp_session_id;
        Ok(Self {
            root,
            journal,
            client,
            reaped,
            session_id,
            capabilities,
        })
    }

    fn source(&self, body: &str) -> SkillSource {
        let caller = CallerContext::local();
        SkillSource::Managed(ManagedSkill {
            scope: LibraryScope::Project,
            library: self.root.path().join(".bitrouter/skills"),
            workspace: self.root.path().to_path_buf(),
            api_key_id: caller.api_key_id().into(),
            user_id: caller.user_id().into(),
            local: true,
            installation_id: "original-installation".into(),
            catalog_revision: 1,
            name: "review".into(),
            description: "review workflow".into(),
            version: sha256(b"whole bundle original"),
            receipt: json!({"provenance":"local_fixture","support":"references/check.md"}),
            instruction_sha256: sha256(body.as_bytes()),
            instruction_bytes: body.len() as u64,
        })
    }

    fn prepare(&self, body: &str) -> anyhow::Result<skills::Prepared> {
        skills::prepare(
            &self.capabilities,
            Target {
                session_id: &self.session_id,
                turn_id: "original-input-1",
                workspace: self.root.path(),
                caller: &CallerContext::local(),
                context_limit: 128 * 1024,
            },
            "perform review",
            vec![Instructions {
                source: self.source(body),
                bytes: body.as_bytes().into(),
            }],
        )
    }

    fn records(&self) -> TestResult<Vec<Value>> {
        let bytes = std::fs::read(&self.journal)?;
        let end = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        Ok(bytes[..end]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(serde_json::from_slice)
            .collect::<Result<_, _>>()?)
    }

    async fn shutdown(self) -> TestResult {
        self.client.shutdown().await?;
        tokio::time::timeout(Duration::from_secs(3), self.reaped).await??;
        Ok(())
    }
}

const BODY: &str =
    "---\nname: review\ndescription: review workflow\n---\nExact 原文\nquotes: \"kept\"\n";

#[tokio::test]
async fn codex_text_reaches_its_independent_model_input_with_exact_version_and_path() -> TestResult
{
    let peer = Peer::open("codex-acp", &[]).await?;
    assert_eq!(peer.capabilities.adapter, Some(Adapter::CodexText));
    assert!(
        !peer.capabilities.native_activation
            && !peer.capabilities.verified_bundle_access
            && !peer.capabilities.mcp_origin
    );
    let prepared = peer.prepare(BODY)?;
    let wire = serde_json::to_value(&prepared.request)?;
    assert_eq!(wire["prompt"][0]["type"], "text");
    let frame = wire["prompt"][0]["text"].as_str().ok_or("frame")?;
    assert_eq!(
        frame
            .split_once("\nSKILL.md:\n")
            .ok_or("root boundary")?
            .1
            .as_bytes(),
        BODY.as_bytes()
    );
    let binding = frame
        .split_once("Exact binding:\n")
        .ok_or("binding")?
        .1
        .split_once("\nSKILL.md:")
        .ok_or("binding end")?
        .0;
    let binding: Value = serde_json::from_str(binding)?;
    assert_eq!(binding["turn_id"], "original-input-1");
    assert!(
        binding["bundle_path"]
            .as_str()
            .ok_or("bundle path")?
            .ends_with(&sha256(b"whole bundle original"))
    );
    assert_eq!(
        prepared.receipt.request_sha256,
        sha256(&serde_json::to_vec(&prepared.request)?)
    );
    assert_eq!(
        peer.client
            .prompt_typed(prepared.request)
            .await?
            .stop_reason,
        StopReason::EndTurn
    );
    let records = peer.records()?;
    let model = records
        .iter()
        .find(|row| row["event"] == "codex_model_input")
        .ok_or("Codex model boundary")?;
    assert_eq!(model["inputs"][0]["text"], wire["prompt"][0]["text"]);
    assert_eq!(model["inputs"][1]["text"], "perform review");
    peer.shutdown().await
}

#[tokio::test]
async fn claude_embedded_context_reaches_its_independent_user_message_without_promotion()
-> TestResult {
    let peer = Peer::open("claude-acp", &[]).await?;
    assert_eq!(
        peer.capabilities.adapter,
        Some(Adapter::ClaudeEmbeddedContext)
    );
    let prepared = peer.prepare(BODY)?;
    let wire = serde_json::to_value(&prepared.request)?;
    assert_eq!(wire["prompt"][0]["type"], "resource");
    assert_eq!(wire["prompt"][0]["resource"]["mimeType"], "text/markdown");
    let text = wire["prompt"][0]["resource"]["text"]
        .as_str()
        .ok_or("embedded text")?;
    assert_eq!(
        text.split_once("\nSKILL.md:\n").ok_or("root boundary")?.1,
        BODY
    );
    let uri = wire["prompt"][0]["resource"]["uri"]
        .as_str()
        .ok_or("resource URI")?;
    peer.client.prompt_typed(prepared.request).await?;
    let records = peer.records()?;
    let model = records
        .iter()
        .find(|row| row["event"] == "claude_user_message")
        .ok_or("Claude user message")?;
    assert_eq!(model["message"]["role"], "user");
    assert_eq!(model["message"]["content"][1]["text"], "perform review");
    assert_eq!(
        model["message"]["content"][2]["text"],
        format!("\n<context ref=\"{uri}\">\n{text}\n</context>")
    );
    peer.shutdown().await
}

#[tokio::test]
async fn unadvertised_claude_context_is_unsupported_without_any_prompt() -> TestResult {
    let peer = Peer::open("claude-acp", &["--no-context"]).await?;
    assert!(!peer.capabilities.prompt_instructions);
    assert!(peer.prepare(BODY).is_err());
    assert!(
        !peer
            .records()?
            .iter()
            .any(|row| row["method"] == "session/prompt")
    );
    peer.shutdown().await
}

#[tokio::test]
async fn corrupt_or_foreign_bindings_and_small_context_never_send_a_prompt() -> TestResult {
    let peer = Peer::open("codex-acp", &[]).await?;
    let original = peer.source(BODY);
    for case in ["bytes", "workspace", "budget"] {
        let mut source = original.clone();
        let mut bytes = BODY.as_bytes().to_vec();
        let mut limit = 128 * 1024;
        match case {
            "bytes" => bytes.push(b'!'),
            "workspace" => {
                if let SkillSource::Managed(binding) = &mut source {
                    binding.workspace = peer.root.path().join("other");
                }
            }
            _ => limit = 8,
        }
        assert!(
            skills::prepare(
                &peer.capabilities,
                Target {
                    session_id: &peer.session_id,
                    turn_id: "bound",
                    workspace: peer.root.path(),
                    caller: &CallerContext::local(),
                    context_limit: limit,
                },
                "task",
                vec![Instructions { source, bytes }]
            )
            .is_err()
        );
    }
    assert!(
        !peer
            .records()?
            .iter()
            .any(|row| row["method"] == "session/prompt")
    );
    peer.shutdown().await
}

#[tokio::test]
async fn both_independent_peers_settle_the_original_request_after_cancel() -> TestResult {
    for profile in ["codex-acp", "claude-acp"] {
        let peer = Peer::open(profile, &["--hold"]).await?;
        let request = peer.prepare(BODY)?.request;
        let client = peer.client.clone();
        let pending = tokio::spawn(async move { client.prompt_typed(request).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if peer.records()?.iter().any(|row| {
                    row["event"] == "codex_model_input" || row["event"] == "claude_user_message"
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await??;
        peer.client.cancel(&peer.session_id).await?;
        let response = tokio::time::timeout(Duration::from_secs(5), pending).await???;
        assert_eq!(response.stop_reason, StopReason::Cancelled);
        assert_eq!(
            peer.records()?
                .iter()
                .filter(|row| row["method"] == "session/prompt")
                .count(),
            1
        );
        peer.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn valid_mcp_entries_remain_unsupported_for_both_carriers_without_laundering() -> TestResult {
    use bitrouter_orchestrator::harness::skills::mcp::{HeldMcpSkill, SkillOrigin, SkillScope};
    use bitrouter_sdk::mcp::skills::{SkillEntry, SkillResources};
    fn canonical(value: Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .collect::<std::collections::BTreeMap<_, _>>()
                    .into_iter()
                    .map(|(key, value)| (key, canonical(value)))
                    .collect(),
            ),
            Value::Array(values) => Value::Array(values.into_iter().map(canonical).collect()),
            value => value,
        }
    }
    for profile in ["codex-acp", "claude-acp"] {
        let peer = Peer::open(profile, &[]).await?;
        for changed in [false, true] {
            let uri = "skill://review/SKILL.md";
            let mut entry: SkillEntry = serde_json::from_value(
                json!({"uri":uri,"frontmatter":{"name":"review","description":"review workflow"},
                "resources":[{"uri":uri,"digest":format!("sha256:{}",sha256(BODY.as_bytes())),"size":BODY.len()}]}),
            )?;
            if changed && let SkillResources::Enumerated(files) = &mut entry.resources {
                files.push(serde_json::from_value(json!({"uri":"skill://review/support.md","digest":format!("sha256:{}",sha256(b"new support")),"size":11}))?);
            }
            if let SkillResources::Enumerated(files) = &mut entry.resources {
                files.sort_by(|left, right| left.uri.cmp(&right.uri));
            }
            let digest = sha256(&serde_json::to_vec(&canonical(serde_json::to_value(
                &entry,
            )?))?);
            let caller = CallerContext::local();
            let held = HeldMcpSkill {
                origin: SkillOrigin {
                    server: "operator-approved-source".into(),
                    configuration_digest: sha256(b"configuration"),
                    scope: SkillScope {
                        workspace: peer.root.path().into(),
                        api_key_id: caller.api_key_id().into(),
                        user_id: caller.user_id().into(),
                        local: true,
                    },
                },
                entry,
                entry_digest: digest,
            };
            held.validate().map_err(std::io::Error::other)?;
            let error = skills::prepare(
                &peer.capabilities,
                Target {
                    session_id: &peer.session_id,
                    turn_id: "mcp-must-not-become-local",
                    workspace: peer.root.path(),
                    caller: &caller,
                    context_limit: 128 * 1024,
                },
                "task",
                vec![Instructions {
                    source: SkillSource::Mcp(held),
                    bytes: BODY.as_bytes().into(),
                }],
            )
            .err()
            .ok_or("MCP origin was accepted")?;
            assert!(error.to_string().contains("MCP-origin"));
        }
        assert!(
            !peer
                .records()?
                .iter()
                .any(|row| row["method"] == "session/prompt")
        );
        peer.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn native_slash_controls_keep_their_original_single_text_shape() -> TestResult {
    for profile in ["codex-acp", "claude-acp"] {
        let peer = Peer::open(profile, &[]).await?;
        let error = skills::prepare(
            &peer.capabilities,
            Target {
                session_id: &peer.session_id,
                turn_id: "native-control",
                workspace: peer.root.path(),
                caller: &CallerContext::local(),
                context_limit: 128 * 1024,
            },
            "/compact",
            vec![Instructions {
                source: peer.source(BODY),
                bytes: BODY.as_bytes().into(),
            }],
        )
        .err()
        .ok_or("native control was rewritten")?;
        assert!(error.to_string().contains("native slash-command"));
        peer.client.prompt(&peer.session_id, "/compact").await?;
        let rows = peer.records()?;
        let sent = rows
            .iter()
            .find(|row| row["method"] == "session/prompt")
            .ok_or("native wire")?;
        assert_eq!(
            sent["params"]["prompt"],
            json!([{"type":"text","text":"/compact"}])
        );
        peer.shutdown().await?;
    }
    Ok(())
}
