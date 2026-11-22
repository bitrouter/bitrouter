//! Explicit retained-data fixtures, not replacements for removed management APIs.
//! Call between awaited test operations; a concurrent authority writer is an error.

use super::*;
use sea_orm::TransactionTrait;

pub(in crate::tenancy::tests) async fn seed(
    service: &Service,
    db: &sea_orm::DatabaseConnection,
    key: ResourceKey,
    expected: Option<bitrouter_ai::tenancy::identity::RevisionId>,
    definition: serde_json::Value,
) -> Result<ResourceRef> {
    let before = service.current().map_err(failure)?;
    let old = crate::tenancy::resources::lifecycle::head(db, &before.installation, &key).await?;
    ensure!(
        old.as_ref().map(|record| &record.reference.revision_id) == expected.as_ref(),
        "historical fixture head changed"
    );
    let reference = ResourceRef {
        owner_kind: key.owner_kind,
        owner_id: key.owner_id,
        resource_kind: key.resource_kind,
        resource_id: key.resource_id,
        revision_id: uuid::Uuid::new_v4().to_string().try_into()?,
    };
    let import = serde_json::to_vec(&serde_json::json!({
        "format":3, "installation_id":before.installation,
        "resources":[{"reference":reference,"definition":definition}],
    }))?;
    let tx = db.begin().await?;
    service
        .resources
        .seed_history_revision_in(&tx, &import, expected.as_ref())
        .await?;
    let mut header = before.header();
    header.revision = header
        .revision
        .get()
        .checked_add(1)
        .context("historical fixture revision overflow")?
        .try_into()?;
    let mut changes = Vec::new();
    let mut references = vec![reference.clone()];
    if let Some(mut old) = old {
        old.is_head = false;
        old.authority_revision = header.revision;
        crate::tenancy::resources::lifecycle::write(&tx, &header.installation, &old).await?;
        references.push(old.reference.clone());
        changes.push(crate::tenancy::store::Change::ResourceLifecycle {
            installation: header.installation.clone(),
            record: old,
        });
    }
    let record = crate::tenancy::resources::lifecycle::Record {
        reference: reference.clone(),
        is_head: true,
        revoked: false,
        authority_revision: header.revision,
    };
    crate::tenancy::resources::lifecycle::write(&tx, &header.installation, &record).await?;
    changes.push(crate::tenancy::store::Change::ResourceLifecycle {
        installation: header.installation.clone(),
        record,
    });
    publish(service, tx, &before, header, &changes, &references).await?;
    Ok(reference)
}

async fn publish(
    service: &Service,
    tx: sea_orm::DatabaseTransaction,
    before: &crate::tenancy::model::Snapshot,
    header: crate::tenancy::model::AuthorityHeader,
    changes: &[crate::tenancy::store::Change],
    references: &[ResourceRef],
) -> Result<()> {
    let updated = tx.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE tenancy_installation SET authority_revision=? WHERE singleton=1 AND authority_revision=?",
        [i64::try_from(header.revision.get())?.into(),i64::try_from(before.revision.get())?.into()])).await?;
    ensure!(
        updated.rows_affected() == 1,
        "concurrent historical fixture authority writer"
    );
    let keys = references.iter().map(ResourceRef::key).collect::<Vec<_>>();
    let admission = service
        .resources
        .admission_in(&tx, header.revision, references, &keys, &[])
        .await?;
    // This fixture does not create a management operation or claim an API ACK.
    // Publish only after the caller commits the exact retained-data transaction.
    let publication = service
        .authority
        .prepare_with_admission(header, changes, None, Some(Arc::new(admission)))
        .map_err(|code| failure(code.into()))?;
    tx.commit().await?;
    publication.publish().map_err(|code| failure(code.into()))?;
    Ok(())
}

pub(in crate::tenancy::tests) async fn revoke(
    service: &Service,
    db: &sea_orm::DatabaseConnection,
    reference: &ResourceRef,
) -> Result<()> {
    let before = service.current().map_err(failure)?;
    let tx = db.begin().await?;
    let mut record =
        crate::tenancy::resources::lifecycle::exact(&tx, &before.installation, reference)
            .await?
            .context("historical revoked revision missing")?;
    let mut header = before.header();
    header.revision = header
        .revision
        .get()
        .checked_add(1)
        .context("historical fixture revision overflow")?
        .try_into()?;
    record.revoked = true;
    record.authority_revision = header.revision;
    crate::tenancy::resources::lifecycle::write(&tx, &before.installation, &record).await?;
    let changes = [crate::tenancy::store::Change::ResourceLifecycle {
        installation: before.installation.clone(),
        record,
    }];
    publish(
        service,
        tx,
        &before,
        header,
        &changes,
        std::slice::from_ref(reference),
    )
    .await
}
