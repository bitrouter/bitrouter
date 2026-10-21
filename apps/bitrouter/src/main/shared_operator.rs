//! Explicit remote operator commands; no local service or credential discovery.

use std::path::PathBuf;

use clap::{Args, Subcommand};

#[derive(Args)]
pub(super) struct Target {
    /// Explicit HTTPS or loopback HTTP operator endpoint.
    #[arg(long)]
    endpoint: String,
    /// Literal environment variable containing the separate operator token.
    #[arg(long)]
    token_env: String,
}

#[cfg(feature = "multi-instance")]
fn environment_name(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 128
        || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return Err("token-env must name a literal environment variable".into());
    }
    Ok(value.into())
}

#[derive(Subcommand)]
pub(super) enum Action {
    /// Submit one explicit JSON mutation; retain its operation ID after uncertainty.
    Apply {
        #[command(flatten)]
        target: Target,
        /// Literal JSON Mutation, or '-' for stdin; bounded to 2 MiB.
        #[arg(long)]
        input: PathBuf,
    },
    /// Read your original operation receipt once; unresolved is not rollback.
    Operation {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        operation_id: String,
    },
    /// Read the active configuration and correlation-key generations.
    Revision {
        #[command(flatten)]
        target: Target,
    },
    /// Read one tenant policy.
    Tenant {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        id: String,
    },
    /// Read one resource-pool policy.
    Resource {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        id: String,
    },
    /// Read principal metadata; requires the global principal grant.
    Principal {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        id: String,
    },
    /// Read credential binding/version metadata, never its secret.
    Credential {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        account: String,
    },
    /// Read durable execution status without repeating inference.
    Execution {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        id: String,
    },
}

#[cfg(feature = "multi-instance")]
use anyhow::{Context, Result, ensure};
#[cfg(feature = "multi-instance")]
use bitrouter::output::{Output, reports::shared::OperatorReport};
#[cfg(feature = "multi-instance")]
use bitrouter_ai::gateway::operator::Mutation;
#[cfg(feature = "multi-instance")]
use bitrouter_sdk::operator::OperatorClient;
#[cfg(feature = "multi-instance")]
use tokio::io::AsyncReadExt as _;

#[cfg(feature = "multi-instance")]
pub(super) async fn run(action: Action, output: &Output) -> Result<()> {
    let report = execute(&action, |name| std::env::var(name).ok()).await?;
    match &report {
        OperatorReport::Receipt(receipt) => output.emit(&report).with_context(|| {
            format!(
                "operator mutation committed; output failed; query original operation {}",
                receipt.operation_id
            )
        })?,
        _ => output.emit(&report)?,
    }
    Ok(())
}

#[cfg(feature = "multi-instance")]
async fn execute(
    action: &Action,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<OperatorReport> {
    let target = match action {
        Action::Apply { target, .. }
        | Action::Operation { target, .. }
        | Action::Revision { target }
        | Action::Tenant { target, .. }
        | Action::Resource { target, .. }
        | Action::Principal { target, .. }
        | Action::Credential { target, .. }
        | Action::Execution { target, .. } => target,
    };
    environment_name(&target.token_env)
        .map_err(|_| anyhow::anyhow!("invalid operator token environment reference"))?;
    // Non-Unicode environment values become a static error, never VarError's
    // secret-bearing source. This is the only environment lookup in this path.
    let token =
        lookup(&target.token_env).context("operator token environment variable is unavailable")?;
    let client = OperatorClient::new(&target.endpoint, &token)?;
    match action {
        Action::Apply { input, .. } => {
            let mutation = if input == std::path::Path::new("-") {
                read_mutation(tokio::io::stdin()).await?
            } else {
                read_mutation(
                    tokio::fs::File::open(input)
                        .await
                        .context("opening explicit operator mutation")?,
                )
                .await?
            };
            Ok(OperatorReport::Receipt(Box::new(
                client.apply(&mutation).await?,
            )))
        }
        Action::Operation { operation_id, .. } => Ok(OperatorReport::Operation(
            client.operation(operation_id).await?,
        )),
        Action::Revision { .. } => Ok(OperatorReport::Revision(client.revision().await?)),
        Action::Tenant { id, .. } => Ok(OperatorReport::Tenant(client.tenant(id).await?)),
        Action::Resource { id, .. } => Ok(OperatorReport::Resource(client.resource(id).await?)),
        Action::Principal { id, .. } => Ok(OperatorReport::Principal(client.principal(id).await?)),
        Action::Credential {
            provider, account, ..
        } => Ok(OperatorReport::Credential(
            client.credential(provider, account).await?,
        )),
        Action::Execution { id, .. } => Ok(OperatorReport::Execution(Box::new(
            client.execution(id).await?,
        ))),
    }
}

#[cfg(feature = "multi-instance")]
async fn read_mutation(reader: impl tokio::io::AsyncRead + Unpin) -> Result<Mutation> {
    const LIMIT: u64 = 2 * 1024 * 1024;
    let mut bytes = Vec::new();
    reader
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .await
        .context("reading explicit operator mutation")?;
    ensure!(
        bytes.len() as u64 <= LIMIT,
        "operator mutation exceeds 2 MiB"
    );
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid operator mutation JSON"))
}

#[cfg(all(test, feature = "multi-instance"))]
#[path = "shared_operator/tests.rs"]
mod tests;
