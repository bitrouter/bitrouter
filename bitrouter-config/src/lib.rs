pub mod config;
pub mod detect;
pub mod env;
pub mod error;
pub mod registry;
pub mod routing;
pub mod writer;

pub use bitrouter_core::routers::routing_table::ApiProtocol;
#[cfg(feature = "mpp-solana")]
pub use config::SolanaMppConfig;
pub use config::{
    AgentConfig, AgentProtocol, AuthConfig, BinaryArchive, BitrouterConfig, ControlEndpoint,
    DatabaseConfig, Distribution, Endpoint, InputTokenPricing, Modality, ModelConfig, ModelInfo,
    ModelPricing, MppConfig, MppNetworksConfig, OutputTokenPricing, ProviderConfig,
    RoutingStrategy, ServerConfig, TempoMppConfig, ToolConfig,
};
pub use detect::{DetectedProvider, detect_providers, detect_providers_from_env};
pub use error::{ConfigError, Result};
pub use registry::{
    BuiltinProvider, BuiltinToolProvider, builtin_agent_defs, builtin_provider_defs,
    builtin_tool_provider_defs,
};
pub use routing::{
    ConfigAgentRegistry, ConfigRoutingTable, ConfigToolRoutingTable, ResolvedTarget,
};
pub use writer::{
    CustomProviderInit, InitOptions, InitResult, ToolProviderInit, write_agent, write_init_config,
};
