//! Plugin identity and database migrations — shared library code (crate root).
//!
//! The `Plugin` trait itself lives in [`crate::app`] (it references
//! `AppBuilder`); this module carries the value types it depends on.

use std::fmt;

/// A plugin identifier — a crate name or a user-chosen string. Used as the key
/// for `PipelineContext` metadata and for config mapping.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginId(String);

impl PluginId {
    /// Wrap a string as a plugin id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for PluginId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for PluginId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The body of a migration.
pub enum MigrationContent {
    /// Raw SQL, run as-is.
    Sql(String),
}

impl fmt::Debug for MigrationContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MigrationContent::Sql(_) => f.write_str("MigrationContent::Sql(..)"),
        }
    }
}

/// One migration declared by a plugin. The plugin also declares the `tables` it
/// touches — the isolation contract is "a hook may only touch tables it
/// declared a migration for".
#[derive(Debug)]
pub struct MigrationItem {
    /// Ordering key. App collects all migrations and runs them by ascending
    /// version.
    pub version: i64,
    /// The migration body.
    pub content: MigrationContent,
    /// The tables this migration creates / alters.
    pub tables: Vec<String>,
}

impl MigrationItem {
    /// Build a raw-SQL migration.
    pub fn sql(version: i64, tables: Vec<String>, sql: impl Into<String>) -> Self {
        Self {
            version,
            content: MigrationContent::Sql(sql.into()),
            tables,
        }
    }
}
