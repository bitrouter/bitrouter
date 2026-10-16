//! Real local IPC exercises the same Goal service assembled by the host.
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bitrouter_orchestrator::goal::{
    GoalCommand, GoalError, GoalReply, GoalService, PROTOCOL_VERSION,
};
use bitrouter_sdk::App;

use super::{
    AcpControlPlane, DaemonCommand, DaemonResponse, LocalControlOptions, NoopObserveStatus,
    NoopReloader, bind_control_socket, run_bound_control_socket, send_command,
};

#[tokio::test]
async fn goals_local_ipc_retries_original_operation_and_rejects_wrong_protocol() -> Result<()> {
    let home = tempfile::tempdir()?;
    let socket = home.path().join("goals.sock");
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let goals = Arc::new(GoalService::new(Arc::new(
        crate::goal_store::DatabaseGoalStore::new(db.clone()),
    )));
    let bound = bind_control_socket(&socket).await?;
    let running = tokio::spawn(run_bound_control_socket(
        bound,
        Arc::new(App::builder().build()?),
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
    let request = GoalCommand::Create {
        operation_id: "ipc-create".into(),
        title: "Local Goal".into(),
        workspace: home.path().into(),
        objective: "Produce a verified patch".into(),
        budget_limits: Default::default(),
    };
    let wrong = send_command(
        &socket,
        &DaemonCommand::Goals {
            protocol: PROTOCOL_VERSION - 1,
            command: request.clone(),
        },
    )
    .await?;
    assert!(matches!(
        wrong,
        DaemonResponse::Goals {
            response: Err(GoalError::Invalid { .. }),
            ..
        }
    ));
    let command = DaemonCommand::Goals {
        protocol: PROTOCOL_VERSION,
        command: request,
    };
    let first = send_command(&socket, &command).await?;
    let retry = send_command(&socket, &command).await?;
    assert_eq!(serde_json::to_value(&first)?, serde_json::to_value(&retry)?);
    let goal_id = match first {
        DaemonResponse::Goals {
            response: Ok(GoalReply::Accepted { operation }),
            ..
        } => operation.goal_id,
        other => anyhow::bail!("unexpected Goal response: {other:?}"),
    };
    let read = send_command(
        &socket,
        &DaemonCommand::Goals {
            protocol: PROTOCOL_VERSION,
            command: GoalCommand::Get { goal_id },
        },
    )
    .await?;
    assert!(
        matches!(read, DaemonResponse::Goals { response: Ok(GoalReply::Snapshot { goal }), .. } if goal.revision == 1)
    );
    assert!(matches!(
        send_command(&socket, &DaemonCommand::Stop).await?,
        DaemonResponse::Ok
    ));
    tokio::time::timeout(Duration::from_secs(3), running)
        .await
        .context("local Goal service did not stop")???;
    Ok(())
}
