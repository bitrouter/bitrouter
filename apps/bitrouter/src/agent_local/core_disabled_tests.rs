//! Local framing and dispatch must retain inspection without native execution.

use std::time::Duration;

use super::*;
use crate::harness::service::core_disabled_tests::offline_fixture;
use crate::harness::storage::ExecutionStore;
use bitrouter_ai::harness::thread::ThreadStatus;

async fn exchange_fixture(service: &ThreadService, operation: Operation) -> Result<ThreadReply> {
    let command = ThreadCommand {
        version: CONTRACT_VERSION,
        command_id: "offline-command".into(),
        server_instance_id: Some(service.capabilities().server_instance_id),
        operation,
    };
    let mut bytes = serde_json::to_vec(&command)?;
    bytes.push(b'\n');
    let (mut client, stream) = tokio::io::duplex(64 * 1024);
    tokio::time::timeout(Duration::from_secs(3), async {
        let exchange = async move {
            client.write_all(&bytes).await?;
            let mut line = String::new();
            BufReader::new(client).read_line(&mut line).await?;
            Ok::<ThreadReply, anyhow::Error>(serde_json::from_str(&line)?)
        };
        let (served, reply) = tokio::join!(serve_connection(stream, service.clone()), exchange);
        served?;
        reply
    })
    .await?
}

#[tokio::test]
async fn offline_local_observation_and_capabilities_do_not_claim_execution()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = offline_fixture(CallerContext::local()).await?;
    let before = serde_json::to_vec(&fixture.store.load(&fixture.target.thread_id).await?)?;
    let reply = exchange_fixture(&fixture.service, Operation::Capabilities).await?;
    let ReplyResult::Capabilities {
        operations,
        runtime,
    } = reply.result
    else {
        return Err("capabilities reply missing".into());
    };
    assert!(!runtime.native_execution);
    assert_eq!(
        operations,
        [
            "list_threads",
            "read_thread",
            "read_turn",
            "history",
            "observe",
            "unload_thread"
        ]
    );
    for operation in [
        Operation::CreateThread {
            workspace: fixture.workspace.path().join("missing"),
            model: "fixture".into(),
            model_mode: Default::default(),
            effort: None,
            max_output_tokens: None,
            read_only: true,
            verification_command: None,
            idempotency_key: "offline-create".into(),
        },
        Operation::StartTurn {
            thread_id: "absent".into(),
            prompt: "blocked".into(),
            idempotency_key: "offline-start".into(),
        },
        Operation::ResumeQueue {
            thread_id: fixture.target.thread_id.clone(),
            idempotency_key: "offline-resume".into(),
        },
    ] {
        assert!(matches!(
            exchange_fixture(&fixture.service, operation).await?.result,
            ReplyResult::Error {
                code: ErrorCode::UnsupportedCapability,
                ..
            }
        ));
    }
    let reply = exchange_fixture(
        &fixture.service,
        Operation::Observe {
            thread_id: fixture.target.thread_id.clone(),
            after: None,
        },
    )
    .await?;
    let ReplyResult::Observation { observation } = reply.result else {
        return Err("stored Thread observation was not delivered".into());
    };
    assert!(
        matches!(*observation, ThreadObservation::Snapshot { view, .. }
        if view.thread.status == ThreadStatus::RecoveryRequired && view.thread.queued.len() == 1)
    );
    let reply = exchange_fixture(
        &fixture.service,
        Operation::UnloadThread {
            thread_id: fixture.target.thread_id.clone(),
        },
    )
    .await?;
    assert!(matches!(reply.result, ReplyResult::Ok));
    fixture.service.shutdown().await;
    assert_eq!(
        before,
        serde_json::to_vec(&fixture.store.load(&fixture.target.thread_id).await?)?
    );
    assert_eq!(std::fs::read_dir(fixture.workspace.path())?.count(), 0);
    Ok(())
}
