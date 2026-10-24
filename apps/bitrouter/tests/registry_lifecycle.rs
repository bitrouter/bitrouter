//! Real official-host lifecycle; all HTTP, credentials and process state are
//! test-owned. No paid provider call or external network is required.
#![cfg(unix)]
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use bitrouter::daemon::{self, DaemonCommand, DaemonResponse};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[path = "registry_lifecycle/live_sync.rs"]
mod live_sync;

struct Fixture {
    registry: RwLock<String>,
    requests: AtomicUsize,
    first: AtomicBool,
    requested: tokio::sync::Notify,
    release: tokio::sync::Notify,
    inference_auth: AtomicBool,
}
async fn registry(State(state): State<Arc<Fixture>>) -> impl IntoResponse {
    state.requests.fetch_add(1, Ordering::SeqCst);
    state.requested.notify_one();
    if state.first.swap(false, Ordering::SeqCst) {
        state.release.notified().await;
    }
    let body = state
        .registry
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    (StatusCode::OK, [("content-type", "application/json")], body)
}
async fn upstream(
    State(state): State<Arc<Fixture>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    if matches!(
        body["model"].as_str(),
        Some("first-native" | "second-native")
    ) && (headers.contains_key("authorization") || headers.contains_key("x-api-key"))
    {
        state.inference_auth.store(true, Ordering::SeqCst);
    }
    Json(
        json!({"id":"fixture-result","object":"chat.completion","model":body["model"],"choices":[{"index":0,"message":{"role":"assistant","content":body["model"]},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
    )
}
fn document(native: &str) -> registry_json::Value {
    registry_json::json!({"version":1,"spec_revision":bitrouter_registry::document::SPEC_REVISION,"as_of":"2026-10-22T00:00:00Z","future":{"$serde_json::private::Number":"opaque","sibling":true},"data":{"models":[],"providers":[{"name":"fixture","status":"active","billing":"usage","api_base":"https://fixture.invalid/v1","auth":{"kind":"none"},"models":[{"provider_model_id":native,"canonical_id":"org/model","status":"active","interfaces":[{"protocol":"chat_completions","operation":"generation","input_modalities":["text"],"output_modalities":["text"]}]}]}]}})
}
struct Host {
    child: Option<Child>,
    home: PathBuf,
    address: SocketAddr,
    log: PathBuf,
    client: reqwest::Client,
    provider: &'static str,
}
impl Host {
    fn new(home: PathBuf, provider: &'static str) -> Result<Self> {
        for dir in ["home", "runtime", "config", "cache", "data", "tmp"] {
            fs::create_dir_all(home.join(dir))?;
        }
        Ok(Self {
            child: None,
            log: home.join("host.log"),
            home,
            address: "127.0.0.1:0".parse()?,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()?,
            provider,
        })
    }
    fn configure(
        &self,
        source: &str,
        auto: bool,
        enabled: bool,
        upstream: &str,
        marker: &str,
    ) -> Result<()> {
        fs::write(
            self.home.join("bitrouter.yaml"),
            format!(
                "server:\n  listen: '{}'\n  control_socket: control.sock\n  skip_auth: true\ndatabase:\n  url: 'sqlite://host.db?mode=rwc'\nregistry:\n  enabled: {enabled}\n  url: '{source}'\n  auto_refresh: {auto}\nproviders:\n  {}:\n    api_base: '{upstream}/v1'\n    api_key: 'fixture-inference-key'\n  manual:\n    api_base: '{upstream}/v1'\n    models: [{{id: '{marker}'}}]\n",
                self.address, self.provider
            ),
        )?;
        Ok(())
    }
    fn spawn(
        &mut self,
        source: &str,
        auto: bool,
        enabled: bool,
        upstream: &str,
        marker: &str,
    ) -> Result<()> {
        let reservation = TcpListener::bind("127.0.0.1:0")?;
        self.address = reservation.local_addr()?;
        self.configure(source, auto, enabled, upstream, marker)?;
        let output = File::create(&self.log)?;
        drop(reservation);
        self.child = Some(
            Command::new(env!("CARGO_BIN_EXE_bro"))
                .args(["serve", "--managed-child", "--config"])
                .arg(self.home.join("bitrouter.yaml"))
                .current_dir(&self.home)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", self.home.join("home"))
                .env("USERPROFILE", self.home.join("home"))
                .env("BITROUTER_HOME", self.home.join("runtime"))
                .env("XDG_CONFIG_HOME", self.home.join("config"))
                .env("XDG_CACHE_HOME", self.home.join("cache"))
                .env("XDG_DATA_HOME", self.home.join("data"))
                .env("TMPDIR", self.home.join("tmp"))
                .stdin(Stdio::null())
                .stdout(output.try_clone()?)
                .stderr(output)
                .spawn()?,
        );
        Ok(())
    }
    fn alive(&mut self) -> Result<()> {
        let child = self.child.as_mut().context("no child")?;
        ensure!(
            child.try_wait()?.is_none(),
            "host exited: {}",
            fs::read_to_string(&self.log).unwrap_or_default()
        );
        Ok(())
    }
    async fn ready(&mut self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                self.alive()?;
                if let Ok(response) = self
                    .client
                    .get(format!("http://{}/health", self.address))
                    .send()
                    .await
                    && response.status().is_success()
                    && let Ok(DaemonResponse::Status { .. }) = daemon::send_command(
                        &self.home.join("control.sock"),
                        &DaemonCommand::Status,
                    )
                    .await
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        Ok(())
    }
    async fn completion(&self, model: &str) -> Result<(u16, Value)> {
        let response=self.client.post(format!("http://{}/v1/chat/completions",self.address)).json(&json!({"model":model,"messages":[{"role":"user","content":"test"}],"max_tokens":1})).send().await?;
        Ok((response.status().as_u16(), response.json().await?))
    }
    async fn expect_model(&mut self, selector: &str, native: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                self.alive()?;
                if let Ok((200, value)) = self.completion(selector).await
                    && value["choices"][0]["message"]["content"] == native
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .with_context(|| {
            format!(
                "route never appeared; {}",
                fs::read_to_string(&self.log).unwrap_or_default()
            )
        })??;
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            daemon::send_command(&self.home.join("control.sock"), &DaemonCommand::Stop),
        )
        .await??;
        ensure!(matches!(response, DaemonResponse::Ok), "stop rejected");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if self
                    .child
                    .as_mut()
                    .context("no child")?
                    .try_wait()?
                    .is_some()
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        self.child = None;
        Ok(())
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn cache_path(home: &std::path::Path, source: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    home.join("cache/bitrouter/registry-v1").join(format!(
        "{}.json",
        hex::encode(Sha256::digest(format!("1\0{source}").as_bytes()))
    ))
}
#[tokio::test]
async fn official_host_registry_download_hot_routes_cache_restart_failure_and_source_isolation()
-> Result<()> {
    tokio::time::timeout(Duration::from_secs(90), async {
        let state = Arc::new(Fixture {
            registry: RwLock::new(registry_json::to_string(&document("first-native"))?),
            requests: AtomicUsize::new(0),
            first: AtomicBool::new(true),
            requested: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            inference_auth: AtomicBool::new(false),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let router = Router::new()
            .route("/registry-a", get(registry))
            .route("/registry-b", get(registry))
            .route("/registry-c", get(registry))
            .route("/v1/chat/completions", post(upstream))
            .with_state(state.clone());
        let stop = CancellationToken::new();
        let _guard = stop.clone().drop_guard();
        let server = tokio::spawn(
            axum::serve(listener, router)
                .with_graceful_shutdown(stop.clone().cancelled_owned())
                .into_future(),
        );
        let directory = tempfile::Builder::new()
            .prefix("bro-reg-")
            .tempdir_in("/tmp")?;
        let mut host = Host::new(directory.path().to_path_buf(), "fixture")?;
        let a = format!("{origin}/registry-a");
        let b = format!("{origin}/registry-b");
        let c = format!("{origin}/registry-c");
        host.spawn(&a, true, true, &origin, "applied")?;
        host.ready().await?;
        // A blocked registry response cannot hold readiness or local routing.
        host.expect_model("applied", "applied").await?;
        host.configure(&a, true, true, &origin, "pending")?;
        tokio::time::timeout(Duration::from_secs(3), state.requested.notified()).await?;
        state.release.notify_one();
        host.expect_model("org/model", "first-native").await?;
        host.expect_model("applied", "applied").await?;
        let models: Value = host
            .client
            .get(format!("http://{}/v1/models", host.address))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            !models["data"]
                .as_array()
                .context("model inventory")?
                .iter()
                .any(|m| m["id"] == "pending"),
            "background refresh applied pending config"
        );
        ensure!(
            !state.inference_auth.load(Ordering::SeqCst),
            "none auth inherited configured bearer credential"
        );
        *state.registry.write().unwrap_or_else(|p| p.into_inner()) =
            registry_json::to_string(&document("second-native"))?;
        host.configure(&b, true, true, &origin, "pending")?;
        let response = daemon::send_command(
            &host.home.join("control.sock"),
            &DaemonCommand::Reload { env: vec![] },
        )
        .await?;
        ensure!(
            matches!(response, DaemonResponse::Ok),
            "reload failed: {response:?}"
        );
        host.expect_model("org/model", "second-native").await?;
        host.expect_model("pending", "pending").await?;
        host.stop().await?;
        let requests = state.requests.load(Ordering::SeqCst);
        let cache = cache_path(&host.home, &b);
        let saved = fs::read(&cache)?;
        ensure!(
            String::from_utf8_lossy(&saved).contains("$serde_json::private::Number"),
            "unknown metadata lost in cache"
        );
        *state.registry.write().unwrap_or_else(|p| p.into_inner()) =
            "{\"version\":1e999999999}".into();
        host.spawn(&b, false, true, &origin, "pending")?;
        host.ready().await?;
        host.expect_model("org/model", "second-native").await?;
        ensure!(
            state.requests.load(Ordering::SeqCst) == requests,
            "offline restart downloaded registry"
        );
        host.stop().await?;
        // Force a due check without changing the complete cached document.
        let split = saved
            .iter()
            .position(|b| *b == b'\n')
            .context("cache header")?;
        let mut header: Value = serde_json::from_slice(&saved[..split])?;
        header["checked_at"] = json!(0);
        let mut due = serde_json::to_vec(&header)?;
        due.push(b'\n');
        due.extend_from_slice(&saved[split + 1..]);
        fs::write(&cache, &due)?;
        host.spawn(&b, true, true, &origin, "pending")?;
        host.ready().await?;
        host.expect_model("org/model", "second-native").await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while state.requests.load(Ordering::SeqCst) == requests {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !fs::read_to_string(&host.log)
                .unwrap_or_default()
                .contains("registry.invalid_data")
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("invalid registry response had no actionable diagnostic")?;
        host.alive()?;
        host.expect_model("org/model", "second-native").await?;
        ensure!(
            fs::read(&cache)? == due,
            "failed refresh advanced cache/checkpoint"
        );
        host.stop().await?;
        host.spawn(&c, false, true, &origin, "pending")?;
        host.ready().await?;
        let (status, _) = host.completion("org/model").await?;
        ensure!(
            status == 404,
            "custom source inherited another source's model: {status}"
        );
        host.expect_model("pending", "pending").await?;
        host.stop().await?;
        host.spawn(&b, true, false, &origin, "pending")?;
        host.ready().await?;
        let (status, _) = host.completion("org/model").await?;
        if status != 404 {
            bail!("disabled registry retained route: {status}");
        }
        host.stop().await?;
        stop.cancel();
        server.await??;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
