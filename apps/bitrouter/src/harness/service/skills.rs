//! Native Thread skill inspection over bounded original store pages. This API
//! has no App, resource host, owner claim, workspace lease or execution path.

use crate::gateway::caller::CallerContext;
use serde::Serialize;

use super::checkpoint_pages::Reader;
use crate::harness::agent::native::Saved;
use crate::harness::storage::{ExecutionRecord, ExecutionStore};
use crate::storage::skills::explain::{Explanation, capture};

#[cfg(test)]
mod tests;

#[derive(Debug, Serialize)]
pub struct ThreadExplanation {
    pub thread_id: String,
    pub turn_id: String,
    pub recorded_server_instance_id: String,
    pub cutoff: u64,
    pub explanation: Option<Explanation>,
}

pub async fn inspect(
    store: &dyn ExecutionStore,
    caller: &CallerContext,
    thread_id: &str,
    turn_id: &str,
) -> Result<ThreadExplanation, String> {
    if caller.is_anonymous()
        || thread_id.is_empty()
        || thread_id.len() > 128
        || turn_id.is_empty()
        || turn_id.len() > 128
    {
        return Err(
            "authenticated caller and bounded native Thread/Turn identities required".into(),
        );
    }
    let limits = crate::harness::service::default_runtime_limits();
    let first = store
        .read_records(thread_id, 0, None, 1, limits.recovery_page_bytes)
        .await?
        .ok_or("native Thread is not in the selected history database")?;
    let cutoff = first.cutoff;
    if cutoff == 0
        || cutoff > limits.recovery_records_per_thread
        || first.records.len() != 1
        || first.next_after != (cutoff > 1).then_some(1)
    {
        return Err("invalid or oversized native Thread history".into());
    }
    let Some(ExecutionRecord::ThreadCreated {
        caller: owner,
        snapshot,
        ..
    }) = first.records.first()
    else {
        return Err("native Thread has no original header".into());
    };
    if snapshot.thread_id != thread_id
        || snapshot.server_instance_id.is_empty()
        || owner.api_key_id() != caller.api_key_id()
        || owner.user_id() != caller.user_id()
    {
        return Err(
            "caller does not own this native Thread or its header identity is invalid".into(),
        );
    }
    let recorded_server_instance_id = snapshot.server_instance_id.clone();
    let mut reader = Reader::default();
    let mut saved = None;
    let mut queued_prompt = None;
    let mut active_turn = None;
    let mut run_id = None;
    let mut explanation = None;
    let mut after = 0_u64;
    let mut records = first.records;
    loop {
        for record in records {
            after = after
                .checked_add(1)
                .ok_or("native history cursor exhausted")?;
            let Some(record) = reader.consume(record)? else {
                continue;
            };
            match record {
                ExecutionRecord::TurnQueued {
                    turn_id: queued,
                    prompt,
                    ..
                } if queued == turn_id => {
                    if queued_prompt.replace(prompt).is_some() {
                        return Err("native Turn has duplicate queued input".into());
                    }
                }
                ExecutionRecord::TurnActivated {
                    turn_id: active, ..
                } => active_turn = Some(active),
                ExecutionRecord::TurnRecord {
                    turn_id: recorded_turn,
                    fact,
                } => {
                    if active_turn.as_deref() != Some(recorded_turn.as_str())
                        && !matches!(fact.as_ref(), ExecutionRecord::TurnLifecycle { .. })
                    {
                        return Err("native execution fact has no matching active Turn".into());
                    }
                    if let Some((snapshot, accepted)) = Saved::replay(&mut saved, &fact)?
                        && recorded_turn == turn_id
                    {
                        if accepted {
                            let run = snapshot
                                .run
                                .as_ref()
                                .ok_or("accepted native input has no run")?;
                            if queued_prompt.as_deref() != Some(run.input.text.as_str())
                                || run_id.is_some()
                            {
                                return Err(
                                    "native Core input differs from its original Turn".into()
                                );
                            }
                            run_id = Some(run.run_id.clone());
                        }
                        if let Some(run_id) = &run_id {
                            explanation =
                                Some(capture(&snapshot, run_id).map_err(|error| error.message)?);
                        }
                    }
                }
                _ => {}
            }
        }
        if after == cutoff {
            break;
        }
        if after > cutoff {
            return Err("native history exceeded its fixed cutoff".into());
        }
        let page = store
            .read_records(
                thread_id,
                after,
                Some(cutoff),
                limits.recovery_page_records,
                limits.recovery_page_bytes,
            )
            .await?
            .ok_or("native Thread disappeared during inspection")?;
        let next = after
            .checked_add(page.records.len() as u64)
            .ok_or("native history cursor exhausted")?;
        if page.cutoff != cutoff
            || page.records.is_empty()
            || next > cutoff
            || page.next_after != (next < cutoff).then_some(next)
        {
            return Err("native history changed cutoff or returned an invalid page".into());
        }
        records = page.records;
        tokio::task::yield_now().await;
    }
    if queued_prompt.is_none() {
        return Err("Turn does not belong to the selected native Thread".into());
    }
    if reader.incomplete()
        || saved
            .as_ref()
            .is_some_and(|saved| !saved.complete_projection())
    {
        return Err("native history has incomplete checkpoint/projection evidence".into());
    }
    Ok(ThreadExplanation {
        thread_id: thread_id.into(),
        turn_id: turn_id.into(),
        recorded_server_instance_id,
        cutoff,
        explanation,
    })
}
