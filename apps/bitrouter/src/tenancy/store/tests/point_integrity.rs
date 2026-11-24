use super::*;

#[tokio::test]
async fn indexed_caller_refuses_missing_authority_links() -> Result<()> {
    for mutation in [
        "DELETE FROM tenancy_principals WHERE id='principal-0000'",
        "DELETE FROM tenancy_tenants WHERE id='tenant-0000'",
        "DELETE FROM tenancy_bindings WHERE tenant_id='tenant-0000'",
    ] {
        let db = database().await?;
        add(&db, 0).await?;
        let caller = Caller::Credential("credential-0000".try_into()?);
        assert!(
            load_caller_in(&db, &header()?, &caller, ROW_BYTES)
                .await
                .is_ok()
        );
        // Simulate retained damage that the normal foreign keys prevent.
        db.execute_unprepared("PRAGMA foreign_keys=OFF").await?;
        db.execute(Statement::from_string(DatabaseBackend::Sqlite, mutation))
            .await?;
        db.execute_unprepared("PRAGMA foreign_keys=ON").await?;
        assert!(
            load_caller_in(&db, &header()?, &caller, ROW_BYTES)
                .await
                .is_err()
        );
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn indexed_credential_refuses_malformed_grants_and_metadata() -> Result<()> {
    for mutation in [
        "UPDATE tenancy_credentials SET key_hash='invalid'",
        "UPDATE tenancy_credentials SET created_at='invalid'",
        "UPDATE tenancy_credentials SET permissions_json='[\"execution.submit\",\"execution.submit\"]'",
        "UPDATE tenancy_credentials SET audit_reader=1",
        "UPDATE tenancy_credentials SET binding_id=NULL",
    ] {
        let db = database().await?;
        add(&db, 0).await?;
        let id = "credential-0000".try_into()?;
        assert!(credential_in(&db, &id, ROW_BYTES).await?.is_some());
        db.execute(Statement::from_string(DatabaseBackend::Sqlite, mutation))
            .await?;
        assert!(credential_in(&db, &id, ROW_BYTES).await.is_err());
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn missing_identity_refuses_retained_execution_or_oauth_without_writing() -> Result<()> {
    for seed in [
        "INSERT INTO tenancy_execution_receipts (installation_id,tenant_id,execution_id,principal_id,correlation_id,receipt_json,content_digest,disposition_json,byte_count,unknown_outcome,recorded_at) VALUES ('old-installation','tenant','execution','principal','correlation','{}','digest','{}',2,0,'2026-01-01T00:00:00Z')",
        "INSERT INTO tenancy_oauth_slots (slot_key,installation_id,owner_kind,owner_id,credential_resource_id,authority_revision,generation,state,metadata_json,payload_json,content_digest) VALUES ('slot','old-installation','tenant','tenant','credential','revision',0,'ready','{}','{}','digest')",
    ] {
        let db = database().await?;
        db.execute(Statement::from_string(DatabaseBackend::Sqlite, seed))
            .await?;
        let write_reached = std::cell::Cell::new(false);
        let rejected = initial_header_with(&db, Utc::now(), || {
            write_reached.set(true);
            Ok(())
        })
        .await;
        assert!(rejected.is_err());
        assert!(!write_reached.get());
        assert!(
            super::super::startup::read_header_in(&db, 4096)
                .await?
                .is_none()
        );
        db.close().await?;
    }
    Ok(())
}
