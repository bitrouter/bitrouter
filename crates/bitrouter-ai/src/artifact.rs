//! Build identity shared by control clients and the application.
//!
//! These are data contracts; feature discovery and build metadata generation
//! belong to the application build script.

use serde::{Deserialize, Serialize};

/// Distribution selected by the application's effective Cargo capabilities.
/// `GatewayShared` is the headless gateway with PostgreSQL shared-mode capability.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistributionProfile {
    Full,
    Gateway,
    #[serde(rename = "gateway-shared")]
    GatewayShared,
    Custom,
}

impl DistributionProfile {
    /// Stable spelling for human-readable and wire-facing output.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Gateway => "gateway",
            Self::GatewayShared => "gateway-shared",
            Self::Custom => "custom",
        }
    }
}

/// Identity of a compiled application, independent of its runtime settings.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactIdentity {
    pub version: String,
    /// Source commit when available; an unknown revision serializes as null.
    #[serde(default)]
    pub source_revision: Option<String>,
    pub target: String,
    /// Sorted effective application features, including transitive features but
    /// excluding the `default` selector and the `otel` alias. Can be passed to
    /// Cargo with `--no-default-features` to reproduce a custom capability set.
    pub features: Vec<String>,
    pub profile: DistributionProfile,
    /// Build fingerprint including the existing handoff and migration sources.
    pub handoff_build_id: String,
}
