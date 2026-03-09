pub mod config;
pub mod env;
pub mod error;
pub mod registry;
pub mod routing;

pub use config::{
    ApiProtocol, AuthConfig, BitrouterConfig, ControlEndpoint, ModelConfig, ModelEndpoint,
    ProviderConfig, RoutingStrategy, ServerConfig,
};
pub use error::{ConfigError, Result};
pub use registry::{BuiltinProvider, builtin_provider_defs};
pub use routing::{ConfigRoutingTable, ResolvedTarget};
