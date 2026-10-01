//! App-owned transactional storage for the BRO native execution runtime.

use bitrouter_orchestrator::store::{ExecutionRecord, ExecutionStore, StoredExecution};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set,
    TransactionTrait,
};

pub struct DatabaseExecutionStore {
    db: DatabaseConnection,
}

impl DatabaseExecutionStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

mod execution {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "bro_executions")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub version: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod record {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "bro_execution_records")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub execution_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub sequence: i64,
        pub payload: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

#[async_trait::async_trait]
impl ExecutionStore for DatabaseExecutionStore {
    async fn commit(
        &self,
        execution_id: &str,
        expected_version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        if records.is_empty() {
            return Err("empty execution commit".into());
        }
        let expected = i64::try_from(expected_version).map_err(|error| error.to_string())?;
        let count = i64::try_from(records.len()).map_err(|error| error.to_string())?;
        let version = expected
            .checked_add(count)
            .ok_or("execution version exhausted")?;
        let payloads = records
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        let transaction = self.db.begin().await.map_err(|error| error.to_string())?;
        if expected == 0 {
            execution::ActiveModel {
                id: Set(execution_id.into()),
                version: Set(version),
            }
            .insert(&transaction)
            .await
            .map_err(|error| format!("create execution: {error}"))?;
        } else {
            let updated = execution::Entity::update_many()
                .col_expr(
                    execution::Column::Version,
                    sea_orm::sea_query::Expr::value(version),
                )
                .filter(execution::Column::Id.eq(execution_id))
                .filter(execution::Column::Version.eq(expected))
                .exec(&transaction)
                .await
                .map_err(|error| error.to_string())?;
            if updated.rows_affected != 1 {
                return Err("execution commit version conflict".into());
            }
        }
        for (offset, payload) in payloads.into_iter().enumerate() {
            let offset = i64::try_from(offset).map_err(|error| error.to_string())?;
            record::ActiveModel {
                execution_id: Set(execution_id.into()),
                sequence: Set(expected + offset + 1),
                payload: Set(payload),
            }
            .insert(&transaction)
            .await
            .map_err(|error| error.to_string())?;
        }
        transaction
            .commit()
            .await
            .map_err(|error| error.to_string())?;
        u64::try_from(version).map_err(|error| error.to_string())
    }

    async fn load(&self, execution_id: &str) -> Result<Option<StoredExecution>, String> {
        let transaction = self.db.begin().await.map_err(|error| error.to_string())?;
        let Some(execution) = execution::Entity::find_by_id(execution_id)
            .one(&transaction)
            .await
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        let rows = record::Entity::find()
            .filter(record::Column::ExecutionId.eq(execution_id))
            .order_by_asc(record::Column::Sequence)
            .all(&transaction)
            .await
            .map_err(|error| error.to_string())?;
        let version = u64::try_from(execution.version).map_err(|error| error.to_string())?;
        if u64::try_from(rows.len()).map_err(|error| error.to_string())? != version {
            return Err("execution record history is incomplete".into());
        }
        let records = rows
            .into_iter()
            .enumerate()
            .map(|(offset, row)| {
                if row.sequence != i64::try_from(offset).map_err(|error| error.to_string())? + 1 {
                    return Err("execution record sequence is not contiguous".into());
                }
                serde_json::from_str(&row.payload).map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, String>>()?;
        transaction
            .commit()
            .await
            .map_err(|error| error.to_string())?;
        Ok(Some(StoredExecution {
            execution_id: execution_id.into(),
            version,
            records,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settled() -> ExecutionRecord {
        ExecutionRecord::Settled {
            messages: Vec::new(),
            model_steps: 1,
            tool_calls: 0,
            estimated_spend_microusd: 0,
            active_duration_ms: 1,
        }
    }

    #[tokio::test]
    async fn committed_records_survive_reopen_and_conflicts_do_not_append() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let url = format!("sqlite://{}/runtime.db", directory.path().display());
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        let store = DatabaseExecutionStore::new(db.clone());
        assert_eq!(
            store
                .commit("execution", 0, &[settled()])
                .await
                .map_err(anyhow::Error::msg)?,
            1
        );
        assert!(store.commit("execution", 0, &[settled()]).await.is_err());
        assert!(store.commit("execution", 7, &[settled()]).await.is_err());
        assert_eq!(
            store
                .commit("execution", 1, &[settled(), settled()])
                .await
                .map_err(anyhow::Error::msg)?,
            3
        );
        drop(store);
        db.close().await?;
        let reopened = DatabaseExecutionStore::new(crate::db::connect(&url).await?);
        let stored = reopened
            .load("execution")
            .await
            .map_err(anyhow::Error::msg)?
            .ok_or_else(|| anyhow::anyhow!("missing execution"))?;
        assert_eq!(stored.version, 3);
        assert_eq!(stored.records.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn crash_record_writer() -> anyhow::Result<()> {
        let Ok(url) = std::env::var("BRO_TEST_EXECUTION_STORE_URL") else {
            return Ok(());
        };
        let store = DatabaseExecutionStore::new(crate::db::connect(&url).await?);
        store
            .commit("crash-execution", 0, &[settled()])
            .await
            .map_err(anyhow::Error::msg)?;
        use std::io::Write;
        println!("BRO_COMMITTED");
        std::io::stdout().flush()?;
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        Ok(())
    }

    #[tokio::test]
    async fn committed_batch_survives_abrupt_process_loss() -> anyhow::Result<()> {
        use tokio::io::AsyncBufReadExt;
        let directory = tempfile::tempdir()?;
        let url = format!("sqlite://{}/crash.db", directory.path().display());
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        db.close().await?;
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "agent_store::tests::crash_record_writer",
                "--nocapture",
            ])
            .env("BRO_TEST_EXECUTION_STORE_URL", &url)
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("child stdout missing"))?;
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(line) = lines.next_line().await? {
                if line == "BRO_COMMITTED" {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            anyhow::bail!("child exited before commit acknowledgement")
        })
        .await??;
        child.kill().await?;
        let reopened = DatabaseExecutionStore::new(crate::db::connect(&url).await?);
        let saved = reopened
            .load("crash-execution")
            .await
            .map_err(anyhow::Error::msg)?
            .ok_or_else(|| anyhow::anyhow!("committed batch lost after process kill"))?;
        assert_eq!(saved.version, 1);
        assert!(matches!(
            saved.records.as_slice(),
            [ExecutionRecord::Settled { .. }]
        ));
        Ok(())
    }
}
