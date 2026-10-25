//! Binary-only adapter for explicit resource configuration commands over HTTP.
//! Local validation reads only the supplied JSON file. Remote operations never
//! load a service configuration, open a database, or discover a local daemon.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bitrouter::contexts::RemoteContext;
use bitrouter::gateway::error::BitrouterError;
use bitrouter::output::{CliReport, Output};
use bitrouter_ai::tenancy::{
    configuration::{Activation, ConfigurationPut},
    identity::{InstallationId, OperationId, RevisionId, TenantId},
    validation::Validate,
};
use bitrouter_sdk::tenancy::{TenancyClient, TenancyOptions};
use clap::{Args, Subcommand};
use serde::de::DeserializeOwned;
use tokio::io::AsyncReadExt as _;

#[path = "tenancy/report.rs"]
mod report;
use report::{Completion, Recovery, Report};

const INPUT_LIMIT: u64 = 2 * 1024 * 1024;

#[derive(Args)]
pub(super) struct Target {
    /// HTTPS origin/control endpoint, or loopback HTTP; conflicts with --context.
    #[arg(long, requires = "token_env", conflicts_with = "context")]
    endpoint: Option<String>,
    /// Name of the environment variable holding the credential, never its value.
    #[arg(long, requires = "endpoint", conflicts_with = "context")]
    token_env: Option<String>,
}

#[derive(Args)]
pub(super) struct TenantRead {
    #[command(flatten)]
    target: Target,
    /// Stable authority store identity; never discovered or substituted.
    #[arg(long, value_parser = parse_id::<InstallationId>)]
    installation_id: InstallationId,
    /// Exact tenant identity.
    #[arg(long, value_parser = parse_id::<TenantId>)]
    tenant: TenantId,
}

#[derive(Subcommand)]
pub(super) enum Action {
    /// Discover tenancy capabilities; this does not authorize dispatch.
    Capabilities {
        #[command(flatten)]
        target: Target,
    },
    /// Read the original caller's nonsecret operation result once.
    Operation {
        #[command(flatten)]
        target: Target,
        #[arg(long, value_parser = parse_id::<InstallationId>)]
        installation_id: InstallationId,
        #[arg(long, value_parser = parse_id::<OperationId>)]
        operation_id: OperationId,
    },
    /// Validate, prepare, read, or activate tenant configuration revisions.
    Configuration {
        #[command(subcommand)]
        action: ConfigurationAction,
    },
}

#[derive(Subcommand)]
pub(super) enum ConfigurationAction {
    /// Check a ConfigurationPut JSON file locally: shape/bounds only, no server checks.
    Validate {
        /// Literal JSON file also accepted by apply; no YAML or environment expansion.
        #[arg(long)]
        input: PathBuf,
    },
    /// Prepare an immutable draft from a complete ConfigurationPut JSON file.
    Apply {
        #[command(flatten)]
        target: Target,
        #[arg(long, value_parser = parse_id::<TenantId>)]
        tenant: TenantId,
        /// JSON with operation_id, installation_id and expected_authority_revision.
        #[arg(long)]
        input: PathBuf,
    },
    /// Read exactly one retained configuration under current authority.
    Read {
        #[command(flatten)]
        request: TenantRead,
        #[arg(long, value_parser = parse_id::<RevisionId>)]
        revision: RevisionId,
    },
    /// Activate or roll back using a complete Activation JSON file with explicit CAS.
    Activate {
        #[command(flatten)]
        target: Target,
        #[arg(long, value_parser = parse_id::<TenantId>)]
        tenant: TenantId,
        /// JSON with both observed revisions, operation_id and installation_id.
        #[arg(long)]
        input: PathBuf,
    },
    /// Read the active revision and current authority without changing a submission.
    Active {
        #[command(flatten)]
        request: TenantRead,
    },
}

fn parse_id<T: TryFrom<String>>(value: &str) -> std::result::Result<T, String> {
    T::try_from(value.to_owned()).map_err(|_| "invalid tenancy identifier".into())
}

pub(super) async fn run(action: &Action, context: Option<&str>, output: &Output) -> Result<()> {
    let completion = execute(action, context, |name| std::env::var(name).ok()).await?;
    output.emit(&completion.report).with_context(|| {
        completion.recovery.as_ref().map_or_else(
            || "writing tenancy command output".to_owned(),
            |recovery| {
                format!(
                    "tenancy response received but output failed; {}",
                    recovery.hint()
                )
            },
        )
    })?;
    let exit_code = completion.report.exit_code();
    if exit_code != 0 {
        // Keep one output object; unresolved/rejected operations are not success.
        std::process::exit(exit_code);
    }
    Ok(())
}

