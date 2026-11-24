use super::*;
use bitrouter_ai::tenancy::operation::{DispatchState, ErrorCode};
use sea_orm::{Statement, TransactionTrait};
use serde_json::json;

async fn fixture(kind: &str) -> Result<(DatabaseConnection, Snapshot, Operation)> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let mut state = initial_header(&db, Utc::now()).await?;
    state.operation_ids = 1;
    let id: OperationId = "operation-point".try_into()?;
    let mut result = OperationResult {
        operation_id: id.clone(),
        status: OperationStatus::Applied,
        authority_revision: Some(state.revision),
        resource_revision: Some("r1".try_into()?),
        resource_id: Some("credential".try_into()?),
        error: None,
    };
    let response = match kind {
        "issued" => StoredResponse::Issued(CredentialIssued {
            operation_id: id.clone(),
            credential_id: "credential".try_into()?,
            principal_id: "recipient".try_into()?,
            tenant_id: "tenant".try_into()?,
            authority_revision: state.revision,
            secret: None,
            expires_at: "2099-01-01T00:00:00Z".try_into()?,
            credential_revision: "r1".try_into()?,
        }),
        "configuration" => StoredResponse::Configuration(serde_json::from_value(json!({
            "tenant_id": "tenant", "revision_id": "r1", "schema_version": 1,
            "compiler_version": "fixture", "content_digest": "b".repeat(64),
            "document": {
                "routes": [], "policy": null,
                "generation_defaults": {"max_output_tokens": 10, "temperature": 1.0},
                "eval_publication": false,
                "limits": {"active_executions": 1, "queued_work": 1}
            }
        }))?),
        "failure" => {
            let error = Error {
                code: ErrorCode::Forbidden,
                message: "forbidden".into(),
                dispatch_state: DispatchState::NotStarted,
                operation_id: Some(id.clone()),
                execution_id: None,
            };
            result.status = OperationStatus::Rejected;
            result.authority_revision = None;
            result.resource_revision = None;
            result.resource_id = None;
            result.error = Some(error.clone());
            StoredResponse::Failure(error)
        }
        _ => StoredResponse::Operation(result.clone()),
    };
    let retained = Operation {
        principal: PrincipalId::for_control_credential("operation-point")?,
        id,
        context: ReadContext::Installation,
        digest: "a".repeat(64),
        result,
        response,
        created_at: Utc::now(),
        safety: false,
    };
    let tx = db.begin().await?;
    write_in(&tx, &state, &[], &retained).await?;
    tx.commit().await?;
    Ok((db, state, retained))
}

/// UT for exact operation lookup and complete-audit response compatibility.
#[tokio::test]
async fn operation_point_and_audit_preserve_every_response_kind() -> Result<()> {
    for kind in ["operation", "issued", "configuration", "failure"] {
        let (db, state, retained) = fixture(kind).await?;
        let loaded = operation(&db, &state.installation, &retained.principal, &retained.id)
            .await?
            .context("operation missing")?;
        assert_eq!(
            serde_json::to_value(&loaded)?,
            serde_json::to_value(&retained)?
        );
        assert!(load_existing(&db, Utc::now()).await.is_ok(), "{kind}");
        assert!(
            operation(
                &db,
                &state.installation,
                &retained.principal,
                &"absent".try_into()?
            )
            .await?
            .is_none()
        );
        db.close().await?;
    }
    Ok(())
}

