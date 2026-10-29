//! Opt-in live provider listing -> production sync CLI -> exact published bytes
//! -> official host -> real gateway HTTP request. Inference stays test-owned.

use super::*;
use bitrouter_registry::document::{Document, ReaderPolicy, SPEC_REVISION};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;

struct Published {
    path: PathBuf,
    downloads: AtomicUsize,
    requested: tokio::sync::Notify,
    release: tokio::sync::Notify,
    first: AtomicBool,
    served_hashes: Mutex<Vec<String>>,
    calls: Mutex<Vec<Value>>,
}

async fn published(State(state): State<Arc<Published>>) -> impl IntoResponse {
    state.downloads.fetch_add(1, Ordering::SeqCst);
    state.requested.notify_one();
    if state.first.swap(false, Ordering::SeqCst) {
        state.release.notified().await;
    }
    match fs::read(&state.path) {
        Ok(bytes) => {
            state
                .served_hashes
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(hex::encode(Sha256::digest(&bytes)));
            (
                StatusCode::OK,
                [("content-type", "application/json")],
                bytes,
            )
        }
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [("content-type", "application/json")],
            b"{}".to_vec(),
        ),
    }
}

async fn inference(
    State(state): State<Arc<Published>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    // Fixture for the same chat-completions contract consumed by the host.
    // https://platform.openai.com/docs/api-reference/chat/create
    let authenticated = headers.get("authorization").and_then(|v| v.to_str().ok())
        == Some("Bearer fixture-inference-key");
    state
        .calls
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(json!({"model":body["model"],"authenticated":authenticated}));
    Json(
        json!({"id":"live-sync-fixture","object":"chat.completion","model":body["model"],"choices":[{"index":0,"message":{"role":"assistant","content":body["model"]},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
    )
}

fn source_tree(root: &Path) -> Result<()> {
    for directory in ["registry/models", "registry/providers"] {
        fs::create_dir_all(root.join(directory))?;
    }
    // Keep the production identity authority and full provider-list URL intact.
    // https://openrouter.ai/docs/api/api-reference/models/list-all-models-and-their-properties
    let url = bitrouter_registry::openrouter::URL;
    for (path, value) in [
        (
            "registry/registry.yaml",
            json!({"version":1,"spec_revision":SPEC_REVISION,"as_of":(chrono::Utc::now()-chrono::Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs,true),"future":{"closed_loop":"preserved"},"identity_source":{"kind":"openrouter","request":{"url":url,"auth":{"kind":"none"}}}}),
        ),
        (
            "registry/providers/openrouter.yaml",
            json!({"name":"openrouter","status":"active","billing":"usage","api_base":"https://openrouter.ai/api/v1","auth":{"kind":"bearer","env":"OPENROUTER_API_KEY"},"models":[],"auto_sync":{"feeds":[{"id":"catalog","kind":"openrouter_models","writes":["models","metadata"],"request":{"url":url,"auth":{"kind":"none"}},"pagination":{"kind":"none"},"inventory":{"mode":"snapshot","scope":"public","complete":true,"allow_empty":false}}]}}),
        ),
        ("registry/model-identities.yaml", json!([])),
        (
            "registry/model-discovery.yaml",
            json!({"families":[],"retired_models":[]}),
        ),
    ] {
        fs::write(root.join(path), serde_json::to_vec_pretty(&value)?)?;
    }
    Ok(())
}

fn evidence(directory: Option<&Path>, name: &str, bytes: &[u8]) -> Result<()> {
    if let Some(directory) = directory {
        fs::create_dir_all(directory)?;
        fs::write(directory.join(name), bytes)?;
    }
    Ok(())
}

async fn synchronize(root: &Path, output: Option<&Path>) -> Result<Value> {
    let binary = std::env::var_os("BITROUTER_TEST_DIST_HELPER_BINARY")
        .context("build dist-helper and set BITROUTER_TEST_DIST_HELPER_BINARY")?;
    let binary = PathBuf::from(binary).canonicalize()?;
    let mut command = tokio::process::Command::new(binary);
    command
        .arg("--root")
        .arg(root)
        .args(["registry", "sync", "--write", "--report"])
        .arg(root.join("target/sync-report.json"))
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    // Retain standard network/trust configuration, never account credentials.
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let result = tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .context("sync CLI exceeded its experiment deadline")??;
    evidence(output, "sync-stdout.log", &result.stdout)?;
    evidence(output, "sync-stderr.log", &result.stderr)?;
    if let Ok(bytes) = fs::read(root.join("target/sync-report.json")) {
        evidence(output, "sync-report.json", &bytes)?;
    }
    ensure!(
        result.status.success(),
        "real sync CLI failed: {}\n{}",
        String::from_utf8_lossy(&result.stderr),
        String::from_utf8_lossy(&result.stdout)
    );
    Ok(serde_json::from_slice(&fs::read(
        root.join("target/sync-report.json"),
    )?)?)
}

#[tokio::test]
#[ignore = "requires the public OpenRouter catalog and a prebuilt dist-helper binary"]
async fn provider_sync_artifact_is_consumed_by_official_host_and_routes_requests() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(180), async {
        let directory = tempfile::Builder::new().prefix("bro-sync-").tempdir_in("/tmp")?;
        let root = directory.path().join("producer");
        source_tree(&root)?;
        let output = std::env::var_os("BITROUTER_TEST_REGISTRY_EVIDENCE_DIR").map(PathBuf::from);
        let output = output.as_deref();
        let report = synchronize(&root, output).await?;
        ensure!(report["candidate_ready"] == true && report["audit"]["outcome"] == "passed");
        ensure!(report["feeds"][0]["provider"] == "openrouter" && report["feeds"][0]["outcome"] == "updated");
        let path = root.join("dist/registry/v1/registry.json");
        let bytes = fs::read(&path)?;
        let hash = hex::encode(Sha256::digest(&bytes));
        ensure!(report["audit"]["output_sha256"] == hash);
        ensure!(fs::read(root.join("apps/bitrouter/registry-dist/registry.json"))? == bytes);
        ensure!(fs::read(root.join(format!("dist/registry/v1/{SPEC_REVISION}/registry.json")))? == bytes);
        let document = Document::parse(&bytes, &ReaderPolicy::default())?;
        ensure!(document.models().is_empty(), "provider inventory unexpectedly became recommendations");
        let provider = document.providers().iter().find(|p| p["name"] == "openrouter").context("synced provider missing")?;
        let models = provider["models"].as_array().context("synced inventory")?;
        ensure!(!models.is_empty(), "real provider sync produced no offers");
        let provider_source = fs::read(root.join("registry/providers/openrouter.yaml"))?;
        let written = bitrouter_registry::source::parse(std::str::from_utf8(&provider_source)?)?;
        let source_models = written["models"].as_array().context("written provider inventory")?;
        let source_ids = source_models.iter().filter_map(|m| m["provider_model_id"].as_str()).collect::<BTreeSet<_>>();
        let published_ids = models.iter().filter_map(|m| m["provider_model_id"].as_str()).collect::<BTreeSet<_>>();
        ensure!(source_models.len() == models.len() && source_ids.len() == models.len() && source_ids == published_ids, "sync did not publish the same inventory to source and artifact");
        evidence(output, "provider-source.yaml", &provider_source)?;
        let model = models.iter().find(|m| m["status"] == "active" && m["interfaces"].as_array().is_some_and(|interfaces| interfaces.iter().any(|i| i["protocol"] == "chat_completions" && i["operation"] == "generation" && i["input_modalities"].as_array().is_some_and(|a|a.iter().any(|v|v=="text")) && i["output_modalities"].as_array().is_some_and(|a|a.iter().any(|v|v=="text"))))).context("live provider has no usable text interface")?;
        let native = model["provider_model_id"].as_str().context("native ID")?.to_owned();
        let canonical = model["canonical_id"].as_str().context("canonical ID")?.to_owned();
        evidence(output, "registry.json", &bytes)?;
        evidence(output, "identity-response.json", &fs::read(root.join("target/registry-identity-response.json"))?)?;
        let state = Arc::new(Published { path, downloads: AtomicUsize::new(0), requested: tokio::sync::Notify::new(), release: tokio::sync::Notify::new(), first: AtomicBool::new(true), served_hashes: Mutex::new(Vec::new()), calls: Mutex::new(Vec::new()) });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let stop = CancellationToken::new();
        let _guard = stop.clone().drop_guard();
        let router = Router::new().route("/registry", get(published)).route("/v1/chat/completions", post(inference)).with_state(state.clone());
        let server = tokio::spawn(axum::serve(listener, router).with_graceful_shutdown(stop.clone().cancelled_owned()).into_future());
        let source = format!("{origin}/registry");
        let mut host = Host::new(directory.path().join("consumer"), "openrouter")?;
        host.spawn(&source, true, true, &origin, "manual-only")?;
        host.ready().await?;
        tokio::time::timeout(Duration::from_secs(5), state.requested.notified()).await?;
        let (status, _) = host.completion(&canonical).await?;
        ensure!(status == 404, "route existed before the produced artifact was served");
        state.release.notify_one();
        host.expect_model(&canonical, &native).await?;
        let native_selector = bitrouter_registry::selector::encode("openrouter", &native)?;
        host.expect_model(&native_selector, &native).await?;
        // Routing activates before background cache persistence completes.
        let cache = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                host.alive()?;
                match fs::read(cache_path(&host.home, &source)) {
                    Ok(cache) => return Ok::<_, anyhow::Error>(cache),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }).await.context("admitted registry was not persisted")??;
        let split = cache.iter().position(|b| *b == b'\n').context("cache header")?;
        // The cache preserves the entire exact document, but its JSON whitespace
        // is private storage encoding rather than the publication wire format.
        let cached = Document::parse(&cache[split + 1..], &ReaderPolicy::default())?;
        ensure!(cached.value() == document.value(), "consumer cache lost or changed registry data");
        ensure!(*state.served_hashes.lock().map_err(|_| anyhow::anyhow!("served hashes lock"))? == vec![hash.clone()]);
        host.stop().await?;
        let downloads = state.downloads.load(Ordering::SeqCst);
        host.spawn(&source, false, true, &origin, "manual-only")?;
        host.ready().await?;
        host.expect_model(&canonical, &native).await?;
        ensure!(state.downloads.load(Ordering::SeqCst) == downloads, "offline restart fetched a registry");
        host.stop().await?;
        let calls = state.calls.lock().map_err(|_| anyhow::anyhow!("inference calls lock"))?.clone();
        ensure!(calls.len() >= 3 && calls.iter().all(|call| call["model"] == native && call["authenticated"] == true), "upstream did not receive the exact synced native model and fixture credential: {calls:?}");
        let summary = json!({"outcome":"passed","provider":"openrouter","observed_models":models.len(),"canonical_id":canonical,"provider_model_id":native,"native_selector":native_selector,"wire_sha256":hash,"served_sha256":state.served_hashes.lock().map_err(|_|anyhow::anyhow!("served hashes lock"))?.clone(),"cache_document_equal":true,"cache_sha256":hex::encode(Sha256::digest(&cache[split+1..])),"upstream_calls":calls,"offline_restart":true,"identity_audit":report["audit"]});
        evidence(output, "closed-loop-report.json", &serde_json::to_vec_pretty(&summary)?)?;
        evidence(output, "host.log", &fs::read(&host.log)?)?;
        println!("{}", serde_json::to_string_pretty(&summary)?);
        stop.cancel();
        server.await??;
        Ok::<_, anyhow::Error>(())
    }).await?
}
