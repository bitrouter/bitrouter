pub mod admin_auth;
pub mod agent_proxy;
pub mod agents;
pub mod auth;
pub mod cloud_auth;
pub mod cloud_credentials;
pub mod key;
pub mod models;
pub mod policy;
pub mod providers;
pub mod route;
pub mod tools;
pub mod update_check;
pub mod wallet;

/// Output format for read commands.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}
