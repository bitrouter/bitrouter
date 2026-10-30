//! Runtime requests must recheck authority after waiting on the real MCP owner.
use super::*;
use crate::storage::checkpoint::sha256;
use crate::storage::skills::SkillSource;
use std::path::Path;

mod local;

const SKILL_URI: &str = "skill://fixture/runtime-skill/SKILL.md";
const FILE_URI: &str = "skill://fixture/runtime-skill/guide.txt";
const INSTRUCTIONS: &str =
    "---\nname: runtime-skill\ndescription: Runtime authority fixture\n---\nUse the bound guide.\n";
const GUIDE: &str = "Only read while the session authority remains valid.\n";

fn entry() -> Value {
    json!({
        "uri": SKILL_URI,
        "frontmatter": {"name":"runtime-skill","description":"Runtime authority fixture"},
        "resources": [
            {"uri":SKILL_URI,"size":INSTRUCTIONS.len(),"digest":format!("sha256:{}", sha256(INSTRUCTIONS.as_bytes()))},
            {"uri":FILE_URI,"size":GUIDE.len(),"digest":format!("sha256:{}", sha256(GUIDE.as_bytes()))}
        ]
    })
}

#[derive(Clone, Default)]
struct RuntimeWire {
    entered: Arc<Notify>,
    release: CancellationToken,
    stop: CancellationToken,
    gets: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
    fail_runtime: Arc<std::sync::atomic::AtomicBool>,
}

fn unavailable(id: &Value) -> Response {
    Json(json!({
        "jsonrpc":"2.0", "id":id,
        "error":{"code":-32000,"message":"fixture skill resource is unavailable"}
    }))
    .into_response()
}

async fn runtime_rpc(State(wire): State<RuntimeWire>, Json(request): Json<Value>) -> Response {
    let Some(id) = request.get("id") else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match request["method"].as_str() {
        Some("initialize") => json!({
            "protocolVersion":"2025-11-25",
            "capabilities":{"resources":{},"extensions":{"io.modelcontextprotocol/skills":{}}},
            "serverInfo":{"name":"runtime-skills","version":"1"}
        }),
        Some("skills/list") => json!({
            "skills":[entry()],"resultType":"complete","ttlMs":0,"cacheScope":"private"
        }),
        Some("skills/get") => {
            let prior = wire.gets.fetch_add(1, Ordering::SeqCst);
            if wire.fail_runtime.load(Ordering::SeqCst) {
                return unavailable(id);
            }
            if request["params"]["uri"] != SKILL_URI {
                return StatusCode::BAD_REQUEST.into_response();
            }
            if prior == 0 {
                // This real request holds HarnessResources' MCP mutex. The
                // test never obtains or substitutes that mutex directly.
                wire.entered.notify_one();
                tokio::select! {
                    () = wire.release.cancelled() => {},
                    () = wire.stop.cancelled() => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
                }
            }
            json!({"skill":entry(),"resultType":"complete","ttlMs":0,"cacheScope":"private"})
        }
        Some("resources/read") => {
            wire.reads.fetch_add(1, Ordering::SeqCst);
            if wire.fail_runtime.load(Ordering::SeqCst) {
                return unavailable(id);
            }
            let text = match request["params"]["uri"].as_str() {
                Some(SKILL_URI) => INSTRUCTIONS,
                Some(FILE_URI) => GUIDE,
                _ => return StatusCode::BAD_REQUEST.into_response(),
            };
            json!({"contents":[{"uri":request["params"]["uri"],"text":text}]})
        }
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}

struct RuntimeEndpoint {
    url: String,
    wire: RuntimeWire,
    task: Option<AbortOnDropHandle<std::io::Result<()>>>,
}

impl RuntimeEndpoint {
    async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let wire = RuntimeWire::default();
        let router = Router::new()
            .route("/mcp", post(runtime_rpc))
            .with_state(wire.clone());
        let stop = wire.stop.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
        }));
        Ok(Self {
            url,
            wire,
            task: Some(task),
        })
    }

    async fn finish(&mut self) -> Result<()> {
        self.wire.release.cancel();
        self.wire.stop.cancel();
        if let Some(task) = self.task.take() {
            tokio::time::timeout(TIMEOUT, task).await???;
        }
        Ok(())
    }
}

