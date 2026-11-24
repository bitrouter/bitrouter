//! Real actor COMMIT overlaps retained-body decode after its point SQL read.
use super::*;
use crate::tenancy::{
    configuration_store,
    transaction::{Disposition, Phase},
};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    operation::OperationStatus,
    ordered::GenerationProtocol,
};
use std::time::Duration;
use tokio_util::task::AbortOnDropHandle;

struct ReleaseGate(Service);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.cleanup.release_gate();
    }
}

#[tokio::test]
async fn corrupt_body_detection_preserves_prepared_healthy_actor_commit_and_ack() -> Result<()> {
    let fixture = Fixture::new().await?;
    let issued = fixture.issue("overlap-credential").await?;
    let secret = issued.secret.as_deref().context("credential secret")?;
    let held = fixture
        .service
        .authenticate_live(secret)
        .await
        .map_err(failure)?;
    let revision = match fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(
                fixture.tenant.clone(),
                fixture.configuration_input("overlap-draft")?,
            ),
        )
        .await
        .map_err(failure)?
    {
        StoredResponse::Configuration(revision) => revision,
        _ => anyhow::bail!("configuration fixture did not create a revision"),
    };
    let mut stored =
        configuration_store::configuration(&fixture.db, &fixture.tenant, &revision.revision_id)
            .await?
            .context("stored configuration")?;
    fixture
        .service
        .resources
        .validate_compiled(&stored)
        .map_err(|code| failure(code.into()))?;
    // The empty compiler envelope authenticates its complete binding. Preserve
    // its JSON shape and metadata while corrupting only the installation claim.
    *stored
        .compiled
        .pointer_mut("/binding/0")
        .context("compiler binding")? = serde_json::json!("foreign-installation");
    let changed = fixture
        .db
        .execute(Statement::from_sql_and_values(
            fixture.db.get_database_backend(),
            "UPDATE tenancy_configurations SET compiled_json=? WHERE tenant_id=? AND revision_id=?",
            [
                serde_json::to_string(&stored.compiled)?.into(),
                fixture.tenant.as_str().into(),
                revision.revision_id.as_str().into(),
            ],
        ))
        .await?;
    ensure!(
        changed.rows_affected() == 1,
        "fixture did not corrupt exactly one stored body"
    );
    // Finish the actual point SQL read before the writer takes SQLite's sole
    // connection. Its real Service decoder runs while the prepared writer waits
    // at COMMIT; no public GET or artificial invalidator call is substituted.
    let corrupt =
        configuration_store::configuration(&fixture.db, &fixture.tenant, &revision.revision_id)
            .await?
            .context("corrupt stored configuration")?;
    let before = fixture.service.current().map_err(failure)?;
    let read_publication = fixture
        .service
        .authority
        .read()
        .map_err(|code| failure(code.into()))?
        .read_publication();
    let epoch = fixture.service.authority.integrity_epoch();
    let healthy_tenant: TenantId = "overlap-healthy-tenant".try_into()?;
    let command = Command::Tenant(
        healthy_tenant.clone(),
        TenantPut {
            operation_id: "overlap-healthy-write".try_into()?,
            expected_revision: None,
            state: TenantState::Active,
            installation_id: fixture.installation.clone(),
        },
    );
    let gate = fixture.service.cleanup.hold(Phase::Committing);
    let release = ReleaseGate(fixture.service.clone());
    let service = fixture.service.clone();
    let operator = fixture.operator.clone();
    let write = AbortOnDropHandle::new(tokio::spawn(async move {
        service.execute(operator, command).await
    }));
    let outcome = async {
        tokio::time::timeout(Duration::from_secs(10), gate.entered.notified()).await?;
        ensure!(
            !write.is_finished() && fixture.service.cleanup.disposition() == Disposition::Pending
        );
        let reference = ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: fixture.tenant.as_str().try_into()?,
            resource_kind: ResourceKind::Route,
            resource_id: "absent-route".try_into()?,
            revision_id: "r1".try_into()?,
        };
        let denied = fixture.service.manifest_from_stored(
            &corrupt,
            before.revision,
            &reference,
            GenerationProtocol::ChatCompletions,
        );
        ensure!(matches!(denied, Err(error) if error.0.code == ErrorCode::AuthorityUnavailable));
        ensure!(fixture.service.authority.integrity_epoch() == epoch + 1);
        ensure!(
            fixture
                .service
                .authority
                .read()
                .map_err(|code| failure(code.into()))?
                .read_publication()
                == read_publication + 1
        );
        ensure!(
            fixture.service.current().is_ok()
                && fixture.service.cleanup.disposition() == Disposition::Pending
        );
        drop(release);
        let response = tokio::time::timeout(Duration::from_secs(10), write)
            .await??
            .map_err(failure)?;
        let applied = result(response)?;
        ensure!(
            applied.status == OperationStatus::Applied && applied.error.is_none(),
            "a prepared healthy write lost its COMMIT/publication acknowledgement"
        );
        // Applied is returned only after the original consuming COMMIT and its
        // publication succeed. Check the durable row/header and live header too.
        let persisted = store::startup::read_header_in(&fixture.db, 4096)
            .await?
            .context("persisted header")?
            .header;
        let current = fixture.service.current().map_err(failure)?;
        ensure!(
            current.revision == persisted.revision
                && current.revision.get() == before.revision.get() + 1
        );
        let row = fixture
            .db
            .query_one(Statement::from_sql_and_values(
                fixture.db.get_database_backend(),
                "SELECT state FROM tenancy_tenants WHERE id=?",
                [healthy_tenant.as_str().into()],
            ))
            .await?
            .context("committed healthy tenant")?;
        ensure!(row.try_get::<String>("", "state")? == "active");
        ensure!(
            fixture
                .service
                .authority
                .read()
                .map_err(|code| failure(code.into()))?
                .read_publication()
                == read_publication + 2
        );
        ensure!(
            fixture
                .service
                .authority
                .read()
                .map_err(|code| failure(code.into()))?
                .check(&held.lease, |_| Ok(()))
                .is_err()
        );
        let healthy = fixture
            .service
            .authenticate_live(secret)
            .await
            .map_err(failure)?;
        fixture
            .service
            .authority
            .read()
            .map_err(|code| failure(code.into()))?
            .check(&healthy.lease, |_| Ok(()))
            .map_err(|code| failure(code.into()))?;
        ensure!(
            fixture.service.current().is_ok() && fixture.service.admission.receipts_ready(),
            "retained corruption fenced healthy traffic after an acknowledged commit"
        );
        Ok(())
    }
    .await;
    let stopped = fixture.service.shutdown().await;
    outcome.and(stopped)
}
