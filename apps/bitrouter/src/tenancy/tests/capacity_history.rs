//! History and DB restore qualification; large work is opt-in, never a product limit.
use super::*;
use crate::tenancy::{self, ownership, resources::Resources, store};
use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode},
};
use bitrouter_ai::{
    control::ControlScope,
    tenancy::{configuration::ResourceRef, operation::OperationStatus},
};
use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection, Statement};
use serde::{Serialize, de::DeserializeOwned};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc};
use tower::ServiceExt;

mod authority;
mod disk;
mod history;
mod offline;
mod serving;

const TOKEN: &str = "history-qualification-operator-at-least-32-bytes";
// Synthetic wrapping key used by real authenticated encryption, never production data.
const KEY: [u8; 32] = [53; 32];
const GIB: u64 = 1024 * 1024 * 1024;

struct Harness {
    fixture: Fixture,
    path: PathBuf,
    lease: Option<ownership::Lease>,
    config: crate::config::tenancy::TenancyConfig,
    _directory: tempfile::TempDir,
}

async fn connect(path: &std::path::Path) -> Result<DatabaseConnection> {
    let mut options = ConnectOptions::new(format!("sqlite://{}?mode=rwc", path.display()));
    options
        .min_connections(1)
        .max_connections(1)
        .sqlx_logging(false);
    let db = Database::connect(options).await?;
    db.execute_unprepared(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA auto_vacuum=NONE;",
    )
    .await?;
    Ok(db)
}

fn sql(text: &str) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Sqlite, text)
}

impl Harness {
    async fn new(large: bool) -> Result<Self> {
        let mut temporary = tempfile::Builder::new();
        temporary
            .prefix("bitrouter-history-")
            .permissions(std::fs::Permissions::from_mode(0o700));
        let directory = if large {
            let root = std::env::var_os("BITROUTER_C4_SCRATCH")
                .context("ignored qualification requires BITROUTER_C4_SCRATCH")?;
            let directory = temporary.tempdir_in(root)?;
            disk::require_free(directory.path(), 40 * GIB)?;
            directory
        } else {
            temporary.tempdir()?
        };
        ensure!(std::fs::metadata(directory.path())?.permissions().mode() & 0o777 == 0o700);
        let path = directory.path().join("history.sqlite");
        // One acquisition protects setup, actor ownership and stopped fixture writes.
        let lease = ownership::acquire_path(&path, true)?;
        let fixture = Fixture::with_database(connect(&path).await?, &mut |_| {}).await?;
        let mut h = Self {
            fixture,
            path,
            lease: Some(lease),
            config: Default::default(),
            _directory: directory,
        };
        h.restart().await?;
        Ok(h)
    }

    async fn restart(&mut self) -> Result<()> {
        self.fixture.service.shutdown().await?;
        self.fixture.db.clone().close().await?;
        self.fixture.db = connect(&self.path).await?;
        let initial = store::load(&self.fixture.db, Utc::now()).await?;
        ensure!(initial.installation == self.fixture.installation);
        self.fixture.service = Service::start_resources_with_continuations(
            self.fixture.db.clone(),
            Some(
                self.lease
                    .as_ref()
                    .context("fixture ownership missing")?
                    .clone(),
            ),
            initial,
            Arc::new(Resources::with_key(self.fixture.installation.clone(), KEY)?),
            tenancy::Provisioning {
                import: None,
                continuations: &Default::default(),
                claim: None,
                receipt_observer: Default::default(),
                config: &self.config,
            },
            None,
        )
        .await?;
        Ok(())
    }

    async fn finish(self, outcome: Result<()>) -> Result<()> {
        // Keep the path lease and temporary directory alive through actual owner
        // drain and pool close on assertion failure as well as on success.
        let shutdown = self.fixture.service.shutdown().await;
        let close = self.fixture.db.close().await;
        let cleanup = shutdown.and(close.map_err(anyhow::Error::from));
        let result = match (outcome, cleanup) {
            (Err(error), Err(cleanup)) => Err(error.context(format!("fixture cleanup: {cleanup}"))),
            (outcome, cleanup) => outcome.and(cleanup),
        };
        if result.is_err() {
            let retained = self._directory.keep();
            eprintln!(
                "history fixture failed; preserved source/copies at {}",
                retained.display()
            );
        }
        result
    }

