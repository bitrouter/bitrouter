//! Read-only catalog profile for the current shared PostgreSQL DDL.
//!
//! A ledger selects a format; it cannot authenticate substituted tables, missing
//! uniqueness, changed comparisons or row filters. Run this before reading it.

use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, QueryResult, Statement};

#[cfg(test)]
mod tests;

struct Table {
    name: &'static str,
    // Name, PostgreSQL type/typmod, NOT NULL, identity generation.
    columns: &'static [(&'static str, &'static str, bool, &'static str)],
    constraints: &'static [&'static str],
    indexes: &'static [Index],
}

struct Index {
    name: &'static str,
    unique: bool,
    primary: bool,
    options: &'static str,
    columns: &'static str,
}

pub(super) const TABLE_COUNT: usize = TABLES.len();

pub(super) fn layout() -> (super::TableNames, super::IdentityColumns) {
    let tables = TABLES.iter().map(|table| table.name).collect();
    let identities = TABLES
        .iter()
        .flat_map(|table| {
            table
                .columns
                .iter()
                .filter_map(move |(name, _, _, identity)| {
                    (!identity.is_empty()).then_some((table.name, *name))
                })
        })
        .collect();
    (tables, identities)
}

pub(super) async fn validate(db: &impl ConnectionTrait) -> Result<()> {
    for table in TABLES {
        validate_table(db, table)
            .await
            .with_context(|| format!("incompatible shared schema: {}", table.name))?;
    }
    Ok(())
}

async fn validate_table(db: &impl ConnectionTrait, table: &Table) -> Result<()> {
    let rows = query(db, TABLE_SQL, table.name, 2).await?;
    ensure!(
        rows.len() == 1 && rows[0].try_get::<bool>("", "compatible")?,
        "table kind, persistence, inheritance, row filters, triggers or rules differ"
    );

    let rows = query(db, COLUMNS_SQL, table.name, table.columns.len() + 1).await?;
    ensure!(rows.len() == table.columns.len(), "column count differs");
    for (row, (name, kind, not_null, identity)) in rows.iter().zip(table.columns) {
        ensure!(
            row.try_get::<String>("", "name")? == *name
                && row.try_get::<String>("", "kind")? == *kind
                && row.try_get::<bool>("", "not_null")? == *not_null
                && row.try_get::<String>("", "identity_generation")? == *identity
                && row.try_get::<bool>("", "compatible")?,
            "column {name} type, nullability, identity, default or collation differs"
        );
    }

    let rows = query(db, CONSTRAINTS_SQL, table.name, table.constraints.len() + 1).await?;
    let mut actual = Vec::with_capacity(rows.len());
    for row in rows {
        ensure!(
            row.try_get::<bool>("", "compatible")?,
            "constraint flags or reference schema differ"
        );
        actual.push(row.try_get::<String>("", "definition")?);
    }
    actual.sort_unstable();
    let mut expected = table.constraints.to_vec();
    expected.sort_unstable();
    ensure!(
        actual == expected,
        "primary, unique, foreign or check constraints differ"
    );

    let rows = query(db, INDEXES_SQL, table.name, table.indexes.len() + 1).await?;
    ensure!(rows.len() == table.indexes.len(), "index count differs");
    for row in rows {
        let name: String = row.try_get("", "name")?;
        let expected = table
            .indexes
            .iter()
            .find(|index| index.name == name)
            .context("unexpected shared index")?;
        ensure!(
            row.try_get::<bool>("", "unique_value")? == expected.unique
                && row.try_get::<bool>("", "primary_value")? == expected.primary
                && row.try_get::<String>("", "options")? == expected.options
                && row.try_get::<String>("", "columns")? == expected.columns
                && row.try_get::<bool>("", "compatible")?,
            "index {name} keys, order, uniqueness, collation, operators or validity differ"
        );
    }
    Ok(())
}

async fn query(
    db: &impl ConnectionTrait,
    sql: &str,
    table: &str,
    limit: usize,
) -> Result<Vec<QueryResult>> {
    db.query_all(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        sql,
        [table.into(), i64::try_from(limit)?.into()],
    ))
    .await
    .context("reading shared schema catalog")
}

