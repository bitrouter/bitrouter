use super::*;
use crate::db::{migration::provider_technical, store_format};
use serde::Deserialize;

#[derive(Deserialize)]
pub(super) struct Object {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub tbl_name: String,
    pub sql: String,
}

// A new baseline table always requires an explicit ownership decision.
pub(super) fn belongs(table: &str, owner: Owner) -> Result<bool> {
    let role = match table {
        "checkpoint_evolution" | "seaql_migrations" => return Ok(true),
        "acp_assessment_revisions"
        | "acp_canonical_sessions"
        | "acp_capture_connections"
        | "acp_capture_events"
        | "acp_checkpoint_members"
        | "acp_checkpoints"
        | "acp_effective_assessments"
        | "acp_resource_observations"
        | "adequacy_exploration"
        | "adequacy_pins"
        | "adequacy_reliability_events"
        | "adequacy_semantic_success"
        | "bro_acceptance_keys"
        | "bro_execution_index"
        | "bro_execution_records"
        | "bro_executions"
        | "bro_managed_sessions"
        | "bro_runtime_owners"
        | "bro_runtime_ownership" => Owner::Bro,
        "api_keys"
        | "eval_admission_events"
        | "eval_import_provenance"
        | "eval_results"
        | "eval_snapshots"
        | "eval_subjects"
        | "provider_continuation_key_epoch"
        | "provider_continuations"
        | "requests"
        | "tenancy_active_configurations"
        | "tenancy_bindings"
        | "tenancy_configurations"
        | "tenancy_continuation_epoch"
        | "tenancy_credentials"
        | "tenancy_dispatch_intents"
        | "tenancy_execution_receipts"
        | "tenancy_installation"
        | "tenancy_oauth_slots"
        | "tenancy_operations"
        | "tenancy_principals"
        | "tenancy_receipt_archive"
        | "tenancy_receipt_auxiliary"
        | "tenancy_resource_lifecycle"
        | "tenancy_resource_revisions"
        | "tenancy_tenants"
        | "tenancy_unknown_obligations"
        | "tenancy_usage_evidence"
        | "trajectory_episodes"
        | "trajectory_events"
        | "trajectory_outbox"
        | "trajectory_prefix_index"
        | "trajectory_requests"
        | "users"
        | "tenancy_provider_technical_facts"
        | "tenancy_provider_technical_schema" => Owner::BitRouter,
        _ => anyhow::bail!("unclassified M0 table"),
    };
    Ok(role == owner)
}

// M0 colocated several producers here. An open paid-work mirror pins receipts
// on the service side. Keep its BRO record and fence activation of that pair.
pub(super) fn evolution(owner: Owner) -> &'static str {
    match owner {
        Owner::BitRouter => {
            "kind IN ('gateway_request','gateway_coverage','execution','judge_attempt','judge_request_owner')"
        }
        Owner::Bro => {
            "kind IN ('control','enrollment','route_intent','owned_block_control_v1','owned_block_enrollment_v1','owned_block_intent_v1','checkpoint_schedule','owned_stopped_prefix','judge_job','owned_judge_job','owned_judge_active_v3','owned_judge_budget','owned_judge_pending','native_experiment_unit_pending_v1')"
        }
    }
}

pub(super) async fn source(db: &DatabaseConnection) -> Result<Vec<Object>> {
    let objects = validate_catalog(db, None, false).await?;
    ensure!(
        store_format::inspect_m0(db).await? == store_format::State::Current,
        "expected current M0"
    );
    integrity(db).await?;
    ensure!(
        db.query_one(sql(format!(
            "SELECT 1 FROM checkpoint_evolution WHERE NOT ({}) AND NOT ({}) LIMIT 1",
            evolution(Owner::BitRouter),
            evolution(Owner::Bro)
        )))
        .await?
        .is_none(),
        "unclassified checkpoint_evolution kind"
    );
    validate_references(db, &objects).await?;
    Ok(objects)
}

