use super::*;
use sea_orm::{ConnectionTrait, Statement};

#[tokio::test]
async fn migration_adds_no_ownership_resumes_populated_and_down_preserves_predecessors()
-> anyhow::Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    let manager = SchemaManager::new(&db);
    crate::db::migration::m20240101_000001_create_auth_tables::Migration
        .up(&manager)
        .await?;
    crate::db::migration::m20240101_000028_resource_tenancy::Migration
        .up(&manager)
        .await?;
    for sql in [
        "INSERT INTO users (id,created_at) VALUES ('legacy-a','original-a'),('legacy-b','original-b')",
        "INSERT INTO tenancy_tenants (id,revision,state,created_at) VALUES ('tenant-a','original','active','original')",
    ] {
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            sql.to_owned(),
        ))
        .await?;
    }
    Migration.up(&manager).await?;
    for table in TABLES {
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT COUNT(*) AS count FROM {table}"),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing count"))?;
        assert_eq!(row.try_get::<i64>("", "count")?, 0);
    }
    for sql in [
        "INSERT INTO tenancy_resource_revisions (owner_kind,owner_id,resource_kind,resource_id,revision_id,compiler_version,payload_json,dependency_json,content_digest,created_at) VALUES ('tenant','tenant-a','route','route','v1','v1','{}','[]','fingerprint','time')",
        "INSERT INTO tenancy_configurations (tenant_id,revision_id,schema_version,document_json,compiler_version,content_digest,compiled_json,dependencies_json,creator_principal_id,authority_revision,created_at) VALUES ('tenant-a','v1',1,'{}','v1','digest','{}','[]','operator.fixture',7,'time')",
        "INSERT INTO tenancy_active_configurations (tenant_id,revision_id,authority_revision) VALUES ('tenant-a','v1',8)",
    ] {
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            sql.to_owned(),
        ))
        .await?;
    }
    Migration.up(&manager).await?;
    for table in TABLES {
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT COUNT(*) AS count FROM {table}"),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing count"))?;
        assert_eq!(row.try_get::<i64>("", "count")?, 1);
    }
    // The active FK is composite: a revision name belonging to another tenant
    // is not a valid activation target for this tenant.
    assert!(db.execute(Statement::from_string(DatabaseBackend::Sqlite,
        "INSERT INTO tenancy_active_configurations (tenant_id,revision_id,authority_revision) VALUES ('tenant-b','v1',8)".to_owned())).await.is_err());
    Migration.down(&manager).await?;
    for table in TABLES {
        assert!(!manager.has_table(table).await?);
    }
    let owners = db
        .query_all(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT id,created_at FROM users ORDER BY id".to_owned(),
        ))
        .await?;
    assert_eq!(owners.len(), 2);
    assert_eq!(owners[0].try_get::<String>("", "created_at")?, "original-a");
    assert_eq!(owners[1].try_get::<String>("", "created_at")?, "original-b");
    let tenant = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT revision,created_at FROM tenancy_tenants WHERE id='tenant-a'".to_owned(),
        ))
        .await?
        .ok_or_else(|| anyhow::anyhow!("predecessor tenant missing"))?;
    assert_eq!(tenant.try_get::<String>("", "revision")?, "original");
    assert_eq!(tenant.try_get::<String>("", "created_at")?, "original");
    Ok(())
}
