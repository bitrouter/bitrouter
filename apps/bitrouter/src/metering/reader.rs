//! Read-side access to the metering database for CLI queries.
//!
//! The daemon chdirs into the bitrouter home before serving, so the
//! default `sqlite://./bitrouter.db` lands in `<home>/bitrouter.db`. A
//! CLI invocation runs from whatever directory the user happens to be
//! in, so this module re-anchors relative SQLite paths against the
//! resolved config home before connecting.
//!
//! Everything here is **best-effort**: agent-facing surfaces
//! (`status --agent`, `events`) must never fail a session over a
//! missing or unreadable database, so absence is `None`, not an error.

use std::path::Path;

use crate::metering::MeteringStore;
use crate::paths::ConfigSource;

/// Open the metering store for read-only CLI queries.
///
/// Returns `None` when the config can't be loaded, when a SQLite file
/// URL points at a file that doesn't exist yet (a fresh install that
/// has never served a request), or when the connection fails. Never
/// creates the database file.
pub async fn open_readonly(source: &ConfigSource) -> Option<MeteringStore> {
    let config = crate::paths::load_config(source).await.ok()?;
    let url = resolve_sqlite_url(&config.database.url, source.home())?;
    let db = crate::db::connect(&url).await.ok()?;
    Some(MeteringStore::new(db))
}

/// Re-anchor a relative SQLite file URL against the config home and pin
/// it read-only. Non-SQLite URLs (Postgres / MySQL) pass through
/// unchanged; a SQLite file that doesn't exist yields `None` so the
/// read side never creates an empty database.
fn resolve_sqlite_url(url: &str, home: &Path) -> Option<String> {
    let after_scheme = match url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
    {
        Some(rest) => rest,
        // Not SQLite — a server-backed URL needs no path anchoring.
        None => return Some(url.to_string()),
    };
    let path_part = after_scheme.split('?').next().unwrap_or(after_scheme);
    if path_part.is_empty() || path_part == ":memory:" {
        return None;
    }
    // Strip a leading `./` so the joined URL stays canonical
    // (`join` would otherwise keep the literal `.` component).
    let path = Path::new(path_part.strip_prefix("./").unwrap_or(path_part));
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        home.join(path)
    };
    if !absolute.exists() {
        return None;
    }
    // `mode=ro` keeps the read side from ever creating or writing the
    // file (`db::connect` only appends `mode=rwc` when no mode is set).
    Some(format!("sqlite://{}?mode=ro", absolute.display()))
}

#[cfg(test)]
mod tests {
    use super::resolve_sqlite_url;
    use std::path::Path;

    #[test]
    fn non_sqlite_urls_pass_through() {
        assert_eq!(
            resolve_sqlite_url("postgres://db.internal/bitrouter", Path::new("/h")),
            Some("postgres://db.internal/bitrouter".to_string())
        );
    }

    #[test]
    fn memory_and_empty_are_none() {
        assert_eq!(resolve_sqlite_url("sqlite::memory:", Path::new("/h")), None);
        assert_eq!(resolve_sqlite_url("sqlite://", Path::new("/h")), None);
    }

    #[test]
    fn missing_file_is_none() {
        assert_eq!(
            resolve_sqlite_url(
                "sqlite://./does-not-exist.db",
                Path::new("/nonexistent-home")
            ),
            None
        );
    }

    #[test]
    fn relative_path_anchors_to_home_when_present() {
        let dir = std::env::temp_dir().join(format!("br-reader-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("bitrouter.db");
        let _ = std::fs::write(&db, b"");
        let got = resolve_sqlite_url("sqlite://./bitrouter.db", &dir);
        assert_eq!(got, Some(format!("sqlite://{}?mode=ro", db.display())));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
