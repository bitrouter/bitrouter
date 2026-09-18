use std::net::SocketAddr;
use std::path::PathBuf;

use bitrouter_guardrails::checker;
use bitrouter_regex_checker::{VERSION, adapter, startup};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "bitrouter-regex-checker", version = VERSION)]
struct Cli {
    /// Loopback listener used by the request-check host.
    #[arg(long, default_value = "127.0.0.1:8081")]
    listen: SocketAddr,

    /// Strict input-only guardrail rules YAML file.
    #[arg(long)]
    rules: PathBuf,

    /// Read an optional HTTP bearer token from this environment variable.
    #[arg(long)]
    credential_env: Option<String>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("bitrouter-regex-checker: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let rules = startup::load_rules(&cli.rules).map_err(|error| error.to_string())?;
    let credential = match cli.credential_env {
        Some(name) => {
            let token = std::env::var(name)
                .ok()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "credential environment variable is missing or empty".to_owned())?;
            Some(adapter::BearerCredential::new(token).map_err(|error| error.to_string())?)
        }
        None => None,
    };
    let callback = checker::callback(rules);
    let implementation_version = format!("bitrouter-regex-checker/{VERSION}");
    let app = adapter::router(callback, credential, implementation_version);
    let listener = tokio::net::TcpListener::bind(cli.listen)
        .await
        .map_err(|_| "cannot bind configured listen address".to_owned())?;
    eprintln!("bitrouter-regex-checker listening on {}", cli.listen);
    axum::serve(listener, app)
        .await
        .map_err(|_| "HTTP server stopped unexpectedly".to_owned())
}