impl Drop for RuntimeEndpoint {
    fn drop(&mut self) {
        self.wire.release.cancel();
        self.wire.stop.cancel();
    }
}

async fn open_session(
    fixture: &Fixture,
    provider: &Provider,
    headers: HeaderMap,
    workspace: &Path,
    config: &HarnessConfig,
    legacy: bool,
) -> Result<NativeSession> {
    let app = host_app(fixture, &provider.url, Arc::new(HostSpies::default()))?;
    if legacy {
        Ok(NativeSession::open_cancellable(
            Arc::new(app),
            CallerContext::local(),
            Arc::new(crate::managed_store::DatabaseNativeStore::new(
                fixture.db.clone(),
                "runtime-skills".into(),
            )?),
            workspace,
            ToolMode::ReadOnly,
            config,
            &CancellationToken::new(),
        )
        .await?)
    } else {
        Ok(NativeSession::open_authenticated(
            Arc::new(app.with_tenancy(fixture.service.clone())),
            headers,
            "runtime-skills",
            workspace,
            ToolMode::ReadOnly,
            config,
        )
        .await?)
    }
}

fn unauthorized<T>(result: std::result::Result<T, CoreError>) -> Result<()> {
    let error = result
        .err()
        .context("revoked runtime skill work succeeded")?;
    ensure!(
        error.code == bitrouter_ai::core::protocol::ErrorCode::UnauthorizedScope,
        "authority rejection was lost: {error}"
    );
    ensure!(error.commit_status == bitrouter_ai::core::protocol::CommitStatus::NotCommitted);
    Ok(())
}

