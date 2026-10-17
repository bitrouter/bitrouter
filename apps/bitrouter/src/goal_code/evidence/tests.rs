//! Actual SQLite/IPC reads of domain fixtures, not harness or model-quality proof.
use super::*;
use bitrouter_orchestrator::goal::OperationKind;
use bitrouter_orchestrator::goal::decision::PermissionDecision;
use bitrouter_orchestrator::goal::workflow::{Change, Execution};
use bitrouter_orchestrator::goal::{GoalService, GoalStore};
use std::sync::Arc;

#[tokio::test]
async fn archive_inspector_reads_committed_chain_and_rejects_uncommitted_or_foreign_material()
-> Result<()> {
    use crate::daemon::{
        AcpControlPlane, DaemonCommand, DaemonResponse, LocalControlOptions, NoopObserveStatus,
        NoopReloader,
    };
    let temporary = tempfile::tempdir()?;
    let socket = temporary.path().join("archive.sock");
    let root = temporary.path().join("records");
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = Arc::new(crate::goal_store::DatabaseGoalStore::new(db.clone()));
    let goals = Arc::new(GoalService::new(store.clone()));
    let bound = crate::daemon::bind_control_socket(&socket).await?;
    let running = tokio::spawn(crate::daemon::run_bound_control_socket(
        bound,
        Arc::new(bitrouter_sdk::App::builder().build()?),
        "127.0.0.1:0".into(),
        Arc::new(NoopReloader),
        Arc::new(NoopObserveStatus { compiled_in: false }),
        AcpControlPlane {
            runtime: Arc::new(crate::acp_runtime::AcpRuntime::new()),
            metering: crate::metering::MeteringStore::new(db),
            inventory: None,
            evolution: None,
        },
        LocalControlOptions {
            administration: None,
            threads: None,
            goals: Some(goals),
            gate: crate::daemon_handoff::HandoffGate::default(),
            cli_owned: false,
        },
    ));
    let client = Client::new(socket.clone());
    let GoalReply::Accepted { operation } = client
        .request(GoalCommand::Create {
            operation_id: "archive-browser-create".into(),
            title: "Archive reader domain fixture".into(),
            workspace: temporary.path().into(),
            objective: "Read existing evidence; no harness is configured".into(),
            budget_limits: Default::default(),
        })
        .await?
    else {
        anyhow::bail!("missing fixture Goal receipt")
    };
    let goal_id = operation.goal_id;
    let material = crate::goal_artifacts::put(&root, b"original scoped tool evidence").await?;
    let decision = PermissionDecision {
        id: "retired-decision".into(),
        execution_id: "original-1".into(),
        run_id: Some("domain-run".into()),
        permission_id: "domain-permission".into(),
        requirements_revision: 1,
        task_revision: 1,
        request: material.clone(),
        options: vec![bitrouter_orchestrator::goal::decision::PermissionOption {
            id: "once".into(),
            label: "Original allow once".into(),
            allows: true,
            persistent: false,
        }],
        state: bitrouter_orchestrator::goal::decision::PermissionState::Obsolete {
            reason: "Retained domain fixture".into(),
        },
    };
    let mut previous = None;
    let mut references = Vec::new();
    for index in 0..2 {
        let execution: Execution = serde_json::from_value(serde_json::json!({
            "id":format!("original-{index}"), "workspace":temporary.path(),
            "requirements_revision":1,"task_revision":1,"dispatch_requested":true,
            "observation":{"state":"candidate","run_id":"domain-run","native_session_id":null,"report":material},
            "stop":{"state":"confirmed","operation_id":"domain-stop","detail":"Domain fixture stop"},
            "reconciliation":{"state":"verified","task_revision":1,"control_source_revision":1,"workspace":material,"transcript":material},
        }))?;
        let decisions = if index == 1 {
            vec![decision.clone()]
        } else {
            Vec::new()
        };
        let bytes = serde_json::to_vec(
            &serde_json::json!({"goal_id":goal_id,"owner":"local","task_id":"left",
            "execution":execution,"decisions":decisions,"previous":previous}),
        )?;
        let reference = crate::goal_artifacts::put(&root, &bytes).await?;
        let mut head = store.get("local", &goal_id).await?;
        let revision = head.revision;
        head.revision += 1;
        // The real store persists a labeled complete domain receipt. Producer
        // eligibility/atomic retirement are exercised by goals/archive.rs.
        store
            .commit(
                Some(revision),
                head.clone(),
                GoalOperation {
                    id: archive_operation_id(&goal_id, &execution.id),
                    fingerprint: format!("domain-archive-{index}"),
                    goal_id: goal_id.clone(),
                    revision: head.revision,
                    requirements_revision: head.requirements_revision,
                    accepted_at_ms: 100,
                    kind: OperationKind::Workflow {
                        change: Box::new(Change::ArchiveExecution {
                            task_id: "left".into(),
                            execution: Box::new(execution),
                            decisions,
                            record: reference.clone(),
                        }),
                    },
                },
            )
            .await?;
        previous = Some(reference.clone());
        references.push(reference);
    }
    let before = store.get("local", &goal_id).await?;
    let target = |reference: ArtifactRef| projection::Target::Archive {
        task_id: "left".into(),
        reference,
    };
    let Read::Archive { detail, artifacts } = inspect(
        &client,
        &root,
        &goal_id,
        &target(references[1].clone()),
        false,
    )
    .await?
    else {
        anyhow::bail!("archive was not opened as read-only detail")
    };
    assert_eq!(detail.title, "Archived execution original-1");
    assert!(
        detail
            .details
            .iter()
            .any(|line| line.contains("retired-decision"))
    );
    assert_eq!(detail.artifacts[0].id, references[0].sha256);
    let older_target = artifacts
        .get(&(goal_id.clone(), references[0].sha256.clone()))
        .context("previous archive link missing")?;
    let Read::Archive { detail: older, .. } =
        inspect(&client, &root, &goal_id, older_target, false).await?
    else {
        anyhow::bail!("previous archive did not open")
    };
    assert_eq!(older.title, "Archived execution original-0");
    let request_target = artifacts
        .get(&(goal_id.clone(), material.sha256.clone()))
        .context("original permission/effect reference missing")?;
    assert!(
        matches!(inspect(&client,&root,&goal_id,request_target,false).await?,Read::Text(text) if text == "original scoped tool evidence")
    );
    assert!(
        inspect(
            &client,
            &root,
            "foreign-goal",
            &target(references[1].clone()),
            false
        )
        .await
        .is_err()
    );
    let wrong_task = projection::Target::Archive {
        task_id: "right".into(),
        reference: references[1].clone(),
    };
    assert!(
        inspect(&client, &root, &goal_id, &wrong_task, false)
            .await
            .is_err()
    );
    let original_bytes = crate::goal_artifacts::read(&root, &references[1]).await?;
    let mut counterfeit: serde_json::Value = serde_json::from_slice(&original_bytes)?;
    counterfeit["execution"]["workspace"] =
        serde_json::json!(temporary.path().join("different-source"));
    let altered = crate::goal_artifacts::put(&root, &serde_json::to_vec(&counterfeit)?).await?;
    assert!(
        inspect(&client, &root, &goal_id, &target(altered), false)
            .await
            .is_err()
    );
    counterfeit["execution"]["id"] = serde_json::json!("uncommitted-original");
    let orphan = crate::goal_artifacts::put(&root, &serde_json::to_vec(&counterfeit)?).await?;
    assert!(
        inspect(&client, &root, &goal_id, &target(orphan), false)
            .await
            .is_err()
    );
    tokio::fs::write(&references[1].path, b"tampered original bytes").await?;
    assert!(
        inspect(
            &client,
            &root,
            &goal_id,
            &target(references[1].clone()),
            false
        )
        .await
        .is_err()
    );
    assert_eq!(store.get("local", &goal_id).await?, before);
    assert_eq!(
        store
            .history("local", &goal_id, 0, None, 16)
            .await?
            .operations
            .len(),
        3
    );
    assert!(matches!(
        crate::daemon::send_command(&socket, &DaemonCommand::Stop).await?,
        DaemonResponse::Ok
    ));
    tokio::time::timeout(Duration::from_secs(3), running)
        .await
        .context("archive reader fixture did not stop")???;
    Ok(())
}
