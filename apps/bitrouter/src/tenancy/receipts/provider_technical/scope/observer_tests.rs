//! Value/mechanism controls only; no admission or provider authority.
use super::*;
use anyhow::{Context, Result, ensure};
#[test]
fn observer_floor_and_busy_duplicate_missing_end_do_not_invalidate_original_scope() -> Result<()> {
    use std::time::Duration;
    ensure!(
        super::super::json_millis(Duration::ZERO) == 0
            && super::super::json_millis(Duration::from_nanos(1)) == 0
            && super::super::json_millis(Duration::MAX) == u64::MAX
    );
    for mode in ["busy", "duplicate", "missing-end"] {
        let capture = Arc::new(Capture::default());
        let hop = capture
            .begin_hop(HopProfile::CoreJsonHopCallbacksV1)
            .context("hop")?;
        hop.armed(&"attempt".try_into()?);
        hop.observer_start();
        match mode {
            "busy" => {
                let held = hop
                    .observer
                    .clock
                    .lock()
                    .map_err(|_| anyhow::anyhow!("unit observer lock"))?;
                hop.observer_end(Some(HopEnd::Completed));
                drop(held);
            }
            "duplicate" => hop.observer_start(),
            _ => {}
        }
        hop.finish(
            HopProfile::CoreJsonHopCallbacksV1,
            HopEnd::Completed,
            77,
            false,
        );
        ensure!(capture.healthy());
        let snapshot = capture
            .scope_snapshot()
            .context("old scope remains usable")?;
        ensure!(snapshot.hops[0].duration_ms == Some(77) && snapshot.observers.is_empty());
    }
    Ok(())
}
#[test]
fn real_observer_cell_keeps_auth_group_and_clears_success_open_without_second_sample() -> Result<()>
{
    for opened in [false, true] {
        let capture = Arc::new(Capture::default());
        let hop = capture
            .begin_hop(if opened {
                HopProfile::CoreStreamOpenCallbacksV1
            } else {
                HopProfile::CoreJsonHopCallbacksV1
            })
            .context("hop")?;
        hop.armed(&"auth-a".try_into()?);
        hop.armed(&"auth-b".try_into()?);
        hop.observer_start();
        hop.observer_end(if opened { None } else { Some(HopEnd::Failed) });
        hop.finish(
            if opened {
                HopProfile::CoreStreamHopNotifyV1
            } else {
                HopProfile::CoreJsonHopCallbacksV1
            },
            HopEnd::Failed,
            999,
            false,
        );
        let snapshot = capture.scope_snapshot().context("snapshot")?;
        ensure!(snapshot.observers.len() == usize::from(!opened));
        if !opened {
            ensure!(
                snapshot.observers[0].attempt_ids == snapshot.hops[0].attempt_ids
                    && snapshot.observers[0].closing_attempt_id.as_str() == "auth-b"
            );
        }
    }
    Ok(())
}
