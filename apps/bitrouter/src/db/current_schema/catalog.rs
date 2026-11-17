//! Backend catalogs. Every query is bounded and read-only.
//! https://www.postgresql.org/docs/current/catalog-pg-index.html
//! https://dev.mysql.com/doc/refman/8.4/en/information-schema-columns-table.html
use sea_orm::DatabaseBackend;

pub(super) fn tables(backend: DatabaseBackend) -> &'static str {
    match backend {
        DatabaseBackend::Postgres => {
            r#"
SELECT t.relname AS table_name,
CASE WHEN t.relkind='r' AND t.relpersistence='p' AND NOT t.relispartition
 AND NOT t.relrowsecurity AND NOT t.relforcerowsecurity AND t.reloftype=0
 AND NOT EXISTS(SELECT 1 FROM pg_inherits h WHERE h.inhrelid=t.oid OR h.inhparent=t.oid)
 AND NOT EXISTS(SELECT 1 FROM pg_trigger g WHERE g.tgrelid=t.oid AND (NOT g.tgisinternal OR g.tgenabled<>'O'))
 AND NOT EXISTS(SELECT 1 FROM pg_policy p WHERE p.polrelid=t.oid)
 AND NOT EXISTS(SELECT 1 FROM pg_rewrite r WHERE r.ev_class=t.oid)
 AND NOT EXISTS(SELECT 1 FROM pg_class shadow WHERE shadow.relnamespace=pg_my_temp_schema() AND shadow.relname=t.relname)
THEN 'yes' ELSE 'no' END AS compatible
FROM pg_class t JOIN pg_namespace n ON n.oid=t.relnamespace
WHERE n.nspname=current_schema() AND t.relkind IN ('r','p','v','m','f')
ORDER BY t.relname LIMIT 8193"#
        }
        DatabaseBackend::MySql => {
            r#"
SELECT t.TABLE_NAME AS table_name,
CASE WHEN t.TABLE_TYPE='BASE TABLE' AND t.ENGINE='InnoDB'
 AND NOT EXISTS(SELECT 1 FROM information_schema.TRIGGERS g WHERE g.TRIGGER_SCHEMA=DATABASE() AND g.EVENT_OBJECT_TABLE=t.TABLE_NAME)
 AND NOT EXISTS(SELECT 1 FROM information_schema.PARTITIONS p WHERE p.TABLE_SCHEMA=DATABASE() AND p.TABLE_NAME=t.TABLE_NAME AND p.PARTITION_NAME IS NOT NULL)
THEN 'yes' ELSE 'no' END AS compatible
FROM information_schema.TABLES t WHERE t.TABLE_SCHEMA=DATABASE()
ORDER BY t.TABLE_NAME LIMIT 8193"#
        }
        DatabaseBackend::Sqlite => "",
    }
}
pub(super) fn columns(backend: DatabaseBackend) -> &'static str {
    match backend {
        DatabaseBackend::Postgres => {
            r#"
SELECT t.relname AS table_name,a.attname AS column_name,format_type(a.atttypid,a.atttypmod) AS kind,
CASE WHEN a.attnotnull THEN 'no' ELSE 'yes' END AS nullable,
CASE WHEN a.attgenerated='' AND a.attidentity='' AND a.attndims=0
 AND tn.nspname='pg_catalog' AND ty.typtype='b'
 AND (d.oid IS NULL OR octet_length(pg_get_expr(d.adbin,d.adrelid))<=2048)
 THEN 'yes' ELSE 'no' END AS compatible,
CASE WHEN co.oid IS NULL OR (cn.nspname='pg_catalog' AND co.collisdeterministic)
 THEN 'yes' ELSE 'no' END AS collation_valid,
CASE WHEN cn.nspname='pg_catalog' AND co.collname='C' AND co.collisdeterministic
 THEN 'yes' ELSE 'no' END AS binary_value,
CASE WHEN octet_length(pg_get_expr(d.adbin,d.adrelid))<=2048 THEN pg_get_expr(d.adbin,d.adrelid) ELSE NULL END AS default_value,
CASE WHEN EXISTS(SELECT 1 FROM pg_depend dep JOIN pg_class seq ON seq.oid=dep.refobjid AND seq.relkind='S'
 JOIN pg_depend own ON own.classid='pg_class'::regclass AND own.objid=seq.oid
 AND own.refclassid='pg_class'::regclass AND own.refobjid=t.oid AND own.refobjsubid=a.attnum AND own.deptype='a'
 WHERE dep.classid='pg_attrdef'::regclass AND dep.objid=d.oid AND dep.refclassid='pg_class'::regclass)
 THEN 'yes' ELSE 'no' END AS automatic
FROM pg_class t JOIN pg_namespace n ON n.oid=t.relnamespace
JOIN pg_attribute a ON a.attrelid=t.oid AND a.attnum>0 AND NOT a.attisdropped
JOIN pg_type ty ON ty.oid=a.atttypid JOIN pg_namespace tn ON tn.oid=ty.typnamespace
LEFT JOIN pg_attrdef d ON d.adrelid=t.oid AND d.adnum=a.attnum
LEFT JOIN pg_collation co ON co.oid=a.attcollation LEFT JOIN pg_namespace cn ON cn.oid=co.collnamespace
WHERE n.nspname=current_schema() AND t.relkind='r'
ORDER BY t.relname,a.attnum LIMIT 8193"#
        }
        DatabaseBackend::MySql => {
            r#"
SELECT TABLE_NAME AS table_name,COLUMN_NAME AS column_name,COLUMN_TYPE AS kind,
CASE WHEN IS_NULLABLE='YES' THEN 'yes' ELSE 'no' END AS nullable,
CASE WHEN GENERATION_EXPRESSION='' AND EXTRA IN ('','auto_increment') THEN 'yes' ELSE 'no' END AS compatible,
CASE WHEN CHARACTER_SET_NAME IS NULL OR CHARACTER_SET_NAME='utf8mb4' THEN 'yes' ELSE 'no' END AS collation_valid,
CASE WHEN CHARACTER_SET_NAME='utf8mb4' AND COLLATION_NAME='utf8mb4_0900_bin'
 AND EXISTS(SELECT 1 FROM information_schema.COLLATIONS c WHERE c.COLLATION_NAME='utf8mb4_0900_bin' AND c.PAD_ATTRIBUTE='NO PAD')
 THEN 'yes' ELSE 'no' END AS binary_value,
COLUMN_DEFAULT AS default_value,
CASE WHEN EXTRA='auto_increment' THEN 'yes' ELSE 'no' END AS automatic
FROM information_schema.COLUMNS WHERE TABLE_SCHEMA=DATABASE()
ORDER BY TABLE_NAME,ORDINAL_POSITION LIMIT 8193"#
        }
        DatabaseBackend::Sqlite => "",
    }
}
pub(super) fn indexes(backend: DatabaseBackend) -> &'static str {
    match backend {
        DatabaseBackend::Postgres => {
            r#"
SELECT t.relname AS table_name,x.relname AS index_name,a.attname AS column_name,
CASE WHEN i.indisunique THEN 'yes' ELSE 'no' END AS unique_value,
CASE WHEN i.indisprimary THEN 'yes' ELSE 'no' END AS primary_value,
CASE WHEN i.indisvalid AND i.indisready AND i.indimmediate AND NOT i.indisexclusion
 AND NOT COALESCE((to_jsonb(i)->>'indnullsnotdistinct')::boolean,false)
 AND i.indpred IS NULL AND i.indexprs IS NULL AND i.indnkeyatts=i.indnatts
 AND am.amname='btree' AND i.indcollation[k.position-1]=a.attcollation
 AND i.indoption[k.position-1]=0 AND opc.opcdefault AND onsp.nspname='pg_catalog'
 THEN 'yes' ELSE 'no' END AS compatible
FROM pg_index i JOIN pg_class t ON t.oid=i.indrelid JOIN pg_namespace n ON n.oid=t.relnamespace
JOIN pg_class x ON x.oid=i.indexrelid JOIN pg_am am ON am.oid=x.relam
CROSS JOIN LATERAL generate_series(1,i.indnatts) k(position)
LEFT JOIN pg_attribute a ON a.attrelid=t.oid AND a.attnum=i.indkey[k.position-1]
JOIN pg_opclass opc ON opc.oid=i.indclass[k.position-1]
JOIN pg_namespace onsp ON onsp.oid=opc.opcnamespace
WHERE n.nspname=current_schema() AND t.relkind='r'
ORDER BY t.relname,x.relname,k.position LIMIT 8193"#
        }
        DatabaseBackend::MySql => {
            r#"
SELECT TABLE_NAME AS table_name,INDEX_NAME AS index_name,COLUMN_NAME AS column_name,
CASE WHEN NON_UNIQUE=0 THEN 'yes' ELSE 'no' END AS unique_value,
CASE WHEN INDEX_NAME='PRIMARY' THEN 'yes' ELSE 'no' END AS primary_value,
CASE WHEN INDEX_TYPE='BTREE' AND COLLATION='A' AND SUB_PART IS NULL
 AND COLUMN_NAME IS NOT NULL AND EXPRESSION IS NULL AND IS_VISIBLE='YES'
 THEN 'yes' ELSE 'no' END AS compatible
FROM information_schema.STATISTICS WHERE TABLE_SCHEMA=DATABASE()
ORDER BY TABLE_NAME,INDEX_NAME,SEQ_IN_INDEX LIMIT 8193"#
        }
        DatabaseBackend::Sqlite => "",
    }
}
pub(super) fn constraints(backend: DatabaseBackend) -> &'static str {
    match backend {
        DatabaseBackend::Postgres => {
            r#"
SELECT t.relname AS table_name,c.conname AS constraint_name,c.contype::text AS kind,
CASE WHEN c.convalidated AND NOT c.condeferrable AND NOT c.condeferred
 AND COALESCE((to_jsonb(c)->>'conenforced')::boolean,true)
 AND NOT COALESCE((to_jsonb(c)->>'conperiod')::boolean,false)
 AND c.conislocal AND c.coninhcount=0 AND c.conparentid=0
 AND (c.contype<>'c' OR NOT c.connoinherit)
 AND (c.contype<>'f' OR (rn.nspname=current_schema() AND c.confmatchtype='s'))
 THEN 'yes' ELSE 'no' END AS compatible,
COALESCE(r.relname,'') AS foreign_table,
CASE c.confdeltype WHEN 'a' THEN 'NO ACTION' WHEN 'r' THEN 'RESTRICT' WHEN 'c' THEN 'CASCADE' WHEN 'n' THEN 'SET NULL' WHEN 'd' THEN 'SET DEFAULT' ELSE '' END AS delete_rule,
CASE c.confupdtype WHEN 'a' THEN 'NO ACTION' WHEN 'r' THEN 'RESTRICT' WHEN 'c' THEN 'CASCADE' WHEN 'n' THEN 'SET NULL' WHEN 'd' THEN 'SET DEFAULT' ELSE '' END AS update_rule,
CASE WHEN c.contype='c' THEN CASE WHEN octet_length(pg_get_expr(c.conbin,c.conrelid))<=8192 THEN pg_get_expr(c.conbin,c.conrelid) ELSE NULL END ELSE '' END AS expression,
array_to_json(ARRAY(SELECT a.attname::text FROM unnest(c.conkey) WITH ORDINALITY k(num,pos) JOIN pg_attribute a ON a.attrelid=t.oid AND a.attnum=k.num ORDER BY k.pos))::text AS columns_json,
array_to_json(ARRAY(SELECT a.attname::text FROM unnest(c.confkey) WITH ORDINALITY k(num,pos) JOIN pg_attribute a ON a.attrelid=c.confrelid AND a.attnum=k.num ORDER BY k.pos))::text AS references_json
FROM pg_constraint c JOIN pg_class t ON t.oid=c.conrelid JOIN pg_namespace n ON n.oid=t.relnamespace
LEFT JOIN pg_class r ON r.oid=c.confrelid LEFT JOIN pg_namespace rn ON rn.oid=r.relnamespace
WHERE n.nspname=current_schema() AND t.relkind='r' AND c.contype IN ('p','u','f','c','x')
ORDER BY t.relname,c.conname LIMIT 8193"#
        }
        DatabaseBackend::MySql => {
            r#"
SELECT t.TABLE_NAME AS table_name,t.CONSTRAINT_NAME AS constraint_name,
CASE t.CONSTRAINT_TYPE WHEN 'PRIMARY KEY' THEN 'p' WHEN 'UNIQUE' THEN 'u' WHEN 'FOREIGN KEY' THEN 'f' WHEN 'CHECK' THEN 'c' ELSE 'unsupported' END AS kind,
CASE WHEN t.ENFORCED='YES' AND (k.REFERENCED_TABLE_SCHEMA IS NULL OR k.REFERENCED_TABLE_SCHEMA=DATABASE()) THEN 'yes' ELSE 'no' END AS compatible,
k.COLUMN_NAME AS column_name,k.REFERENCED_COLUMN_NAME AS foreign_column,
COALESCE(k.REFERENCED_TABLE_NAME,'') AS foreign_table,
COALESCE(r.DELETE_RULE,'') AS delete_rule,COALESCE(r.UPDATE_RULE,'') AS update_rule,
COALESCE(c.CHECK_CLAUSE,'') AS expression
FROM information_schema.TABLE_CONSTRAINTS t
LEFT JOIN information_schema.KEY_COLUMN_USAGE k ON k.CONSTRAINT_SCHEMA=t.CONSTRAINT_SCHEMA AND k.TABLE_NAME=t.TABLE_NAME AND k.CONSTRAINT_NAME=t.CONSTRAINT_NAME
LEFT JOIN information_schema.REFERENTIAL_CONSTRAINTS r ON r.CONSTRAINT_SCHEMA=t.CONSTRAINT_SCHEMA AND r.TABLE_NAME=t.TABLE_NAME AND r.CONSTRAINT_NAME=t.CONSTRAINT_NAME
LEFT JOIN information_schema.CHECK_CONSTRAINTS c ON c.CONSTRAINT_SCHEMA=t.CONSTRAINT_SCHEMA AND c.CONSTRAINT_NAME=t.CONSTRAINT_NAME
WHERE t.TABLE_SCHEMA=DATABASE()
ORDER BY t.TABLE_NAME,t.CONSTRAINT_NAME,k.ORDINAL_POSITION LIMIT 8193"#
        }
        DatabaseBackend::Sqlite => "",
    }
}
pub(super) const SEQUENCES: &str = r#"
SELECT t.relname AS table_name,a.attname AS column_name,
CASE WHEN own.deptype='a' AND s.seqstart=1 AND s.seqincrement=1 AND s.seqmin=1
 AND NOT s.seqcycle AND s.seqcache=1 AND s.seqtypid='int8'::regtype
 AND s.seqmax=9223372036854775807 AND tn.nspname=current_schema()
 THEN 'yes' ELSE 'no' END AS compatible
FROM pg_class seq JOIN pg_namespace n ON n.oid=seq.relnamespace
JOIN pg_sequence s ON s.seqrelid=seq.oid
LEFT JOIN pg_depend own ON own.classid='pg_class'::regclass AND own.objid=seq.oid AND own.refclassid='pg_class'::regclass AND own.deptype IN ('a','i')
LEFT JOIN pg_class t ON t.oid=own.refobjid LEFT JOIN pg_namespace tn ON tn.oid=t.relnamespace
LEFT JOIN pg_attribute a ON a.attrelid=t.oid AND a.attnum=own.refobjsubid
WHERE n.nspname=current_schema() AND seq.relkind='S'
ORDER BY seq.relname LIMIT 8193"#;