async fn execute(
    action: &Action,
    context: Option<&str>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Completion> {
    let target = match action {
        Action::Configuration {
            action: ConfigurationAction::Validate { input },
        } => {
            if context.is_some() {
                return Err(BitrouterError::bad_request(
                    "configuration validate is local shape validation; omit --context",
                )
                .into());
            }
            read_input::<ConfigurationPut>(input).await?;
            return Ok(Completion::read(Report::validated()));
        }
        Action::Capabilities { target }
        | Action::Operation { target, .. }
        | Action::Configuration {
            action:
                ConfigurationAction::Apply { target, .. } | ConfigurationAction::Activate { target, .. },
        } => target,
        Action::Configuration {
            action:
                ConfigurationAction::Read { request, .. } | ConfigurationAction::Active { request },
        } => &request.target,
    };
    let target = resolve_target(target, context)?;
    // Drop VarError, which may contain a non-Unicode secret value. Only the
    // selected variable is read; no provider credentials or dotenv are loaded.
    let token = lookup(&target.token_env).ok_or_else(|| {
        BitrouterError::Unauthorized("tenancy token environment variable is unavailable".into())
    })?;
    let client = TenancyClient::with_options(
        &target.endpoint,
        &token,
        TenancyOptions {
            system_proxy: true,
            ..TenancyOptions::default()
        },
    )
    .map_err(|error| report::client_error(error, None))?;
    execute_remote(action, &client).await
}

fn resolve_target(target: &Target, context: Option<&str>) -> Result<RemoteContext> {
    let remote = match (context, &target.endpoint, &target.token_env) {
        (Some("local"), _, _) => {
            return Err(BitrouterError::bad_request(
                "tenancy HTTP commands require a named remote context or --endpoint with --token-env; local daemon discovery is unavailable",
            )
            .into());
        }
        (Some(name), None, None) => bitrouter::contexts::resolve(name)?
            .context("tenancy HTTP commands require an explicit remote target")?,
        (None, Some(endpoint), Some(token_env)) => RemoteContext {
            endpoint: endpoint.clone(),
            token_env: token_env.clone(),
        },
        _ => {
            return Err(BitrouterError::bad_request(
                "choose either --context NAME or --endpoint URL with --token-env NAME",
            )
            .into());
        }
    };
    let mut bytes = remote.token_env.bytes();
    if !bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(BitrouterError::bad_request(
            "token-env must name a literal environment variable",
        )
        .into());
    }
    Ok(remote)
}

async fn execute_remote(action: &Action, client: &TenancyClient) -> Result<Completion> {
    let report = match action {
        Action::Capabilities { .. } => Report::Capabilities(
            client
                .capabilities()
                .await
                .map_err(|e| report::client_error(e, None))?,
        ),
        Action::Operation {
            installation_id,
            operation_id,
            ..
        } => Report::Operation(
            client
                .operation(installation_id, operation_id)
                .await
                .map_err(|e| report::client_error(e, None))?,
        ),
        Action::Configuration { action } => match action {
            ConfigurationAction::Apply { tenant, input, .. } => {
                let input: ConfigurationPut = read_input(input).await?;
                let recovery = Recovery::new(&input.installation_id, &input.operation_id);
                let result = client
                    .put_configuration(tenant, &input)
                    .await
                    .map_err(|e| report::client_error(e, Some(&recovery)))?;
                return Ok(Completion::mutation(
                    Report::Configuration(Box::new(result)),
                    recovery,
                ));
            }
            ConfigurationAction::Activate { tenant, input, .. } => {
                let input: Activation = read_input(input).await?;
                let recovery = Recovery::new(&input.installation_id, &input.operation_id);
                let result = client
                    .activate(tenant, &input)
                    .await
                    .map_err(|e| report::client_error(e, Some(&recovery)))?;
                return Ok(Completion::mutation(Report::Operation(result), recovery));
            }
            ConfigurationAction::Read { request, revision } => Report::Configuration(Box::new(
                client
                    .configuration(&request.installation_id, &request.tenant, revision)
                    .await
                    .map_err(|e| report::client_error(e, None))?,
            )),
            ConfigurationAction::Active { request } => Report::Active(
                client
                    .active_revision(&request.installation_id, &request.tenant)
                    .await
                    .map_err(|e| report::client_error(e, None))?,
            ),
            ConfigurationAction::Validate { .. } => {
                anyhow::bail!("local shape validation cannot use an HTTP client");
            }
        },
    };
    Ok(Completion::read(report))
}

async fn read_input<T: DeserializeOwned + Validate>(path: &Path) -> Result<T> {
    let file = tokio::fs::File::open(path)
        .await
        .context("opening explicit tenancy JSON file")?;
    if !file
        .metadata()
        .await
        .context("reading tenancy input metadata")?
        .is_file()
    {
        return Err(
            BitrouterError::bad_request("tenancy input must be a regular JSON file").into(),
        );
    }
    read_json(file).await
}

async fn read_json<T: DeserializeOwned + Validate>(
    reader: impl tokio::io::AsyncRead + Unpin,
) -> Result<T> {
    let mut bytes = Vec::new();
    reader
        .take(INPUT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await
        .context("reading explicit tenancy JSON")?;
    if bytes.len() as u64 > INPUT_LIMIT {
        return Err(BitrouterError::bad_request("tenancy input exceeds 2 MiB").into());
    }
    // Typed structs reject duplicate/unknown fields, including nested objects.
    // Never include serde's input-bearing error in the CLI diagnostic chain.
    let input: T = serde_json::from_slice(&bytes)
        .map_err(|_| BitrouterError::bad_request("invalid tenancy JSON contract"))?;
    input.validate().map_err(BitrouterError::bad_request)?;
    Ok(input)
}

#[cfg(test)]
#[path = "tenancy/tests.rs"]
mod tests;
