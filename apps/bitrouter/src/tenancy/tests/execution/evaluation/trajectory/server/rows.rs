//! Exact stored cells, including namespace, creator, timestamps and all closure edges.
use super::*;
use std::collections::BTreeMap;

// Enumerate every column of the five trajectory and four Eval tables. Compare
// strings as stored, not parsed JSON: whitespace/spelling changes must be visible.
const TABLES: &[(&str, &[&str])] = &[
    (
        "trajectory_episodes",
        &[
            "episode_id",
            "owner_user_id",
            "correlation_digest",
            "correlation_key_id",
            "correlation_source",
            "history_completeness",
            "next_sequence",
            "first_captured_at",
            "last_captured_at",
            "closed_at",
            "latest_request_id",
            "installation_id",
            "tenant_id",
            "creator_principal_id",
            "logical_identity_json",
        ],
    ),
    (
        "trajectory_requests",
        &[
            "request_id",
            "owner_user_id",
            "episode_id",
            "start_event_id",
            "settlement_event_id",
            "settlement_outbox_id",
            "full_input_digest",
            "native_parent_id",
            "protocol",
            "status",
            "installation_id",
            "tenant_id",
            "creator_principal_id",
            "logical_identity_json",
        ],
    ),
    (
        "trajectory_events",
        &[
            "event_id",
            "owner_user_id",
            "episode_id",
            "request_id",
            "sequence",
            "kind",
            "event_json",
            "content_digest",
            "captured_at",
            "installation_id",
            "tenant_id",
            "creator_principal_id",
        ],
    ),
    (
        "trajectory_prefix_index",
        &[
            "owner_user_id",
            "full_input_digest",
            "episode_id",
            "ambiguous",
            "installation_id",
            "tenant_id",
            "creator_principal_id",
            "logical_identity_json",
        ],
    ),
    (
        "trajectory_outbox",
        &[
            "outbox_id",
            "owner_user_id",
            "topic",
            "payload_json",
            "payload_digest",
            "attempts",
            "created_at",
            "delivered_at",
            "installation_id",
            "tenant_id",
            "creator_principal_id",
            "logical_identity_json",
        ],
    ),
    (
        "eval_subjects",
        &[
            "eval_id",
            "owner_user_id",
            "subject_id",
            "scope",
            "policy_digest",
            "holdout",
            "content_digest",
            "subject_json",
            "created_at",
            "installation_id",
            "tenant_id",
            "creator_principal_id",
        ],
    ),
    (
        "eval_results",
        &[
            "result_id",
            "owner_user_id",
            "eval_id",
            "idempotency_key",
            "authority_id",
            "evaluator_kind",
            "content_digest",
            "result_json",
            "created_at",
            "installation_id",
            "tenant_id",
            "creator_principal_id",
        ],
    ),
    (
        "eval_admission_events",
        &[
            "sequence",
            "result_id",
            "status",
            "reason",
            "authority_id",
            "created_at",
            "installation_id",
            "tenant_id",
            "creator_principal_id",
        ],
    ),
    (
        "eval_snapshots",
        &[
            "evidence_root",
            "owner_user_id",
            "manifest_json",
            "result_count",
            "frozen_at",
            "installation_id",
            "tenant_id",
            "creator_principal_id",
        ],
    ),
];

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Snapshot(BTreeMap<&'static str, Vec<Vec<Option<String>>>>);

impl Snapshot {
    pub(super) async fn read(connection: &impl ConnectionTrait) -> Result<Self> {
        let backend = connection.get_database_backend();
        let (quote, cast) = match backend {
            DatabaseBackend::Postgres => ('"', "TEXT"),
            DatabaseBackend::MySql => ('`', "CHAR"),
            _ => anyhow::bail!("compound snapshot requires a server backend"),
        };
        let mut tables = BTreeMap::new();
        for &(table, columns) in TABLES {
            let projection = columns
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    format!("CAST({quote}{column}{quote} AS {cast}) AS c{index}")
                })
                .collect::<Vec<_>>()
                .join(",");
            let mut rows = connection
                .query_all(Statement::from_string(
                    backend,
                    format!("SELECT {projection} FROM {table}"),
                ))
                .await?
                .into_iter()
                .map(|row| {
                    (0..columns.len())
                        .map(|index| row.try_get::<Option<String>>("", &format!("c{index}")))
                        .collect::<std::result::Result<Vec<_>, _>>()
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows.sort();
            tables.insert(table, rows);
        }
        Ok(Self(tables))
    }

    pub(super) fn count(&self, table: &str) -> Result<usize> {
        Ok(self.0.get(table).context("snapshot table missing")?.len())
    }

    pub(super) fn cell(&self, table: &str, column: &str) -> Result<Option<&str>> {
        let rows = self.0.get(table).context("snapshot table missing")?;
        ensure!(rows.len() == 1, "expected one {table} row");
        let index = TABLES
            .iter()
            .find(|(name, _)| *name == table)
            .and_then(|(_, columns)| columns.iter().position(|name| *name == column))
            .context("snapshot column missing")?;
        Ok(rows[0][index].as_deref())
    }

    pub(super) fn owned_by(&self, fixture: &Fixture) -> Result<()> {
        for &(table, columns) in TABLES {
            for (column, expected) in [
                ("installation_id", fixture.installation.as_str()),
                ("tenant_id", fixture.tenant.as_str()),
                ("creator_principal_id", fixture.principal.as_str()),
            ] {
                let index = columns
                    .iter()
                    .position(|name| *name == column)
                    .context("ownership column")?;
                for row in self.0.get(table).context("table")? {
                    ensure!(
                        row[index].as_deref() == Some(expected),
                        "wrong {table} {column}"
                    );
                }
            }
        }
        Ok(())
    }

    pub(super) fn started(&self) -> Result<()> {
        ensure!(self.count("trajectory_requests")? == 1);
        ensure!(self.count("trajectory_episodes")? == 1);
        ensure!(self.count("trajectory_prefix_index")? == 1);
        ensure!(self.count("trajectory_events")? >= 2);
        ensure!(self.cell("trajectory_requests", "status")? == Some("started"));
        ensure!(
            self.cell("trajectory_requests", "settlement_event_id")?
                .is_none()
        );
        ensure!(
            self.cell("trajectory_requests", "settlement_outbox_id")?
                .is_none()
        );
        for table in [
            "trajectory_outbox",
            "eval_subjects",
            "eval_results",
            "eval_admission_events",
            "eval_snapshots",
        ] {
            ensure!(self.count(table)? == 0, "start unexpectedly wrote {table}");
        }
        Ok(())
    }

    pub(super) fn terminal(&self, started_events: usize) -> Result<()> {
        ensure!(self.count("trajectory_requests")? == 1 && self.count("trajectory_episodes")? == 1);
        ensure!(self.count("trajectory_prefix_index")? == 1);
        ensure!(self.count("trajectory_events")? == started_events + 1);
        ensure!(self.cell("trajectory_requests", "status")? == Some("failed"));
        ensure!(
            self.cell("trajectory_requests", "settlement_event_id")?
                .is_some()
        );
        for table in [
            "trajectory_outbox",
            "eval_subjects",
            "eval_results",
            "eval_admission_events",
        ] {
            ensure!(self.count(table)? == 1, "incomplete {table} closure");
        }
        ensure!(self.count("eval_snapshots")? == 0);
        ensure!(
            self.cell("trajectory_requests", "settlement_outbox_id")?
                == self.cell("trajectory_outbox", "outbox_id")?
        );
        ensure!(self.cell("trajectory_outbox", "attempts")? == Some("1"));
        ensure!(
            self.cell("trajectory_outbox", "delivered_at")?
                == self.cell("trajectory_outbox", "created_at")?
        );
        ensure!(self.cell("trajectory_outbox", "delivered_at")?.is_some());
        ensure!(self.cell("eval_results", "eval_id")? == self.cell("eval_subjects", "eval_id")?);
        ensure!(
            self.cell("eval_admission_events", "result_id")?
                == self.cell("eval_results", "result_id")?
        );
        ensure!(self.cell("eval_admission_events", "status")? == Some("admitted"));
        ensure!(self.cell("eval_admission_events", "reason")? == Some("admitted"));
        ensure!(self.cell("eval_results", "authority_id")? == Some("bitrouter.builtin"));
        Ok(())
    }
}