const TABLE_SQL: &str = r#"
SELECT (c.relkind='r' AND c.relpersistence='p' AND NOT c.relispartition
    AND NOT c.relrowsecurity AND NOT c.relforcerowsecurity
    AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i WHERE i.inhrelid=c.oid OR i.inhparent=c.oid)
    AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_trigger t WHERE t.tgrelid=c.oid AND NOT t.tgisinternal)
    AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_rewrite r WHERE r.ev_class=c.oid)
    AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_policy p WHERE p.polrelid=c.oid)
    AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_class shadow WHERE shadow.relnamespace=pg_my_temp_schema() AND shadow.relname=c.relname)
    AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_constraint k WHERE k.conrelid=c.oid
        AND (NOT k.convalidated OR k.condeferrable OR k.condeferred
            OR NOT COALESCE((to_jsonb(k)->>'conenforced')::boolean,true)
            OR COALESCE((to_jsonb(k)->>'conperiod')::boolean,false)))) AS compatible
FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
WHERE n.nspname=current_schema() AND c.relname=$1 LIMIT $2
"#;

const COLUMNS_SQL: &str = r#"
SELECT a.attname AS name, pg_catalog.format_type(a.atttypid,a.atttypmod) AS kind,
    a.attnotnull AS not_null, a.attidentity::text AS identity_generation,
    (t.typnamespace='pg_catalog'::regnamespace AND NOT a.attisdropped
        AND a.attislocal AND a.attinhcount=0 AND NOT a.atthasdef AND NOT a.atthasmissing
        AND a.attgenerated='' AND a.attcollation=t.typcollation
        AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_attrdef d WHERE d.adrelid=c.oid AND d.adnum=a.attnum)
        AND (a.attidentity='' OR (SELECT count(*)=1 AND bool_and(
                s.seqtypid='pg_catalog.int8'::regtype AND s.seqstart=1 AND s.seqincrement=1
                AND s.seqmin=1 AND s.seqmax=9223372036854775807 AND s.seqcache=1 AND NOT s.seqcycle
                AND sequence.relnamespace=c.relnamespace AND sequence.relpersistence='p')
            FROM pg_catalog.pg_depend dependency
            JOIN pg_catalog.pg_sequence s ON s.seqrelid=dependency.objid
            JOIN pg_catalog.pg_class sequence ON sequence.oid=s.seqrelid
            WHERE dependency.classid='pg_catalog.pg_class'::regclass
                AND dependency.refclassid='pg_catalog.pg_class'::regclass
                AND dependency.refobjid=c.oid AND dependency.refobjsubid=a.attnum
                AND dependency.deptype='i'))) AS compatible
FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid=a.attrelid
JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
LEFT JOIN pg_catalog.pg_type t ON t.oid=a.atttypid
WHERE n.nspname=current_schema() AND c.relname=$1 AND a.attnum>0
ORDER BY a.attnum LIMIT $2
"#;

const CONSTRAINTS_SQL: &str = r#"
SELECT CASE WHEN octet_length(pg_catalog.pg_get_constraintdef(k.oid,false))<=16384
    THEN pg_catalog.pg_get_constraintdef(k.oid,false) ELSE NULL END AS definition,
    (k.convalidated AND NOT k.condeferrable AND NOT k.condeferred
        AND COALESCE((to_jsonb(k)->>'conenforced')::boolean,true)
        AND NOT COALESCE((to_jsonb(k)->>'conperiod')::boolean,false)
        AND k.conislocal AND k.coninhcount=0 AND (k.contype<>'c' OR NOT k.connoinherit)
        AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d
            LEFT JOIN pg_catalog.pg_proc f ON d.refclassid='pg_catalog.pg_proc'::regclass AND f.oid=d.refobjid
            LEFT JOIN pg_catalog.pg_operator o ON d.refclassid='pg_catalog.pg_operator'::regclass AND o.oid=d.refobjid
            WHERE d.classid='pg_catalog.pg_constraint'::regclass AND d.objid=k.oid
                AND (f.pronamespace<>'pg_catalog'::regnamespace OR o.oprnamespace<>'pg_catalog'::regnamespace))
        AND (k.confrelid=0 OR EXISTS (SELECT 1 FROM pg_catalog.pg_class referenced
            JOIN pg_catalog.pg_namespace rn ON rn.oid=referenced.relnamespace
            WHERE referenced.oid=k.confrelid AND rn.nspname=current_schema()))) AS compatible
