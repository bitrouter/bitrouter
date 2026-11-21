//! Store test accessors over the same bounded primitives used by the server.

use super::*;

impl EvalStore {
    pub(crate) async fn subject(&self, eval_id: &str) -> Result<Option<EvalSubject>> {
        self.read_subject(eval_id, None).await
    }

    pub(crate) async fn list_subjects(&self) -> Result<Vec<EvalSubject>> {
        self.list_subjects_in_owner(None).await
    }

    pub(crate) async fn insert_result(
        &self,
        result: &EvaluationResult,
    ) -> Result<ResultInsertOutcome> {
        let owner = self.owner("local")?;
        let write = crate::storage::write::Write::start(&self.storage_budget, false)?;
        let (write, tx) = self.write_transaction(write).await?;
        let result = async {
            let outcome = self
                .insert_result_in(&tx, result, &owner, &mut Budget::new(self))
                .await?;
            Ok(outcome)
        }
        .await;
        write.finish(tx, result).await
    }

    pub(crate) async fn result(&self, result_id: &str) -> Result<Option<StoredEvaluationResult>> {
        let tx = self.transaction(false).await?;
        let mut budget = Budget::new(self);
        let row = io::one(
            &tx,
            self.results().filter(
                result_entity::Column::ResultId.eq(self.key(KeyDomain::EvalResult, result_id)?),
            ),
            &mut budget,
        )
        .await?;
        let result = if let Some(row) = row {
            let value = self.checked_result(&tx, &row, &mut budget).await?;
            ensure!(
                value.result_id == result_id,
                "eval result logical identity conflict"
            );
            Some(value)
        } else {
            None
        };
        tx.commit().await?;
        Ok(result)
    }
}
