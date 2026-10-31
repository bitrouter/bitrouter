//! Exact immutable primary-key and trigger checks, shared by startup and archives.
use super::*;
async fn rows_on(
    db: &impl ConnectionTrait,
    sql: &str,
    values: Vec<Value>,
) -> Result<Vec<sea_orm::QueryResult>, DbErr> {
    db.query_all(Statement::from_sql_and_values(
        db.get_database_backend(),
        sql,
        values,
    ))
    .await
}
pub(super) async fn indexes_on(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    let backend = db.get_database_backend();
    let query = match backend {
        DatabaseBackend::Sqlite => {
            "SELECT CASE WHEN length(CAST(name AS BLOB))<=256 THEN name ELSE NULL END AS name, origin FROM pragma_index_list('tenancy_usage_evidence') LIMIT 3"
        }
        DatabaseBackend::Postgres => {
            "SELECT x.relname AS name, CASE WHEN i.indisprimary THEN 'pk' ELSE 'c' END AS origin FROM pg_index i JOIN pg_class t ON t.oid=i.indrelid JOIN pg_namespace n ON n.oid=t.relnamespace JOIN pg_class x ON x.oid=i.indexrelid WHERE n.nspname=current_schema() AND t.relname='tenancy_usage_evidence'"
        }
        DatabaseBackend::MySql => {
            "SELECT DISTINCT INDEX_NAME AS name, CASE WHEN INDEX_NAME='PRIMARY' THEN 'pk' ELSE 'c' END AS origin FROM information_schema.statistics WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME='tenancy_usage_evidence'"
        }
    };
    let mut primary = false;
    for row in rows_on(db, query, vec![]).await? {
        let name: String = row.try_get("", "name")?;
        let origin: String = row.try_get("", "origin")?;
        let is_primary = origin == "pk";
        let expected: &[&str] = if is_primary && !primary {
            primary = true;
            &["installation_id", "tenant_id", "execution_id"]
        } else {
            return Err(incompatible());
        };
        let query = match backend {
            DatabaseBackend::Sqlite => {
                "SELECT name AS column_name, CASE WHEN key=1 AND cid>=0 AND coll='BINARY' AND \"desc\"=0 THEN 'yes' ELSE 'no' END AS compatible FROM pragma_index_xinfo(?) WHERE key=1 ORDER BY seqno LIMIT 5"
            }
            DatabaseBackend::Postgres => {
                "SELECT a.attname AS column_name, CASE WHEN i.indisvalid AND i.indisready AND i.indimmediate AND i.indisunique=$2 AND i.indisprimary=$2 AND i.indpred IS NULL AND i.indexprs IS NULL AND i.indnkeyatts=i.indnatts AND am.amname='btree' AND i.indcollation[k.position-1]=a.attcollation AND i.indoption[k.position-1]=0 AND opc.opcmethod=x.relam AND opc.opcdefault AND opc.opcnamespace='pg_catalog'::regnamespace AND opc.opcname='text_ops' THEN 'yes' ELSE 'no' END AS compatible FROM pg_index i JOIN pg_class x ON x.oid=i.indexrelid JOIN pg_namespace n ON n.oid=x.relnamespace JOIN pg_class t ON t.oid=i.indrelid JOIN pg_am am ON am.oid=x.relam CROSS JOIN LATERAL generate_series(1,i.indnatts) k(position) JOIN pg_attribute a ON a.attrelid=t.oid AND a.attnum=i.indkey[k.position-1] JOIN pg_opclass opc ON opc.oid=i.indclass[k.position-1] WHERE n.nspname=current_schema() AND x.relname=$1 AND t.relname='tenancy_usage_evidence' ORDER BY k.position"
            }
            DatabaseBackend::MySql => {
                "SELECT COLUMN_NAME AS column_name, CASE WHEN NON_UNIQUE=? AND SUB_PART IS NULL AND COLLATION='A' AND INDEX_TYPE='BTREE' AND IS_VISIBLE='YES' AND EXPRESSION IS NULL THEN 'yes' ELSE 'no' END AS compatible FROM information_schema.statistics WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME='tenancy_usage_evidence' AND INDEX_NAME=? ORDER BY SEQ_IN_INDEX"
            }
        };
        let values = match backend {
            DatabaseBackend::Sqlite => vec![name.clone().into()],
            DatabaseBackend::Postgres => vec![name.clone().into(), is_primary.into()],
            DatabaseBackend::MySql => vec![u32::from(!is_primary).into(), name.clone().into()],
        };
        let actual = rows_on(db, query, values).await?;
        if actual.len() != expected.len() {
            return Err(incompatible());
        }
        for (row, column) in actual.into_iter().zip(expected) {
            if row.try_get::<String>("", "compatible")? != "yes"
                || row.try_get::<String>("", "column_name")? != *column
            {
                return Err(incompatible());
            }
        }
        if backend == DatabaseBackend::Sqlite {
            let actual=rows_on(db,"SELECT \"unique\" AS unique_value,partial FROM pragma_index_list('tenancy_usage_evidence') WHERE name=? LIMIT 2",vec![name.into()]).await?;
            let row = actual.first().ok_or_else(incompatible)?;
            if actual.len() != 1
                || row.try_get::<i64>("", "unique_value")? != i64::from(is_primary)
                || row.try_get::<i64>("", "partial")? != 0
            {
                return Err(incompatible());
            }
        }
    }
    if !primary {
        return Err(incompatible());
    }
    // Row transformations or foreign/check constraints are outside this immutable store.
    let query = match backend {
        DatabaseBackend::Sqlite => {
            "SELECT 1 AS present FROM sqlite_schema WHERE type='trigger' AND tbl_name='tenancy_usage_evidence' UNION ALL SELECT 1 AS present FROM temp.sqlite_schema WHERE type='trigger' AND tbl_name='tenancy_usage_evidence' LIMIT 1"
        }
        DatabaseBackend::Postgres => {
            "SELECT 1 AS present FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND c.relname='tenancy_usage_evidence' AND (c.relrowsecurity OR c.relforcerowsecurity OR EXISTS (SELECT 1 FROM pg_trigger g WHERE g.tgrelid=c.oid AND NOT g.tgisinternal) OR EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conrelid=c.oid AND k.contype<>'p')) LIMIT 1"
        }
        DatabaseBackend::MySql => {
            "SELECT 1 AS present FROM information_schema.table_constraints WHERE CONSTRAINT_SCHEMA=DATABASE() AND TABLE_NAME='tenancy_usage_evidence' AND CONSTRAINT_TYPE<>'PRIMARY KEY' UNION ALL SELECT 1 AS present FROM information_schema.triggers WHERE TRIGGER_SCHEMA=DATABASE() AND EVENT_OBJECT_TABLE='tenancy_usage_evidence' LIMIT 1"
        }
    };
    if !rows_on(db, query, vec![]).await?.is_empty() {
        return Err(incompatible());
    }
    Ok(())
}