async fn close_session(fixture: &Fixture, session: NativeSession) -> Result<()> {
    session.close().await?;
    ensure!(fixture.service.host_slots_for_test() == 1);
    ensure!(
        fixture.service.cleanup.disposition()
            != crate::tenancy::transaction::Disposition::Ambiguous
    );
    let row = fixture
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT payload FROM bro_managed_sessions",
        ))
        .await?
        .context("closed native journal")?;
    let saved: Value = serde_json::from_str(&row.try_get::<String>("", "payload")?)?;
    let journal = match saved.get("journal") {
        Some(journal) => journal,
        None => &saved,
    };
    ensure!(
        journal["released"] == true,
        "resource owner was not released"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_mcp_skills_recheck_after_connection_mutex_waits() -> Result<()> {
    for change in ["allowed", "revoke", "scope", "legacy"] {
        let provider = Provider::start(false).await?;
        let mut endpoint = RuntimeEndpoint::start().await?;
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let (fixture, route) = file_fixture(&provider, directory.path()).await?;
        let checked = tokio::time::timeout(Duration::from_secs(20), async {
            let (headers, key) = ready(&fixture, &route).await?;
            let legacy = change == "legacy";
            let config = HarnessConfig {
                servers: vec![McpServerConfig::with_defaults(
                    "runtime",
                    McpTransport::Http {
                        url: endpoint.url.clone(),
                        headers: Default::default(),
                    },
                )],
                skills: crate::config::skills::SkillsConfig {
                    mcp_sources: [(
                        "runtime".into(),
                        crate::config::skills::McpSkillSourceGrant {
                            workspaces: vec![workspace.clone()],
                            local: legacy,
                            api_key_ids: vec![key.credential_id.to_string()],
                            ..Default::default()
                        },
                    )]
                    .into(),
                },
                ..HarnessConfig::default()
            };
            let session =
                open_session(&fixture, &provider, headers, &workspace, &config, legacy).await?;
            let port = session.harness_port_for_test();
            let catalog = port
                .skill_catalog()
                .await?
                .context("native skill catalog")?;
            ensure!(catalog.mcp.excluded.is_empty() && catalog.mcp.skills.len() == 1);
            let held = catalog
                .mcp
                .skills
                .into_iter()
                .next()
                .context("bound MCP skill")?;
            let source = SkillSource::Mcp(held.clone());
            let first_port = port.clone();
            let first = AbortOnDropHandle::new(tokio::spawn(async move {
                first_port
                    .resolve_mcp_skill("runtime", SKILL_URI, &CancellationToken::new())
                    .await
            }));
            endpoint.wire.entered.notified().await;

            let cancel = CancellationToken::new();
            let mut lookup = Box::pin(port.resolve_mcp_skill("runtime", SKILL_URI, &cancel));
            let mut instructions = Box::pin(port.skill_instructions(&source, 4096, None, &cancel));
            let mut file = Box::pin(port.skill_file(&source, FILE_URI, 4096, None, &cancel));
            // Poll each real method through its admission check into the mutex
            // queue before revoking; task creation or a sleep is not a barrier.
            ensure!(futures::poll!(lookup.as_mut()).is_pending());
            ensure!(futures::poll!(instructions.as_mut()).is_pending());
            ensure!(futures::poll!(file.as_mut()).is_pending());
            ensure!(endpoint.wire.gets.load(Ordering::SeqCst) == 1);
            ensure!(endpoint.wire.reads.load(Ordering::SeqCst) == 0);
            if change != "allowed" {
                change_authority(
                    &fixture,
                    &key,
                    &route,
                    if legacy { "revoke" } else { change },
                )
                .await?;
            }
            endpoint.wire.release.cancel();
            let first = first.await?;
            let (lookup, instructions, file) = tokio::join!(lookup, instructions, file);
            if matches!(change, "revoke" | "scope") {
                unauthorized(lookup)?;
                unauthorized(instructions)?;
                unauthorized(file)?;
                // Its request was admitted before revoke, so its one wire
                // request is permitted. Delivery still crosses a fenced poll.
                unauthorized(first)?;
                ensure!(
                    endpoint.wire.gets.load(Ordering::SeqCst) == 1,
                    "{change}: queued skills/get escaped"
                );
                ensure!(
                    endpoint.wire.reads.load(Ordering::SeqCst) == 0,
                    "{change}: queued resources/read escaped"
                );
            } else {
                ensure!(first? == held && lookup? == held);
                ensure!(instructions? == INSTRUCTIONS.as_bytes());
                ensure!(file? == GUIDE.as_bytes());
                ensure!(endpoint.wire.gets.load(Ordering::SeqCst) == 2);
                ensure!(endpoint.wire.reads.load(Ordering::SeqCst) == 2);

                // Transport/resource errors from admitted work retain their
                // ordinary error category; they are not authority failures.
                endpoint.wire.fail_runtime.store(true, Ordering::SeqCst);
                let failures = [
                    port.resolve_mcp_skill("runtime", SKILL_URI, &cancel)
                        .await
                        .map(|_| ()),
                    port.skill_instructions(&source, 4096, None, &cancel)
                        .await
                        .map(|_| ()),
                    port.skill_file(&source, FILE_URI, 4096, None, &cancel)
                        .await
                        .map(|_| ()),
                ];
                for failure in failures {
                    let error = failure.err().context("server failure was hidden")?;
                    ensure!(
                        error.code == bitrouter_ai::core::protocol::ErrorCode::ArtifactUnavailable
                    );
                    ensure!(
                        error.commit_status
                            == bitrouter_ai::core::protocol::CommitStatus::NotCommitted
                    );
                }
                ensure!(endpoint.wire.gets.load(Ordering::SeqCst) == 3);
                ensure!(endpoint.wire.reads.load(Ordering::SeqCst) == 4);
            }
            drop(port);
            close_session(&fixture, session).await?;
            ensure!(provider.calls() == 0);
            Ok(())
        })
        .await
        .context("runtime MCP authority deadline")
        .and_then(|outcome| outcome);
        endpoint.wire.release.cancel();
        let endpoint_closed = endpoint.finish().await;
        let cleaned = finish(&fixture, provider, checked).await;
        endpoint_closed?;
        cleaned.with_context(|| format!("runtime MCP case: {change}"))?;
    }
    Ok(())
}
