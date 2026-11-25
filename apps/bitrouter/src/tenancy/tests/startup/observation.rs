use super::*;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

const HISTORY: &[&str] = &[
    "tenancy_tenants",
    "tenancy_principals",
    "tenancy_bindings",
    "tenancy_credentials",
    "tenancy_operations",
    "tenancy_resource_revisions",
    "tenancy_resource_lifecycle",
    "tenancy_configurations",
    "tenancy_active_configurations",
    "tenancy_execution_receipts",
    "tenancy_receipt_archive",
    "tenancy_receipt_auxiliary",
    "tenancy_unknown_obligations",
    "tenancy_usage_evidence",
    "eval_subjects",
    "eval_results",
    "eval_admissions",
    "eval_snapshots",
    "tenancy_oauth_slots",
    "provider_continuations",
    "trajectory_outbox",
    "tenancy_provider_technical_facts",
];

fn normalize(sql: &str) -> String {
    let mut parameters = String::with_capacity(sql.len());
    let mut characters = sql.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '$' && characters.peek().is_some_and(char::is_ascii_digit) {
            while characters.peek().is_some_and(char::is_ascii_digit) {
                let _digit = characters.next();
            }
            parameters.push('?');
        } else {
            parameters.push(character);
        }
    }
    parameters
        .to_ascii_lowercase()
        .replace(['"', '`'], "")
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn statement_shape(statement: &Statement) -> String {
    let mut shape = normalize(&statement.sql);
    // SeaQuery binds LIMIT on server backends. Admit the one-row existence
    // probe only when the observed bound value proves it is actually one.
    if shape.ends_with("limit ?")
        && statement
            .values
            .as_ref()
            .and_then(|values| values.0.last())
            .is_some_and(|value| {
                matches!(
                    value,
                    sea_orm::Value::BigUnsigned(Some(1))
                        | sea_orm::Value::BigInt(Some(1))
                        | sea_orm::Value::Int(Some(1))
                )
            })
    {
        let _parameter = shape.pop();
        shape.push('1');
    }
    shape
}

fn retained(sql: &str) -> bool {
    HISTORY.iter().any(|table| {
        ["from", "join"].iter().any(|source| {
            sql.contains(&format!("{source} {table}"))
                || sql.contains(&format!("{source} main.{table}"))
                || sql.contains(&format!("{source} public.{table}"))
        })
    })
}

fn reading(sql: &str) -> bool {
    sql.starts_with("select ") || sql.starts_with("with ")
}

fn first_row(sql: &str) -> bool {
    (sql.starts_with("select 1 ") || sql.starts_with("select ? "))
        && sql.ends_with("limit 1")
        && !sql.contains("where ")
        && !sql.contains("join ")
        && !sql.contains("order by ")
}

fn root_clauses(predicate: &str) -> Option<Vec<String>> {
    // All identity equalities must be independent root AND conjuncts. An OR
    // in a nested byte/type guard can only narrow that already indexed range;
    // a root OR or identity hidden inside an OR group cannot prove a point read.
    let mut root = String::new();
    let mut depth = 0_usize;
    for character in predicate.chars() {
        match character {
            '(' => {
                depth += 1;
                if depth == 1 {
                    root.push(' ');
                }
            }
            ')' => {
                let next = depth.checked_sub(1)?;
                depth = next;
                if depth == 0 {
                    root.push(' ');
                }
            }
            _ if depth == 0 => root.push(character),
            _ => {}
        }
    }
    if depth != 0 || root.contains(" or ") {
        return None;
    }
    Some(
        root.split(" and ")
            .map(|clause| {
                clause
                    .chars()
                    .filter(|character| !character.is_ascii_whitespace())
                    .collect()
            })
            .collect(),
    )
}

fn exact_execution_predicate(predicate: &str) -> bool {
    let Some(clauses) = root_clauses(predicate) else {
        return false;
    };
    let keys = |tenant: &str, execution: &str| {
        ["installation_id=?", tenant, execution]
            .iter()
            .all(|key| clauses.iter().any(|clause| clause.as_str() == *key))
    };
    keys("tenant_id=?", "execution_id=?") || keys("pending_tenant_id=?", "pending_execution_id=?")
}

fn scheduler_read(shape: &str) -> bool {
    retained_from(shape, "tenancy_unknown_obligations")
        && shape.contains(" order by ")
        && shape.contains("due_seconds")
        && !shape.contains(" join ")
}

fn retained_from(shape: &str, table: &str) -> bool {
    ["", "main.", "public."]
        .iter()
        .any(|schema| shape.contains(&format!("from {schema}{table} ")))
}

fn ordered_keys(shape: &str) -> bool {
    scheduler_read(shape)
        || (retained_from(shape, "tenancy_execution_receipts")
            && shape.contains(" order by ")
            && !shape.contains("receipt_json")
            && !shape.contains(" join ")
            && !shape.contains(" group by "))
}

