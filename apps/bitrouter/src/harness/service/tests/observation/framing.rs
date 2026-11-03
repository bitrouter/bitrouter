use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use bitrouter_ai::core::checkpoint::{BatchIdentity, CheckpointBatch};

#[tokio::test]
async fn mixed_transaction_keeps_framed_bytes_and_snapshot_cursors()
-> Result<(), Box<dyn std::error::Error>> {
    for page_bytes in [1024, 4096, 4 * 1024 * 1024] {
        let workspace = TempDir::new()?;
        let memory = Arc::new(MemoryExecutionStore::default());
        let service = ThreadService::with_limits_and_store(
            app(vec![])?,
            &[workspace.path().into()],
            RuntimeLimits {
                recovery_page_bytes: page_bytes,
                ..crate::harness::service::default_runtime_limits()
            },
            memory.clone(),
        )?;
        let thread = service
            .create_thread(
                &service.inner.instance_id,
                thread_request(&workspace, "framing"),
            )
            .await?;
        let stored = memory
            .load(&thread.thread_id)
            .await?
            .ok_or("created Thread missing")?;
        let created = stored
            .records
            .iter()
            .find(|record| matches!(record, ExecutionRecord::ThreadCreated { .. }))
            .ok_or("creation fact missing")?
            .clone();
        // This opaque body tests storage framing, not checkpoint admission.
        // Native execution/recovery tests separately exercise valid Core state.
        let checkpoint = ExecutionRecord::TurnRecord {
            turn_id: "turn".into(),
            fact: Box::new(ExecutionRecord::CoreCheckpoint {
                batch: CheckpointBatch {
                    identity: BatchIdentity {
                        batch_id: "batch".into(),
                        session_id: "session".into(),
                        core_instance_id: "core".into(),
                        execution_epoch: 1,
                    },
                    payload_encoding: "base64-json".into(),
                    payload_bytes: STANDARD.encode(vec![b'x'; 96 * 1024]),
                    payload_sha256: "a".repeat(64),
                },
                limits: Default::default(),
            }),
        };
        let facts = vec![
            created,
            checkpoint,
            ExecutionRecord::ThreadCheckpoint {
                snapshot: thread.clone(),
                messages: vec![],
            },
        ];
        let original = serde_json::to_value(&facts)?;
        let overflow = service
            .thread_transaction(&thread.thread_id, u64::MAX, &facts, None)
            .err()
            .ok_or("invalid transaction accepted")?;
        if page_bytes == 1024 {
            // Framing refusal precedes cursor arithmetic, as before.
            assert!(overflow.to_string().contains("bounded record framing"));
            service.shutdown().await;
            continue;
        }
        assert!(overflow.to_string().contains("Thread cursor exhausted"));
        let (framed, event) =
            service.thread_transaction(&thread.thread_id, stored.version, &facts, None)?;
        assert_eq!(event.seq, stored.version + u64::try_from(framed.len())?);
        assert_eq!(framed.len() > facts.len() + 1, page_bytes == 4096);
        assert_eq!(serde_json::to_value(&facts)?, original);
        assert_eq!(event.changes.len(), 2);
        assert!(matches!(&event.changes[0], ThreadChange::Created { view }
            if view.thread.cursor == event.seq));
        assert!(
            matches!(&event.changes[1], ThreadChange::Checkpoint { snapshot }
            if snapshot.cursor == event.seq)
        );

        let mut reader = crate::harness::service::checkpoint_pages::Reader::default();
        let mut restored = Vec::new();
        for record in framed {
            if let Some(record) = reader.consume(record)? {
                restored.push(record);
            }
        }
        assert!(!reader.incomplete());
        assert_eq!(restored.len(), facts.len() + 1);
        assert_eq!(serde_json::to_value(&restored[1])?, original[1]);
        for index in [0, 2] {
            let value = serde_json::to_value(&restored[index])?;
            let mut expected = original[index].clone();
            expected["snapshot"]["cursor"] = serde_json::json!(event.seq);
            assert_eq!(value, expected);
        }
        let ExecutionRecord::ThreadEvent { event: saved } = &restored[3] else {
            return Err("transaction event missing".into());
        };
        assert_eq!(serde_json::to_value(saved)?, serde_json::to_value(&event)?);
        assert_eq!(
            memory
                .load(&thread.thread_id)
                .await?
                .ok_or("Thread lost")?
                .version,
            stored.version,
        );
        service.shutdown().await;
    }
    Ok(())
}
