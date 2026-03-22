pub mod config;
pub mod detect;
pub mod env;
pub mod error;
pub mod registry;
pub mod routing;
pub mod writer;

#[cfg(feature = "mpp-solana")]
pub use config::SolanaMppConfig;
pub use config::{
    ApiProtocol, AuthConfig, BitrouterConfig, ControlEndpoint, DatabaseConfig, InputTokenPricing,
    Modality, ModelConfig, ModelEndpoint, ModelInfo, ModelPricing, MppConfig, MppNetworksConfig,
    OutputTokenPricing, ProviderConfig, RoutingStrategy, ServerConfig, TempoMppConfig,
};
pub use detect::{DetectedProvider, detect_providers, detect_providers_from_env};
pub use error::{ConfigError, Result};
pub use registry::{BuiltinProvider, builtin_provider_defs};
pub use routing::{ConfigRoutingTable, ResolvedTarget};
pub use writer::{CustomProviderInit, InitOptions, InitResult, write_init_config};
