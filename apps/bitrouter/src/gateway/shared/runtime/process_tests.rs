//! Exercise the official executable, never an in-process replacement host.

use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::process::{Child, Command};

use super::super::{
    authority::Authority,
    crypto::DeploymentKey,
    publication::Configuration,
    schema,
    store::{self, Kind, Store},
};

struct Running {
    child: Child,
    log: PathBuf,
    address: std::net::SocketAddr,
}

impl Running {
    async fn exited(&mut self) -> Result<ExitStatus> {
        match tokio::time::timeout(Duration::from_secs(8), self.child.wait()).await {
            Ok(result) => result.context("waiting for fixture gateway"),
            Err(_) => {
                self.child
                    .kill()
                    .await
                    .context("reaping stalled fixture gateway")?;
                anyhow::bail!("fixture gateway did not exit: {}", self.logs());
            }
        }
    }

    async fn stop(&mut self) -> Result<ExitStatus> {
        if let Some(status) = self.child.try_wait()? {
            return Ok(status);
        }
        let pid = self.child.id().context("fixture gateway has no PID")?;
        // Signal only the Child created by this fixture, never a discovered PID.
        let signal = Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .await;
        let exited = self.exited().await;
        ensure!(signal?.success(), "could not signal fixture gateway");
        exited
    }

    fn logs(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    async fn ready(&mut self) -> Result<()> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()?;
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                ensure!(
                    self.child.try_wait()?.is_none(),
                    "gateway exited before readiness: {}",
                    self.logs()
                );
                if client
                    .get(format!("http://{}/health", self.address))
                    .send()
                    .await
                    .is_ok_and(|response| response.status() == StatusCode::OK)
                {
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("gateway readiness timed out")?
    }
}

struct Fixture {
    root: tempfile::TempDir,
    binary: PathBuf,
    db: sea_orm::DatabaseConnection,
    authority: Authority,
    key: [u8; 32],
    url: String,
    children: Vec<Running>,
}

impl Fixture {
    async fn new(published: bool) -> Result<Self> {
        let binary = PathBuf::from(std::env::var("BITROUTER_TEST_GATEWAY_BINARY")?);
        ensure!(
            binary.is_absolute() && binary.is_file(),
            "an absolute official gateway binary is required"
        );
        let url = std::env::var("BITROUTER_TEST_DATABASE_URL")?;
        let db = store::connect(&url, 2, Duration::from_secs(3)).await?;
        let prepared = async {
            crate::db::run_migrations(&db).await?;
            let namespace = format!("process-{}", uuid::Uuid::new_v4());
            let store = Store::new(db.clone(), &namespace, Duration::from_secs(3))?;
            schema::initialize(&store).await?;
            let key = rand::random();
            let authority = Authority::new(store, DeploymentKey::from_bytes(key)?);
            authority.initialize().await?;
            if published {
                authority
                    .publish(
                        0,
                        &Configuration {
                            format: 1,
                            routing: json!({}),
                            policies: Vec::new(),
                            policy_lock: None,
                            credentials: Vec::new(),
                            require_known_pricing: false,
                        },
                    )
                    .await?;
            }
            let root = tempfile::tempdir()?;
            tokio::fs::create_dir(root.path().join("launch")).await?;
            Ok::<_, anyhow::Error>((authority, key, root))
        }
        .await;
        let (authority, key, root) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                db.close().await?;
                return Err(error);
            }
        };
        Ok(Self {
            root,
            binary,
            db,
            authority,
            key,
            url,
            children: Vec::new(),
        })
    }

    async fn spawn(
        &mut self,
        action: &str,
        address: std::net::SocketAddr,
        extra: &[&str],
    ) -> Result<usize> {
        let index = self.children.len();
        let config = self.root.path().join(format!("gateway-{index}.json"));
        let document = json!({
            "server": {"listen": address.to_string(), "log_level": "info"},
            "database": {"url": "${FIXTURE_SHARED_DATABASE}"},
            "gateway_state": {
                "mode": "shared", "namespace": self.authority.store().namespace(),
                "deployment_key_env": "FIXTURE_SHARED_KEY", "pool_connections": 3,
                "transaction_timeout_ms": 1000, "owner_lease_ms": 10000,
                "heartbeat_ms": 250, "worker_interval_ms": 100, "drain_timeout_ms": 2000
            }
        });
        tokio::fs::write(&config, serde_json::to_vec(&document)?).await?;
        let log = self.root.path().join(format!("gateway-{index}.log"));
        let output = std::fs::File::create(&log)?;
        let child = Command::new(&self.binary)
            .arg(action)
            .arg("--config")
            .arg(&config)
            .args(extra)
            .current_dir(self.root.path().join("launch"))
            .env("FIXTURE_SHARED_DATABASE", &self.url)
            .env("FIXTURE_SHARED_KEY", STANDARD.encode(self.key))
            .env("BITROUTER_HOME", self.root.path().join("unexpected-home"))
            .env("HOME", self.root.path().join("unexpected-user-home"))
            .env_remove("RUST_LOG")
            .env_remove("BITROUTER_WORKFLOW_TRACE_JSONL")
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone()?))
            .stderr(Stdio::from(output))
            .kill_on_drop(true)
            .spawn()?;
        self.children.push(Running {
            child,
            log,
            address,
        });
        Ok(index)
    }

    fn child(&mut self, index: usize) -> Result<&mut Running> {
        self.children
            .get_mut(index)
            .context("fixture child missing")
    }

    async fn owners(&self) -> Result<Vec<Value>> {
        Ok(self
            .authority
            .store()
            .due_states::<Value>(Kind::Instance, &["live", "draining"], i64::MAX, 256)
            .await?
            .into_iter()
            .map(|record| record.value)
            .collect())
    }

    fn no_local_files(&self) -> Result<()> {
        ensure!(!self.root.path().join("unexpected-home").exists());
        ensure!(!self.root.path().join("unexpected-user-home").exists());
        ensure!(
            std::fs::read_dir(self.root.path().join("launch"))?
                .next()
                .is_none()
        );
        for entry in std::fs::read_dir(self.root.path())? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            ensure!(
                name == "launch"
                    || (name.starts_with("gateway-")
                        && (name.ends_with(".json") || name.ends_with(".log"))),
                "shared launch created unexpected local state: {name}"
            );
        }
        Ok(())
    }

    async fn finish(mut self, result: Result<()>) -> Result<()> {
        // Always drive every retained Child to an exit, even after assertions fail.
        let stopped = futures::future::join_all(self.children.iter_mut().map(Running::stop)).await;
        let closed = self.db.close().await;
        result?;
        for result in stopped {
            result?;
        }
        closed?;
        Ok(())
    }
}

