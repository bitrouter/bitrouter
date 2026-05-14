//! `App` and `AppBuilder` — shared library code (crate root).
//!
//! `App` holds one `Pipeline` per enabled protocol. `AppBuilder` configures
//! each protocol through its own sub-builder. `Plugin` is an **optional**
//! convenience packaging — the atomic unit is a single hook, and hooks can be
//! registered one by one without ever touching a `Plugin`.

use std::sync::Arc;

use crate::error::Result;
use crate::language_model::{self, PipelineBuilder};
use crate::metrics::MetricsStore;
use crate::plugin::{MigrationItem, PluginId};

/// An optional convenience packaging: registers a related set of hooks +
/// migrations into a builder in one call. `Plugin` is **not** a strong,
/// indivisible unit and **not** the only way to register hooks.
pub trait Plugin {
    /// The plugin's identity (for config mapping and logs).
    fn id(&self) -> &PluginId;

    /// Database migrations carried by this plugin. Empty = no database.
    fn migrations(&self) -> Vec<MigrationItem> {
        Vec::new()
    }

    /// Install this plugin's hooks into the builder.
    fn install(&self, app: &mut AppBuilder);
}

/// A fully assembled application: one pipeline per enabled protocol, plus the
/// injected infrastructure and the collected migration set.
pub struct App {
    language_model: Option<Arc<language_model::Pipeline>>,
    #[allow(dead_code)]
    metrics_store: Option<Arc<dyn MetricsStore>>,
    migrations: Vec<MigrationItem>,
}

impl App {
    /// Start configuring an application.
    pub fn builder() -> AppBuilder {
        AppBuilder::new()
    }

    /// The `language_model` pipeline, if that protocol was configured.
    pub fn language_model(&self) -> Option<&Arc<language_model::Pipeline>> {
        self.language_model.as_ref()
    }

    /// The collected migration set (sorted by version).
    pub fn migrations(&self) -> &[MigrationItem] {
        &self.migrations
    }
}

/// Configures an [`App`]. Each protocol is configured through its own
/// sub-builder; `plugin()` is a convenience that drives those sub-builders for
/// you.
pub struct AppBuilder {
    language_model: PipelineBuilder,
    metrics_store: Option<Arc<dyn MetricsStore>>,
    migrations: Vec<MigrationItem>,
}

impl AppBuilder {
    /// A fresh, empty builder.
    pub fn new() -> Self {
        Self {
            language_model: PipelineBuilder::new(),
            metrics_store: None,
            migrations: Vec::new(),
        }
    }

    /// Configure the `language_model` protocol pipeline.
    pub fn language_model<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(&mut PipelineBuilder),
    {
        configure(&mut self.language_model);
        self
    }

    /// Inject the `MetricsStore` infrastructure (read by PreRequest hooks,
    /// written by `ReceiptRecorder`).
    pub fn metrics_store(mut self, store: Arc<dyn MetricsStore>) -> Self {
        self.metrics_store = Some(store);
        self
    }

    /// Install a `Plugin` convenience package. Equivalent to calling its hook
    /// registrations one by one.
    pub fn plugin(mut self, plugin: impl Plugin) -> Self {
        self.migrations.extend(plugin.migrations());
        plugin.install(&mut self);
        self
    }

    /// Mutable access to the `language_model` sub-builder — the entry point a
    /// `Plugin::install` implementation uses.
    pub fn language_model_builder(&mut self) -> &mut PipelineBuilder {
        &mut self.language_model
    }

    /// Add migrations directly (used by `Plugin::install` when it wants to add
    /// migrations beyond what `Plugin::migrations` declared).
    pub fn add_migrations(&mut self, migrations: impl IntoIterator<Item = MigrationItem>) {
        self.migrations.extend(migrations);
    }

    /// Finalise into an [`App`]. Builds a pipeline for each protocol that was
    /// configured (the `language_model` pipeline needs at least a routing table
    /// and an executor).
    pub fn build(mut self) -> Result<App> {
        let language_model = if self.language_model.is_configured() {
            Some(Arc::new(self.language_model.build()?))
        } else {
            None
        };

        self.migrations.sort_by_key(|m| m.version);

        Ok(App {
            language_model,
            metrics_store: self.metrics_store,
            migrations: self.migrations,
        })
    }
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self::new()
    }
}
