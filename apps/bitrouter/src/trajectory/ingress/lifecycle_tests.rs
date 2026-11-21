use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use axum::{Router, routing::post};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::{
    auth::{db, keys},
    gateway::http::{AppState, RouterOptions, build_router_with_options},
    trajectory::{store::TrajectoryStore, types::RequestStatus},
};

struct Server {
    address: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Server {
    async fn start(router: Router) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        });
        Ok(Self {
            address,
            stop: Some(stop),
            task,
        })
    }

    async fn finish(&mut self) -> Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        tokio::time::timeout(Duration::from_secs(5), &mut self.task).await???;
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Fixture {
    _home: tempfile::TempDir,
    assembled: crate::assemble::Assembled,
    server: Server,
    secret: String,
}

impl Fixture {
    async fn new(upstream: SocketAddr, denied: bool) -> Result<Self> {
        let home = tempfile::tempdir()?;
        let policy_dir = home.path().join("access");
        tokio::fs::create_dir(&policy_dir).await?;
        tokio::fs::write(
            policy_dir.join("restricted.yaml"),
            "id: restricted\nallowed_models: [vendor:other]\n",
        )
        .await?;
        let config = crate::config::parse_with(
            &format!(
                "inherit_defaults: false\ndatabase:\n  url: 'sqlite::memory:'\ntrajectory:\n  enabled: true\nplugins:\n  bitrouter-policy:\n    policy_dir: '{}'\nproviders:\n  vendor:\n    api_base: 'http://{upstream}'\n    api_key: provider-fixture\n    api_protocol: [{{'*': chat_completions}}]\n    models: [{{id: economy}}]\n",
                policy_dir.display()
            ),
            |_| None,
        )?;
        let assembled = crate::assemble::build_app_with_path(
            &config,
            Some(&home.path().join("bitrouter.yaml")),
        )
        .await?;
        db::upsert_user(&assembled.db, "capture-owner").await?;
        let key = keys::generate();
        db::insert_api_key(
            &assembled.db,
            &db::NewApiKey {
                id: "capture-key".into(),
                key_hash: key.hash,
                user_id: "capture-owner".into(),
                spend_limit_micro_usd: None,
                rpm_limit: None,
                policy_id: denied.then(|| "restricted".into()),
            },
        )
        .await?;
        let router = build_router_with_options(
            AppState {
                language_model: assembled
                    .app
                    .language_model()
                    .context("missing pipeline")?
                    .clone(),
                mcp: None,
                skip_auth: false,
                metrics_renderer: None,
                prompt_transforms: Vec::new(),
            },
            RouterOptions::default(),
        );
        Ok(Self {
            _home: home,
            assembled,
            server: Server::start(router).await?,
            secret: key.secret,
        })
    }

    async fn request_ids(&self) -> Result<Vec<String>> {
        self.assembled.db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT request_id FROM trajectory_requests WHERE owner_user_id = 'capture-owner' ORDER BY request_id".to_owned()))
            .await?.into_iter().map(|row| Ok(row.try_get("", "request_id")?)).collect()
    }

    async fn assert_failed(&self, expected: usize, error: &str) -> Result<()> {
        let ids = self.request_ids().await?;
        ensure!(
            ids.len() == expected,
            "captured {} requests, expected {expected}",
            ids.len()
        );
        let store = TrajectoryStore::new(self.assembled.db.clone());
        for id in ids {
            let request = store
                .request("capture-owner", &id)
                .await?
                .context("missing request")?;
            ensure!(
                request.status == RequestStatus::Failed,
                "request still {:?}",
                request.status
            );
            let events = store
                .events_for_episode("capture-owner", &request.episode_id)
                .await?;
            let terminal = events.last().context("missing terminal event")?;
            ensure!(
                terminal
                    .evidence
                    .categorical
                    .values()
                    .any(|value| value == error),
                "missing {error} outcome: {:?}",
                terminal.evidence
            );
            ensure!(
                !terminal
                    .evidence
                    .structural
                    .contains_key("settlement.total_tokens")
            );
        }
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        self.server.finish().await?;
        tokio::time::timeout(
            Duration::from_secs(5),
            self.assembled
                .app
                .language_model()
                .context("missing pipeline")?
                .drain_required_pending_settlements(),
        )
        .await??;
        self.assembled.shutdown().await
    }
}

#[tokio::test]
async fn authenticated_policy_denial_finishes_captured_requests() -> Result<()> {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = calls.clone();
    let upstream = Server::start(Router::new().route(
        "/chat/completions",
        post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                axum::http::StatusCode::BAD_GATEWAY
            }
        }),
    ))
    .await?;
    let mut fixture = Fixture::new(upstream.address, true).await?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;
    for stream in [false, true] {
        let response = client.post(format!("http://{}/v1/chat/completions", fixture.server.address))
            .bearer_auth(&fixture.secret)
            .json(&json!({"model":"vendor:economy","messages":[{"role":"user","content":"private denied text"}],"stream":stream}))
            .send().await?;
        ensure!(
            response.status() == 403,
            "unexpected status {}",
            response.status()
        );
        let _ = response.bytes().await?;
    }
    fixture.finish().await?;
    fixture.assert_failed(2, "forbidden").await?;
    ensure!(calls.load(std::sync::atomic::Ordering::SeqCst) == 0);
    Ok(())
}

#[tokio::test]
async fn disconnect_before_upstream_headers_is_settled_and_drained() -> Result<()> {
    let entered = Arc::new(tokio::sync::Notify::new());
    let signal = entered.clone();
    let upstream = Server::start(Router::new().route(
        "/chat/completions",
        post(move || {
            let signal = signal.clone();
            async move {
                signal.notify_one();
                std::future::pending::<axum::http::StatusCode>().await
            }
        }),
    ))
    .await?;
    let mut fixture = Fixture::new(upstream.address, false).await?;
    let mut socket = tokio::net::TcpStream::connect(fixture.server.address).await?;
    let body = json!({"model":"vendor:economy","messages":[{"role":"user","content":"private disconnected text"}],"stream":true}).to_string();
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        fixture.server.address,
        fixture.secret,
        body.len()
    );
    socket.write_all(request.as_bytes()).await?;
    tokio::time::timeout(Duration::from_secs(5), entered.notified()).await?;
    ensure!(fixture.request_ids().await?.len() == 1);
    drop(socket);
    fixture.finish().await?;
    fixture.assert_failed(1, "request_cancelled").await?;
    Ok(())
}