fn scheduler_lookup(shape: &str) -> bool {
    if !scheduler_read(shape) || !shape.ends_with("limit 1") {
        return false;
    }
    let Some((_, predicate)) = shape.split_once(" where ") else {
        return false;
    };
    let Some((predicate, order)) = predicate.split_once(" order by ") else {
        return false;
    };
    let Some(clauses) = root_clauses(predicate) else {
        return false;
    };
    if !["installation_id=?", "escalated=?"]
        .iter()
        .all(|key| clauses.iter().any(|clause| clause.as_str() == *key))
    {
        return false;
    }
    order.strip_suffix(" limit 1").is_some_and(|order| {
        let fields: Vec<_> = order
            .split(',')
            .map(|field| {
                field
                    .trim()
                    .strip_prefix("tenancy_unknown_obligations.")
                    .unwrap_or(field.trim())
            })
            .collect();
        fields
            == [
                "due_seconds asc",
                "due_nanos asc",
                "tenant_id asc",
                "execution_id asc",
            ]
    })
}

fn require_no_ordered_sort(shape: &str, detail: &str) -> Result<()> {
    if !ordered_keys(shape) {
        return Ok(());
    }
    let node = detail
        .trim()
        .strip_prefix("->")
        .unwrap_or(detail.trim())
        .trim();
    ensure!(
        !node.contains("temp b-tree")
            && !node.starts_with("sort")
            && !node.starts_with("incremental sort"),
        "ordered retained lookup sorts its range: {shape}: {detail}"
    );
    ensure!(
        !node.contains("seq scan on tenancy_unknown_obligations")
            && !node.contains("seq scan on tenancy_execution_receipts"),
        "ordered retained lookup scans its range: {shape}: {detail}"
    );
    Ok(())
}

#[derive(Default)]
pub(super) struct Queries {
    enabled: AtomicBool,
    statements: Mutex<Vec<String>>,
    // Bound parameters are retained only for startup SELECT plans. They never
    // enter diagnostics; authentication and provisioning run outside this window.
    reads: Mutex<Vec<Statement>>,
}

impl Queries {
    pub fn record(&self, info: &sea_orm::metric::Info<'_>) {
        if self.enabled.load(Ordering::Acquire)
            && let Ok(mut statements) = self.statements.lock()
        {
            let shape = statement_shape(info.statement);
            if reading(&shape)
                && retained(&shape)
                && let Ok(mut reads) = self.reads.lock()
            {
                reads.push(info.statement.clone());
            }
            statements.push(shape);
        }
    }

    pub fn start(&self) -> Result<()> {
        self.statements
            .lock()
            .map_err(|_| anyhow::anyhow!("SQL observation poisoned"))?
            .clear();
        self.reads
            .lock()
            .map_err(|_| anyhow::anyhow!("SQL plan observation poisoned"))?
            .clear();
        self.enabled.store(true, Ordering::Release);
        Ok(())
    }

    pub fn stop(&self) {
        self.enabled.store(false, Ordering::Release);
    }

    pub fn snapshot(&self) -> Result<Vec<String>> {
        Ok(self
            .statements
            .lock()
            .map_err(|_| anyhow::anyhow!("SQL observation poisoned"))?
            .clone())
    }

    pub fn scheduler_statement(&self) -> Result<Statement> {
        self.reads
            .lock()
            .map_err(|_| anyhow::anyhow!("SQL plan observation poisoned"))?
            .iter()
            .find(|query| scheduler_read(&statement_shape(query)))
            .cloned()
            .context("actual startup scheduler query was not observed")
    }

    pub fn require_no_history_writes(&self) -> Result<()> {
        for statement in self.snapshot()? {
            let writing = ["insert ", "update ", "delete ", "replace "]
                .iter()
                .any(|verb| statement.starts_with(verb));
            ensure!(
                !writing || !HISTORY.iter().any(|table| statement.contains(table)),
                "mature startup rewrote retained history: {statement}"
            );
        }
        Ok(())
    }

    pub async fn require_indexed_retained_reads(&self, db: &impl ConnectionTrait) -> Result<()> {
        let reads = self
            .reads
            .lock()
            .map_err(|_| anyhow::anyhow!("SQL plan observation poisoned"))?
            .clone();
        for query in reads {
            let shape = statement_shape(&query);
            if first_row(&shape) {
                continue;
            }
            if db.get_database_backend() == sea_orm::DatabaseBackend::Postgres {
                // Planner sorts are checked only for the scheduler and canonical
                // receipt-key pages, never for catalog/operator queries. Explain
                // keeps the exact observed SQL and parameter values.
                if !ordered_keys(&shape) {
                    continue;
                }
                let plan = db
                    .query_all(Statement {
                        sql: format!("EXPLAIN (COSTS OFF) {}", query.sql),
                        ..query
                    })
                    .await?;
                for step in plan {
                    let detail: String = step.try_get("", "QUERY PLAN")?;
                    require_no_ordered_sort(&shape, &detail.to_ascii_lowercase())?;
                }
                continue;
            }
            let plan = db
                .query_all(Statement {
                    sql: format!("EXPLAIN QUERY PLAN {}", query.sql),
                    ..query
                })
                .await?;
            for step in plan {
                let detail: String = step.try_get("", "detail")?;
                let detail = detail.to_ascii_lowercase();
                require_no_ordered_sort(&shape, &detail)?;
                ensure!(
                    !HISTORY
                        .iter()
                        .any(|table| detail.contains(&format!("scan {table}"))
                            || detail.contains(&format!("scan main.{table}"))),
                    "startup plan scans retained rows: {shape}: {detail}"
                );
            }
        }
        Ok(())
    }

