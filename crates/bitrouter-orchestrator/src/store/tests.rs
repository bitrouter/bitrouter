use super::*;
use crate::thread::ThreadChange;
use crate::turn::TurnReceipt;

#[tokio::test]
async fn record_pages_bound_bytes_and_count_without_changing_captured_cutoff() -> Result<(), String>
{
    let store = MemoryExecutionStore::default();
    store
        .commit(
            "thread",
            0,
            &[
                event(1, "small"),
                event(2, &"x".repeat(2048)),
                event(3, "small"),
            ],
        )
        .await?;
    let first = store
        .read_records("thread", 0, None, 3, 1000)
        .await?
        .ok_or("missing first page")?;
    assert_eq!(first.cutoff, 3);
    assert_eq!(first.records.len(), 1);
    assert_eq!(first.next_after, Some(1));
    assert!(
        store
            .read_records("thread", 1, Some(3), 3, 1000)
            .await
            .is_err()
    );
    store.commit("thread", 3, &[event(4, "later")]).await?;
    let next = store
        .read_records("thread", 1, Some(first.cutoff), 1, 4096)
        .await?
        .ok_or("missing second page")?;
    assert_eq!(next.cutoff, first.cutoff);
    assert_eq!(next.records.len(), 1);
    assert_eq!(next.next_after, Some(2));
    assert!(
        store
            .read_records("thread", 3, Some(3), 1, 4096)
            .await?
            .ok_or("missing empty page")?
            .records
            .is_empty()
    );
    assert!(
        store
            .read_records("thread", 0, Some(5), 1, 4096)
            .await
            .is_err()
    );
    Ok(())
}

fn event(seq: u64, text: &str) -> ExecutionRecord {
    ExecutionRecord::ThreadEvent {
        event: ThreadEvent {
            server_instance_id: "epoch".into(),
            thread_id: "thread".into(),
            seq,
            timestamp_ms: 1,
            changes: vec![ThreadChange::TurnQueued {
                receipt: TurnReceipt {
                    thread_id: "thread".into(),
                    turn_id: format!("turn-{seq}"),
                    queue_order: seq,
                    status: crate::turn::TurnStatus::Queued,
                },
                user_item_id: format!("user-{seq}"),
                prompt: text.into(),
            }],
        },
    }
}

#[tokio::test]
async fn history_pages_keep_cutoff_and_report_byte_capacity_without_skipping_an_event()
-> Result<(), String> {
    let store = MemoryExecutionStore::default();
    store
        .commit("thread", 0, &[event(1, "first"), event(2, "second")])
        .await?;
    let first = store.thread_history("thread", 0, 2, 1, 4096).await?;
    assert_eq!(first.events.len(), 1);
    assert!(first.more);
    let bytes = serde_json::to_vec(&first.events[0])
        .map_err(|error| error.to_string())?
        .len();
    let one = store.thread_history("thread", 0, 2, 100, bytes).await?;
    assert_eq!(one.events[0].seq, 1);
    assert_eq!(one.events.len(), 1);
    assert!(one.more);
    assert!(
        store
            .thread_history("thread", 0, 2, 100, bytes - 1)
            .await
            .is_err()
    );
    store.commit("thread", 2, &[event(3, "later")]).await?;
    let next = store
        .thread_history("thread", one.events[0].seq, 2, 100, 4096)
        .await?;
    assert_eq!(next.events.len(), 1);
    assert_eq!(next.events[0].seq, 2);
    assert!(!next.more);
    assert!(
        store
            .thread_history("thread", 2, 2, 100, 4096)
            .await?
            .events
            .is_empty()
    );
    assert!(
        store
            .thread_history("thread", 0, 4, 100, 4096)
            .await
            .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn owner_fence_blocks_peers_raw_writes_and_retired_tokens() -> Result<(), String> {
    let store = MemoryExecutionStore::default();
    let OwnerClaim::Acquired { owner: first } = store.claim_owner("first").await? else {
        return Err("initial claim failed".into());
    };
    assert_eq!(
        store.claim_owner("first").await?,
        OwnerClaim::Acquired {
            owner: first.clone()
        }
    );
    assert_eq!(
        store.claim_owner("second").await?,
        OwnerClaim::Blocked {
            owner: first.clone()
        }
    );
    let fact = ExecutionRecord::Settled {
        outcome: None,
        messages: Vec::new(),
        context_version: 0,
        model_steps: 0,
        tool_calls: 0,
        estimated_spend_microusd: 0,
        active_duration_ms: 0,
    };
    assert_eq!(
        store
            .commit_owned(&first, "execution", 0, std::slice::from_ref(&fact))
            .await?,
        1
    );
    assert!(
        store
            .commit("execution", 1, std::slice::from_ref(&fact))
            .await
            .is_err()
    );
    let mut wrong = first.clone();
    wrong.generation += 1;
    assert!(
        store
            .commit_owned(&wrong, "execution", 1, std::slice::from_ref(&fact))
            .await
            .is_err()
    );
    let stopped = store.stop_owner(&first).await?;
    assert!(stopped.stopped_at_ms.is_some());
    assert_eq!(store.read_owner("first").await?, Some(stopped));
    assert!(
        store
            .commit_owned(&first, "execution", 1, std::slice::from_ref(&fact))
            .await
            .is_err()
    );
    let OwnerClaim::Acquired { owner: second } = store.claim_owner("second").await? else {
        return Err("stopped owner did not transfer".into());
    };
    assert_eq!(second.generation, first.generation + 1);
    assert!(store.stop_owner(&first).await.is_err());
    assert!(
        store
            .commit_owned(&first, "execution", 1, std::slice::from_ref(&fact))
            .await
            .is_err()
    );
    assert_eq!(
        store.commit_owned(&second, "execution", 1, &[fact]).await?,
        2
    );
    store.stop_owner(&second).await?;
    assert!(store.claim_owner("first").await.is_err());
    Ok(())
}
