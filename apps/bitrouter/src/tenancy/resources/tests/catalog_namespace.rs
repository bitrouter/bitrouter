//! File ownership is established before the original bootstrap import transaction.

use super::*;

async fn inventory(db: &DatabaseConnection) -> Result<Vec<i64>> {
    let mut counts = Vec::new();
    for table in [
        "tenancy_resource_revisions",
        "tenancy_resource_lifecycle",
        "tenancy_admission_index",
    ] {
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT COUNT(*) AS count FROM {table}"),
            ))
            .await?
            .context("inventory row")?;
        counts.push(row.try_get("", "count")?);
    }
    Ok(counts)
}

fn operator_credential() -> Result<Value> {
    let mut resource = credential("platform")?;
    resource["reference"]["owner_kind"] = "operator_pool".into();
    Ok(resource)
}

#[tokio::test]
async fn file_pool_import_rejection_rolls_back_all_resource_metadata() -> Result<()> {
    let (db, mut resources) = fixture().await?;
    resources.file_pool = Some("platform".into());
    let before = inventory(&db).await?;
    let bytes = serde_json::to_vec(&json!({"format":1,"installation_id":resources.installation,
        "resources":[credential("tenant")?, operator_credential()?]}))?;
    let transaction = db.begin().await?;
    let result = resources.bootstrap(&transaction, Some(&bytes)).await;
    transaction.rollback().await?;
    assert!(
        result.is_err(),
        "file-owned import must refuse before publication"
    );
    assert_eq!(inventory(&db).await?, before);
    Ok(())
}

#[tokio::test]
async fn retained_operator_pool_conflicts_but_same_named_tenant_remains_owned() -> Result<()> {
    let (db, mut resources) = fixture().await?;
    import(&resources, &db, vec![operator_credential()?]).await?;
    let before = inventory(&db).await?;
    resources.file_pool = Some("platform".into());
    let transaction = db.begin().await?;
    let result = resources.bootstrap(&transaction, None).await;
    transaction.rollback().await?;
    assert!(result.is_err());
    assert_eq!(inventory(&db).await?, before);

    let (db, mut resources) = fixture().await?;
    resources.file_pool = Some("platform".into());
    let tenant_reference = reference("platform", ResourceKind::Credential, "slot")?;
    assert!(!resources.file_owned_key(&tenant_reference.key()));
    let bytes = serde_json::to_vec(&json!({"format":1,"installation_id":resources.installation,
        "resources":[credential("platform")?]}))?;
    let transaction = db.begin().await?;
    resources.bootstrap(&transaction, Some(&bytes)).await?;
    transaction.commit().await?;
    assert!(resources.load(&db, &tenant_reference).await.is_ok());
    assert_eq!(inventory(&db).await?[0], 1);
    Ok(())
}
