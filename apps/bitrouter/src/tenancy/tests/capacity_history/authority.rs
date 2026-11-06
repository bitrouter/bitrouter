use super::*;
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKey, ResourceKind},
    identity::RevisionId,
    resources::{Definition, ResourcePut, ResourceRevoke},
};
use sea_orm::TransactionTrait;

pub(super) struct Retained {
    pub head: ResourceRef,
    first: ResourceRef,
    first_command: ResourcePut,
    last_command: ResourcePut,
    revisions: u64,
    operations: u64,
    seed_count: u64,
}

fn put(h: &Harness, ordinal: u64, previous: Option<RevisionId>) -> Result<ResourcePut> {
    Ok(ResourcePut {
        format: 1,
        operation_id: format!("history-rotate-{ordinal:06}").try_into()?,
        installation_id: h.fixture.installation.clone(),
        key: ResourceKey {
            owner_kind: OwnerKind::Tenant,
            owner_id: h.fixture.tenant.as_str().try_into()?,
            resource_kind: ResourceKind::Credential,
            resource_id: "history-secret".try_into()?,
        },
        expected_revision: previous,
        definition: Definition::Credential {
            secret: format!("synthetic-history-secret-{ordinal}"),
        },
    })
}

fn reference(input: &ResourcePut, result: &OperationResult) -> Result<ResourceRef> {
    ensure!(result.status == OperationStatus::Applied);
    Ok(ResourceRef {
        owner_kind: input.key.owner_kind,
        owner_id: input.key.owner_id.clone(),
        resource_kind: input.key.resource_kind,
        resource_id: input.key.resource_id.clone(),
        revision_id: result
            .resource_revision
            .clone()
            .context("resource revision")?,
    })
}

async fn rotate(h: &Harness, input: &ResourcePut) -> Result<ResourceRef> {
    let result: OperationResult = h
        .api(
            Method::POST,
            &format!("/control/v1/tenants/{}/resources", h.fixture.tenant),
            input,
        )
        .await?;
    reference(input, &result)
}

fn rejected_command(h: &Harness, ordinal: u64) -> Result<Command> {
    // The tenant already exists. None is a genuine failing create/CAS request.
    Ok(Command::Tenant(
        h.fixture.tenant.clone(),
        TenantPut {
            operation_id: format!("history-seed-{ordinal:06}").try_into()?,
            expected_revision: None,
            state: TenantState::Active,
            installation_id: h.fixture.installation.clone(),
        },
    ))
}

async fn seed_to(h: &mut Harness, target: u64) -> Result<u64> {
    let command = rejected_command(h, 0)?;
    let response = h
        .fixture
        .service
        .execute(h.fixture.operator.clone(), rejected_command(h, 0)?)
        .await
        .map_err(failure)?;
    let StoredResponse::Failure(template) = response else {
        anyhow::bail!("expected durable CAS rejection")
    };
    let Caller::Installation(principal) = &h.fixture.operator else {
        anyhow::bail!("operator")
    };
    let baseline = store::operation(
        &h.fixture.db,
        &h.fixture.installation,
        principal,
        command.operation_id(),
    )
    .await?
    .context("actual rejected operation missing")?;
    ensure!(baseline.digest == command.digest(principal)?);
    h.fixture.service.shutdown().await?;
    let mut state = store::load(&h.fixture.db, Utc::now()).await?;
    ensure!(state.operation_ids < target);
    let count = target - state.operation_ids;
    let mut next = 1;
    while next <= count {
        let tx = h.fixture.db.begin().await?;
        for _ in 0..128 {
            if next > count {
                break;
            }
            let command = rejected_command(h, next)?;
            let mut error = template.clone();
            error.operation_id = Some(command.operation_id().clone());
            let result = OperationResult {
                operation_id: command.operation_id().clone(),
                status: OperationStatus::Rejected,
                authority_revision: Some(state.revision),
                resource_revision: None,
                resource_id: None,
                error: Some(error.clone()),
            };
            state.operation_ids = state
                .operation_ids
                .checked_add(1)
                .context("operation count")?;
            let operation = store::Operation {
                principal: principal.clone(),
                id: command.operation_id().clone(),
                context: baseline.context.clone(),
                digest: command.digest(principal)?,
                result,
                response: StoredResponse::Failure(error),
                created_at: Utc::now(),
                safety: false,
            };
            store::write_in(&tx, &state, &[], &operation).await?;
            next += 1;
        }
        tx.commit().await?;
    }
    ensure!(state.operation_ids == target);
    h.restart().await?;
    Ok(count)
}

