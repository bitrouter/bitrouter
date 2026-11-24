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
    sql.to_ascii_lowercase()
        .replace(['"', '`'], "")
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn retained(sql: &str) -> bool {
    HISTORY.iter().any(|table| {
        ["from", "join"].iter().any(|source| {
            sql.contains(&format!("{source} {table}"))
                || sql.contains(&format!("{source} main.{table}"))
        })
    })
}

fn reading(sql: &str) -> bool {
    sql.starts_with("select ") || sql.starts_with("with ")
}

fn first_row(sql: &str) -> bool {
    sql.starts_with("select 1 ")
        && sql.ends_with("limit 1")
        && !sql.contains("where ")
        && !sql.contains("join ")
        && !sql.contains("order by ")
}

fn exact_execution_predicate(predicate: &str) -> bool {
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
                let Some(next) = depth.checked_sub(1) else {
                    return false;
                };
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
        return false;
    }
    let clauses: Vec<String> = root
        .split(" and ")
        .map(|clause| {
            clause
                .chars()
                .filter(|character| !character.is_ascii_whitespace())
                .collect()
        })
        .collect();
    let keys = |tenant: &str, execution: &str| {
        ["installation_id=?", tenant, execution]
            .iter()
            .all(|key| clauses.iter().any(|clause| clause.as_str() == *key))
    };
    keys("tenant_id=?", "execution_id=?") || keys("pending_tenant_id=?", "pending_execution_id=?")
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
            let shape = normalize(&info.statement.sql);
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
            let shape = normalize(&query.sql);
            if first_row(&shape) {
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
                existence || point,
                "startup traversed retained history: {statement}"
            );
        }
        Ok(statements)
    }
}