    pub fn require_bounded(&self) -> Result<Vec<String>> {
        let statements = self.snapshot()?;
        ensure!(
            statements
                .iter()
                .any(|s| s.contains("from tenancy_installation")),
            "startup driver observation missing"
        );
        for statement in &statements {
            if !reading(statement) {
                continue;
            }
            if !retained(statement) {
                continue;
            }
            let compact: String = statement
                .chars()
                .filter(|c| !c.is_ascii_whitespace())
                .collect();
            ensure!(
                !compact.contains("count(") && !compact.contains("sum("),
                "startup aggregated retained history: {statement}"
            );
            let existence = first_row(statement);
            // Recovery may inspect only the exact receipt/evidence key for an
            // unfinished intent; checking just LIMIT would permit a hidden scan.
            let predicate = statement.split_once(" where ").map(|(_, rest)| {
                rest.split(" order by ")
                    .next()
                    .unwrap_or(rest)
                    .split(" limit ")
                    .next()
                    .unwrap_or(rest)
            });
            let point = predicate.is_some_and(exact_execution_predicate)
                && statement.contains("limit ")
                && !statement.contains("join ");
            ensure!(
                existence || point || scheduler_lookup(statement),
                "startup traversed retained history: {statement}"
            );
        }
        Ok(statements)
    }
}

#[test]
fn postgres_bound_parameters_preserve_exact_recovery_predicates() {
    let point = normalize("installation_id=$1 AND tenant_id=$2 AND execution_id=$12");
    assert!(exact_execution_predicate(&point));
    let pending = normalize(
        "installation_id=$1 AND pending_tenant_id=$2 AND pending_execution_id=$3 AND slot_key>$4",
    );
    assert!(exact_execution_predicate(&pending));
    assert!(!exact_execution_predicate(&normalize(
        "installation_id=$1 AND tenant_id=$2 OR execution_id=$3"
    )));
    assert!(!exact_execution_predicate(&normalize(
        "installation_id=$1 AND (tenant_id=$2 OR execution_id=$3)"
    )));
    assert!(first_row(&normalize(
        "SELECT $1 FROM public.tenancy_execution_receipts LIMIT 1"
    )));
    assert!(!first_row(&normalize(
        "SELECT $1 FROM public.tenancy_execution_receipts WHERE unknown_outcome=$2 LIMIT 1"
    )));
    let probe = |limit: u64| {
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT $1 FROM public.tenancy_execution_receipts LIMIT $2",
            [1_i32.into(), limit.into()],
        )
    };
    assert!(first_row(&statement_shape(&probe(1))));
    assert!(!first_row(&statement_shape(&probe(257))));
}

#[test]
fn only_required_single_scheduler_lookup_is_allowed_during_startup() {
    let scheduler = normalize(
        "SELECT due_seconds FROM tenancy_unknown_obligations WHERE installation_id=$1 AND escalated=$2 ORDER BY tenancy_unknown_obligations.due_seconds ASC, tenancy_unknown_obligations.due_nanos ASC, tenancy_unknown_obligations.tenant_id ASC, tenancy_unknown_obligations.execution_id ASC LIMIT 1",
    );
    assert!(scheduler_lookup(&scheduler));
    assert!(!scheduler_lookup(
        &scheduler.replace("limit 1", "limit 257")
    ));
    assert!(!scheduler_lookup(&scheduler.replace(
        "installation_id=? and escalated=?",
        "installation_id=? or escalated=?"
    )));
    assert!(require_no_ordered_sort(&scheduler, "use temp b-tree for order by").is_err());
    assert!(
        require_no_ordered_sort(
            &scheduler,
            "  -> Incremental Sort".to_ascii_lowercase().as_str()
        )
        .is_err()
    );
    // Catalog and operator inspection plans may legitimately sort. They do not
    // become scheduler queries merely because they mention the table's name.
    let catalog = "select indexdef from pg_indexes where tablename='tenancy_unknown_obligations' order by indexname";
    assert!(require_no_ordered_sort(catalog, "sort").is_ok());
}