pub(super) async fn qualify(h: &mut Harness, revisions: u64, operations: u64) -> Result<Retained> {
    ensure!(revisions >= 3 && operations > revisions + 8);
    let first_command = put(h, 1, None)?;
    let first = rotate(h, &first_command).await?;
    let mut head = first.clone();
    for ordinal in 2..=revisions - 2 {
        head = rotate(h, &put(h, ordinal, Some(head.revision_id.clone()))?).await?;
    }
    // Real revoke before acceleration; its durable result is retained as well.
    let revoke = ResourceRevoke {
        format: 1,
        operation_id: "history-revoke-first".try_into()?,
        installation_id: h.fixture.installation.clone(),
        reference: first.clone(),
        expected_authority_revision: h.fixture.service.current().map_err(failure)?.revision,
    };
    let result: OperationResult = h
        .api(
            Method::POST,
            &format!("/control/v1/tenants/{}/resources/revoke", h.fixture.tenant),
            &revoke,
        )
        .await?;
    ensure!(result.status == OperationStatus::Applied);
    let seed_count = seed_to(h, operations - 2).await?;
    let mut last_command = first_command.clone();
    for ordinal in revisions - 1..=revisions {
        last_command = put(h, ordinal, Some(head.revision_id.clone()))?;
        head = rotate(h, &last_command).await?;
        let count = h.fixture.service.current().map_err(failure)?.operation_ids;
        ensure!(count == operations - (revisions - ordinal));
        eprintln!("history API checkpoint lifecycle={ordinal} ordinary_operations={count}");
    }
    let retained = Retained {
        head,
        first,
        first_command,
        last_command,
        revisions,
        operations,
        seed_count,
    };
    verify(h, &retained).await?;
    expired(h, &retained).await?;
    Ok(retained)
}

async fn expired(h: &mut Harness, retained: &Retained) -> Result<()> {
    let command = rejected_command(h, retained.seed_count / 3)?;
    h.fixture.service.shutdown().await?;
    let tx = h.fixture.db.begin().await?;
    let changed = tx
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Sqlite,
            "UPDATE tenancy_operations SET created_at=? WHERE operation_id=?",
            [
                (Utc::now() - chrono::Duration::days(31))
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    .into(),
                command.operation_id().as_str().into(),
            ],
        ))
        .await?;
    ensure!(changed.rows_affected() == 1);
    tx.commit().await?;
    h.restart().await?;
    let before = h.fixture.service.current().map_err(failure)?;
    ensure!(
        matches!(h.fixture.service.execute(h.fixture.operator.clone(), command).await,
        Err(error) if error.0.code == ErrorCode::OperationExpired)
    );
    let after = h.fixture.service.current().map_err(failure)?;
    ensure!(before.operation_ids == after.operation_ids && before.revision == after.revision);
    Ok(())
}

pub(super) async fn verify(h: &Harness, retained: &Retained) -> Result<()> {
    let before = h.fixture.service.current().map_err(failure)?;
    ensure!(before.operation_ids >= retained.operations);
    let row = h.fixture.db.query_one(sql("SELECT COUNT(*) AS count FROM tenancy_resource_lifecycle WHERE resource_id='history-secret'"))
        .await?.context("lifecycle count")?;
    ensure!(u64::try_from(row.try_get::<i64>("", "count")?)? == retained.revisions);
    for (reference, revoked) in [(&retained.first, true), (&retained.head, false)] {
        // resource_status authenticates encrypted retained contents, not just row counts.
        let status = h
            .fixture
            .service
            .resource_status(&h.fixture.operator, &h.fixture.tenant, reference)
            .await
            .map_err(failure)?;
        ensure!(
            status.revoked == revoked
                && status.head_revision.as_ref() == Some(&retained.head.revision_id)
        );
    }
    for input in [&retained.first_command, &retained.last_command] {
        let replay: OperationResult = h
            .api(
                Method::POST,
                &format!("/control/v1/tenants/{}/resources", h.fixture.tenant),
                input,
            )
            .await?;
        ensure!(replay.status == OperationStatus::Applied);
        let expected = if input.operation_id == retained.first_command.operation_id {
            &retained.first
        } else {
            &retained.head
        };
        ensure!(replay.resource_revision.as_ref() == Some(&expected.revision_id));
    }
    for ordinal in [0, retained.seed_count / 2, retained.seed_count] {
        let command = rejected_command(h, ordinal)?;
        let Caller::Installation(principal) = &h.fixture.operator else {
            anyhow::bail!("operator")
        };
        let stored = store::operation(
            &h.fixture.db,
            &h.fixture.installation,
            principal,
            command.operation_id(),
        )
        .await?
        .context("seed operation")?;
        ensure!(stored.digest == command.digest(principal)?);
        let replay = h
            .fixture
            .service
            .execute(h.fixture.operator.clone(), command)
            .await
            .map_err(failure)?;
        ensure!(serde_json::to_vec(&replay)? == serde_json::to_vec(&stored.response)?);
    }
    let mut conflicting = retained.last_command.clone();
    conflicting.definition = Definition::Credential {
        secret: "changed-synthetic-secret".into(),
    };
    ensure!(
        matches!(h.fixture.service.execute(h.fixture.operator.clone(), Command::Resource(h.fixture.tenant.clone(), conflicting)).await,
        Err(error) if error.0.code == ErrorCode::OperationConflict)
    );
    let after = h.fixture.service.current().map_err(failure)?;
    ensure!(before.revision == after.revision && before.operation_ids == after.operation_ids);
    Ok(())
}