FROM pg_catalog.pg_constraint k JOIN pg_catalog.pg_class c ON c.oid=k.conrelid
JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
WHERE n.nspname=current_schema() AND c.relname=$1 AND k.contype<>'n'
ORDER BY k.oid LIMIT $2
"#;

const INDEXES_SQL: &str = r#"
SELECT x.relname AS name, i.indisunique AS unique_value, i.indisprimary AS primary_value,
    i.indoption::text AS options,
    CASE WHEN octet_length(keys.columns)<=16384 THEN keys.columns ELSE NULL END AS columns,
    (i.indisvalid AND i.indisready AND i.indislive AND i.indimmediate
        AND NOT i.indnullsnotdistinct AND i.indpred IS NULL AND i.indexprs IS NULL
        AND i.indnatts=i.indnkeyatts AND am.amname='btree'
        AND x.relkind='i' AND x.relpersistence='p' AND NOT x.relispartition
        AND x.relnamespace=c.relnamespace AND keys.compatible) AS compatible
FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid=i.indrelid
JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
JOIN pg_catalog.pg_class x ON x.oid=i.indexrelid
JOIN pg_catalog.pg_am am ON am.oid=x.relam
CROSS JOIN LATERAL (
    SELECT string_agg(pg_catalog.pg_get_indexdef(i.indexrelid,k.position,false),', ' ORDER BY k.position) AS columns,
        bool_and(a.attnum IS NOT NULL AND NOT a.attisdropped
            AND i.indcollation[k.position-1]=a.attcollation
            AND opc.opcnamespace='pg_catalog'::regnamespace AND opc.opcdefault
            AND opc.opcmethod=x.relam) AS compatible
    FROM generate_series(1,i.indnkeyatts) k(position)
    LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid=c.oid AND a.attnum=i.indkey[k.position-1]
    LEFT JOIN pg_catalog.pg_opclass opc ON opc.oid=i.indclass[k.position-1]
) keys
WHERE n.nspname=current_schema() AND c.relname=$1 ORDER BY x.relname LIMIT $2
"#;

