//! Old generic management results and sealed artifacts survive file cutover.

use super::*;
use crate::tenancy::{model::ReadContext, store};
use bitrouter_ai::tenancy::{
    identity::OperationId,
    operation::{OperationResult, OperationStatus},
};
use sea_orm::TransactionTrait;

async fn encoded_operation(fixture: &Fixture, id: &OperationId) -> Result<Vec<String>> {
    let row = fixture.db.query_one(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT command_digest,result_json,response_json FROM tenancy_operations WHERE installation_id=? AND operation_id=?",
        [fixture.installation.as_str().into(), id.as_str().into()],
    )).await?.context("retained operation row")?;
    ["command_digest", "result_json", "response_json"]
        .into_iter()
        .map(|column| row.try_get("", column).map_err(Into::into))
        .collect()
}

#[tokio::test]
async fn legacy_pending_result_and_sealed_profile_survive_file_catalog_start_without_replay()
-> Result<()> {
    let provider = Provider::start().await?;
    let mut harness = Harness::new(&provider.url).await?;
    let exposure = harness
        .config
        .tenancy
        .catalog
        .take()
        .context("file exposure")?;
    harness = harness.restart().await?;
    ensure!(harness.fixture.service.resources.configured().is_none());
    let historical = super::super::resources::history::seed(
        &harness.fixture.service,
        &harness.fixture.db,
        ResourceKey {
            owner_kind: OwnerKind::OperatorPool,
            owner_id: "old-published-pool".try_into()?,
            resource_kind: ResourceKind::Policy,
            resource_id: "old-published-profile".try_into()?,
        },
        None,
        json!({"kind":"admission_profile_v2","profile":{"schema_version":2,
            "rate":{"rate_group_id":"old-provider","member_rpm":17},
            "health":null,"health_group_id":null,"fallback":"ordered_terminal_rejections_v1"}}),
    )
    .await?;
    let before = harness.fixture.service.current().map_err(failure)?;
    let Caller::Installation(principal) = &harness.fixture.operator else {
        anyhow::bail!("installation operation owner missing");
    };
    let id: OperationId = "old-resource-v2-pending".try_into()?;
    // Old and current journals store the generic result/response and opaque
    // digest, not a serialized ResourcePutV2 or a resumable command body.
    let pending: OperationResult = serde_json::from_value(json!({
        "operation_id":id,"status":"pending","authority_revision":null,
        "resource_revision":null,"resource_id":null,"error":null,
    }))?;
    pending.validate()?;
    let operation = store::Operation {
        principal: principal.clone(),
        id: id.clone(),
        context: ReadContext::Installation,
        digest: "1".repeat(64),
        result: pending.clone(),
        response: StoredResponse::Operation(pending),
        created_at: Utc::now(),
        safety: false,
    };
    let mut state = crate::tenancy::model::Snapshot::from_header(&before.header());
    state.operation_ids = state
        .operation_ids
        .checked_add(1)
        .context("fixture operation count")?;
    harness.fixture.service.shutdown().await?;
    let tx = harness.fixture.db.begin().await?;
    store::write_in(&tx, &state, &[], &operation).await?;
    tx.commit().await?;
    let original = encoded_operation(&harness.fixture, &id).await?;
    let counts = publication_counts(&harness.fixture).await?;

    harness.config.tenancy.catalog = Some(exposure);
    harness = harness.restart().await?;
    ensure!(harness.fixture.service.resources.configured().is_some());
    let retained = harness
        .fixture
        .service
        .operation(&harness.fixture.operator, &id)
        .await
        .map_err(failure)?;
    ensure!(
        retained.status == OperationStatus::Pending
            && retained.operation_id == id
            && retained.authority_revision.is_none()
            && retained.resource_revision.is_none()
            && retained.resource_id.is_none()
            && retained.error.is_none()
    );
    let status = harness
        .fixture
        .service
        .resource_status(
            &harness.fixture.operator,
            &harness.fixture.tenant,
            &historical,
        )
        .await
        .map_err(failure)?;
    ensure!(
        status.reference == historical
            && !status.revoked
            && status.head_revision.as_ref() == Some(&historical.revision_id)
    );
    catalog(
        &harness.fixture,
        &harness.fixture.operator,
        &harness.fixture.tenant,
    )
    .await?
    .validate()?;
    ensure!(encoded_operation(&harness.fixture, &id).await? == original);
    ensure!(publication_counts(&harness.fixture).await? == counts);
    ensure!(
        provider.calls() == 0,
        "startup or readback replayed provider work"
    );
    let absent = harness
        .fixture
        .service
        .operation(
            &harness.fixture.operator,
            &"uncommitted-old-resource-v2".try_into()?,
        )
        .await;
    ensure!(matches!(absent, Err(error) if error.0.code == ErrorCode::NotFound));
    ensure!(encoded_operation(&harness.fixture, &id).await? == original);
    harness.fixture.service.shutdown().await?;
    Ok(())
}
