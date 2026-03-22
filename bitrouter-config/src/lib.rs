pub mod agent;
pub mod config;
pub mod detect;
pub mod env;
pub mod error;
pub mod model;
pub mod registry;
pub mod routing;
pub mod tool;
pub mod writer;

pub use config::{BitrouterConfig, ControlEndpoint, DatabaseConfig, ServerConfig};
pub use detect::{DetectedProvider, detect_providers, detect_providers_from_env};
pub use error::{ConfigError, Result};
pub use model::{
    ApiProtocol, AuthConfig, InputTokenPricing, Modality, ModelConfig, ModelEndpoint, ModelInfo,
    ModelPricing, OutputTokenPricing, ProviderConfig, RoutingStrategy,
};
pub use registry::{BuiltinProvider, builtin_provider_defs};
pub use routing::{ConfigRoutingTable, ResolvedTarget};
pub use writer::{CustomProviderInit, InitOptions, InitResult, write_init_config};
