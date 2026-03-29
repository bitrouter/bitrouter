pub mod compat;
pub mod config;
pub mod detect;
pub mod env;
pub mod error;
pub mod registry;
pub mod routing;
pub mod skill;
pub mod tool;
pub mod writer;

pub use bitrouter_core::routers::routing_table::ApiProtocol;
#[cfg(feature = "mpp-solana")]
pub use config::SolanaMppConfig;
pub use config::{
    AuthConfig, BitrouterConfig, ControlEndpoint, DatabaseConfig, InputTokenPricing, Modality,
    ModelConfig, ModelEndpoint, ModelInfo, ModelPricing, MppConfig, MppNetworksConfig,
    OutputTokenPricing, ProviderConfig, RoutingStrategy, ServerConfig, TempoMppConfig, ToolConfig,
    ToolEndpoint,
};
pub use detect::{DetectedProvider, detect_providers, detect_providers_from_env};
pub use error::{ConfigError, Result};
pub use registry::{BuiltinProvider, builtin_provider_defs};
pub use routing::{ConfigRoutingTable, ConfigToolRoutingTable, ResolvedTarget, ResolvedToolTarget};
pub use writer::{CustomProviderInit, InitOptions, InitResult, write_init_config};