/// UT for fail-closed lookup/replay after removing the startup history audit.
#[tokio::test]
async fn operation_point_rejects_result_response_and_owner_corruption() -> Result<()> {
    for (kind, assignment) in [
        (
            "operation",
            "result_json=json_set(result_json,'$.operation_id','other')",
        ),
        (
            "operation",
            "result_json=json_set(result_json,'$.authority_revision',1),response_json=json_set(response_json,'$.value.authority_revision',1)",
        ),
        (
            "operation",
            "response_json=json_set(response_json,'$.value.resource_id','other')",
        ),
        ("operation", "command_digest='invalid'"),
        ("operation", "authority_class='tenant',tenant_id='tenant'"),
        ("operation", "tenant_id='tenant'"),
        (
            "issued",
            "response_json=json_set(response_json,'$.value.operation_id','other')",
        ),
        (
            "issued",
            "response_json=json_set(response_json,'$.value.credential_revision','other')",
        ),
        (
            "issued",
            "response_json=json_set(response_json,'$.value.secret','must-not-leak')",
        ),
        (
            "configuration",
            "response_json=json_set(response_json,'$.value.revision_id','other')",
        ),
        (
            "configuration",
            "response_json=json_set(response_json,'$.value.schema_version',2)",
        ),
        (
            "failure",
            "response_json=json_set(response_json,'$.value.operation_id','other')",
        ),
        (
            "failure",
            "response_json=json_set(response_json,'$.value.message','different')",
        ),
    ] {
        let (db, state, retained) = fixture(kind).await?;
        db.execute(Statement::from_string(
            db.get_database_backend(),
            format!("UPDATE tenancy_operations SET {assignment}"),
        ))
        .await?;
        assert!(
            operation(&db, &state.installation, &retained.principal, &retained.id)
                .await
                .is_err(),
            "accepted {kind}: {assignment}"
        );
        assert!(
            load_existing(&db, Utc::now()).await.is_err(),
            "audit accepted {kind}: {assignment}"
        );
        db.close().await?;
    }
    Ok(())
}

/// UT for operation reads retaining the original installation publication bound.
#[tokio::test]
async fn operation_point_rejects_missing_or_foreign_authority_header() -> Result<()> {
    for sql in [
        "DELETE FROM tenancy_installation",
        "UPDATE tenancy_installation SET installation_id='foreign'",
    ] {
        let (db, state, retained) = fixture("operation").await?;
        db.execute_unprepared(sql).await?;
        assert!(
            operation(&db, &state.installation, &retained.principal, &retained.id)
                .await
                .is_err(),
            "accepted {sql}"
        );
        db.close().await?;
    }
    Ok(())
}

/// UT for corrupt operation isolation after both read and replay rollback.
#[tokio::test]
async fn corrupt_operation_does_not_fence_later_healthy_reads_or_replay() -> Result<()> {
    use crate::tenancy::{Service, command::Command, model::Caller};
    use bitrouter_ai::tenancy::authority::{TenantPut, TenantState};

    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let service = Service::open(db.clone(), None).await?;
    let installation = service
        .current()
        .map_err(|e| anyhow::anyhow!("{:?}", e.0.code))?
        .installation
        .clone();
    let caller = Caller::Installation(PrincipalId::for_control_credential("point-isolation")?);
    let command = |id: &str| -> Result<Command> {
        Ok(Command::Tenant(
            id.try_into()?,
            TenantPut {
                operation_id: id.try_into()?,
                expected_revision: None,
                state: TenantState::Active,
                installation_id: installation.clone(),
            },
        ))
    };
    for id in ["corrupt-operation", "healthy-operation"] {
        service
            .execute(caller.clone(), command(id)?)
            .await
            .map_err(|e| anyhow::anyhow!("fixture failed: {:?}", e.0.code))?;
    }
    db.execute_unprepared("UPDATE tenancy_operations SET response_json=json_set(response_json,'$.value.operation_id','other') WHERE operation_id='corrupt-operation'").await?;
    let bad: OperationId = "corrupt-operation".try_into()?;
    let error = service
        .operation(&caller, &bad)
        .await
        .err()
        .context("corrupt read admitted")?;
    assert_eq!(error.0.code, ErrorCode::AuthorityUnavailable);
    let error = service
        .execute(caller.clone(), command(bad.as_str())?)
        .await
        .err()
        .context("corrupt replay admitted")?;
    assert_eq!(error.0.code, ErrorCode::AuthorityUnavailable);
    let healthy: OperationId = "healthy-operation".try_into()?;
    let result = service
        .operation(&caller, &healthy)
        .await
        .map_err(|e| anyhow::anyhow!("healthy read fenced: {:?}", e.0.code))?;
    assert_eq!(result.operation_id, healthy);
    service
        .execute(caller, command(healthy.as_str())?)
        .await
        .map_err(|e| anyhow::anyhow!("healthy replay fenced: {:?}", e.0.code))?;
    service.shutdown().await?;
    db.close().await?;
    Ok(())
}
