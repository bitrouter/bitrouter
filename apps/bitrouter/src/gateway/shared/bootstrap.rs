//! Explicit local bootstrap entry used by the stock executable.

use crate::runtime_crypto::DeploymentKey;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::operator::{BootstrapPlan, BootstrapReceipt, Grant};
use tokio::io::AsyncReadExt;

use super::{
    authority::Authority,
    operator::{self, Actor},
    store::{self, Store},
};

const MAX_PLAN_BYTES: u64 = 2 * 1024 * 1024;

pub(crate) async fn run(
    config: &Path,
    plan: &Path,
    operator_id: &str,
    operation_id: &str,
    global_principals: bool,
) -> Result<BootstrapReceipt> {
    operator::validate_handle(operation_id)?;
    let mut grants = [Grant::PolicyWrite, Grant::CredentialWrite, Grant::Publish]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    if global_principals {
        grants.insert(Grant::GlobalPrincipalWrite);
    }
    let actor = Actor::new(operator_id.into(), grants)?;
    let plan = if plan == Path::new("-") {
        read_plan(tokio::io::stdin()).await?
    } else {
        read_plan(
            tokio::fs::File::open(plan)
                .await
                .context("opening explicit bootstrap plan")?,
        )
        .await?
    };
    ensure!(
        plan.principals.is_empty() || global_principals,
        "bootstrap principal creation requires --global-principals; virtual keys are database-wide"
    );
    let process = crate::config::shared_process::select_shared(config).await?;
    let key = DeploymentKey::from_environment(&process.shared.deployment_key_env)?;
    let timeout = Duration::from_millis(process.shared.transaction_timeout_ms);
    // Bootstrap needs one transaction at a time and never reserves a heartbeat
    // connection or creates a serving owner.
    let database = store::connect(&process.database_url, 1, timeout).await?;
    let result = async {
        let authority = Authority::new(
            Store::new(database.clone(), &process.shared.namespace, timeout)?,
            key,
        );
        operator::bootstrap::initialize(authority, &actor, operation_id, &plan).await
    }
    .await;
    let closed = database.close().await;
    let receipt = result?;
    closed.context("closing bootstrap database pool")?;
    Ok(receipt)
}

async fn read_plan(reader: impl tokio::io::AsyncRead + Unpin) -> Result<BootstrapPlan> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_PLAN_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .context("reading explicit bootstrap plan")?;
    ensure!(
        bytes.len() as u64 <= MAX_PLAN_BYTES,
        "bootstrap plan exceeds 2 MiB"
    );
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid shared bootstrap JSON plan"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bootstrap_plan_is_bounded_and_parse_errors_are_redacted() -> Result<()> {
        let secret = br#"{"credentials":[{"credential":"fixture-secret"}]}"#;
        let error = read_plan(&secret[..])
            .await
            .err()
            .context("accepted invalid plan")?;
        ensure!(error.to_string() == "invalid shared bootstrap JSON plan");
        let oversized = vec![b' '; MAX_PLAN_BYTES as usize + 1];
        let error = read_plan(&oversized[..])
            .await
            .err()
            .context("accepted oversized plan")?;
        ensure!(error.to_string() == "bootstrap plan exceeds 2 MiB");
        let valid = br#"{"configuration":{"format":1,"routing":{},"policies":[],"policy_lock":null,"credentials":[],"require_known_pricing":false}}"#;
        let mut boundary = valid.to_vec();
        boundary.resize(MAX_PLAN_BYTES as usize, b' ');
        ensure!(read_plan(&boundary[..]).await?.configuration.format == 1);
        Ok(())
    }

    #[tokio::test]
    async fn invalid_bootstrap_identity_rejects_before_any_file_access() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let absent = directory.path().join("absent");
        for (actor, operation) in [("invalid actor", "root"), ("actor", "invalid operation")] {
            let error = run(&absent, &absent, actor, operation, false)
                .await
                .err()
                .context("accepted invalid bootstrap identity")?;
            ensure!(
                error
                    .downcast_ref::<super::super::operator::Failure>()
                    .is_some()
            );
        }
        ensure!(std::fs::read_dir(directory.path())?.next().is_none());
        Ok(())
    }
}