fn available_address() -> Result<std::net::SocketAddr> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?)
}

#[tokio::test]
#[ignore = "requires PostgreSQL and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_replicas_start_and_drain_without_local_daemon_state() -> Result<()> {
    let mut fixture = Fixture::new(true).await?;
    let result = async {
        let a_address = available_address()?;
        let a = fixture.spawn("serve", a_address, &[]).await?;
        fixture.child(a)?.ready().await?;
        let b = fixture.spawn("serve", available_address()?, &[]).await?;
        fixture.child(b)?.ready().await?;
        let owners = fixture.owners().await?;
        ensure!(owners.len() == 2 && owners.iter().all(|owner| owner["accepting"] == true));
        let response = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()?
            .post(format!("http://{a_address}/v1/chat/completions"))
            .json(
                &json!({"model":"unconfigured", "messages":[{"role":"user", "content":"fixture"}]}),
            )
            .send()
            .await?;
        ensure!(response.status() == StatusCode::UNAUTHORIZED);
        ensure!(
            fixture.child(a)?.stop().await?.success(),
            "first replica failed graceful shutdown"
        );
        fixture.child(b)?.ready().await?;
        ensure!(
            fixture
                .owners()
                .await?
                .iter()
                .filter(|owner| owner["accepting"] == true)
                .count()
                == 1
        );
        ensure!(
            fixture.child(b)?.stop().await?.success(),
            "second replica failed graceful shutdown"
        );
        ensure!(
            fixture
                .owners()
                .await?
                .iter()
                .all(|owner| owner["accepting"] == false)
        );
        fixture.no_local_files()?;
        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_startup_failures_do_not_leave_accepting_owners_or_daemon_files() -> Result<()> {
    let mut fixture = Fixture::new(true).await?;
    let result = async {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let child = fixture.spawn("serve", occupied.local_addr()?, &[]).await?;
        ensure!(!fixture.child(child)?.exited().await?.success());
        let owners = fixture.owners().await?;
        ensure!(owners.len() == 1 && owners[0]["accepting"] == false);
        for (action, extra) in [
            ("start", &[][..]),
            ("restart", &[][..]),
            ("serve", &["--managed-child"][..]),
        ] {
            let child = fixture.spawn(action, available_address()?, extra).await?;
            ensure!(
                !fixture.child(child)?.exited().await?.success(),
                "accepted unsupported {action}"
            );
            ensure!(
                fixture
                    .child(child)?
                    .logs()
                    .contains("shared gateway must run")
            );
        }
        ensure!(
            fixture.owners().await?.len() == 1,
            "local launcher joined a shared owner"
        );
        fixture.no_local_files()?;
        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_join_does_not_publish_an_unconfigured_namespace() -> Result<()> {
    let mut fixture = Fixture::new(false).await?;
    let result = async {
        let child = fixture.spawn("serve", available_address()?, &[]).await?;
        ensure!(!fixture.child(child)?.exited().await?.success());
        ensure!(
            fixture
                .child(child)?
                .logs()
                .contains("no published configuration")
        );
        ensure!(fixture.authority.head().await?.config_generation == 0);
        ensure!(fixture.owners().await?.is_empty());
        fixture.no_local_files()?;
        Ok(())
    }
    .await;
    fixture.finish(result).await
}
