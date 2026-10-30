//! SQLite pool construction with a lifetime-bound file ownership lease.

use std::sync::Arc;

use anyhow::{Context, Result};
use sea_orm::sqlx::{ConnectOptions as _, Sqlite, sqlite::SqliteConnectOptions};
use sea_orm::{ConnectOptions, DatabaseConnection};

/// Queries still use SeaORM. Its underlying pool's lifecycle hook retains the
/// lease across every pool clone and checked-out transaction, including close
/// and cancellation. A close-event observer would release too early: that event
/// announces the start of closing, before active transactions have settled.
/// https://docs.rs/sqlx/0.8.6/sqlx/pool/struct.PoolOptions.html#method.after_connect
pub(super) async fn connect(
    url: &str,
    mut options: ConnectOptions,
    owner: Option<&crate::tenancy::ownership::Lease>,
) -> Result<(DatabaseConnection, bool)> {
    let source = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
        .context("SQLite URL must have exactly one scheme prefix")?;
    anyhow::ensure!(
        !source.starts_with("sqlite:"),
        "repeated SQLite URI schemes are unsupported"
    );
    let mut sqlite = url
        .parse::<SqliteConnectOptions>()?
        .disable_statement_logging();
    let parameters = url.split_once('?').map_or("", |(_, query)| query);
    anyhow::ensure!(
        url::form_urlencoded::parse(parameters.as_bytes())
            .filter(|(key, _)| key == "mode")
            .count()
            <= 1,
        "SQLite URL has duplicate mode parameters"
    );
    let has_mode = |mode: &str| {
        url::form_urlencoded::parse(parameters.as_bytes())
            .any(|(key, value)| key == "mode" && value == mode)
    };
    let literal_memory = source.split('?').next() == Some(":memory:");
    anyhow::ensure!(
        literal_memory
            || !sqlite
                .get_filename()
                .as_os_str()
                .as_encoded_bytes()
                .starts_with(b"file:"),
        "SQLite URLs require a file path, not a nested file: URI"
    );
    let memory = literal_memory || has_mode("memory");
    let readonly = has_mode("ro");
    let lease = if let Some(owner) = owner {
        let resolved =
            crate::tenancy::ownership::resolve_path(sqlite.get_filename(), has_mode("rwc"))?;
        anyhow::ensure!(
            resolved == owner.path,
            "database path changed after acquiring authority ownership"
        );
        Some(owner.clone())
    } else if cfg!(unix) && !memory && !readonly {
        Some(crate::tenancy::ownership::acquire_writer(
            sqlite.get_filename(),
            has_mode("rwc"),
        )?)
    } else {
        None
    };
    if let Some(lease) = &lease {
        sqlite = sqlite.filename(&lease.path);
    } else if cfg!(unix) && !memory {
        let path = crate::tenancy::ownership::resolve_path(sqlite.get_filename(), has_mode("rwc"))?;
        sqlite = sqlite.filename(path);
    }
    // Preserve SeaORM's single-connection SQLite default.
    options.max_connections(1);
    let lease = lease.map(Arc::new);
    let pool = options
        .sqlx_pool_options::<Sqlite>()
        .after_connect(move |_, _| {
            let lease = lease.clone();
            Box::pin(async move {
                // Both the hook stored in PoolOptions and each in-flight
                // connection attempt retain the same lease.
                drop(lease);
                Ok(())
            })
        })
        .connect_with(sqlite)
        .await?;
    Ok((DatabaseConnection::from(pool), readonly))
}