    async fn wire(
        &self,
        method: Method,
        path: &str,
        token: &str,
        body: &impl Serialize,
    ) -> Result<(StatusCode, Vec<u8>)> {
        let auth = crate::remote_control::auth::ControlAuth::from_lookup(
            &crate::config::ControlConfig {
                credentials: vec![crate::config::ControlCredentialConfig {
                    id: "test-operator".into(),
                    token_env: "HISTORY_TEST_OPERATOR".into(),
                    scopes: vec![ControlScope::Tenancy],
                }],
                ..Default::default()
            },
            |_| Some(TOKEN.into()),
        )?;
        let response = tenancy::http::router(self.fixture.service.clone(), auth)
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri(path)
                    .header("authorization", format!("Bearer {token}"))
                    .header(
                        "bitrouter-installation-id",
                        self.fixture.installation.as_str(),
                    )
                    .header("content-type", "application/json")
                    .body(if method == Method::GET {
                        Body::empty()
                    } else {
                        Body::from(serde_json::to_vec(body)?)
                    })?,
            )
            .await?;
        let status = response.status();
        Ok((
            status,
            to_bytes(response.into_body(), 2 * 1024 * 1024)
                .await?
                .to_vec(),
        ))
    }

    async fn api<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T> {
        let (status, bytes) = self.wire(method, path, TOKEN, body).await?;
        ensure!(status.is_success(), "history API {path}: {status}");
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn fresh_mutation(&self, id: &str) -> Result<()> {
        let result: OperationResult = self
            .api(
                Method::PUT,
                &format!("/control/v1/principals/{id}"),
                &PrincipalPut {
                    operation_id: id.try_into()?,
                    expected_revision: None,
                    issuer: "history-fixture".into(),
                    subject: id.into(),
                    installation_id: self.fixture.installation.clone(),
                },
            )
            .await?;
        ensure!(result.status == OperationStatus::Applied);
        Ok(())
    }
}

#[tokio::test]
async fn canonical_seed_replay_and_lifecycle_survive_restart() -> Result<()> {
    let mut h = Harness::new(false).await?;
    let outcome = async {
        let retained = authority::qualify(&mut h, 9, 129).await?;
        h.restart().await?;
        authority::verify(&h, &retained).await?;
        h.fresh_mutation("after-quick-restart").await
    }
    .await;
    h.finish(outcome).await
}

#[tokio::test]
async fn archive_cycles_preserve_unknowns_and_current_read_authority() -> Result<()> {
    let mut h = Harness::new(false).await?;
    let outcome = async {
        let retained = authority::qualify(&mut h, 3, 33).await?;
        history::exercise(&mut h, &retained, false).await
    }
    .await;
    h.finish(outcome).await
}

#[tokio::test]
#[ignore = "dedicated >8GiB history/DB restore qualification; requires BITROUTER_C4_SCRATCH and 40GiB free"]
async fn large_history_qualification() -> Result<()> {
    let started = std::time::Instant::now();
    let mut h = Harness::new(true).await?;
    eprintln!(
        "history qualification os={} database={} installation={} tenant={} principal={} profile={} external_state_recovery_qualified=false",
        std::env::consts::OS,
        h.path.display(),
        h.fixture.installation,
        h.fixture.tenant,
        h.fixture.principal,
        if cfg!(debug_assertions) {
            "debug-assertions"
        } else {
            "optimized"
        }
    );
    let outcome = async {
        let retained = authority::qualify(&mut h, 4097, 100001).await?;
        history::exercise(&mut h, &retained, true).await
    }
    .await;
    h.finish(outcome).await?;
    eprintln!(
        "history and v2 DB snapshot/verify/restore qualification succeeded elapsed={:?}",
        started.elapsed()
    );
    Ok(())
}