pub(super) async fn validate_role(
    db: &impl ConnectionTrait,
    owner: Owner,
    serving: bool,
) -> Result<()> {
    validate_catalog(db, Some(owner), serving).await?;
    let rows = db
        .query_all(sql(
            "SELECT version, typeof(applied_at) AS clock_type FROM seaql_migrations LIMIT 2",
        ))
        .await?;
    ensure!(rows.len() == 1, "wrong product format marker");
    let version: String = rows[0].try_get("", "version")?;
    let sealed = owner == Owner::Bro
        && version
            .strip_prefix(owner.marker())
            .and_then(|v| v.strip_prefix(":sealed-v1:"))
            .is_some_and(|v| bitrouter_ai::core::checkpoint::validate_digest(v).is_ok());
    ensure!(
        (version == owner.marker() || sealed)
            && rows[0].try_get::<String>("", "clock_type")? == "integer",
        "wrong product format marker"
    );
    Ok(())
}

pub(super) async fn owned(db: &impl ConnectionTrait, owner: Owner) -> Result<()> {
    validate_role(db, owner, false).await?;
    ensure!(
        db.query_one(sql(format!(
            "SELECT 1 FROM checkpoint_evolution WHERE NOT ({}) LIMIT 1",
            evolution(owner)
        )))
        .await?
        .is_none(),
        "wrong product checkpoint_evolution kind"
    );
    integrity(db).await
}

pub(super) fn frozen() -> Result<Vec<Object>> {
    Ok(serde_json::from_str(include_str!(
        "../migration/baseline/fixtures/sqlite-current-schema.json"
    ))?)
}

