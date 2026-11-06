//! Owned connections, bounded SQLite work and acknowledged worker shutdown.
use anyhow::{Result, ensure};
use sea_orm::sqlx::{ConnectOptions, Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub(crate) const CONNECTION_BYTES: usize = 2 * 1024 * 1024 + 64 * 1024;

pub(crate) struct Connections {
    pub(crate) source: Option<SqliteConnection>,
    pub(crate) copy: Option<SqliteConnection>,
    pub(crate) uncertain: bool,
    pub(crate) owner: Option<crate::tenancy::ownership::Lease>,
    cancel: CancellationToken,
    deadline: Option<Instant>,
    source_charge: Option<super::large::Charge>,
    copy_charge: Option<super::large::Charge>,
}
impl Connections {
    pub(crate) fn new(
        cancel: CancellationToken,
        owner: Option<crate::tenancy::ownership::Lease>,
    ) -> Self {
        Self {
            source: None,
            copy: None,
            uncertain: false,
            owner,
            cancel,
            deadline: Some(Instant::now() + Duration::from_secs(120)),
            source_charge: None,
            copy_charge: None,
        }
    }
    pub(crate) fn check(&self) -> Result<()> {
        ensure!(
            !self.cancel.is_cancelled()
                && self
                    .deadline
                    .is_none_or(|deadline| Instant::now() < deadline),
            "offline operation ended"
        );
        Ok(())
    }
    pub(crate) async fn open(&mut self, path: &Path, immutable: bool) -> Result<()> {
        self.check()?;
        ensure!(path.to_str().is_some(), "SQLite path must be UTF-8");
        let options = options(path, immutable);
        #[cfg(all(test, unix))]
        let options = tests::initialization_options(path, options);
        self.open_options(options, immutable, None).await
    }
    pub(crate) async fn open_budgeted(
        &mut self,
        path: &Path,
        immutable: bool,
        workspace: &super::large::Workspace,
    ) -> Result<()> {
        self.check()?;
        ensure!(path.to_str().is_some(), "SQLite path must be UTF-8");
        let options = options(path, immutable);
        #[cfg(all(test, unix))]
        let options = tests::initialization_options(path, options);
        self.open_options(options, immutable, Some(workspace)).await
    }
    /// Open a clean, externally fenced source without creating SQLite sidecars.
    /// The caller retains its metadata witness and checks it after actual close.
    #[cfg(all(test, unix))]
    pub(crate) async fn open_immutable_source(&mut self, path: &Path) -> Result<()> {
        self.immutable_source(path)?;
        self.open_options(options(path, true), false, None).await
    }
    pub(crate) async fn open_immutable_source_budgeted(
        &mut self,
        path: &Path,
        workspace: &super::large::Workspace,
    ) -> Result<()> {
        self.immutable_source(path)?;
        self.open_options(options(path, true), false, Some(workspace))
            .await
    }
    fn immutable_source(&self, path: &Path) -> Result<()> {
        self.check()?;
        ensure!(path.to_str().is_some(), "SQLite path must be UTF-8");
        ensure!(
            self.owner
                .as_ref()
                .is_some_and(|owner| owner.exclusive() && owner.path == path),
            "immutable source requires its canonical exclusive owner"
        );
        super::large::Source::inspect(path)?;
        // Immutable skips locking and change detection; external writers must
        // remain fenced until publication. https://sqlite.org/uri.html#uriimmutable
        Ok(())
    }
    pub(super) async fn open_destination(&mut self, path: &Path) -> Result<()> {
        self.check()?;
        // One direct worker under the same file-first owner; no pool, migrations,
        // initialize-if-empty or cancellation-selected consuming SQL futures.
        self.open_options(options(path, false).read_only(false), false, None)
            .await
    }
    #[cfg(feature = "core")]
    pub(crate) async fn open_destination_budgeted(
        &mut self,
        path: &Path,
        workspace: &super::large::Workspace,
    ) -> Result<()> {
        self.check()?;
        self.open_options(
            options(path, false).read_only(false),
            false,
            Some(workspace),
        )
        .await
    }
    pub(super) async fn destination_cleanup_budget(&mut self) -> Result<()> {
        self.source
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("destination missing"))?
            .lock_handle()
            .await?
            .remove_progress_handler();
        Ok(())
    }
    /// Complete validation is cancellable, but has no cumulative time/work cap.
    pub(crate) async fn validation_budget(&mut self) -> Result<()> {
        self.deadline = None;
        for db in [&mut self.source, &mut self.copy].into_iter().flatten() {
            policy(db, self.cancel.clone(), None).await?;
        }
        self.check()
    }
    async fn open_options(
        &mut self,
        options: SqliteConnectOptions,
        copy: bool,
        workspace: Option<&super::large::Workspace>,
    ) -> Result<()> {
        ensure!(
            if copy {
                self.copy.is_none()
            } else {
                self.source.is_none()
            },
            "connection already open"
        );
        if workspace.is_some() || self.source_charge.is_some() || self.copy_charge.is_some() {
            ensure!(!self.uncertain, "offline connection cleanup unconfirmed");
        }
        let charge = workspace
            .map(|workspace| workspace.reserve(CONNECTION_BYTES))
            .transpose()?;
        if copy {
            self.copy_charge = charge;
        } else {
            self.source_charge = charge;
        }
        // Direct ConnectOptions has no pool acquire timeout. This future is
        // owned by the command task and is never selected away on cancellation.
        // https://docs.rs/sqlx-sqlite/0.8.6/src/sqlx_sqlite/options/connect.rs.html
        // An Err can drop a connection before SQLx acknowledges worker exit;
        // keep the source owner quarantined in that case, even after replying.
        self.uncertain = true;
        let connection = options.connect().await?;
        let slot = if copy {
            &mut self.copy
        } else {
            &mut self.source
        };
        *slot = Some(connection);
        self.uncertain = false;
        let db = slot
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("connection unavailable"))?;
        policy(db, self.cancel.clone(), self.deadline).await?;
        self.check()
    }
    /// Reset only query work; cleanup never shares a cancellable await.
    pub(crate) async fn read_budget(
        &mut self,
        cancel: CancellationToken,
        remaining: Duration,
    ) -> Result<()> {
        self.cancel = cancel;
        self.deadline = Some(Instant::now() + remaining.min(Duration::from_secs(5)));
        let db = self
            .copy
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("audit connection missing"))?;
        policy(db, self.cancel.clone(), self.deadline).await
    }
    pub(crate) async fn cleanup_budget(&mut self) -> Result<()> {
        let db = self
            .copy
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("audit connection missing"))?;
        db.lock_handle().await?.remove_progress_handler();
        Ok(())
    }
    pub(crate) async fn close(&mut self) -> Result<()> {
        let unresolved = self.uncertain;
        self.uncertain = true;
        let mut failed = false;
        for connection in [&mut self.copy, &mut self.source] {
            if let Some(mut db) = connection.take() {
                // Cleanup is not cancelled by the query work budget. The handle
                // lock waits for any interrupted statement to leave the worker.
                match db.lock_handle().await {
                    Ok(mut handle) => handle.remove_progress_handler(),
                    Err(_) => failed = true,
                }
                // optimize_on_close is disabled by the default connection options.
                // SQLx's close awaits its worker Shutdown acknowledgement.
                // https://docs.rs/sqlx-sqlite/0.8.6/src/sqlx_sqlite/connection/mod.rs.html
                if db.close().await.is_err() {
                    failed = true;
                }
            }
        }
        self.uncertain = unresolved || failed;
        ensure!(!self.uncertain, "offline connection cleanup unconfirmed");
        drop(self.copy_charge.take());
        drop(self.source_charge.take());
        Ok(())
    }
}

