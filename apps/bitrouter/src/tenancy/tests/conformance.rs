//! Literal remaining M0 scenarios through the existing authority actor.
use super::*;
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::{Activation, Configuration, ConfigurationRevision, ResourceRef},
    identity::{RevisionId, Timestamp},
};
use std::sync::Arc;

mod authority;
mod configuration;

async fn denied(
    fixture: &Fixture,
    caller: Caller,
    command: Command,
    code: ErrorCode,
) -> Result<()> {
    match fixture.service.execute(caller, command).await {
        Err(error) => ensure!(
            error.0.code == code,
            "unexpected refusal: {:?}",
            error.0.code
        ),
        Ok(StoredResponse::Failure(error)) => {
            ensure!(
                error.code == code,
                "unexpected stored refusal: {:?}",
                error.code
            );
        }
        _ => anyhow::bail!("forbidden command unexpectedly succeeded"),
    }
    ensure!(
        fixture.service.current().is_ok(),
        "ordinary refusal fenced authority"
    );
    Ok(())
}

async fn binding(
    fixture: &Fixture,
    operation: &str,
    scope: GrantScope,
    role: TenantRole,
    expires_at: Timestamp,
) -> Result<()> {
    let snapshot = fixture.service.current().map_err(failure)?;
    let old = snapshot
        .bindings
        .get(&(fixture.tenant.clone(), fixture.binding.clone()))
        .context("current binding")?;
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Binding(
                    fixture.tenant.clone(),
                    fixture.binding.clone(),
                    BindingPut {
                        operation_id: operation.try_into()?,
                        expected_revision: Some(old.revision.clone()),
                        principal_id: fixture.principal.clone(),
                        role,
                        scope,
                        expires_at,
                        expected_management_owner: old
                            .managed_by
                            .as_ref()
                            .map(|owner| owner.principal.clone()),
                        installation_id: fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    Ok(())
}

fn caller(fixture: &Fixture, credential: &CredentialIssued) -> Result<Caller> {
    fixture
        .service
        .authenticate(credential.secret.as_deref().context("issued secret")?)
        .map_err(failure)
}

async fn finish(fixture: Fixture, checked: Result<()>) -> Result<()> {
    // Always settle the real actor before releasing the database, including a failed assertion.
    let shutdown = fixture.service.shutdown().await;
    let close = fixture.db.close().await;
    checked?;
    shutdown?;
    close?;
    Ok(())
}