pub(super) async fn validate_catalog(
    db: &impl ConnectionTrait,
    role: Option<Owner>,
    serving: bool,
) -> Result<Vec<Object>> {
    ensure!(
        db.get_database_backend() == DatabaseBackend::Sqlite,
        "SQLite required"
    );
    ensure!(db.query_one(sql("SELECT 1 FROM temp.sqlite_schema t JOIN main.sqlite_schema m ON t.name=m.name COLLATE NOCASE LIMIT 1")).await?.is_none(), "shadowed migration catalog");
    let mut expected = BTreeMap::new();
    for object in frozen()? {
        if let Some(role) = role
            && !belongs(&object.tbl_name, role)?
        {
            continue;
        }
        expected.insert(object.name.clone(), object);
    }
    let technical = !serving && provider_technical::available(db).await?;
    ensure!(
        !technical || role != Some(Owner::Bro),
        "provider facts in BRO store"
    );
    // Preserve quoted literals, identifiers and token boundaries across SQLite
    // pretty-print differences. Executable DDL comes from the frozen schema.
    let tokens = regex::Regex::new(r#""(?:[^"]|"")*"|'(?:[^']|'')*'|[a-zA-Z0-9_]+|[^\x09-\x0d ]"#)?;
    let mut objects = Vec::new();
    // Each borrowed read fits the smallest audit page. Validate and release the
    // page before reading another, retaining only recognized catalog objects.
    const PAGE: usize = 8;
    for offset in (0..128).step_by(PAGE) {
        let rows = db.query_all(sql(format!("SELECT type,name,tbl_name,CASE WHEN length(CAST(sql AS BLOB))<=65536 THEN sql END AS sql FROM main.sqlite_schema WHERE NOT (type='index' AND name GLOB 'sqlite_autoindex_*' AND sql IS NULL) ORDER BY name,type LIMIT {PAGE} OFFSET {offset}"))).await?;
        ensure!(offset + rows.len() < 128, "unrecognized SQLite catalog");
        let last = rows.len() < PAGE;
        for row in rows {
            let name: String = row.try_get("", "name")?;
            let kind: String = row.try_get("", "type")?;
            let table: String = row.try_get("", "tbl_name")?;
            let actual: String = row.try_get("", "sql")?;
            if name == "sqlite_sequence" {
                ensure!(
                    kind == "table"
                        && table == name
                        && actual == "CREATE TABLE sqlite_sequence(name,seq)",
                    "invalid sequence catalog"
                );
                continue;
            }
            if matches!(name.as_str(), "sqlite_stat1" | "sqlite_stat4") {
                let columns = if name == "sqlite_stat1" {
                    "tbl,idx,stat"
                } else {
                    "tbl,idx,neq,nlt,ndlt,sample"
                };
                ensure!(
                    kind == "table"
                        && name == table
                        && actual == format!("CREATE TABLE {name}({columns})"),
                    "invalid SQLite statistics catalog"
                );
                continue;
            }
            // A passive table occupying an optional name cannot confer capability.
            // The technical reader/installer checks its exact DDL and stamp before
            // use. Import and archive validation still require the exact catalog.
            if serving
                && matches!(
                    name.as_str(),
                    provider_technical::TABLE | "tenancy_provider_technical_schema"
                )
            {
                ensure!(
                    kind == "table" && name == table && actual.starts_with("CREATE TABLE "),
                    "conflicting optional schema object"
                );
                if db
                    .query_one(Statement::from_sql_and_values(
                        DatabaseBackend::Sqlite,
                        "SELECT 1 FROM pragma_foreign_key_list(?, 'main') LIMIT 1",
                        [name.as_str().into()],
                    ))
                    .await?
                    .is_some()
                {
                    ensure!(
                        name == provider_technical::TABLE
                            && actual
                                == provider_technical::definition(DatabaseBackend::Sqlite)
                                    .to_string(sea_orm::sea_query::SqliteQueryBuilder),
                        "optional table changes gateway foreign keys"
                    );
                }
                continue;
            }
            if technical
                && matches!(
                    name.as_str(),
                    provider_technical::TABLE | "tenancy_provider_technical_schema"
                )
            {
                // available() checked both exact compiled DDL strings and their stamp.
                objects.push(Object {
                    kind,
                    name,
                    tbl_name: table,
                    sql: actual,
                });
                continue;
            }
            let object = expected
                .remove(&name)
                .context("unexpected object in migration catalog")?;
            ensure!(
                kind == object.kind
                    && table == object.tbl_name
                    && tokens
                        .find_iter(&actual)
                        .map(|t| t.as_str())
                        .eq(tokens.find_iter(&object.sql).map(|t| t.as_str())),
                "incompatible schema for {name}"
            );
            objects.push(object);
        }
        if last {
            break;
        }
    }
    ensure!(expected.is_empty(), "incomplete migration catalog");
    Ok(objects)
}

pub(super) async fn integrity(db: &impl ConnectionTrait) -> Result<()> {
    let row = db
        .query_one(sql("PRAGMA main.integrity_check(1)"))
        .await?
        .context("missing integrity result")?;
    ensure!(
        row.try_get::<String>("", "integrity_check")? == "ok",
        "SQLite integrity check failed"
    );
    ensure!(
        db.query_one(sql("PRAGMA main.foreign_key_check"))
            .await?
            .is_none(),
        "broken SQLite foreign key"
    );
    Ok(())
}

async fn validate_references(db: &DatabaseConnection, objects: &[Object]) -> Result<()> {
    // Validate scoped installation/tenant associations while both identity
    // directories are still available. Legacy, unscoped rows retain NULLs.
    for object in objects.iter().filter(|o| o.kind == "table") {
        let table = &object.name;
        let cols = db
            .query_all(sql(format!(
                "SELECT name FROM pragma_table_info('{table}')"
            )))
            .await?;
        let names = cols
            .iter()
            .map(|c| c.try_get::<String>("", "name"))
            .collect::<Result<Vec<_>, _>>()?;
        if table != "tenancy_installation" && names.iter().any(|s| s == "installation_id") {
            let mut invalid = String::from(
                "r.installation_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM tenancy_installation i WHERE i.installation_id=r.installation_id)",
            );
            if names.iter().any(|s| s == "tenant_id") {
                invalid.push_str(" OR (r.tenant_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM tenancy_tenants t WHERE t.id=r.tenant_id))");
            }
            if names.iter().any(|s| s == "creator_principal_id") {
                invalid.push_str(" OR ((r.installation_id IS NULL) <> (r.tenant_id IS NULL)) OR ((r.installation_id IS NULL) <> (r.creator_principal_id IS NULL)) OR (r.creator_principal_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM tenancy_principals p WHERE p.id=r.creator_principal_id))");
            }
            ensure!(
                db.query_one(sql(format!(
                    "SELECT 1 FROM {table} r WHERE {invalid} LIMIT 1"
                )))
                .await?
                .is_none(),
                "unresolved owner identity in {table}"
            );
        }
    }
    for (label, query) in [
        (
            "managed session envelope",
            "SELECT 1 FROM bro_managed_sessions WHERE typeof(payload)<>'text' OR revision<=0 OR payload_bytes<>length(CAST(payload AS BLOB))",
        ),
        (
            "execution journal",
            "SELECT 1 FROM bro_executions e WHERE e.format_version NOT BETWEEN 2 AND 7 OR e.version<=0 OR NOT EXISTS (SELECT 1 FROM bro_execution_index i WHERE i.execution_id=e.id) OR (SELECT count(*) FROM bro_execution_records r WHERE r.execution_id=e.id)<>e.version OR (SELECT min(sequence) FROM bro_execution_records r WHERE r.execution_id=e.id)<>1 OR (SELECT max(sequence) FROM bro_execution_records r WHERE r.execution_id=e.id)<>e.version",
        ),
        (
            "checkpoint session",
            "SELECT 1 FROM acp_checkpoints c WHERE NOT EXISTS (SELECT 1 FROM acp_canonical_sessions s WHERE s.session_key=c.session_key AND s.installation_id IS c.installation_id AND s.tenant_id IS c.tenant_id)",
        ),
        (
            "checkpoint membership",
            "SELECT 1 FROM acp_checkpoint_members m WHERE NOT EXISTS (SELECT 1 FROM acp_checkpoints c WHERE c.checkpoint_id=m.checkpoint_id AND c.installation_id IS m.installation_id AND c.tenant_id IS m.tenant_id) OR NOT EXISTS (SELECT 1 FROM acp_canonical_sessions s WHERE s.session_key=m.session_key AND s.installation_id IS m.installation_id AND s.tenant_id IS m.tenant_id)",
        ),
        (
            "resource revision",
            "SELECT 1 FROM tenancy_resource_lifecycle l LEFT JOIN tenancy_resource_revisions r USING(owner_kind,owner_id,resource_kind,resource_id,revision_id) WHERE r.revision_id IS NULL",
        ),
        (
            "configuration or resource dependency",
            r#"
            WITH refs(ref) AS (
                SELECT json_extract(document_json,'$.policy') FROM tenancy_configurations WHERE json_type(document_json,'$.policy') <> 'null'
                UNION ALL SELECT j.value FROM tenancy_configurations c,json_each(c.document_json,'$.routes') j
                UNION ALL SELECT j.value FROM tenancy_configurations c,json_each(c.dependencies_json) j
                UNION ALL SELECT j.value FROM tenancy_resource_revisions r,json_each(r.dependency_json) j)
            SELECT 1 FROM refs WHERE NOT EXISTS (SELECT 1 FROM tenancy_resource_revisions r
                WHERE (r.owner_kind,r.owner_id,r.resource_kind,r.resource_id,r.revision_id)=
                (json_extract(ref,'$.owner_kind'),json_extract(ref,'$.owner_id'),json_extract(ref,'$.resource_kind'),
                 json_extract(ref,'$.resource_id'),json_extract(ref,'$.revision_id')))"#,
        ),
        (
            "auxiliary receipt",
            "SELECT 1 FROM tenancy_receipt_auxiliary a LEFT JOIN tenancy_execution_receipts r USING(installation_id,tenant_id,execution_id) WHERE r.execution_id IS NULL OR r.content_digest<>a.receipt_digest",
        ),
        (
            "unknown obligation source",
            r#"SELECT 1 FROM tenancy_unknown_obligations o WHERE
            ((o.source_mask & 1) != 0 AND NOT EXISTS (SELECT 1 FROM tenancy_execution_receipts r WHERE (r.installation_id,r.tenant_id,r.execution_id)=(o.installation_id,o.tenant_id,o.execution_id)))
            OR ((o.source_mask & 2) != 0 AND NOT EXISTS (SELECT 1 FROM tenancy_receipt_auxiliary a WHERE (a.installation_id,a.tenant_id,a.execution_id)=(o.installation_id,o.tenant_id,o.execution_id)))
            OR ((o.source_mask & 4) != 0 AND NOT EXISTS (SELECT 1 FROM tenancy_oauth_slots s WHERE (s.installation_id,s.pending_tenant_id,s.pending_execution_id)=(o.installation_id,o.tenant_id,o.execution_id)))"#,
        ),
        (
            "usage receipt",
            "SELECT 1 FROM tenancy_usage_evidence u LEFT JOIN tenancy_execution_receipts r USING(installation_id,tenant_id,execution_id) WHERE r.execution_id IS NULL OR r.content_digest<>u.receipt_digest",
        ),
        (
            "archived receipt",
            "SELECT 1 FROM tenancy_receipt_archive a LEFT JOIN tenancy_execution_receipts r USING(installation_id,tenant_id,execution_id) WHERE r.execution_id IS NULL OR r.storage_kind<>1",
        ),
    ] {
        ensure!(
            db.query_one(sql(format!("{query} LIMIT 1")))
                .await?
                .is_none(),
            "invalid M0 {label} reference"
        );
    }
    Ok(())
}
