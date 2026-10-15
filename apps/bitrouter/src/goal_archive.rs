//! An archive is evidence only when its exact content matches a committed receipt.
use anyhow::Result;
use bitrouter_orchestrator::goal::decision::PermissionDecision;
use bitrouter_orchestrator::goal::workflow::{
    ArtifactRef, Change, Execution, archive_operation_id,
};
use bitrouter_orchestrator::goal::{GoalOperation, OperationKind};
use sha2::{Digest, Sha256};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    goal_id: String,
    owner: String,
    task_id: String,
    pub(crate) execution: Execution,
    pub(crate) decisions: Vec<PermissionDecision>,
    pub(crate) previous: Option<ArtifactRef>,
}

pub(crate) fn decode(
    goal_id: &str,
    task_id: &str,
    reference: &ArtifactRef,
    bytes: &[u8],
    operation: &GoalOperation,
) -> Result<Record> {
    reference.validate()?;
    anyhow::ensure!(
        bytes.len() <= bitrouter_orchestrator::goal::MAX_RECORD_BYTES
            && u64::try_from(bytes.len())? == reference.bytes
            && hex::encode(Sha256::digest(bytes)) == reference.sha256,
        "archive content identity mismatch"
    );
    let record: Record = serde_json::from_slice(bytes)?;
    anyhow::ensure!(
        record.goal_id == goal_id && record.owner == "local" && record.task_id == task_id,
        "archive scope does not match the opened Goal and task"
    );
    anyhow::ensure!(
        operation.id == archive_operation_id(goal_id, &record.execution.id)
            && operation.goal_id == goal_id,
        "archive receipt identity mismatch"
    );
    let OperationKind::Workflow { change } = &operation.kind else {
        anyhow::bail!("archive receipt has a different operation kind")
    };
    let Change::ArchiveExecution {
        task_id: saved_task,
        execution,
        decisions,
        record: saved_record,
    } = change.as_ref()
    else {
        anyhow::bail!("archive receipt has a different workflow change")
    };
    anyhow::ensure!(
        saved_task == task_id
            && **execution == record.execution
            && *decisions == record.decisions
            && saved_record == reference,
        "archive material does not match its complete committed record"
    );
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_content_needs_the_exact_goal_task_and_archive_receipt() -> Result<()> {
        let execution: Execution = serde_json::from_value(serde_json::json!({
            "id":"old","workspace":"/work","requirements_revision":1,"task_revision":1,
            "observation":{"state":"failed","run_id":"run","reason":"failed"}
        }))?;
        let material = serde_json::json!({"goal_id":"goal","owner":"local","task_id":"left",
            "execution":execution,"decisions":[],"previous":null});
        let bytes = serde_json::to_vec(&material)?;
        let reference = ArtifactRef {
            path: "/archive".into(),
            sha256: hex::encode(Sha256::digest(&bytes)),
            bytes: u64::try_from(bytes.len())?,
        };
        let operation = GoalOperation {
            id: archive_operation_id("goal", "old"),
            fingerprint: "f".repeat(64),
            goal_id: "goal".into(),
            revision: 3,
            requirements_revision: 2,
            accepted_at_ms: 10,
            kind: OperationKind::Workflow {
                change: Box::new(Change::ArchiveExecution {
                    task_id: "left".into(),
                    execution: Box::new(execution.clone()),
                    decisions: vec![],
                    record: reference.clone(),
                }),
            },
        };
        assert_eq!(
            decode("goal", "left", &reference, &bytes, &operation)?.execution,
            execution
        );
        assert!(decode("other", "left", &reference, &bytes, &operation).is_err());
        assert!(decode("goal", "other", &reference, &bytes, &operation).is_err());
        assert!(decode("goal", "left", &reference, b"changed", &operation).is_err());
        let mut unrelated = operation.clone();
        unrelated.id = "not-the-archive-receipt".into();
        assert!(decode("goal", "left", &reference, &bytes, &unrelated).is_err());
        let mut unrelated = operation;
        let OperationKind::Workflow { change } = &mut unrelated.kind else {
            anyhow::bail!("test receipt missing")
        };
        let Change::ArchiveExecution { execution, .. } = change.as_mut() else {
            anyhow::bail!("test archive missing")
        };
        execution.workspace = "/another-workspace".into();
        assert!(decode("goal", "left", &reference, &bytes, &unrelated).is_err());
        Ok(())
    }
}