// These profiles describe the current DDL in schema.rs and SeaORM's ledger.
// Constraint text uses PostgreSQL's non-pretty catalog deparser; NOT NULL is
// checked through pg_attribute so PostgreSQL 17 and 18 share the same profile.
const TABLES: &[Table] = &[
    Table {
        name: "gateway_shared_eval_admission_events",
        columns: &[
            ("namespace", "character varying(64)", true, ""),
            ("sequence", "bigint", true, "d"),
            ("result_id", "character varying(128)", true, ""),
            ("status", "character varying(32)", true, ""),
            ("reason", "character varying(512)", true, ""),
            ("authority_id", "character varying(512)", true, ""),
            ("created_at", "character varying(64)", true, ""),
        ],
        constraints: &[
            "CHECK (((status)::text = ANY ((ARRAY['admitted'::character varying, 'rejected'::character varying, 'held_out'::character varying, 'disputed'::character varying])::text[])))",
            "FOREIGN KEY (namespace, result_id) REFERENCES gateway_shared_eval_results(namespace, result_id)",
            "PRIMARY KEY (namespace, sequence)",
        ],
        indexes: &[
            Index {
                name: "gateway_shared_eval_admission_events_pkey",
                unique: true,
                primary: true,
                options: "0 0",
                columns: "namespace, sequence",
            },
            Index {
                name: "gateway_shared_eval_admission_latest",
                unique: false,
                primary: false,
                options: "0 0 3",
                columns: "namespace, result_id, sequence",
            },
        ],
    },
    Table {
        name: "gateway_shared_eval_results",
        columns: &[
            ("namespace", "character varying(64)", true, ""),
            ("result_id", "character varying(128)", true, ""),
            ("owner_user_id", "character varying(512)", true, ""),
            ("eval_id", "character varying(512)", true, ""),
            ("idempotency_key", "character varying(512)", true, ""),
            ("authority_id", "character varying(512)", true, ""),
            ("evaluator_kind", "character varying(32)", true, ""),
            ("content_digest", "character varying(128)", true, ""),
            ("result_json", "text", true, ""),
            ("created_at", "character varying(64)", true, ""),
        ],
        constraints: &[
            "CHECK ((octet_length(result_json) <= 1048576))",
            "FOREIGN KEY (namespace, eval_id) REFERENCES gateway_shared_eval_subjects(namespace, eval_id)",
            "PRIMARY KEY (namespace, result_id)",
            "UNIQUE (namespace, owner_user_id, idempotency_key)",
        ],
        indexes: &[
            Index {
                name: "gateway_shared_eval_result_owner",
                unique: false,
                primary: false,
                options: "0 0 0",
                columns: "namespace, owner_user_id, result_id",
            },
            Index {
                name: "gateway_shared_eval_result_subject",
                unique: false,
                primary: false,
                options: "0 0 0 0",
                columns: "namespace, owner_user_id, eval_id, result_id",
            },
            Index {
                name: "gateway_shared_eval_results_namespace_owner_user_id_idempot_key",
                unique: true,
                primary: false,
                options: "0 0 0",
                columns: "namespace, owner_user_id, idempotency_key",
            },
            Index {
                name: "gateway_shared_eval_results_pkey",
                unique: true,
                primary: true,
                options: "0 0",
                columns: "namespace, result_id",
            },
        ],
    },
    Table {
        name: "gateway_shared_eval_snapshots",
        columns: &[
            ("namespace", "character varying(64)", true, ""),
            ("evidence_root", "character varying(128)", true, ""),
            ("owner_user_id", "character varying(512)", true, ""),
            ("manifest_json", "text", true, ""),
            ("result_count", "integer", true, ""),
            ("frozen_at", "character varying(64)", true, ""),
        ],
        constraints: &[
            "CHECK ((octet_length(manifest_json) <= 1048576))",
            "CHECK ((result_count >= 0))",
            "PRIMARY KEY (namespace, evidence_root)",
        ],
        indexes: &[Index {
            name: "gateway_shared_eval_snapshots_pkey",
            unique: true,
            primary: true,
            options: "0 0",
            columns: "namespace, evidence_root",
        }],
    },
    Table {
        name: "gateway_shared_eval_subjects",
        columns: &[
            ("namespace", "character varying(64)", true, ""),
            ("eval_id", "character varying(512)", true, ""),
            ("owner_user_id", "character varying(512)", true, ""),
            ("subject_id", "character varying(512)", true, ""),
            ("scope", "character varying(32)", true, ""),
            ("policy_digest", "character varying(128)", true, ""),
            ("holdout", "boolean", true, ""),
            ("content_digest", "character varying(128)", true, ""),
            ("subject_json", "text", true, ""),
            ("created_at", "character varying(64)", true, ""),
        ],
        constraints: &[
            "CHECK ((octet_length(subject_json) <= 1048576))",
            "PRIMARY KEY (namespace, eval_id)",
        ],
        indexes: &[
            Index {
                name: "gateway_shared_eval_subject_owner",
                unique: false,
                primary: false,
                options: "0 0 0",
                columns: "namespace, owner_user_id, eval_id",
            },
            Index {
                name: "gateway_shared_eval_subjects_pkey",
                unique: true,
                primary: true,
                options: "0 0",
                columns: "namespace, eval_id",
            },
        ],
    },
    Table {
        name: "gateway_shared_metering_requests",
        columns: &[
            ("namespace", "character varying(64)", true, ""),
            ("execution_id", "character varying(256)", true, ""),
            ("tenant_id", "character varying(256)", true, ""),
            ("principal_id", "character varying(256)", true, ""),
            ("request_id", "character varying(256)", false, ""),
            ("event_id", "character varying(256)", true, ""),
            ("source_revision", "bigint", true, ""),
            ("source_digest", "character varying(64)", true, ""),
            ("accepted_at_ms", "bigint", true, ""),
            ("terminal_at_ms", "bigint", true, ""),
            ("known_subtotal_micro_usd", "numeric(20,0)", true, ""),
            ("complete_total_micro_usd", "numeric(20,0)", false, ""),
            ("known_attempts", "integer", true, ""),
            ("unknown_attempts", "integer", true, ""),
            ("not_dispatched_attempts", "integer", true, ""),
            ("payload", "text", true, ""),
        ],
        constraints: &[
            "CHECK ((accepted_at_ms >= 0))",
            "CHECK (((complete_total_micro_usd >= (0)::numeric) AND (complete_total_micro_usd <= '18446744073709551615'::numeric)))",
            "CHECK ((known_attempts >= 0))",
            "CHECK ((((known_attempts = 0) AND (known_subtotal_micro_usd = (0)::numeric)) OR (known_attempts > 0)))",
            "CHECK ((((known_attempts + unknown_attempts) + not_dispatched_attempts) <= 32))",
            "CHECK (((known_subtotal_micro_usd >= (0)::numeric) AND (known_subtotal_micro_usd <= '18446744073709551615'::numeric)))",
            "CHECK ((not_dispatched_attempts >= 0))",
            "CHECK ((octet_length(payload) <= 6701056))",
            "CHECK ((source_revision > 0))",
            "CHECK ((terminal_at_ms >= accepted_at_ms))",
            "CHECK ((unknown_attempts >= 0))",
            "CHECK ((((unknown_attempts = 0) AND (known_attempts > 0) AND (complete_total_micro_usd IS NOT NULL) AND (complete_total_micro_usd = known_subtotal_micro_usd)) OR (((unknown_attempts > 0) OR (known_attempts = 0)) AND (complete_total_micro_usd IS NULL))))",
            "PRIMARY KEY (namespace, execution_id)",
            "UNIQUE (namespace, event_id)",
            "UNIQUE (namespace, request_id)",
        ],
        indexes: &[
            Index {
                name: "gateway_shared_metering_principal_time",
                unique: false,
                primary: false,
                options: "0 0 0 0 0",
                columns: "namespace, tenant_id, principal_id, accepted_at_ms, execution_id",
            },
            Index {
                name: "gateway_shared_metering_requests_namespace_event_id_key",
                unique: true,
                primary: false,
                options: "0 0",
                columns: "namespace, event_id",
            },
            Index {
                name: "gateway_shared_metering_requests_namespace_request_id_key",
                unique: true,
                primary: false,
                options: "0 0",
                columns: "namespace, request_id",
            },
            Index {
                name: "gateway_shared_metering_requests_pkey",
                unique: true,
                primary: true,
                options: "0 0",
                columns: "namespace, execution_id",
            },
            Index {
                name: "gateway_shared_metering_tenant_time",
                unique: false,
                primary: false,
                options: "0 0 0 0",
                columns: "namespace, tenant_id, accepted_at_ms, execution_id",
            },
        ],
    },
    Table {
        name: "gateway_shared_migrations",
        columns: &[
            ("version", "character varying", true, ""),
            ("applied_at", "bigint", true, ""),
        ],
        constraints: &["PRIMARY KEY (version)"],
        indexes: &[Index {
            name: "gateway_shared_migrations_pkey",
            unique: true,
            primary: true,
            options: "0",
            columns: "version",
        }],
    },
    Table {
        name: "gateway_shared_native_continuations",
        columns: &[
            ("namespace", "character varying(64)", true, ""),
            ("identity", "character varying(256)", true, ""),
            ("revision", "bigint", true, ""),
            ("body", "text", true, ""),
            ("publication_state", "character varying(32)", true, ""),
            ("lease_until", "character varying(32)", true, ""),
            ("expires_at", "character varying(32)", true, ""),
            ("purge_after", "character varying(32)", true, ""),
        ],
        constraints: &[
            "CHECK ((revision >= 0))",
            "PRIMARY KEY (namespace, identity)",
        ],
        indexes: &[
            Index {
                name: "gateway_shared_native_continuations_pkey",
                unique: true,
                primary: true,
                options: "0 0",
                columns: "namespace, identity",
            },
            Index {
                name: "gateway_shared_native_expiry",
                unique: false,
                primary: false,
                options: "0 0 0",
                columns: "namespace, expires_at, identity",
            },
            Index {
                name: "gateway_shared_native_lease",
                unique: false,
                primary: false,
                options: "0 0 0 0",
                columns: "namespace, publication_state, lease_until, identity",
            },
            Index {
                name: "gateway_shared_native_purge",
                unique: false,
                primary: false,
                options: "0 0 0",
                columns: "namespace, purge_after, identity",
            },
        ],
    },
    Table {
        name: "gateway_shared_records",
        columns: &[
            ("id", "character varying(64)", true, ""),
            ("namespace", "character varying(64)", true, ""),
            ("kind", "character varying(32)", true, ""),
            ("key", "character varying(256)", true, ""),
            ("revision", "bigint", true, ""),
            ("status", "character varying(32)", true, ""),
            ("due_at_ms", "bigint", false, ""),
            ("updated_at_ms", "bigint", true, ""),
            ("body", "text", true, ""),
        ],
        constraints: &["PRIMARY KEY (id)"],
        indexes: &[
            Index {
                name: "gateway_shared_due",
                unique: false,
                primary: false,
                options: "0 0 0 0 0",
                columns: "namespace, kind, status, due_at_ms, id",
            },
            Index {
                name: "gateway_shared_records_pkey",
                unique: true,
                primary: true,
                options: "0",
                columns: "id",
            },
        ],
    },
    Table {
        name: "gateway_shared_trajectory_episodes",
        columns: &[
            ("namespace", "character varying(64)", true, ""),
            ("episode_id", "character varying(512)", true, ""),
            ("owner_user_id", "character varying(512)", true, ""),
            ("correlation_digest", "character varying(512)", true, ""),
            ("correlation_key_id", "character varying(128)", true, ""),
            ("correlation_source", "character varying(512)", true, ""),
            ("history_completeness", "character varying(32)", true, ""),
            ("next_sequence", "bigint", true, ""),
            ("first_captured_at", "character varying(64)", true, ""),
            ("last_captured_at", "character varying(64)", true, ""),
            ("closed_at", "character varying(64)", false, ""),
            ("latest_request_id", "character varying(512)", false, ""),
        ],
        constraints: &[
            "CHECK ((next_sequence > 0))",
            "PRIMARY KEY (namespace, episode_id)",
        ],
        indexes: &[
            Index {
                name: "gateway_shared_trajectory_episode_owner",
                unique: false,
                primary: false,
                options: "0 0 0",
                columns: "namespace, owner_user_id, episode_id",
            },
            Index {
                name: "gateway_shared_trajectory_episodes_pkey",
                unique: true,
                primary: true,
                options: "0 0",
                columns: "namespace, episode_id",
            },
        ],
    },
    Table {
        name: "gateway_shared_trajectory_events",
        columns: &[
            ("namespace", "character varying(64)", true, ""),
            ("event_id", "character varying(512)", true, ""),
            ("owner_user_id", "character varying(512)", true, ""),
            ("episode_id", "character varying(512)", true, ""),
            ("request_id", "character varying(512)", false, ""),
            ("sequence", "bigint", true, ""),
            ("kind", "character varying(32)", true, ""),
            ("event_json", "text", true, ""),
            ("content_digest", "character varying(128)", true, ""),
            ("captured_at", "character varying(64)", true, ""),
        ],
        constraints: &[
            "CHECK ((sequence > 0))",
            "PRIMARY KEY (namespace, event_id)",
            "UNIQUE (namespace, episode_id, sequence)",
        ],
        indexes: &[
            Index {
                name: "gateway_shared_trajectory_eve_namespace_episode_id_sequence_key",
                unique: true,
                primary: false,
                options: "0 0 0",
                columns: "namespace, episode_id, sequence",
            },
            Index {
                name: "gateway_shared_trajectory_events_pkey",
                unique: true,
                primary: true,
                options: "0 0",
                columns: "namespace, event_id",
            },
            Index {
                name: "gateway_shared_trajectory_history",
                unique: false,
                primary: false,
                options: "0 0 0 0",
                columns: "namespace, owner_user_id, episode_id, sequence",
            },
        ],
    },
    Table {
        name: "gateway_shared_trajectory_outbox",
        columns: &[
            ("namespace", "character varying(64)", true, ""),
            ("outbox_id", "character varying(512)", true, ""),
            ("owner_user_id", "character varying(512)", true, ""),
            ("topic", "character varying(512)", true, ""),
            ("payload_json", "text", true, ""),
            ("payload_digest", "character varying(128)", true, ""),
            ("attempts", "bigint", true, ""),
            ("created_at", "character varying(64)", true, ""),
            ("delivered_at", "character varying(64)", false, ""),
        ],
        constraints: &[
            "CHECK ((attempts >= 0))",
            "PRIMARY KEY (namespace, outbox_id)",
        ],
        indexes: &[
            Index {
                name: "gateway_shared_trajectory_outbox_pending",
                unique: false,
                primary: false,
                options: "0 0 0 0 0",
                columns: "namespace, delivered_at, attempts, created_at, outbox_id",
            },
            Index {
                name: "gateway_shared_trajectory_outbox_pkey",
                unique: true,
                primary: true,
                options: "0 0",
                columns: "namespace, outbox_id",
            },
        ],
    },
    Table {
        name: "gateway_shared_trajectory_prefix_index",
        columns: &[
            ("namespace", "character varying(64)", true, ""),
            ("owner_user_id", "character varying(512)", true, ""),
            ("full_input_digest", "character varying(512)", true, ""),
            ("episode_id", "character varying(512)", true, ""),
            ("ambiguous", "boolean", true, ""),
        ],
        constraints: &["PRIMARY KEY (namespace, owner_user_id, full_input_digest)"],
        indexes: &[
            Index {
                name: "gateway_shared_trajectory_prefix_episode",
                unique: false,
                primary: false,
                options: "0 0 0",
                columns: "namespace, owner_user_id, episode_id",
            },
            Index {
                name: "gateway_shared_trajectory_prefix_index_pkey",
                unique: true,
                primary: true,
                options: "0 0 0",
                columns: "namespace, owner_user_id, full_input_digest",
            },
        ],
    },
    Table {
        name: "gateway_shared_trajectory_requests",
        columns: &[
            ("namespace", "character varying(64)", true, ""),
            ("request_id", "character varying(512)", true, ""),
            ("owner_user_id", "character varying(512)", true, ""),
            ("episode_id", "character varying(512)", true, ""),
            ("start_event_id", "character varying(512)", true, ""),
            ("settlement_event_id", "character varying(512)", false, ""),
            ("settlement_outbox_id", "character varying(512)", false, ""),
            ("full_input_digest", "character varying(512)", true, ""),
            ("native_parent_id", "character varying(512)", false, ""),
            ("protocol", "character varying(512)", true, ""),
            ("status", "character varying(32)", true, ""),
        ],
        constraints: &["PRIMARY KEY (namespace, request_id)"],
        indexes: &[
            Index {
                name: "gateway_shared_trajectory_request_episode",
                unique: false,
                primary: false,
                options: "0 0 0 0",
                columns: "namespace, owner_user_id, episode_id, request_id",
            },
            Index {
                name: "gateway_shared_trajectory_requests_pkey",
                unique: true,
                primary: true,
                options: "0 0",
                columns: "namespace, request_id",
            },
        ],
    },
];