impl Drop for Connections {
    fn drop(&mut self) {
        if self.uncertain || self.source.is_some() || self.copy.is_some() {
            // No public SQLx API can confirm a dropped bootstrap/worker close.
            // Also covers runtime/task teardown before our continuation finishes.
            // Retain this one canonical file fence until process exit, rather
            // than allowing a later SQLite descriptor to race unknown cleanup.
            if let Some(owner) = self.owner.take() {
                std::mem::forget(owner);
            }
            for charge in [self.source_charge.take(), self.copy_charge.take()]
                .into_iter()
                .flatten()
            {
                std::mem::forget(charge);
            }
            tracing::error!(
                "offline SQLite cleanup unconfirmed; source ownership retained until process exit"
            );
        }
    }
}

fn options(path: &Path, immutable: bool) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .create_if_missing(false)
        .busy_timeout(Duration::from_secs(30))
        .row_buffer_size(1)
        .statement_cache_capacity(0)
        .immutable(immutable)
        // These initial PRAGMAs do not evaluate source expressions. Install the
        // work budget before subsequent schema-dependent PRAGMAs or SQL.
        .pragma("trusted_schema", "OFF")
        .disable_statement_logging()
}

async fn policy(
    db: &mut SqliteConnection,
    cancel: CancellationToken,
    deadline: Option<Instant>,
) -> Result<()> {
    {
        let mut handle = db.lock_handle().await?;
        // VDBE work is finite per connection. Allocation-amplifying source
        // functions are refused by schema qualification before integrity/copy;
        // a progress callback alone cannot interrupt a single such function.
        // https://www.sqlite.org/c3ref/progress_handler.html
        let mut batches = 0_u64;
        handle.set_progress_handler(10_000, move || {
            batches = batches.saturating_add(1);
            !cancel.is_cancelled()
                && deadline.is_none_or(|deadline| batches <= 200_000 && Instant::now() < deadline)
        });
    }
    // No memory-backed temp store, mmap, source writer, or host connection pool.
    for sql in [
        "PRAGMA cache_size=-2048",
        "PRAGMA temp_store=FILE",
        "PRAGMA mmap_size=0",
    ] {
        sea_orm::sqlx::query(sql).execute(&mut *db).await?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests;
