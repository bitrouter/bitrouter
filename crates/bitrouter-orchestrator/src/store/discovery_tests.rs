use super::*;
fn fact() -> ExecutionRecord {
    ExecutionRecord::Settled {
        outcome: None,
        messages: Vec::new(),
        context_version: 0,
        model_steps: 0,
        tool_calls: 0,
        estimated_spend_microusd: 0,
        active_duration_ms: 0,
    }
}
#[tokio::test]
async fn root_index_is_atomic_bounded_and_has_a_fixed_membership_cutoff() -> Result<(), String> {
    let store = MemoryExecutionStore::default();
    store.commit("z-root", 0, &[fact()]).await?;
    store.commit("a-root", 0, &[fact()]).await?;
    let first = store.read_index(0, None, 1, 1024).await?;
    assert_eq!(first.entries.len(), 1);
    assert_eq!(first.entries[0].execution_id, "z-root");
    let after = first.next_after.ok_or("more root missing")?;
    store.commit("new-root", 0, &[fact()]).await?;
    store.commit("a-root", 1, &[fact()]).await?;
    let next = store.read_index(after, Some(first.cutoff), 1, 1024).await?;
    assert_eq!(next.entries[0].execution_id, "a-root");
    assert_eq!(next.entries[0].version, 2); // A head is not a mutable journal snapshot.
    assert!(next.next_after.is_none());
    assert!(store.read_index(0, None, 1, 1).await.is_err());
    assert!(store.read_index(0, None, 0, 1024).await.is_err());
    assert!(
        store
            .read_index(first.cutoff + 1, Some(first.cutoff), 1, 1024)
            .await
            .is_err()
    );
    assert!(store.commit("failed-root", 1, &[fact()]).await.is_err());
    let all = store.read_index(0, None, 128, 4096).await?;
    assert_eq!(all.entries.len(), 3);
    assert!(
        !all.entries
            .iter()
            .any(|entry| entry.execution_id == "failed-root")
    );
    Ok(())
}
